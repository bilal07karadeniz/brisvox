//! CPU cost of the two models every call runs: one Smart Turn analysis per ONNX Runtime
//! session configuration, and one Silero 32 ms window per backend.
//!
//! The question is CPU *time*, not wall time: on a shared machine the wall clock moves with every
//! other process, the process's CPU time does not (much). So each figure is the process's
//! accumulated CPU time (all threads, so ONNX Runtime's intra-op workers and their spinning are
//! included) divided by the number of analyses in a batch; the wall time of each call is
//! reported next to it because the trade is "CPU saved against latency added".
//!
//! Two modes per Smart Turn configuration:
//! - `tight`: analyses back to back. Shows the CPU of the work itself.
//! - `paced`: one analysis, then `gap` of sleep, per analysis. The CPU is read after the sleep, so
//!   what the worker threads burn while waiting for work that does not come is in the figure;
//!   this is what a pool thread sees under load (200 calls make ~20 analyses a second).
//!   An `idle` control (the same period, no analysis, all sessions loaded; it contains the cost of
//!   the CPU readings themselves) is measured first and subtracted from every paced batch.
//!
//! The configurations run interleaved (round `r` starts at configuration `r mod n`, every round
//! visits every configuration once), so a noisy neighbour hits all of them alike. The process
//! CPU clock is coarse (sysinfo reports whole milliseconds, Windows accumulates it per tick), so
//! the p50/p99 of the CPU columns are over batch means, not over single analyses.
//!
//! Exactness: every configuration runs every golden case; the probabilities must be
//! bit-identical to the first configuration's and within the golden tolerance of the reference.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use brisvox_core::{Clock, SystemClock};
use brisvox_inference::SpeechModel;
use brisvox_inference::mel::{FEATURES, WINDOW_SAMPLES, WhisperLogMel};
use brisvox_inference::silero::SileroVad;
use brisvox_inference::silero_tract::SileroTractModel;
use brisvox_inference::smart_turn::TurnModel;
use brisvox_inference::smart_turn::ort_backend::{OrtOptions, SmartTurnGraph, SmartTurnOrt};
use brisvox_sim::fixture::{Fixture, fixtures_dir};

use crate::BenchError;
use crate::resources::ProcessProbe;
use crate::stats::{Summary, ms};

/// What to measure.
#[derive(Debug, Clone)]
pub struct ModelCpuConfig {
    /// Directory with `smart-turn-v3.2-cpu.onnx` and `silero_vad.onnx`.
    pub models_dir: PathBuf,
    /// The Smart Turn session configurations; the first is the exactness baseline.
    pub smart_turn: Vec<OrtOptions>,
    /// Rounds (each visits every configuration once per mode).
    pub rounds: usize,
    /// Smart Turn analyses per batch.
    pub batch: usize,
    /// Sleep after each analysis in the paced mode.
    pub gap: Duration,
    /// Silero windows per batch (`0`: skip Silero).
    pub silero_batch: usize,
}

impl ModelCpuConfig {
    /// The default plan: 1 and 2 intra-op threads, spinning on and off, 30 rounds of 20.
    pub fn new(models_dir: PathBuf) -> Self {
        let options = |intra_threads, intra_op_spinning| OrtOptions {
            intra_threads,
            intra_op_spinning,
            ..OrtOptions::default()
        };
        Self {
            models_dir,
            smart_turn: vec![
                options(2, true),
                options(1, true),
                options(2, false),
                options(1, false),
            ],
            rounds: 30,
            batch: 20,
            gap: Duration::from_millis(40),
            silero_batch: 2_000,
        }
    }
}

/// Parses `"2:1,1:0:original"` (threads:spinning[:fused|original]) into session options.
///
/// # Errors
/// [`BenchError::Config`] on a malformed entry.
pub fn parse_configs(text: &str) -> Result<Vec<OrtOptions>, BenchError> {
    text.split(',')
        .map(|entry| {
            let bad = || {
                BenchError::Config(format!(
                    "bad config {entry:?}, expected THREADS:0|1[:fused|original]"
                ))
            };
            let mut parts = entry.trim().split(':');
            let (Some(threads), Some(spin)) = (parts.next(), parts.next()) else {
                return Err(bad());
            };
            let graph = match parts.next() {
                None | Some("fused") => SmartTurnGraph::Fused,
                Some("original") => SmartTurnGraph::Original,
                Some(_) => return Err(bad()),
            };
            if parts.next().is_some() {
                return Err(bad());
            }
            Ok(OrtOptions {
                intra_threads: threads.parse().map_err(|_| bad())?,
                intra_op_spinning: match spin {
                    "0" => false,
                    "1" => true,
                    _ => return Err(bad()),
                },
                graph,
            })
        })
        .collect()
}

/// One mode of one Smart Turn configuration.
#[derive(Debug, Clone, Default)]
pub struct ModeResult {
    /// Wall time of each analysis.
    pub wall: Summary,
    /// Mean CPU time per analysis over all batches.
    pub cpu_mean: Duration,
    /// CPU per analysis, one value per batch: its distribution.
    pub cpu_batches: Summary,
}

/// Result for one Smart Turn configuration.
#[derive(Debug, Clone)]
pub struct SmartTurnResult {
    /// The configuration.
    pub options: OrtOptions,
    /// Back-to-back analyses.
    pub tight: ModeResult,
    /// Analyses with a sleep after each.
    pub paced: ModeResult,
    /// Golden cases whose probability differs in bits from the baseline's.
    pub bit_diffs: usize,
    /// Largest `|probability - golden|` over the golden cases.
    pub worst_golden_error: f32,
    /// Golden cases whose verdict (> 0.5) differs from the reference.
    pub verdict_diffs: usize,
}

/// Result for one Silero backend.
#[derive(Debug, Clone)]
pub struct SileroResult {
    /// Backend name.
    pub name: &'static str,
    /// Wall time of each window.
    pub wall: Summary,
    /// Mean CPU time per window.
    pub cpu_mean: Duration,
    /// CPU per window, one value per batch.
    pub cpu_batches: Summary,
}

/// Everything measured.
#[derive(Debug, Clone)]
pub struct ModelCpuReport {
    /// Smart Turn, in the order of the configuration list.
    pub smart_turn: Vec<SmartTurnResult>,
    /// CPU per `batch * gap` period of the idle control (subtracted from every paced batch).
    pub idle_per_period: Duration,
    /// Silero backends (tract, then ONNX Runtime); empty if skipped.
    pub silero: Vec<SileroResult>,
    /// Silero: largest `|p_tract - p_ort|` on the fixture, and the windows where the 0.5
    /// decision differs.
    pub silero_diff: Option<SileroAgreement>,
    /// Number of golden cases.
    pub golden_cases: usize,
    /// Rounds and batch sizes used, for the report.
    pub plan: String,
    /// Analyses per batch (the idle control lasts `batch * gap`).
    pub batch: usize,
}

fn model_err(what: &str, e: impl std::fmt::Display) -> BenchError {
    BenchError::Config(format!("{what}: {e}"))
}

fn fixture_samples(name: &str) -> Result<Vec<i16>, BenchError> {
    let fixture = Fixture::load(&fixtures_dir(), name).map_err(|e| model_err(name, e))?;
    Ok(fixture
        .pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect())
}

/// The reference's input: the last 8 s, zero-padded at the start.
fn last_8_seconds(pcm: &[i16]) -> Vec<f32> {
    let audio: Vec<f32> = pcm.iter().map(|&s| f32::from(s) / 32_768.0).collect();
    let tail = audio
        .get(audio.len().saturating_sub(WINDOW_SAMPLES)..)
        .unwrap_or(&[]);
    let mut window = vec![0.0; WINDOW_SAMPLES.saturating_sub(tail.len())];
    window.extend_from_slice(tail);
    window
}

struct GoldenCase {
    features: Vec<f32>,
    probability: f32,
}

fn golden_cases() -> Result<Vec<GoldenCase>, BenchError> {
    let text = std::fs::read_to_string(fixtures_dir().join("smart_turn_golden.json"))?;
    let golden: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| model_err("smart_turn_golden.json", e))?;
    let cases = golden
        .get("cases")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| BenchError::Config("golden file has no cases".into()))?;
    let mut mel = WhisperLogMel::new();
    let mut out = Vec::new();
    for (key, case) in cases {
        let field = |name: &str| case.get(name).and_then(serde_json::Value::as_u64);
        let fixture = case
            .get("fixture")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| BenchError::Config(format!("{key}: no fixture")))?;
        let (Some(start), Some(end), Some(probability)) = (
            field("start"),
            field("end"),
            case.get("probability").and_then(serde_json::Value::as_f64),
        ) else {
            return Err(BenchError::Config(format!("{key}: incomplete golden case")));
        };
        let pcm = fixture_samples(fixture)?;
        let (start, end) = (
            usize::try_from(start).unwrap_or(0),
            usize::try_from(end).unwrap_or(0),
        );
        let slice = pcm
            .get(start..end)
            .ok_or_else(|| BenchError::Config(format!("{key}: range outside the fixture")))?;
        let mut features = vec![0.0; FEATURES];
        mel.compute(&last_8_seconds(slice), &mut features)
            .map_err(|e| model_err("features", e))?;
        #[allow(clippy::cast_possible_truncation)] // goldens are f32 values
        out.push(GoldenCase {
            features,
            probability: probability as f32,
        });
    }
    Ok(out)
}

/// Process CPU time, read through [`ProcessProbe`].
struct Cpu(ProcessProbe);

impl Cpu {
    fn now(&mut self) -> Result<Duration, BenchError> {
        self.0
            .read()
            .map(|r| r.cpu_time)
            .ok_or_else(|| BenchError::Config("this platform reports no process CPU time".into()))
    }
}

#[derive(Default)]
struct Acc {
    walls: Vec<Duration>,
    batch_cpu: Vec<Duration>,
    cpu_total: Duration,
    count: usize,
}

impl Acc {
    fn add(&mut self, walls: Vec<Duration>, cpu: Duration, count: usize) {
        self.walls.extend(walls);
        self.cpu_total += cpu;
        self.count += count;
        if count > 0 {
            self.batch_cpu.push(cpu / u32::try_from(count).unwrap_or(1));
        }
    }

    fn finish(self) -> ModeResult {
        ModeResult {
            wall: Summary::of(self.walls),
            cpu_mean: self
                .cpu_total
                .checked_div(u32::try_from(self.count).unwrap_or(1).max(1))
                .unwrap_or_default(),
            cpu_batches: Summary::of(self.batch_cpu),
        }
    }
}

#[allow(clippy::disallowed_methods)] // a standalone measuring thread, not a tokio worker
fn sleep(d: Duration) {
    std::thread::sleep(d);
}

/// Runs `batch` analyses, each followed by `gap` of sleep (none if zero); the CPU is read after
/// the last sleep.
fn smart_turn_batch(
    model: &SmartTurnOrt,
    features: &[f32],
    batch: usize,
    gap: Duration,
    cpu: &mut Cpu,
    clock: &SystemClock,
) -> Result<(Vec<Duration>, Duration), BenchError> {
    let mut walls = Vec::with_capacity(batch);
    let before = cpu.now()?;
    for _ in 0..batch {
        let t = clock.now();
        model
            .predict(features)
            .map_err(|e| model_err("predict", e))?;
        walls.push(clock.now().saturating_duration_since(t));
        if !gap.is_zero() {
            sleep(gap);
        }
    }
    let after = cpu.now()?;
    Ok((walls, after.saturating_sub(before)))
}

/// Measures everything in `config`.
///
/// # Errors
/// [`BenchError::Config`] if a model or fixture does not load or a run fails.
pub fn run(config: &ModelCpuConfig) -> Result<ModelCpuReport, BenchError> {
    let clock = SystemClock;
    let mut cpu = Cpu(ProcessProbe::new());
    let cases = golden_cases()?;
    let timing_features = &cases
        .first()
        .ok_or_else(|| BenchError::Config("no golden cases".into()))?
        .features;

    // Load every configuration up front, warm them, and check exactness while they are cold-free.
    let path = config
        .models_dir
        .join(brisvox_inference::smart_turn::MODEL_FILE);
    let mut models = Vec::new();
    for options in &config.smart_turn {
        let model = SmartTurnOrt::load_with_options(&path, *options)
            .map_err(|e| model_err(&path.display().to_string(), e))?;
        for _ in 0..5 {
            model
                .predict(timing_features)
                .map_err(|e| model_err("warm-up", e))?;
        }
        models.push(model);
    }
    let mut probabilities: Vec<Vec<f32>> = Vec::new();
    for model in &models {
        let mut row = Vec::new();
        for case in &cases {
            row.push(
                model
                    .predict(&case.features)
                    .map_err(|e| model_err("golden", e))?,
            );
        }
        probabilities.push(row);
    }
    let baseline = probabilities.first().cloned().unwrap_or_default();

    let n = models.len();
    let mut tight: Vec<Acc> = (0..n).map(|_| Acc::default()).collect();
    let mut paced: Vec<Acc> = (0..n).map(|_| Acc::default()).collect();
    // Control first: what a quiet period costs with every session loaded (it contains the cost
    // of the two CPU readings). The process clock is sampled per tick, so it is a mean over many.
    let period = config.gap * u32::try_from(config.batch).unwrap_or(1);
    let idle_period = {
        let before = cpu.now()?;
        for _ in 0..10 {
            sleep(period);
        }
        cpu.now()?.saturating_sub(before) / 10
    };
    for round in 0..config.rounds {
        eprintln!(
            "model-cpu: smart turn round {}/{}",
            round + 1,
            config.rounds
        );
        for k in 0..n {
            let i = (round + k) % n;
            let Some(model) = models.get(i) else { continue };
            let (walls, used) = smart_turn_batch(
                model,
                timing_features,
                config.batch,
                Duration::ZERO,
                &mut cpu,
                &clock,
            )?;
            if let Some(acc) = tight.get_mut(i) {
                acc.add(walls, used, config.batch);
            }
        }
        for k in 0..n {
            let i = (round + k) % n;
            let Some(model) = models.get(i) else { continue };
            let (walls, used) = smart_turn_batch(
                model,
                timing_features,
                config.batch,
                config.gap,
                &mut cpu,
                &clock,
            )?;
            if let Some(acc) = paced.get_mut(i) {
                acc.add(walls, used.saturating_sub(idle_period), config.batch);
            }
        }
    }

    let mut smart_turn = Vec::new();
    for (i, (t, p)) in tight.into_iter().zip(paced).enumerate() {
        let row = probabilities.get(i).cloned().unwrap_or_default();
        let (mut bit_diffs, mut verdict_diffs, mut worst) = (0, 0, 0.0_f32);
        for ((got, base), case) in row.iter().zip(&baseline).zip(&cases) {
            bit_diffs += usize::from(got.to_bits() != base.to_bits());
            verdict_diffs += usize::from((*got > 0.5) != (case.probability > 0.5));
            worst = worst.max((got - case.probability).abs());
        }
        smart_turn.push(SmartTurnResult {
            options: config.smart_turn.get(i).copied().unwrap_or_default(),
            tight: t.finish(),
            paced: p.finish(),
            bit_diffs,
            worst_golden_error: worst,
            verdict_diffs,
        });
    }

    let (silero, silero_diff) = if config.silero_batch == 0 {
        (Vec::new(), None)
    } else {
        silero_run(config, &mut cpu, &clock)?
    };
    Ok(ModelCpuReport {
        smart_turn,
        idle_per_period: idle_period,
        silero,
        silero_diff,
        golden_cases: cases.len(),
        batch: config.batch,
        plan: format!(
            "{} rounds x {} analyses per configuration and mode (paced: {} ms sleep after each), \
             Silero {} windows per batch",
            config.rounds,
            config.batch,
            config.gap.as_millis(),
            config.silero_batch
        ),
    })
}

/// 64 + 512 sample inputs from the fixtures, as the VAD worker builds them.
fn silero_windows() -> Result<Vec<Vec<f32>>, BenchError> {
    let mut windows = Vec::new();
    for name in ["tr_two_utterances", "en_two_utterances"] {
        let pcm: Vec<f32> = fixture_samples(name)?
            .iter()
            .map(|&s| f32::from(s) / 32_768.0)
            .collect();
        let mut context = vec![0.0_f32; 64];
        for w in pcm.as_chunks::<512>().0 {
            let mut input = context.clone();
            input.extend_from_slice(w.as_slice());
            context = input.get(512..).unwrap_or(&[]).to_vec();
            windows.push(input);
        }
    }
    Ok(windows)
}

/// Largest probability difference between the two Silero backends, windows where their 0.5
/// decisions differ, windows compared.
pub type SileroAgreement = (f32, usize, usize);

fn silero_run(
    config: &ModelCpuConfig,
    cpu: &mut Cpu,
    clock: &SystemClock,
) -> Result<(Vec<SileroResult>, Option<SileroAgreement>), BenchError> {
    let windows = silero_windows()?;
    let tract_model = SileroTractModel::bundled().map_err(|e| model_err("silero tract", e))?;
    let mut backends: Vec<(&'static str, Box<dyn SpeechModel>)> = vec![
        (
            "tract",
            Box::new(
                tract_model
                    .stream()
                    .map_err(|e| model_err("silero tract", e))?,
            ),
        ),
        (
            "ONNX Runtime",
            Box::new(
                SileroVad::load(&config.models_dir.join("silero_vad.onnx"))
                    .map_err(|e| model_err("silero ort", e))?,
            ),
        ),
    ];
    // Agreement of the two backends over the fixture windows (fresh state each).
    let mut probabilities: Vec<Vec<f32>> = Vec::new();
    for (_, model) in &mut backends {
        model.reset();
        let mut row = Vec::new();
        for w in &windows {
            row.push(model.infer(w).map_err(|e| model_err("silero", e))?);
        }
        model.reset();
        probabilities.push(row);
    }
    let diff = match (probabilities.first(), probabilities.get(1)) {
        (Some(a), Some(b)) => {
            let worst = a
                .iter()
                .zip(b)
                .map(|(x, y)| (x - y).abs())
                .fold(0.0_f32, f32::max);
            let decisions = a
                .iter()
                .zip(b)
                .filter(|(x, y)| (**x >= 0.5) != (**y >= 0.5))
                .count();
            Some((worst, decisions, windows.len()))
        }
        _ => None,
    };

    let mut accs: Vec<Acc> = backends.iter().map(|_| Acc::default()).collect();
    let n = backends.len();
    let mut cursor = 0_usize;
    for round in 0..config.rounds {
        for k in 0..n {
            let i = (round + k) % n;
            let (Some((_, model)), Some(acc)) = (backends.get_mut(i), accs.get_mut(i)) else {
                continue;
            };
            let mut walls = Vec::with_capacity(config.silero_batch);
            let before = cpu.now()?;
            for _ in 0..config.silero_batch {
                let w = windows
                    .get(cursor % windows.len())
                    .ok_or_else(|| BenchError::Config("no silero windows".into()))?;
                if cursor.is_multiple_of(windows.len()) {
                    model.reset();
                }
                cursor += 1;
                let t = clock.now();
                model.infer(w).map_err(|e| model_err("silero", e))?;
                walls.push(clock.now().saturating_duration_since(t));
            }
            let used = cpu.now()?.saturating_sub(before);
            acc.add(walls, used, config.silero_batch);
        }
    }
    let results = backends
        .iter()
        .zip(accs)
        .map(|((name, _), acc)| {
            let m = acc.finish();
            SileroResult {
                name,
                wall: m.wall,
                cpu_mean: m.cpu_mean,
                cpu_batches: m.cpu_batches,
            }
        })
        .collect();
    Ok((results, diff))
}

fn us(d: Duration) -> String {
    format!("{:.1}", d.as_secs_f64() * 1e6)
}

fn graph_name(graph: SmartTurnGraph) -> &'static str {
    match graph {
        SmartTurnGraph::Fused => "fused",
        SmartTurnGraph::Original => "original",
    }
}

fn mode_rows(out: &mut String, title: &str, results: &[SmartTurnResult], paced: bool) {
    let _ = writeln!(out, "\n{title}\n");
    let _ = writeln!(
        out,
        "| intra-op threads | spinning | graph | CPU mean (ms) | CPU p50 / p99 of batch means (ms) | wall p50 / p99 (ms) |"
    );
    let _ = writeln!(out, "|---:|:---:|:---:|---:|---:|---:|");
    for r in results {
        let m = if paced { &r.paced } else { &r.tight };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} / {} | {} / {} |",
            r.options.intra_threads,
            if r.options.intra_op_spinning {
                "on"
            } else {
                "off"
            },
            graph_name(r.options.graph),
            ms(m.cpu_mean),
            ms(m.cpu_batches.p50),
            ms(m.cpu_batches.p99),
            ms(m.wall.p50),
            ms(m.wall.p99),
        );
    }
}

/// The report as markdown.
pub fn markdown(report: &ModelCpuReport, environment: &[String]) -> String {
    let mut out = String::from("# Model CPU per call\n\n");
    for line in environment {
        let _ = writeln!(out, "- {line}");
    }
    let _ = writeln!(out, "- plan: {}", report.plan);
    mode_rows(
        &mut out,
        "## Smart Turn, back to back (CPU of the work itself)",
        &report.smart_turn,
        false,
    );
    mode_rows(
        &mut out,
        "## Smart Turn, paced (CPU includes what idle workers burn between analyses)",
        &report.smart_turn,
        true,
    );
    let _ = writeln!(
        out,
        "\nControl, subtracted from every paced batch above: a quiet period of {} analysis slots \
         with every session loaded (and two CPU readings) costs {} ms ({} us per slot).",
        report.batch,
        ms(report.idle_per_period),
        us(report.idle_per_period / u32::try_from(report.batch.max(1)).unwrap_or(1)),
    );
    let _ = writeln!(
        out,
        "\n## Exactness on the {} golden cases (vs the first configuration)\n",
        report.golden_cases
    );
    let _ = writeln!(
        out,
        "| intra-op threads | spinning | graph | probabilities differing in bits | worst abs error vs reference | verdicts differing from reference |"
    );
    let _ = writeln!(out, "|---:|:---:|:---:|---:|---:|---:|");
    for r in &report.smart_turn {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {:.2e} | {} |",
            r.options.intra_threads,
            if r.options.intra_op_spinning {
                "on"
            } else {
                "off"
            },
            graph_name(r.options.graph),
            r.bit_diffs,
            r.worst_golden_error,
            r.verdict_diffs
        );
    }
    if !report.silero.is_empty() {
        let _ = writeln!(out, "\n## Silero VAD, one 32 ms window\n");
        let _ = writeln!(
            out,
            "| backend | CPU mean (us) | CPU p50 / p99 of batch means (us) | wall p50 / p99 (us) |"
        );
        let _ = writeln!(out, "|---|---:|---:|---:|");
        for r in &report.silero {
            let _ = writeln!(
                out,
                "| {} | {} | {} / {} | {} / {} |",
                r.name,
                us(r.cpu_mean),
                us(r.cpu_batches.p50),
                us(r.cpu_batches.p99),
                us(r.wall.p50),
                us(r.wall.p99)
            );
        }
        if let Some((worst, decisions, windows)) = report.silero_diff {
            let _ = writeln!(
                out,
                "\nBackends on {windows} fixture windows: max |p_tract - p_ort| = {worst:.2e}, \
                 {decisions} windows where the 0.5 decision differs."
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configs_parse_as_threads_and_spinning() {
        let parsed = parse_configs("2:1, 1:0").unwrap();
        assert_eq!(
            parsed,
            vec![
                OrtOptions {
                    intra_threads: 2,
                    intra_op_spinning: true,
                    ..OrtOptions::default()
                },
                OrtOptions {
                    intra_threads: 1,
                    intra_op_spinning: false,
                    ..OrtOptions::default()
                }
            ]
        );
        let graphs = parse_configs("1:1:original,1:1:fused").unwrap();
        assert_eq!(graphs[0].graph, SmartTurnGraph::Original);
        assert_eq!(graphs[1].graph, SmartTurnGraph::Fused);
        for bad in ["2", "x:1", "2:2", "2:1,", "", "1:1:fast", "1:1:fused:x"] {
            assert!(parse_configs(bad).is_err(), "{bad:?}");
        }
    }
}
