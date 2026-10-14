//! Per-window cost of the Silero backends on the fixtures. Prints numbers; asserts only
//! a generous ceiling (the CONTRIBUTING.md inference-pool threshold is about 50 µs per job, so a VAD
//! window always runs on the pool). Meaningful only in release: `cargo test --release`.
#![cfg(all(feature = "silero", feature = "silero-tract"))]
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods, // wall-clock timing is the point of this test
    clippy::cast_precision_loss,
    clippy::chunks_exact_to_as_chunks
)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use brisvox_inference::SpeechModel;
use brisvox_inference::silero::SileroVad;
use brisvox_inference::silero_tract::SileroTractModel;
use brisvox_sim::fixture::{Fixture, fixtures_dir};

fn windows() -> Vec<Vec<f32>> {
    let fixture = Fixture::load(&fixtures_dir(), "tr_two_utterances").unwrap();
    let pcm: Vec<f32> = fixture
        .pcm
        .chunks_exact(2)
        .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32_768.0)
        .collect();
    let mut context = vec![0.0_f32; 64];
    pcm.chunks_exact(512)
        .map(|w| {
            let mut input = context.clone();
            input.extend_from_slice(w);
            context = input[512..].to_vec();
            input
        })
        .collect()
}

fn time(model: &mut dyn SpeechModel, windows: &[Vec<f32>]) -> (Duration, Duration) {
    for w in windows.iter().take(50) {
        model.infer(w).unwrap();
    }
    let mut times: Vec<Duration> = windows
        .iter()
        .map(|w| {
            let t = Instant::now();
            model.infer(w).unwrap();
            t.elapsed()
        })
        .collect();
    times.sort_unstable();
    (times[times.len() / 2], times[times.len() * 99 / 100])
}

#[test]
fn both_backends_cost_well_under_a_millisecond_per_window() {
    let models = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models");
    let windows = windows();
    let mut ort = SileroVad::load(&models.join("silero_vad.onnx")).unwrap();
    let tract = SileroTractModel::bundled().unwrap();
    let mut tract = tract.stream().unwrap();
    let (ort_p50, ort_p99) = time(&mut ort, &windows);
    let (tract_p50, tract_p99) = time(&mut tract, &windows);
    println!(
        "per 32 ms window ({} windows): ort p50 {ort_p50:?} p99 {ort_p99:?} | tract p50 {tract_p50:?} p99 {tract_p99:?}",
        windows.len()
    );
    // Debug builds are far slower; the ceiling only catches a pathological regression.
    assert!(ort_p50 < Duration::from_millis(20) && tract_p50 < Duration::from_millis(20));
}
