//! Silero VAD against the reference runtime and the fixtures' ground truth.
//! Needs `models/silero_vad.onnx` (scripts/models/fetch_models.py; `scripts/verify.sh` fetches it).
#![cfg(feature = "silero")]
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::chunks_exact_to_as_chunks
)] // test helpers

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::PathBuf;
use std::time::Duration;

use brisvox_core::{Clock, SimClock};
use brisvox_inference::silero::{CONTEXT, SileroVad, WINDOW};
use brisvox_inference::{SpeechModel, VadEvent, VadParams, VadStream};
use brisvox_sim::fixture::{Fixture, fixtures_dir};

const FIXTURES: [&str; 3] = ["en_two_utterances", "tr_two_utterances", "en_tr_mixed"];

/// The documented accuracy: every window within this of the onnxruntime reference. The
/// goldens are rounded to 7 decimals, so the measured worst case (5.96e-8, one f32 ulp at 0.5)
/// sits at the rounding floor; 1e-7 leaves room for another platform's float kernels.
const GOLDEN_TOLERANCE: f32 = 1e-7;

/// The documented boundary accuracy: start and stop within this of the ground truth.
const BOUNDARY_TOLERANCE: Duration = Duration::from_millis(50);

/// Counts the heap allocations of the calling thread while armed. Only this test binary uses it
/// (a global allocator is per binary); the counters are thread-local, so other tests running in
/// parallel do not disturb a measurement.
#[allow(unsafe_code)] // `unsafe impl GlobalAlloc` is the only way to count allocations.
mod counting {
    use super::*;

    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static BYTES: Cell<usize> = const { Cell::new(0) };
    }

    /// The counting allocator: forwards every request to the system allocator.
    pub struct Counting;

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let _ = ARMED.try_with(|armed| {
                if armed.get() {
                    let _ = BYTES.try_with(|bytes| bytes.set(bytes.get() + layout.size()));
                }
            });
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    /// Runs `f` and returns its result with the bytes this thread allocated while it ran.
    pub fn bytes_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
        BYTES.with(|bytes| bytes.set(0));
        ARMED.with(|armed| armed.set(true));
        let out = f();
        ARMED.with(|armed| armed.set(false));
        (out, BYTES.with(Cell::get))
    }
}

#[global_allocator]
static ALLOCATOR: counting::Counting = counting::Counting;

fn model_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/silero_vad.onnx");
    assert!(
        path.exists(),
        "{} missing: run python3 scripts/models/fetch_models.py",
        path.display()
    );
    path
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
        let mut stream = VadStream::new(
            SileroVad::load(&model_path()).unwrap(),
            VadParams::default(),
        );
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
        assert!(worst <= GOLDEN_TOLERANCE, "{name}: max deviation {worst}");
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
        let mut stream = VadStream::new(
            SileroVad::load(&model_path()).unwrap(),
            VadParams::default(),
        );
        let mut events = Vec::new();
        // Feed 20 ms frames stamped with their position in the fixture, then close the stream
        // (a stop still open at the end is reported by `finish`).
        for (i, frame) in pcm.chunks(320).enumerate() {
            let at = origin + Duration::from_millis(20 * i as u64);
            stream.push(frame, at, &mut events).unwrap();
        }
        events.extend(stream.finish());
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
            assert!(
                error <= BOUNDARY_TOLERANCE,
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
    let mut used = SileroVad::load(&model_path()).unwrap();
    let mut buffer = vec![0.0_f32; 64 + WINDOW];
    for window in pcm.chunks_exact(WINDOW).take(40) {
        for (dst, src) in buffer[64..].iter_mut().zip(window) {
            *dst = f32::from(*src) / 32_768.0;
        }
        used.infer(&buffer).unwrap();
    }
    used.reset();
    let mut fresh = SileroVad::load(&model_path()).unwrap();
    let input = vec![0.01_f32; 64 + WINDOW];
    assert_eq!(used.infer(&input).unwrap(), fresh.infer(&input).unwrap());
    assert!(SileroVad::load(&PathBuf::from("no/such/model.onnx")).is_err());
    assert!(
        used.infer(&[0.0; 10]).is_err(),
        "wrong input length is an error"
    );
}

#[test]
fn a_call_makes_no_heap_copies_of_its_own() {
    let mut vad = SileroVad::load(&model_path()).unwrap();
    let input = vec![0.01_f32; CONTEXT + WINDOW];
    // Warm up: the thread's session loads and the runtime sizes its own buffers on the first run.
    vad.infer(&input).unwrap();
    const CALLS: usize = 200;
    let ((), bytes) = counting::bytes_during(|| {
        for _ in 0..CALLS {
            vad.infer(&input).unwrap();
        }
    });
    let per_call = bytes / CALLS;
    println!("{per_call} bytes allocated per call");
    // The runtime's own bookkeeping per call measured about 1.3 KB. Copying the input alone would
    // add CONTEXT + WINDOW f32s (2.3 KB), and each state copy 1 KB, so any such copy breaks this.
    let one_input_copy = (CONTEXT + WINDOW) * std::mem::size_of::<f32>();
    assert!(
        per_call < one_input_copy,
        "{per_call} bytes allocated per call (limit {one_input_copy})"
    );
}

/// Prints what one window costs on this machine. Timing needs an optimised build:
/// `cargo test --release -p brisvox-inference --all-features --test silero -- --ignored --nocapture`
#[test]
#[ignore = "timing measurement, run with --release --ignored --nocapture"]
#[allow(clippy::disallowed_methods)] // measuring real elapsed time is the point (not logic)
fn window_cost_on_this_machine() {
    const PASSES: usize = 20;
    let fixture = Fixture::load(&fixtures_dir(), "en_two_utterances").unwrap();
    let pcm = samples(&fixture);
    let mut vad = SileroVad::load(&model_path()).unwrap();
    vad.warm().unwrap();
    let mut input = vec![0.0_f32; CONTEXT + WINDOW];
    let mut durations = Vec::with_capacity(PASSES * pcm.len() / WINDOW);
    for _ in 0..PASSES {
        vad.reset();
        input.fill(0.0);
        for window in pcm.chunks_exact(WINDOW) {
            // Same layout as VadStream: the last CONTEXT samples lead, then the new window.
            input.copy_within(WINDOW.., 0);
            for (dst, src) in input[CONTEXT..].iter_mut().zip(window) {
                *dst = f32::from(*src) / 32_768.0;
            }
            let started = std::time::Instant::now();
            vad.infer(&input).unwrap();
            durations.push(started.elapsed());
        }
    }
    durations.sort();
    let n = durations.len();
    let total: Duration = durations.iter().sum();
    let mean = total / u32::try_from(n).unwrap();
    println!(
        "{n} windows: mean {mean:?}, p50 {:?}, p99 {:?}, max {:?}",
        durations[n / 2],
        durations[n * 99 / 100],
        durations[n - 1]
    );
}
