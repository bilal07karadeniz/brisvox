//! Does Hush attenuate clean speech, and how does that depend on how it is fed? (dev-only.)
//!
//! Reads a 16 kHz mono WAV, runs it through `Hush::enhance` (offline, whole file), `HushStream`
//! and the crate's `HushDenoiser`, with and without a gain applied to the input, and prints the
//! output level against the input level of the speech frames.
//!
//! Run: `cargo run --release -p brisvox-inference --features denoise-hush --example hush_probe --
//! file.wav`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::chunks_exact_to_as_chunks,
    clippy::cast_possible_truncation
)] // dev harness, not library code

use std::sync::Arc;

use brisvox_inference::denoise::Denoiser;
use brisvox_inference::denoise::hush::HushDenoiser;
use hush_vani::{Hush, HushStream};

fn read(path: &str) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap();
    bytes[44..]
        .chunks_exact(2)
        .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
        .collect()
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

/// Median gain in dB over the 20 ms frames whose input level is above `-40 dBFS`.
fn gain_db(input: &[f32], output: &[f32]) -> f32 {
    let mut gains: Vec<f32> = input
        .chunks(320)
        .zip(output.chunks(320))
        .filter(|(i, _)| rms(i) > 0.01)
        .map(|(i, o)| 20.0 * (rms(o) / rms(i)).log10())
        .collect();
    gains.sort_by(f32::total_cmp);
    gains.get(gains.len() / 2).copied().unwrap_or(0.0)
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: hush_probe file.wav");
    let input = read(&path);
    let hush = Arc::new(Hush::new().unwrap());
    for scale in [2.0_f32, 1.0, 0.5, 0.25, 0.1] {
        let x: Vec<f32> = input.iter().map(|v| (v * scale).clamp(-1.0, 1.0)).collect();
        let offline = hush.enhance(&x).unwrap();
        let offline = &offline[Hush::LATENCY_SAMPLES..];
        let mut stream = HushStream::new(Arc::clone(&hush));
        let streamed = stream.process(&x);
        let streamed = &streamed[Hush::LATENCY_SAMPLES..];
        let pcm: Vec<i16> = x.iter().map(|v| (v * 32768.0) as i16).collect();
        let mut d = HushDenoiser::new(16_000).unwrap();
        let mut out = Vec::new();
        for c in pcm.chunks(320) {
            d.process(c, &mut out).unwrap();
        }
        let ours: Vec<f32> = out.iter().map(|s| f32::from(*s) / 32768.0).collect();
        let ours = &ours[160..];
        // Fill the empty band above 4 kHz of telephone audio with a little noise (dither of
        // +-`lsb` samples) before the model, and measure against the clean input.
        for lsb in [8.0_f32] {
            let mut seed = 12_345_u32;
            let dithered: Vec<i16> = pcm
                .iter()
                .map(|v| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let n = ((seed >> 16) as f32 / 65_536.0 - 0.5) * 2.0 * lsb;
                    (f32::from(*v) + n).clamp(-32_768.0, 32_767.0) as i16
                })
                .collect();
            let mut d = HushDenoiser::new(16_000).unwrap();
            let mut out = Vec::new();
            for c in dithered.chunks(320) {
                d.process(c, &mut out).unwrap();
            }
            let o: Vec<f32> = out.iter().map(|s| f32::from(*s) / 32768.0).collect();
            println!(
                "   with +-{lsb} LSB of white noise added: {:+.1} dB",
                gain_db(&x, &o[160..])
            );
        }
        println!(
            "input scale {scale}: offline {:+.1} dB, stream {:+.1} dB, ours {:+.1} dB (median over frames above -40 dBFS)",
            gain_db(&x, offline),
            gain_db(&x, streamed),
            gain_db(&x, ours),
        );
    }
}
