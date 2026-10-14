//! Smart Turn v3.2: log-mel features and probabilities against the reference pipeline
//! (scripts/models/make_smart_turn_goldens.py: Pipecat's numpy extractor + onnxruntime), and
//! the pool path at candidate end of turn with its latency.
//! Model tests need `models/smart-turn-v3.2-cpu.onnx` (`scripts/verify.sh` fetches it).
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::chunks_exact_to_as_chunks
)] // test helpers

#[cfg(feature = "smart-turn-ort")]
use std::path::PathBuf;

use brisvox_inference::mel::{FEATURES, FRAMES, N_MELS, WINDOW_SAMPLES, WhisperLogMel};
use brisvox_sim::fixture::{Fixture, fixtures_dir};

fn samples(name: &str) -> Vec<i16> {
    let fixture = Fixture::load(&fixtures_dir(), name).unwrap();
    fixture
        .pcm
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

fn golden() -> serde_json::Value {
    let text = std::fs::read_to_string(fixtures_dir().join("smart_turn_golden.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// `(key, fixture, start, end, probability)` of every golden case.
fn cases() -> Vec<(String, String, usize, usize, f32)> {
    let golden = golden();
    golden["cases"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, case)| {
            (
                key.clone(),
                case["fixture"].as_str().unwrap().to_owned(),
                case["start"].as_u64().unwrap() as usize,
                case["end"].as_u64().unwrap() as usize,
                case["probability"].as_f64().unwrap() as f32,
            )
        })
        .collect()
}

/// The reference's input: the last 8 s, zero-padded at the start.
fn last_8_seconds(pcm: &[i16]) -> Vec<f32> {
    let audio: Vec<f32> = pcm.iter().map(|&s| f32::from(s) / 32_768.0).collect();
    let tail = &audio[audio.len().saturating_sub(WINDOW_SAMPLES)..];
    let mut window = vec![0.0; WINDOW_SAMPLES - tail.len()];
    window.extend_from_slice(tail);
    window
}

#[test]
fn features_match_the_reference_extractor() {
    let golden = golden();
    let key = golden["features_case"].as_str().unwrap();
    assert_eq!(
        golden["features_shape"],
        serde_json::json!([N_MELS, FRAMES])
    );
    let (_, fixture, start, end, _) = cases().into_iter().find(|c| c.0 == key).unwrap();
    let bytes = std::fs::read(fixtures_dir().join("smart_turn_features.f32")).unwrap();
    let expected: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    assert_eq!(expected.len(), FEATURES);

    let pcm = samples(&fixture);
    let mut mel = WhisperLogMel::new();
    let mut out = vec![0.0; FEATURES];
    mel.compute(&last_8_seconds(&pcm[start..end]), &mut out)
        .unwrap();
    let differing = out.iter().zip(&expected).filter(|(a, b)| a != b).count();
    let worst = out
        .iter()
        .zip(&expected)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0_f32, f32::max);
    println!("features: {differing} values differ, max |rust - reference| = {worst:e}");
    // Bit for bit: the int8 model moves by up to 0.13 for 1e-6 feature changes.
    assert_eq!(differing, 0, "worst {worst}");
}

/// FNV-1a 64 of f32 values as little-endian bytes (what the golden script records).
fn fnv1a64(values: &[f32]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in values.iter().flat_map(|v| v.to_le_bytes()) {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[test]
fn features_of_every_golden_case_are_bit_identical_to_the_reference() {
    let golden = golden();
    let mut mel = WhisperLogMel::new();
    let mut out = vec![0.0; FEATURES];
    let mut checked = 0;
    for (key, case) in golden["cases"].as_object().unwrap() {
        let pcm = samples(case["fixture"].as_str().unwrap());
        let (start, end) = (
            case["start"].as_u64().unwrap() as usize,
            case["end"].as_u64().unwrap() as usize,
        );
        mel.compute(&last_8_seconds(&pcm[start..end]), &mut out)
            .unwrap();
        assert_eq!(
            fnv1a64(&out),
            case["features_fnv1a64"].as_str().unwrap(),
            "{key}: features differ from the reference extractor"
        );
        checked += 1;
    }
    assert_eq!(checked, 12);
}

#[test]
fn features_reject_wrong_lengths_and_reuse_is_deterministic() {
    let mut mel = WhisperLogMel::new();
    let mut out = vec![0.0; FEATURES];
    assert!(mel.compute(&[0.0; 10], &mut out).is_err());
    assert!(
        mel.compute(&vec![0.0; WINDOW_SAMPLES], &mut [0.0; 3])
            .is_err()
    );
    // Silence: normalization divides by sqrt(1e-7), every bin is at the floor.
    mel.compute(&vec![0.0; WINDOW_SAMPLES], &mut out).unwrap();
    assert!(
        out.iter()
            .all(|v| (v - out[0]).abs() < 1e-6 && v.is_finite())
    );
    let audio: Vec<f32> = (0..WINDOW_SAMPLES)
        .map(|i| (i as f32 * 0.05).sin() * 0.3)
        .collect();
    let mut first = vec![0.0; FEATURES];
    mel.compute(&audio, &mut first).unwrap();
    mel.compute(&audio, &mut out).unwrap();
    assert_eq!(first, out, "scratch state leaked between calls");
}

#[cfg(feature = "smart-turn-ort")]
fn model_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../models")
        .join(brisvox_inference::smart_turn::MODEL_FILE)
}

/// Every golden case through a backend: `(key, rust, reference)`.
#[cfg(feature = "smart-turn-ort")]
fn run_cases(
    model: &std::sync::Arc<dyn brisvox_inference::smart_turn::TurnModel>,
) -> Vec<(String, f32, f32)> {
    use brisvox_inference::smart_turn::SmartTurn;
    cases()
        .into_iter()
        .map(|(key, fixture, start, end, expected)| {
            let pcm = samples(&fixture);
            let mut turn = SmartTurn::new(std::sync::Arc::clone(model));
            turn.push(&pcm[start..end]);
            (key, turn.analyze_blocking().unwrap(), expected)
        })
        .collect()
}

#[cfg(feature = "smart-turn-ort")]
mod ort {
    use std::sync::Arc;
    use std::time::Duration;

    use brisvox_core::{Clock, SharedClock, SystemClock};
    use brisvox_inference::smart_turn::ort_backend::{
        DEFAULT_INTRA_THREADS, OrtOptions, SmartTurnOrt,
    };
    use brisvox_inference::smart_turn::{MODEL_BYTES, SmartTurn, TurnModel};
    use brisvox_inference::{InferencePool, PoolConfig};

    use super::{WINDOW_SAMPLES, cases, model_path, run_cases, samples};

    fn model() -> Arc<dyn TurnModel> {
        ort_model()
    }

    fn ort_model() -> Arc<SmartTurnOrt> {
        let path = model_path();
        SmartTurnOrt::load(&path).unwrap_or_else(|e| {
            panic!(
                "{e}: run scripts/models/fetch_models.py ({})",
                path.display()
            )
        })
    }

    #[test]
    fn rejects_a_file_that_is_not_the_pinned_model() {
        let dir = std::env::temp_dir().join(format!("brisvox-st-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wrong = dir.join("wrong.onnx");
        std::fs::write(&wrong, [0_u8; 64]).unwrap();
        assert!(SmartTurnOrt::load(&wrong).is_err());
        assert!(SmartTurnOrt::load(&dir.join("missing.onnx")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(std::fs::metadata(model_path()).unwrap().len(), MODEL_BYTES);
    }

    fn check_goldens(model: &Arc<dyn TurnModel>, label: &str) {
        let mut worst = 0.0_f32;
        for (key, got, expected) in run_cases(model) {
            let error = (got - expected).abs();
            worst = worst.max(error);
            println!("{label} {key}: rust {got} vs reference {expected} ({error:.2e})");
            // Goldens are rounded to 7 decimals.
            assert!(
                error < 1e-6,
                "{label} {key}: rust {got} vs reference {expected}"
            );
            assert_eq!(got > 0.5, expected > 0.5, "{label} {key}: verdict differs");
        }
        println!("ort {label}: worst |rust - reference| probability: {worst}");
    }

    #[test]
    fn probabilities_match_the_reference_runtime() {
        check_goldens(&model(), "default");
    }

    /// The module doc's claim: the goldens hold at 1, 2 and 4 intra-op threads, through a
    /// session made by `warm` (the stack is LIFO, so the calls use the warmed session).
    #[test]
    fn probabilities_match_the_reference_at_1_2_and_4_threads() {
        for threads in [1, 2, 4] {
            let model = SmartTurnOrt::load_with_threads(&model_path(), threads).unwrap();
            assert_eq!(model.intra_threads(), threads);
            model.warm(2).unwrap();
            let model: Arc<dyn TurnModel> = model;
            check_goldens(&model, &format!("{threads} threads"));
        }
    }

    /// Threading and spinning only change scheduling. The probabilities of every golden
    /// case are bit-identical (not just within the 1e-6 tolerance) across intra-op threads and
    /// `allow_spinning`, and equal the single-thread ONNX Runtime default the goldens were made
    /// with (the model moves by up to 0.13 for 1e-6 of noise, so "close" is not enough).
    #[test]
    fn probabilities_are_bit_identical_across_session_configurations() {
        let bits = |options: OrtOptions| -> Vec<u32> {
            let model = SmartTurnOrt::load_with_options(&model_path(), options).unwrap();
            assert_eq!(model.options().intra_threads, options.intra_threads.max(1));
            model.warm(2).unwrap();
            let model: Arc<dyn TurnModel> = model;
            run_cases(&model)
                .into_iter()
                .map(|(_, got, _)| got.to_bits())
                .collect()
        };
        let reference = bits(OrtOptions {
            intra_threads: 1,
            intra_op_spinning: true,
        });
        assert_eq!(reference.len(), 12);
        for intra_threads in [1, 2, 3, 4] {
            for intra_op_spinning in [true, false] {
                let options = OrtOptions {
                    intra_threads,
                    intra_op_spinning,
                };
                assert_eq!(bits(options), reference, "{options:?}");
            }
        }
        assert_eq!(bits(OrtOptions::default()), reference, "the default");
    }

    #[test]
    fn the_thread_setting_defaults_to_2_and_is_at_least_1() {
        assert_eq!(DEFAULT_INTRA_THREADS, 2);
        assert_eq!(ort_model().intra_threads(), DEFAULT_INTRA_THREADS);
        // Spinning stays on (ONNX Runtime's default); off was cheaper at idle but slower
        // under load (docs/LATENCY.md).
        assert!(ort_model().options().intra_op_spinning);
        let clamped = SmartTurnOrt::load_with_threads(&model_path(), 0).unwrap();
        assert_eq!(clamped.intra_threads(), 1);
        // A zero would mean "one per physical core" to ONNX Runtime; the clamped model still runs.
        let features = vec![0.0; brisvox_inference::mel::FEATURES];
        clamped.predict(&features).unwrap();
    }

    #[test]
    fn history_keeps_the_last_8_seconds_like_the_reference() {
        let model = model();
        let (_, fixture, ..) = cases().into_iter().next().unwrap();
        let pcm = samples(&fixture);
        assert!(pcm.len() > WINDOW_SAMPLES, "fixture shorter than 8 s");
        // Pushed in uneven pieces, wrapping the ring several times.
        let mut turn = SmartTurn::new(Arc::clone(&model));
        for piece in pcm.chunks(317) {
            turn.push(piece);
        }
        assert_eq!(turn.buffered(), Duration::from_secs(8));
        let mut direct = SmartTurn::new(model);
        direct.push(&pcm[pcm.len() - WINDOW_SAMPLES..]);
        assert_eq!(
            turn.analyze_blocking().unwrap(),
            direct.analyze_blocking().unwrap()
        );
    }

    #[test]
    fn warm_sessions_serve_concurrent_calls_without_cold_starts() {
        let model = ort_model();
        model.warm(3).unwrap();
        assert_eq!(model.idle_sessions(), 3);
        let features = vec![0.0; brisvox_inference::mel::FEATURES];
        std::thread::scope(|scope| {
            for _ in 0..3 {
                scope.spawn(|| model.predict(&features).unwrap());
            }
        });
        assert_eq!(model.cold_starts(), 0);
        assert_eq!(model.idle_sessions(), 3, "sessions are returned after use");
        // A wrong input fails without losing the session.
        assert!(model.predict(&[0.0; 3]).is_err());
        assert_eq!(model.idle_sessions(), 3);
    }

    #[test]
    fn concurrent_warm_calls_create_the_target_in_all_not_each() {
        // The engine fans the warm-up out as several pool jobs with the same target.
        let model = ort_model();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| model.warm(4).unwrap());
            }
        });
        assert_eq!(model.sessions(), 4);
        assert_eq!(model.idle_sessions(), 4);
        // Already there: nothing more is created.
        model.warm(4).unwrap();
        model.warm(2).unwrap();
        assert_eq!(model.idle_sessions(), 4);
    }

    /// Latency of the two parts of one analysis (features on the calling thread, the model with
    /// the default 2 intra-op threads). Run with --release to compare with docs/LATENCY.md; the
    /// bounds only catch gross regressions. Whether the thread setting takes effect is checked
    /// by the ignored `more_threads_cut_the_latency_of_warm_and_cold_sessions` (ort_backend.rs).
    #[test]
    fn features_and_model_latency() {
        use brisvox_inference::mel::{FEATURES, WhisperLogMel};
        let clock = SystemClock;
        let model = model();
        let (_, fixture, start, end, _) = cases().into_iter().next().unwrap();
        let pcm = samples(&fixture);
        let audio = super::last_8_seconds(&pcm[start..end]);
        let mut mel = WhisperLogMel::new();
        let mut features = vec![0.0; FEATURES];
        model.predict(&features).unwrap(); // session creation
        let (mut mel_runs, mut model_runs) = (Vec::new(), Vec::new());
        for _ in 0..15 {
            let t = clock.now();
            mel.compute(&audio, &mut features).unwrap();
            let m = clock.now();
            model.predict(&features).unwrap();
            mel_runs.push(m.saturating_duration_since(t));
            model_runs.push(clock.now().saturating_duration_since(m));
        }
        mel_runs.sort();
        model_runs.sort();
        println!(
            "median: features {:?}, model {:?} (max {:?} / {:?})",
            mel_runs[7], model_runs[7], mel_runs[14], model_runs[14]
        );
        // Release on the reference desktop: ~1.2 ms features (bench/results/smart-turn.md) and a
        // ~17 ms model with 2 threads (~26 ms with 1; docs/LATENCY.md).
        let (features_bound, model_bound) = if cfg!(debug_assertions) {
            (Duration::from_millis(400), Duration::from_millis(250))
        } else {
            (Duration::from_millis(10), Duration::from_millis(100))
        };
        assert!(mel_runs[7] < features_bound, "features {:?}", mel_runs[7]);
        assert!(model_runs[7] < model_bound, "model {:?}", model_runs[7]);
    }

    /// Candidate end of turn = the tract VAD's stop event: Smart Turn runs on the pool once per
    /// stop and never otherwise; wait/run latency is measured.
    #[cfg(feature = "silero-tract")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runs_on_the_pool_only_at_candidate_end_of_turn() {
        use brisvox_inference::silero_tract::SileroTractModel;
        use brisvox_inference::{VadEvent, VadParams, VadStream};

        let clock: SharedClock = Arc::new(SystemClock);
        let pool = InferencePool::new(
            &PoolConfig {
                threads: 1,
                queue_capacity: 4,
                name: "smart-turn-test".to_owned(),
            },
            Arc::clone(&clock),
        )
        .unwrap();
        let handle = pool.handle();
        let ort = ort_model();
        ort.warm(1).unwrap(); // one session per pool thread, at startup
        let model: Arc<dyn TurnModel> = ort.clone();
        let vad_model = SileroTractModel::bundled().unwrap();
        let mut runs = Vec::new();
        for name in ["en_two_utterances", "tr_two_utterances", "en_tr_mixed"] {
            let pcm = samples(name);
            let mut vad = VadStream::new(vad_model.stream().unwrap(), VadParams::default());
            let mut turn = SmartTurn::new(Arc::clone(&model));
            let mut events = Vec::new();
            let mut analyses = 0;
            let t0 = clock.now();
            for (i, frame) in pcm.chunks(320).enumerate() {
                turn.push(frame);
                let at = t0 + Duration::from_millis(20 * i as u64);
                events.clear();
                vad.push(frame, at, &mut events).unwrap();
                for event in &events {
                    if let VadEvent::Stop { .. } = event {
                        let started = clock.now();
                        let verdict = turn.analyze(&handle, Duration::from_secs(5)).await.unwrap();
                        let total = clock.now().saturating_duration_since(started);
                        analyses += 1;
                        assert!((0.0..=1.0).contains(&verdict.probability));
                        assert!(verdict.timing.run > Duration::ZERO);
                        runs.push(verdict.timing.run);
                        println!(
                            "{name}: p={:.3} complete={} wait={:?} run={:?} total={total:?}",
                            verdict.probability,
                            verdict.complete,
                            verdict.timing.wait,
                            verdict.timing.run
                        );
                    }
                }
            }
            assert_eq!(analyses, 2, "{name}: one analysis per VAD stop");
        }
        let metrics = pool.metrics();
        assert_eq!(metrics.completed, 6, "{metrics:?}");
        assert_eq!(
            ort.cold_starts(),
            0,
            "the warmed session served every analysis"
        );
        runs.sort();
        println!(
            "run (features + model): min {:?} max {:?}",
            runs[0],
            runs[runs.len() - 1]
        );
        pool.shutdown();
    }
}

/// Why the features cannot be computed incrementally over the ring: the waveform is
/// normalized with the mean and variance of the WHOLE 8 s window, so sliding the window by 100 ms
/// (10 hops) changes every sample the two windows share, hence every column. Reusing the
/// columns of the previous analysis would feed the int8 model features that differ from the
/// reference by far more than the 1e-6 that moves its output by 0.13.
#[test]
fn a_slid_window_changes_every_shared_column_so_columns_cannot_be_reused() {
    const SHIFT_SAMPLES: usize = 1_600; // 100 ms = 10 hops
    const SHIFT_COLUMNS: usize = SHIFT_SAMPLES / 160;
    let (_, fixture, start, end, _) = cases().into_iter().next().unwrap();
    let pcm = samples(&fixture);
    let end = end.min(pcm.len());
    assert!(end >= start + SHIFT_SAMPLES * 2, "fixture too short");
    let mut mel = WhisperLogMel::new();
    let mut before = vec![0.0; FEATURES];
    let mut after = vec![0.0; FEATURES];
    mel.compute(
        &last_8_seconds(&pcm[start..end - SHIFT_SAMPLES]),
        &mut before,
    )
    .unwrap();
    mel.compute(&last_8_seconds(&pcm[start..end]), &mut after)
        .unwrap();
    // Column t of `after` is column t + 10 of `before`'s audio, 10 hops later.
    let (mut same, mut total) = (0usize, 0usize);
    let mut worst = 0.0_f32;
    for m in 0..N_MELS {
        for t in 0..FRAMES - SHIFT_COLUMNS {
            let (a, b) = (
                after[m * FRAMES + t],
                before[m * FRAMES + t + SHIFT_COLUMNS],
            );
            total += 1;
            same += usize::from(a == b);
            worst = worst.max((a - b).abs());
        }
    }
    println!("slid by 100 ms: {same} of {total} shared values identical, max diff {worst:e}");
    assert!(
        same * 10 < total,
        "{same} of {total} shared values are bit-identical: columns could be reused"
    );
    assert!(
        worst > 1e-4,
        "max difference {worst:e} is within the model's noise floor"
    );
}
