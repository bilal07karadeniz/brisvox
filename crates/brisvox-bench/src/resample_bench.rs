//! Resampler benchmark: cost per 20 ms frame, delay and quality for every rate pair.

use std::time::Duration;

use brisvox_audio::StreamResampler;
use brisvox_audio::analysis::{rms, sine, snr_db};
use brisvox_core::frame::SampleRate;
use brisvox_core::{SharedClock, SystemClock};

use crate::BenchError;

/// The engine's telephony and provider rates.
pub const RATES: [SampleRate; 6] = [
    SampleRate::Hz8000,
    SampleRate::Hz16000,
    SampleRate::Hz22050,
    SampleRate::Hz24000,
    SampleRate::Hz44100,
    SampleRate::Hz48000,
];

/// One rate pair.
#[derive(Debug, Clone, PartialEq)]
pub struct PairReport {
    /// Input rate.
    pub from: SampleRate,
    /// Output rate.
    pub to: SampleRate,
    /// Mean CPU time per 20 ms input frame.
    pub per_frame: Duration,
    /// Filter delay.
    pub delay: Duration,
    /// SNR of a 1 kHz tone.
    pub snr_1k: f64,
    /// SNR of a tone at 0.8 x the lower Nyquist frequency.
    pub snr_edge: f64,
}

fn frames_20ms(rate: SampleRate) -> usize {
    usize::try_from(rate.hz() / 50).unwrap_or(960)
}

/// Streams `input` in 20 ms frames; returns the output and the time spent in `process`.
fn stream(
    rs: &mut StreamResampler,
    input: &[i16],
    clock: &SharedClock,
) -> Result<(Vec<i16>, Duration), BenchError> {
    let frame = frames_20ms(rs.from_rate());
    let mut buf = vec![0i16; rs.max_output_len(frame) + rs.max_output_len(rs.chunk_frames())];
    let mut out = Vec::with_capacity(input.len() * 6);
    let mut spent = Duration::ZERO;
    for piece in input.chunks(frame) {
        let start = clock.now();
        let n = rs
            .process(piece, &mut buf)
            .map_err(|e| BenchError::Config(e.to_string()))?;
        spent += clock.now().saturating_duration_since(start);
        out.extend_from_slice(buf.get(..n).unwrap_or_default());
    }
    Ok((out, spent))
}

/// Measures every pair of [`RATES`] over `seconds` of audio per measurement.
///
/// # Errors
/// [`BenchError::Config`] if a resampler cannot be built or fails.
pub fn run(seconds: usize) -> Result<Vec<PairReport>, BenchError> {
    let clock = SystemClock::shared();
    let mut reports = Vec::new();
    for from in RATES {
        for to in RATES {
            let new =
                || StreamResampler::new(from, to).map_err(|e| BenchError::Config(e.to_string()));
            let n = usize::try_from(from.hz())
                .unwrap_or(0)
                .saturating_mul(seconds);
            let tone = sine(1_000.0, from.hz(), n, 0.5);
            // Warm up, then time a fresh instance (steady state, no allocation per call).
            stream(&mut new()?, &tone, &clock)?;
            let mut rs = new()?;
            let (out, spent) = stream(&mut rs, &tone, &clock)?;
            let frames = u32::try_from(n / frames_20ms(from).max(1))
                .unwrap_or(1)
                .max(1);
            let skip = rs.filter_delay_frames() * 2 + 64;
            let snr_1k = snr_db(&out, 1_000.0, to.hz(), skip);
            let edge = f64::from(from.hz().min(to.hz())) / 2.0 * 0.8;
            let mut rs_edge = new()?;
            let (edge_out, _) = stream(&mut rs_edge, &sine(edge, from.hz(), n, 0.5), &clock)?;
            let snr_edge = snr_db(&edge_out, edge, to.hz(), skip);
            reports.push(PairReport {
                from,
                to,
                per_frame: spent / frames,
                delay: rs.delay(),
                snr_1k,
                snr_edge,
            });
        }
    }
    Ok(reports)
}

/// Alias rejection: level of a tone 1.5x above the output Nyquist after 48 kHz -> 8 kHz, in dB.
pub fn alias_rejection_db() -> Result<f64, BenchError> {
    let mut rs = StreamResampler::new(SampleRate::Hz48000, SampleRate::Hz8000)
        .map_err(|e| BenchError::Config(e.to_string()))?;
    let input = sine(6_000.0, 48_000, 48_000, 0.99);
    let (out, _) = stream(&mut rs, &input, &SystemClock::shared())?;
    let skip = (rs.filter_delay_frames() * 2 + 64).min(out.len());
    Ok(20.0 * (rms(out.get(skip..).unwrap_or_default()) / rms(&input)).log10())
}

/// A markdown report.
pub fn markdown(reports: &[PairReport], alias_db: f64, environment: &[String]) -> String {
    let mut out = String::from("# Streaming resampler\n\n");
    for line in environment {
        out.push_str(&format!("- {line}\n"));
    }
    let alias = if alias_db.is_finite() {
        format!("{alias_db:.1} dB")
    } else {
        "nothing left above the 16-bit quantisation floor (< -96 dB)".to_owned()
    };
    out.push_str(&format!(
        "\n`brisvox_audio::StreamResampler` (rubato `Fft`, one FFT block = 160 samples of the lower rate; windowed sinc for pairs that output 8 kHz), mono \
         PCM16, fed 20 ms frames. Cost = mean time in `process` per 20 ms input frame (release, \
         one warm instance: a best case; with 500 instances cycling through the cache the perf \
         audit measured ~1.5-2x). Identity rows time only the clock reads and a copy. SNR \
         against the best-fit sine after the start-up transient; edge = 0.8 x the lower \
         Nyquist. Alias rejection (48 kHz -> 8 kHz, full-scale 6 kHz tone): {alias}.\n\n\
         | from Hz | to Hz | cost µs/frame | filter delay ms | SNR 1 kHz dB | SNR edge dB |\n\
         |---:|---:|---:|---:|---:|---:|\n"
    ));
    for r in reports {
        out.push_str(&format!(
            "| {} | {} | {:.2} | {:.2} | {:.1} | {:.1} |\n",
            r.from.hz(),
            r.to.hz(),
            r.per_frame.as_secs_f64() * 1e6,
            r.delay.as_secs_f64() * 1e3,
            r.snr_1k,
            r.snr_edge
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_run_measures_every_pair() {
        let reports = run(1).unwrap();
        assert_eq!(reports.len(), RATES.len() * RATES.len());
        assert!(
            reports.iter().all(|r| r.snr_1k > 60.0 && r.snr_edge > 40.0),
            "{reports:?}"
        );
        assert!(alias_rejection_db().unwrap() < -50.0);
        let md = markdown(&reports, -60.0, &[]);
        assert!(md.contains("| 48000 | 16000 |"));
    }
}
