//! Smart Turn on ONNX Runtime (feature `smart-turn-ort`): the runtime Pipecat uses, so the
//! probabilities match the reference pipeline.
//!
//! An ort `Session` runs through `&mut`, so the model keeps a stack of idle sessions: a call
//! takes one, runs it, and puts it back. Creating a session takes ~100 ms, so
//! [`SmartTurnOrt::load`] creates the first one and [`SmartTurnOrt::warm`] tops the stack up to
//! the number of pool threads at startup; a call that finds the stack empty creates a session
//! itself (a cold start, counted by [`SmartTurnOrt::cold_starts`]). The stack never holds more
//! sessions than calls ever ran at once (at most the pool's threads). Each session uses
//! [`DEFAULT_INTRA_THREADS`] (2) intra-op threads, measured as the best latency-for-CPU trade
//! with spinning on ([`DEFAULT_INTRA_OP_SPINNING`], measured with the model-CPU benchmark); [`OrtOptions`] with
//! [`SmartTurnOrt::load_with_options`] (or [`SmartTurnOrt::load_with_threads`]) changes both.
//! Neither changes a single bit of the output. Every session (the first, the
//! warmed ones and cold starts) is created by one method from the stored thread count. Outputs
//! stay within the golden tolerance at 1, 2 and 4 threads (`tests/smart_turn.rs`); the ignored
//! test `more_threads_cut_the_latency_of_warm_and_cold_sessions` checks the setting takes effect.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use ort::session::Session;
use ort::value::TensorRef;

use super::{TurnModel, check_features, check_probability, check_size};
use crate::error::{Error, Result};
use crate::mel::{FRAMES, N_MELS};

/// ONNX Runtime threads per analysis by default. 2 cuts the verdict from ~26 ms to ~17 ms for
/// ~30% more CPU per call (docs/LATENCY.md); `load_with_threads(path, 1)` restores the
/// cheapest setting.
pub const DEFAULT_INTRA_THREADS: usize = 2;

/// Whether the intra-op workers spin by default: on, ONNX Runtime's own default.
/// Spinning off halves the idle burn (73 -> 44 ms of CPU per analysis at 25 analyses a second,
/// same bits, +4% latency on an idle machine) but under load each analysis took 16% longer
/// (21.9 -> 25.5 ms on the pool thread, workers woken by the OS instead of already running) and
/// the 2-thread pool saturated at 250 calls on Windows (p95 1.8-2.0 s, twice; spinning on: 1.0-1.2
/// s), so the pool, not the CPU, was the limit and the default stays on. The Linux container may differ:
/// docs/LATENCY.md "Model CPU per call".
pub const DEFAULT_INTRA_OP_SPINNING: bool = true;

fn model_error(e: impl std::fmt::Display) -> Error {
    Error::Model(e.to_string())
}

/// How a Smart Turn session is configured. Every field changes only scheduling, never
/// the numbers: the probabilities are bit-identical across settings
/// (`probabilities_are_bit_identical_across_session_configurations` in `tests/smart_turn.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrtOptions {
    /// Intra-op threads per analysis (at least 1).
    pub intra_threads: usize,
    /// `session.intra_op.allow_spinning`: whether the intra-op worker threads busy-wait for the
    /// next piece of work instead of sleeping. ONNX Runtime's own default is on, and so is ours.
    pub intra_op_spinning: bool,
}

impl Default for OrtOptions {
    fn default() -> Self {
        Self {
            intra_threads: DEFAULT_INTRA_THREADS,
            intra_op_spinning: DEFAULT_INTRA_OP_SPINNING,
        }
    }
}

fn new_session(path: &Path, options: OrtOptions) -> Result<Session> {
    Session::builder()
        .map_err(model_error)?
        .with_intra_threads(options.intra_threads.max(1))
        .map_err(model_error)?
        .with_intra_op_spinning(options.intra_op_spinning)
        .map_err(model_error)?
        .with_inter_threads(1)
        .map_err(model_error)?
        .with_parallel_execution(false)
        .map_err(model_error)?
        .with_independent_thread_pool()
        .map_err(model_error)?
        .commit_from_file(path)
        .map_err(model_error)
}

/// The pinned model file and its idle ONNX Runtime sessions.
pub struct SmartTurnOrt {
    model: PathBuf,
    idle: Mutex<Vec<Session>>,
    cold_starts: AtomicU64,
    options: OrtOptions,
    /// Sessions that exist (idle, in use, or being created by `warm`): `warm` reserves here
    /// before it creates one, so concurrent warm jobs create the target in all, not each.
    sessions: AtomicUsize,
    /// Unique per loaded instance (its sessions are its own): the warm-up key.
    instance: u64,
}

/// Numbers the loaded instances (see [`TurnModel::warm_key`]).
static INSTANCES: AtomicU64 = AtomicU64::new(0);

impl std::fmt::Debug for SmartTurnOrt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmartTurnOrt")
            .field("model", &self.model)
            .field("idle", &self.idle_sessions())
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl SmartTurnOrt {
    /// Checks the model file (size), then creates one session (~100 ms: do it at startup).
    ///
    /// # Errors
    /// [`Error::Model`] if the file is missing, not the pinned size, or rejected by ONNX
    /// Runtime.
    pub fn load(path: &Path) -> Result<Arc<Self>> {
        Self::load_with_threads(path, DEFAULT_INTRA_THREADS)
    }

    /// [`Self::load`] with `intra_threads` ONNX Runtime threads per analysis (at least 1).
    /// Measured on the reference desktop (onnxruntime 1.28.0): 1 thread 25.9 ms,
    /// 2 threads 16.8 ms, 4 threads 12.6 ms per call, at about 26 / 34 / 50 ms of CPU; each
    /// pool job then occupies that many cores while it runs (docs/LATENCY.md).
    ///
    /// # Errors
    /// [`Error::Model`] if the file is missing, not the pinned size, or rejected by ONNX
    /// Runtime.
    pub fn load_with_threads(path: &Path, intra_threads: usize) -> Result<Arc<Self>> {
        Self::load_with_options(
            path,
            OrtOptions {
                intra_threads,
                ..OrtOptions::default()
            },
        )
    }

    /// [`Self::load`] with explicit [`OrtOptions`].
    ///
    /// # Errors
    /// [`Error::Model`] if the file is missing, not the pinned size, or rejected by ONNX
    /// Runtime.
    pub fn load_with_options(path: &Path, options: OrtOptions) -> Result<Arc<Self>> {
        let size = std::fs::metadata(path)
            .map_err(|e| model_error(format!("{}: {e}", path.display())))?
            .len();
        check_size(size)?;
        let options = OrtOptions {
            intra_threads: options.intra_threads.max(1),
            ..options
        };
        let first = new_session(path, options)?;
        Ok(Arc::new(Self {
            model: path.to_path_buf(),
            idle: Mutex::new(vec![first]),
            cold_starts: AtomicU64::new(0),
            options,
            sessions: AtomicUsize::new(1),
            instance: INSTANCES.fetch_add(1, Ordering::Relaxed),
        }))
    }

    /// Creates sessions until at least `sessions` exist (call it at startup with the pool's
    /// thread count, so no analysis pays a cold start). Safe to call from several threads at
    /// once: each session is reserved before it is created, so N concurrent calls with the same
    /// target create the target in all (each about one session when N equals the target), which
    /// is how the engine fans the warm-up out over the pool. Sessions in use and cold
    /// starts count, so the stack never grows past what is needed.
    ///
    /// # Errors
    /// [`Error::Model`] if ONNX Runtime rejects the model (the reservation is given back).
    pub fn warm(&self, sessions: usize) -> Result<()> {
        while self
            .sessions
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < sessions).then_some(n + 1)
            })
            .is_ok()
        {
            match self.create_session() {
                Ok(session) => self.lock().push(session),
                Err(error) => {
                    self.sessions.fetch_sub(1, Ordering::AcqRel);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// Sessions that exist: idle, in use by an analysis, or reserved by a running `warm`.
    #[must_use]
    pub fn sessions(&self) -> usize {
        self.sessions.load(Ordering::Acquire)
    }

    /// ONNX Runtime intra-op threads of every session of this model (the value given to
    /// [`Self::load_with_threads`], at least 1; [`DEFAULT_INTRA_THREADS`] after [`Self::load`]).
    #[must_use]
    pub fn intra_threads(&self) -> usize {
        self.options.intra_threads
    }

    /// The session options of this model (what every session, warmed or cold, is built with).
    #[must_use]
    pub fn options(&self) -> OrtOptions {
        self.options
    }

    /// Sessions idle right now.
    #[must_use]
    pub fn idle_sessions(&self) -> usize {
        self.lock().len()
    }

    /// Calls that found no idle session and created one.
    #[must_use]
    pub fn cold_starts(&self) -> u64 {
        self.cold_starts.load(Ordering::Relaxed)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Session>> {
        // A panic while holding the lock cannot leave the Vec half-updated (push/pop only).
        self.idle.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn take(&self) -> Result<Session> {
        if let Some(session) = self.lock().pop() {
            return Ok(session);
        }
        self.cold_starts.fetch_add(1, Ordering::Relaxed);
        let session = self.create_session()?;
        self.sessions.fetch_add(1, Ordering::AcqRel);
        Ok(session)
    }

    /// The only way sessions after the first are made: warm and cold-start sessions cannot
    /// drift from the configured thread count.
    fn create_session(&self) -> Result<Session> {
        new_session(&self.model, self.options)
    }
}

impl TurnModel for SmartTurnOrt {
    fn warm(&self, sessions: usize) -> Result<()> {
        // The inherent `warm` (an associated function of the type wins over the trait's).
        SmartTurnOrt::warm(self, sessions)
    }

    fn warm_key(&self) -> Option<String> {
        Some(format!(
            "smart-turn-ort:{}#{}",
            self.model.display(),
            self.instance
        ))
    }

    fn predict(&self, features: &[f32]) -> Result<f32> {
        check_features(features)?;
        let mut session = self.take()?;
        let probability = run(&mut session, features);
        // Put it back even after a failed run: a run error does not poison the session.
        self.lock().push(session);
        probability
    }
}

fn run(session: &mut Session, features: &[f32]) -> Result<f32> {
    #[allow(clippy::cast_possible_wrap)] // 80 and 800
    let shape = [1_i64, N_MELS as i64, FRAMES as i64];
    let input = TensorRef::from_array_view((shape, features)).map_err(model_error)?;
    let outputs = session
        .run(ort::inputs!["input_features" => input])
        .map_err(model_error)?;
    let (_, output) = outputs
        .get("logits")
        .ok_or_else(|| model_error("no logits tensor"))?
        .try_extract_tensor::<f32>()
        .map_err(model_error)?;
    check_probability(output)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use brisvox_core::{Clock, SystemClock};

    use super::{Session, SmartTurnOrt, run};
    use crate::mel::FEATURES;
    use crate::smart_turn::MODEL_FILE;

    fn median_run(session: &mut Session, features: &[f32]) -> Duration {
        let clock = SystemClock;
        run(session, features).unwrap(); // first run: allocations
        let mut runs: Vec<Duration> = (0..21)
            .map(|_| {
                let t = clock.now();
                run(session, features).unwrap();
                clock.now().saturating_duration_since(t)
            })
            .collect();
        runs.sort();
        runs[10]
    }

    /// The thread count reaches ONNX Runtime for the warmed sessions and for cold starts alike
    /// (a session built with 1 thread, or a runtime build that ignores `with_intra_threads`,
    /// would not get faster). Timing, so ignored: run it alone, in release, on an idle machine:
    /// `cargo test -p brisvox-inference --release --features smart-turn-ort --lib -- --ignored`.
    #[test]
    #[ignore = "timing: run alone in release on an idle machine"]
    fn more_threads_cut_the_latency_of_warm_and_cold_sessions() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../models")
            .join(MODEL_FILE);
        let features = vec![0.0_f32; FEATURES];
        let mut medians = Vec::new();
        for threads in [1, 2, 4] {
            let model = SmartTurnOrt::load_with_threads(&path, threads).unwrap();
            model.warm(2).unwrap();
            let mut warmed = model.take().unwrap(); // pushed by `warm` (LIFO)
            let _first = model.take().unwrap(); // created by `load_with_threads`
            let mut cold = model.take().unwrap(); // the stack is empty: a cold start
            assert_eq!(model.cold_starts(), 1);
            let (w, c) = (
                median_run(&mut warmed, &features),
                median_run(&mut cold, &features),
            );
            println!("{threads} intra-op threads: warmed {w:?}, cold start {c:?}");
            medians.push((w, c));
        }
        // Measured 25.9 -> 16.8 ms (0.65x) from 1 to 2 threads (docs/LATENCY.md).
        let faster = |two: Duration, one: Duration| two.as_secs_f64() < 0.85 * one.as_secs_f64();
        assert!(faster(medians[1].0, medians[0].0), "warmed: {medians:?}");
        assert!(
            faster(medians[1].1, medians[0].1),
            "cold start: {medians:?}"
        );
    }
}
