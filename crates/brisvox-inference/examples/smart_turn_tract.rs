//! Does tract reproduce the Smart Turn v3.2 goldens? (dev-only harness.)
//!
//! Loads `models/smart-turn-v3.2-cpu.onnx` into tract 0.23.x in two plans, runs the 12 golden
//! cases (`fixtures/smart_turn_golden.json`, produced by onnxruntime `ORT_ENABLE_ALL`) through
//! the crate's own bit-exact feature extractor, and prints per plan the largest difference to the
//! goldens, the number of cases off by more than 1e-3, the verdict flips (threshold 0.5) and the
//! median latency over the 12 cases (single thread):
//!
//! * `typed`: `into_typed()` only, no declutter, no optimizer: the graph exactly as written.
//! * `optimized`: `into_optimized()`, tract's default (declutter rewrites the graph, among others
//!   fusing `DequantizeLinear -> Softmax -> QuantizeLinear` into a lookup table).
//! * `--bisect`: applies the declutter `Optimizer` one patch at a time on the first golden case
//!   and prints every step that changes the operator mix or the probability.
//!
//! Run: `cargo run --release -p brisvox-inference --features silero-tract --example
//! smart_turn_tract [-- --json out.json]`. With `--json` the per-case probabilities of both
//! plans are written so `scripts/models/smart_turn_runtime_variance.py` can compare them with the
//! other ONNX plans (`BRISVOX_TRACT_JSON=out.json`). Results: docs/providers/smart-turn.md.
//! Set `BRISVOX_MODELS_DIR` when `models/` lives elsewhere (a git worktree).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::chunks_exact_to_as_chunks,
    clippy::disallowed_methods // wall-clock Instant: the harness measures real model latency
)] // dev harness, not library code
#![allow(clippy::wildcard_imports)] // tract's extension traits

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use brisvox_inference::mel::{FEATURES, FRAMES, N_MELS, WINDOW_SAMPLES, WhisperLogMel};
use brisvox_sim::fixture::{Fixture, fixtures_dir};
use tract_onnx::prelude::*;

type Plan = Arc<RunnableModel<TypedFact, Box<dyn TypedOp>>>;

fn load(path: &PathBuf, optimized: bool) -> Plan {
    let typed = typed_model(path);
    let model = if optimized {
        typed.into_optimized().unwrap()
    } else {
        typed
    };
    model.into_runnable().unwrap()
}

/// The graph typed with batch 1; tract cannot unify the file's declared value shapes with it, so
/// they are ignored (`with_ignore_value_info`).
fn typed_model(path: &PathBuf) -> TypedModel {
    tract_onnx::onnx()
        .with_ignore_value_info(true)
        .model_for_path(path)
        .unwrap()
        .with_input_fact(
            0,
            InferenceFact::dt_shape(f32::datum_type(), tvec!(1, N_MELS as i64, FRAMES as i64)),
        )
        .unwrap()
        .into_typed()
        .unwrap()
}

/// `--bisect`: applies tract's declutter optimizer one patch at a time
/// (`Optimizer::declutter().stopping_at(n)`) and reports every step that moves the first golden
/// case's probability, with the operator kinds that appeared or disappeared in that step.
fn bisect(path: &PathBuf, features: &[f32]) {
    use std::collections::BTreeMap;
    let kinds = |m: &TypedModel| {
        let mut out: BTreeMap<String, i64> = BTreeMap::new();
        for node in m.nodes() {
            *out.entry(node.op().name().to_string()).or_default() += 1;
        }
        out
    };
    let base = typed_model(path);
    let mut previous_kinds = kinds(&base);
    let mut previous = run(&base.clone().into_runnable().unwrap(), features);
    println!("bisect: step 0 (typed plan) p = {previous:.4}");
    let (mut step, mut idle) = (1, 0);
    loop {
        let mut model = base.clone();
        let optimizer = tract_onnx::tract_core::optim::Optimizer::declutter().stopping_at(step);
        optimizer.optimize(&mut model).unwrap();
        let now = kinds(&model);
        let p = run(&model.clone().into_runnable().unwrap(), features);
        let mut changes = Vec::new();
        for key in now.keys().chain(previous_kinds.keys()) {
            let (a, b) = (
                now.get(key).copied().unwrap_or(0),
                previous_kinds.get(key).copied().unwrap_or(0),
            );
            if a != b && !changes.iter().any(|c: &String| c.starts_with(key.as_str())) {
                changes.push(format!("{key} {b}->{a}"));
            }
        }
        if (p - previous).abs() > 1e-6 || !changes.is_empty() {
            println!(
                "bisect: step {step:3} p = {p:.4} ({:+.4}) {}",
                p - previous,
                changes.join(", ")
            );
        }
        // Exhausted once 25 consecutive steps change nothing (patches that only rename nodes
        // keep the operator kinds): the optimizer stops applying patches at its fixed point.
        idle = if now == previous_kinds && (p - previous).abs() <= 1e-6 {
            idle + 1
        } else {
            0
        };
        if idle >= 25 {
            break;
        }
        previous_kinds = now;
        previous = p;
        step += 1;
        if step > 2000 {
            break;
        }
    }
}

fn run(plan: &Plan, features: &[f32]) -> f32 {
    let input = tract_ndarray::Array3::from_shape_vec((1, N_MELS, FRAMES), features.to_vec())
        .unwrap()
        .into_tensor();
    let out = plan.run(tvec!(input.into())).unwrap();
    out[0]
        .to_plain_array_view::<f32>()
        .unwrap()
        .iter()
        .copied()
        .next()
        .unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let json_out = args
        .iter()
        .position(|a| a == "--json")
        .and_then(|i| args.get(i + 1));
    let models = std::env::var("BRISVOX_MODELS_DIR").map_or_else(
        |_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models"),
        PathBuf::from,
    );
    let model_path = models.join("smart-turn-v3.2-cpu.onnx");

    let text = std::fs::read_to_string(fixtures_dir().join("smart_turn_golden.json")).unwrap();
    let golden: serde_json::Value = serde_json::from_str(&text).unwrap();
    let mut mel = WhisperLogMel::new();
    let mut cases = Vec::new();
    for (key, case) in golden["cases"].as_object().unwrap() {
        let fixture = Fixture::load(&fixtures_dir(), case["fixture"].as_str().unwrap()).unwrap();
        let audio: Vec<f32> = fixture
            .pcm
            .chunks_exact(2)
            .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32_768.0)
            .collect();
        let (start, end) = (
            case["start"].as_u64().unwrap() as usize,
            case["end"].as_u64().unwrap() as usize,
        );
        let tail = &audio[start..end];
        let tail = &tail[tail.len().saturating_sub(WINDOW_SAMPLES)..];
        let mut window = vec![0.0; WINDOW_SAMPLES - tail.len()];
        window.extend_from_slice(tail);
        let mut features = vec![0.0; FEATURES];
        mel.compute(&window, &mut features).unwrap();
        cases.push((
            key.clone(),
            case["probability"].as_f64().unwrap() as f32,
            features,
        ));
    }

    if args.iter().any(|a| a == "--bisect") {
        bisect(&model_path, &cases[0].2);
        return;
    }
    let mut report = serde_json::Map::new();
    for (name, optimized) in [("typed", false), ("optimized", true)] {
        let plan = load(&model_path, optimized);
        let mut probabilities = serde_json::Map::new();
        let (mut worst, mut over, mut flips) = (0.0_f32, 0, 0);
        let mut times = Vec::new();
        for (key, expected, features) in &cases {
            let started = Instant::now();
            let p = run(&plan, features);
            times.push(started.elapsed());
            let d = (p - expected).abs();
            worst = worst.max(d);
            over += usize::from(d > 1e-3);
            flips += usize::from((p > 0.5) != (*expected > 0.5));
            probabilities.insert(key.clone(), serde_json::json!(p));
        }
        times.sort();
        println!(
            "tract {name:9} max|d|={worst:.4} cases>1e-3={over:2}/{} verdict flips={flips} median {:.1} ms",
            cases.len(),
            times[times.len() / 2].as_secs_f64() * 1e3
        );
        report.insert(name.to_owned(), probabilities.into());
    }
    if let Some(path) = json_out {
        std::fs::write(path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    }
}
