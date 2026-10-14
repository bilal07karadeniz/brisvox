//! Signal analysis for tests and benchmarks: test tones and SNR against a known sine.
//!
//! Not for the hot path (allocates, uses `f64` math per sample).

use std::f64::consts::TAU;

/// `n` samples of a sine at `freq` Hz sampled at `rate` Hz, amplitude `amp` of full scale.
pub fn sine(freq: f64, rate: u32, n: usize, amp: f64) -> Vec<i16> {
    let step = TAU * freq / f64::from(rate);
    (0..n)
        .map(|i| to_i16(amp * (step * i as f64).sin()))
        .collect()
}

/// Full-scale `f64` (-1..1) to PCM16, rounded and clamped.
fn to_i16(x: f64) -> i16 {
    // The clamp makes the cast exact.
    (x * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16
}

/// SNR in dB of `signal` against its best-fitting sine at `freq` Hz (least squares over
/// `a·sin + b·cos + c`): everything that is not that sine counts as noise (aliasing, imaging,
/// quantization, distortion). `skip` leading samples are ignored (filter start-up).
pub fn snr_db(signal: &[i16], freq: f64, rate: u32, skip: usize) -> f64 {
    let w = TAU * freq / f64::from(rate);
    let samples = || {
        signal
            .iter()
            .enumerate()
            .skip(skip)
            .map(|(i, &s)| ((w * i as f64).sin(), (w * i as f64).cos(), f64::from(s)))
    };
    // Normal equations of the fit, written out (3 unknowns: a, b, c).
    let (mut ss, mut sc, mut s1, mut cc, mut c1, mut n) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    let (mut ys, mut yc, mut y1) = (0.0, 0.0, 0.0);
    for (s, c, y) in samples() {
        ss += s * s;
        sc += s * c;
        s1 += s;
        cc += c * c;
        c1 += c;
        n += 1.0;
        ys += y * s;
        yc += y * c;
        y1 += y;
    }
    let det3 = |a: [f64; 3], b: [f64; 3], c: [f64; 3]| {
        a[0] * (b[1] * c[2] - b[2] * c[1]) - a[1] * (b[0] * c[2] - b[2] * c[0])
            + a[2] * (b[0] * c[1] - b[1] * c[0])
    };
    // Rows of the symmetric matrix [[ss, sc, s1], [sc, cc, c1], [s1, c1, n]].
    let d = det3([ss, sc, s1], [sc, cc, c1], [s1, c1, n]);
    if d.abs() < f64::EPSILON {
        return f64::NEG_INFINITY;
    }
    let a = det3([ys, sc, s1], [yc, cc, c1], [y1, c1, n]) / d;
    let b = det3([ss, ys, s1], [sc, yc, c1], [s1, y1, n]) / d;
    let c = det3([ss, sc, ys], [sc, cc, yc], [s1, c1, y1]) / d;
    let (mut signal_energy, mut noise_energy) = (0.0, 0.0);
    for (sin, cos, y) in samples() {
        let fit = a * sin + b * cos + c;
        signal_energy += fit * fit;
        noise_energy += (y - fit) * (y - fit);
    }
    10.0 * (signal_energy / noise_energy.max(1e-12)).log10()
}

/// RMS relative to full scale (a full-scale square wave is 1.0).
pub fn rms(signal: &[i16]) -> f64 {
    let sum: f64 = signal.iter().map(|&s| f64::from(s).powi(2)).sum();
    (sum / signal.len().max(1) as f64).sqrt() / 32_768.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_sine_measures_as_quantization_limited() {
        // 16-bit quantization of a -6 dBFS sine: ~92 dB theoretical (6.02*16 + 1.76 - 6).
        let tone = sine(440.0, 16_000, 16_000, 0.5);
        let snr = snr_db(&tone, 440.0, 16_000, 0);
        assert!((85.0..100.0).contains(&snr), "{snr}");
        assert!((rms(&tone) - 0.5 / 2f64.sqrt()).abs() < 0.01);
    }

    #[test]
    fn added_tones_and_wrong_frequency_lower_the_snr() {
        let mut tone = sine(1_000.0, 16_000, 16_000, 0.5);
        // A second tone 20 dB down: SNR ~20 dB.
        let other = sine(3_000.0, 16_000, 16_000, 0.05);
        for (a, b) in tone.iter_mut().zip(&other) {
            *a += *b;
        }
        let snr = snr_db(&tone, 1_000.0, 16_000, 0);
        assert!((19.0..21.0).contains(&snr), "{snr}");
        assert!(snr_db(&tone, 1_500.0, 16_000, 0) < 0.0);
        // Skipping a corrupted start ignores it.
        let mut start = sine(1_000.0, 16_000, 16_000, 0.5);
        for s in start.iter_mut().take(100) {
            *s = 0;
        }
        assert!(snr_db(&start, 1_000.0, 16_000, 100) > 85.0);
        assert!(snr_db(&[], 1_000.0, 16_000, 0).is_infinite());
    }
}
