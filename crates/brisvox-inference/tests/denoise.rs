//! The denoiser backends on real speech: the delay they report is the delay they have, the
//! speech survives, and an 8 kHz call works.
#![cfg(feature = "denoise")]
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::print_stderr,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::chunks_exact_to_as_chunks
)]

use brisvox_inference::denoise::Denoiser;
use brisvox_sim::fixture::{Fixture, fixtures_dir};

fn fixture_16k() -> Vec<i16> {
    let f = Fixture::load(&fixtures_dir(), "en_two_utterances").unwrap();
    f.pcm
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

/// Decimates 16 kHz to 8 kHz by averaging pairs (a crude but adequate telephone-band stand-in).
fn to_8k(pcm: &[i16]) -> Vec<i16> {
    pcm.chunks_exact(2)
        .map(|p| ((i32::from(p[0]) + i32::from(p[1])) / 2) as i16)
        .collect()
}

fn run(d: &mut dyn Denoiser, input: &[i16], chunk: usize) -> Vec<i16> {
    let mut out = Vec::new();
    for piece in input.chunks(chunk) {
        d.process(piece, &mut out).unwrap();
    }
    out
}

/// The lag (in samples) at which `output` best matches `input`, searched in `0..=max_lag`.
fn best_lag(input: &[i16], output: &[i16], max_lag: usize) -> usize {
    let n = output.len().min(input.len()) - max_lag;
    let score = |lag: usize| -> f64 {
        (max_lag..n)
            .map(|i| f64::from(output[i]) * f64::from(input[i - lag]))
            .sum()
    };
    (0..=max_lag)
        .max_by(|a, b| score(*a).total_cmp(&score(*b)))
        .unwrap()
}

fn check_delay(d: &mut dyn Denoiser, input: &[i16]) {
    let chunk = d.sample_rate() as usize / 50;
    let out = run(d, input, chunk);
    let hz = f64::from(d.sample_rate());
    let max_lag = (0.08 * hz) as usize;
    let lag = best_lag(input, &out, max_lag);
    let claimed = d.delay().as_secs_f64() * hz;
    // Within 2 ms: cross-correlation of an enhanced signal with the original is not sample exact.
    assert!(
        (lag as f64 - claimed).abs() < 0.002 * hz,
        "measured lag {lag} samples, delay() says {claimed}"
    );
    // Little is held back at the end.
    assert!(input.len() - out.len() < (0.04 * hz) as usize);
}

#[cfg(feature = "denoise-rnnoise")]
mod rnnoise {
    use super::*;
    use brisvox_inference::denoise::rnnoise::RnnoiseDenoiser;

    #[test]
    fn the_reported_delay_is_the_measured_one_at_16_khz() {
        let input = fixture_16k();
        check_delay(&mut RnnoiseDenoiser::new(16_000).unwrap(), &input);
    }

    #[test]
    fn the_reported_delay_is_the_measured_one_at_8_khz() {
        let input = to_8k(&fixture_16k());
        check_delay(&mut RnnoiseDenoiser::new(8_000).unwrap(), &input);
    }

    #[test]
    fn clean_speech_keeps_most_of_its_energy() {
        let input = fixture_16k();
        let mut d = RnnoiseDenoiser::new(16_000).unwrap();
        let out = run(&mut d, &input, 320);
        let energy = |s: &[i16]| s.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>();
        let ratio = energy(&out) / energy(&input[..out.len()]);
        assert!(ratio > 0.4 && ratio < 1.5, "energy ratio {ratio}");
    }
}

#[cfg(feature = "denoise-hush")]
mod hush {
    use super::*;
    use brisvox_inference::denoise::hush::HushDenoiser;

    #[test]
    fn the_reported_delay_is_the_measured_one() {
        let input = fixture_16k();
        check_delay(&mut HushDenoiser::new(16_000).unwrap(), &input);
        check_delay(&mut HushDenoiser::new(8_000).unwrap(), &to_8k(&input));
    }
}

#[cfg(feature = "denoise-fastenhancer")]
mod fastenhancer {
    use super::*;
    use brisvox_inference::denoise::fastenhancer::FastEnhancer;

    fn model() -> Option<std::path::PathBuf> {
        let dir = std::env::var_os("BRISVOX_MODELS_DIR").map_or_else(
            || std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models"),
            std::path::PathBuf::from,
        );
        let path = dir.join("fastenhancer_t.onnx");
        path.exists().then_some(path)
    }

    #[test]
    fn the_reported_delay_is_the_measured_one() {
        let Some(path) = model() else {
            eprintln!(
                "skipped: models/fastenhancer_t.onnx is missing (scripts/models/fetch_models.py)"
            );
            return;
        };
        let input = fixture_16k();
        check_delay(&mut FastEnhancer::load(&path, 16_000).unwrap(), &input);
        let input8 = to_8k(&input);
        check_delay(&mut FastEnhancer::load(&path, 8_000).unwrap(), &input8);
    }
}
