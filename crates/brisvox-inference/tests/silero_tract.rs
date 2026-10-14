//! Silero VAD on tract (pure Rust) against the reference runtime and the ground truth.
//! Uses the bundled prepared graph (assets/models/silero_vad_16k_tract.onnx).
#![cfg(feature = "silero-tract")]
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::chunks_exact_to_as_chunks
)] // test helpers

use std::path::PathBuf;
use std::time::Duration;

use brisvox_core::{Clock, SimClock};
use std::sync::Arc;

use brisvox_inference::silero_tract::{SileroTract, SileroTractModel, WINDOW};
use brisvox_inference::{SpeechModel, VadEvent, VadParams, VadStream};
use brisvox_sim::fixture::{Fixture, fixtures_dir};

const FIXTURES: [&str; 3] = ["en_two_utterances", "tr_two_utterances", "en_tr_mixed"];

fn model_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/models/silero_vad_16k_tract.onnx")
}

fn model() -> Arc<SileroTractModel> {
    SileroTractModel::bundled().unwrap()
}

fn vad() -> SileroTract {
    model().stream().unwrap()
}

fn samples(fixture: &Fixture) -> Vec<i16> {
    fixture
        .pcm
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

fn golden(name: &str) -> Vec<f32> {
    let text = std::fs::read_to_string(fixtures_dir().join("vad_golden.json")).unwrap();
    let all: serde_json::Value = serde_json::from_str(&text).unwrap();
    all[name]["probs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap() as f32)
        .collect()
}

#[tokio::test(start_paused = true)]
async fn probabilities_match_the_reference_runtime_on_every_window() {
    let clock = SimClock::new();
    for name in FIXTURES {
        let fixture = Fixture::load(&fixtures_dir(), name).unwrap();
        let pcm = samples(&fixture);
        let expected = golden(name);
        let mut stream = VadStream::new(vad(), VadParams::default());
        let mut events = Vec::new();
        let mut got = Vec::new();
        for window in pcm.chunks_exact(WINDOW) {
            stream.push(window, clock.now(), &mut events).unwrap();
            got.push(stream.last_probability());
        }
        assert_eq!(got.len(), expected.len(), "{name}");
        let worst = got
            .iter()
            .zip(&expected)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f32, f32::max);
        println!(
            "{name}: {} windows, max |p - reference| = {worst:e}",
            got.len()
        );
        // The prepared graph is constant-folded by onnxruntime: ~2e-6 from the stock model.
        assert!(worst < 1e-5, "{name}: max deviation {worst}");
    }
}

#[tokio::test(start_paused = true)]
async fn speech_boundaries_match_the_fixture_ground_truth() {
    let clock = SimClock::new();
    let origin = clock.now();
    let mut worst = Duration::ZERO;
    for name in FIXTURES {
        let fixture = Fixture::load(&fixtures_dir(), name).unwrap();
        let pcm = samples(&fixture);
        let mut stream = VadStream::new(vad(), VadParams::default());
        let mut events = Vec::new();
        // Feed 20 ms frames stamped with their position in the fixture.
        for (i, frame) in pcm.chunks(320).enumerate() {
            let at = origin + Duration::from_millis(20 * i as u64);
            stream.push(frame, at, &mut events).unwrap();
        }
        let found: Vec<Duration> = events
            .iter()
            .map(|e| match e {
                VadEvent::Start { at } | VadEvent::Stop { at } => *at - origin,
            })
            .collect();
        let truth: Vec<Duration> = fixture
            .speech_segments()
            .into_iter()
            .flat_map(|(start, end)| [start, end])
            .collect();
        println!("{name}: truth {truth:?}\n  silero {found:?}");
        assert_eq!(found.len(), truth.len(), "{name}: {events:?}");
        for (i, (f, t)) in found.iter().zip(&truth).enumerate() {
            let error = if f > t { *f - *t } else { *t - *f };
            worst = worst.max(error);
            // Window 32 ms + padding 30 ms: boundaries within 100 ms of the ground truth.
            assert!(
                error <= Duration::from_millis(100),
                "{name} boundary {i}: {f:?} vs truth {t:?}"
            );
            let is_start = i % 2 == 0;
            assert_eq!(
                matches!(events[i], VadEvent::Start { .. }),
                is_start,
                "{name}: event order"
            );
        }
    }
    println!("worst boundary error {worst:?}");
}

#[test]
fn reset_restores_a_fresh_stream() {
    let fixture = Fixture::load(&fixtures_dir(), "en_two_utterances").unwrap();
    let pcm = samples(&fixture);
    let mut used = vad();
    let mut buffer = vec![0.0_f32; 64 + WINDOW];
    for window in pcm.chunks_exact(WINDOW).take(40) {
        for (dst, src) in buffer[64..].iter_mut().zip(window) {
            *dst = f32::from(*src) / 32_768.0;
        }
        used.infer(&buffer).unwrap();
    }
    used.reset();
    let mut fresh = vad();
    let input = vec![0.01_f32; 64 + WINDOW];
    assert_eq!(used.infer(&input).unwrap(), fresh.infer(&input).unwrap());
    assert!(SileroTractModel::load(&PathBuf::from("no/such/model.onnx")).is_err());
    assert!(
        used.infer(&[0.0; 10]).is_err(),
        "wrong input length is an error"
    );
}

#[test]
fn the_asset_file_loads_like_the_bundled_model_and_wrong_files_are_refused() {
    let from_file = SileroTractModel::load(&model_path()).unwrap();
    let mut a = from_file.stream().unwrap();
    let mut b = vad();
    let input = vec![0.02_f32; 64 + WINDOW];
    assert_eq!(a.infer(&input).unwrap(), b.infer(&input).unwrap());
    assert!(SileroTractModel::from_bytes(b"not a model").is_err());
}
