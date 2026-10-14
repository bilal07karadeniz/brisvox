//! The real models of the engine target: Silero VAD and Smart Turn v3.2, the two models
//! every production call runs, in place of the simulator's energy VAD and fixed Smart Turn stub.
//!
//! [`RealModels`] is built exactly like `brisvox-server` builds them (`runner.rs`, `Vad::production`
//! and `load_smart_turn`): the bundled Silero VAD on tract, shared by every call with a stream
//! state per call, installed through the production [`VadSetup`] with the engine's own
//! [`VadWorkerConfig`] (so the VAD worker batches windows and runs them on the inference pool like
//! a real call); and Smart Turn on ONNX Runtime with the default two threads per analysis, one
//! session per pool thread warmed by the engine when a call starts. Both need the `models`
//! feature of this crate (`silero-tract` and `smart-turn-ort` of `brisvox-inference`, the
//! server's feature names); without it [`RealModels::load`] refuses, so a flag asking for models
//! never silently measures the stub.
//!
//! The Smart Turn model is wrapped in `TimedTurnModel`, which counts the analyses and the time
//! inside the model (on the pool thread, from the injected clock): the CPU split between the
//! Smart Turn model and everything else on the pool (the VAD windows, the feature extraction) in
//! a capacity run's `stats` lines.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use brisvox_core::SharedClock;
use brisvox_engine::{EngineConfig, Offload, SessionBuilder};
#[cfg(feature = "models")]
use brisvox_engine::{VadSetup, VadWorkerConfig};
use brisvox_inference::smart_turn::TurnModel;
use brisvox_providers::{LlmProvider, SttProvider, TtsProvider};

use crate::BenchError;

/// What the Smart Turn model has done since it was loaded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnModelStats {
    /// Analyses (`predict` calls) that ran.
    pub analyses: u64,
    /// Time inside `predict`, summed.
    pub run_total: Duration,
}

/// A [`TurnModel`] that counts what the wrapped model does. Everything else (the warm-up and its
/// key) is the wrapped model's own, so the engine warms and claims it exactly as without the
/// wrapper.
struct TimedTurnModel {
    inner: Arc<dyn TurnModel>,
    clock: SharedClock,
    analyses: AtomicU64,
    run_ns: AtomicU64,
}

impl TurnModel for TimedTurnModel {
    fn predict(&self, features: &[f32]) -> brisvox_inference::Result<f32> {
        let start = self.clock.now();
        let result = self.inner.predict(features);
        let ran = self.clock.now().saturating_duration_since(start);
        self.analyses.fetch_add(1, Ordering::Relaxed);
        self.run_ns.fetch_add(
            u64::try_from(ran.as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        result
    }

    fn warm(&self, sessions: usize) -> brisvox_inference::Result<()> {
        self.inner.warm(sessions)
    }

    fn warm_key(&self) -> Option<String> {
        self.inner.warm_key()
    }
}

/// Environment variable that overrides the Smart Turn session configuration of the bench (for
/// A/B runs of the capacity benchmark): `THREADS:SPINNING`, e.g. `2:1` (the old default: two
/// intra-op threads, spinning) or `2:0`. Unset: the library default, as the server runs.
pub const SMART_TURN_ENV: &str = "BRISVOX_BENCH_SMART_TURN";

/// The session options for the bench's Smart Turn model.
#[cfg(feature = "models")]
fn smart_turn_options() -> Result<brisvox_inference::smart_turn::ort_backend::OrtOptions, BenchError>
{
    match std::env::var(SMART_TURN_ENV) {
        Ok(text) => crate::model_cpu::parse_configs(&text)?
            .first()
            .copied()
            .ok_or_else(|| BenchError::Config(format!("{SMART_TURN_ENV} is empty"))),
        Err(_) => Ok(Default::default()),
    }
}

/// Silero VAD and Smart Turn v3.2, loaded once and shared by every call of an engine server.
#[derive(Clone)]
pub struct RealModels {
    #[cfg(feature = "models")]
    silero: Arc<brisvox_inference::silero_tract::SileroTractModel>,
    smart_turn: Arc<TimedTurnModel>,
}

impl fmt::Debug for RealModels {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RealModels")
            .field("stats", &self.turn_stats())
            .finish_non_exhaustive()
    }
}

impl RealModels {
    /// Loads the bundled Silero VAD and the Smart Turn model file of `dir`
    /// (`smart-turn-v3.2-cpu.onnx`, fetched by `scripts/models/fetch_models.py`). Blocking: the
    /// file read and the first ONNX Runtime session take ~100 ms; call it before the runtime or
    /// on a blocking thread.
    ///
    /// # Errors
    /// [`BenchError::Config`] if a model does not load (missing or wrong file), or if this
    /// binary is built without the `models` feature.
    #[cfg(feature = "models")]
    pub fn load(dir: &Path, clock: SharedClock) -> Result<Self, BenchError> {
        let silero = brisvox_inference::silero_tract::SileroTractModel::bundled()
            .map_err(|e| BenchError::Config(format!("cannot load the Silero VAD: {e}")))?;
        let path = dir.join(brisvox_inference::smart_turn::MODEL_FILE);
        let options = smart_turn_options()?;
        let inner: Arc<dyn TurnModel> =
            brisvox_inference::smart_turn::ort_backend::SmartTurnOrt::load_with_options(
                &path, options,
            )
            .map_err(|e| {
                BenchError::Config(format!(
                    "cannot load the Smart Turn model {}: {e} (fetch it with \
                     scripts/models/fetch_models.py)",
                    path.display()
                ))
            })?;
        Ok(Self {
            silero,
            smart_turn: Arc::new(TimedTurnModel {
                inner,
                clock,
                analyses: AtomicU64::new(0),
                run_ns: AtomicU64::new(0),
            }),
        })
    }

    /// Without the `models` feature there is nothing to load.
    ///
    /// # Errors
    /// Always [`BenchError::Config`]: rebuild with `--features models`.
    #[cfg(not(feature = "models"))]
    pub fn load(dir: &Path, clock: SharedClock) -> Result<Self, BenchError> {
        let _ = (dir, clock);
        Err(BenchError::Config(
            "--models needs a binary built with `--features models` (Silero VAD on tract and \
             Smart Turn on ONNX Runtime)"
                .to_owned(),
        ))
    }

    /// Whether this binary can load the real models.
    pub const fn available() -> bool {
        cfg!(feature = "models")
    }

    /// What the Smart Turn model has done so far.
    pub fn turn_stats(&self) -> TurnModelStats {
        TurnModelStats {
            analyses: self.smart_turn.analyses.load(Ordering::Relaxed),
            run_total: Duration::from_nanos(self.smart_turn.run_ns.load(Ordering::Relaxed)),
        }
    }

    /// The Smart Turn model of a call (the shared instance).
    pub fn smart_turn(&self) -> Arc<dyn TurnModel> {
        Arc::clone(&self.smart_turn) as Arc<dyn TurnModel>
    }

    /// Installs the Silero VAD on `builder`, its jobs on `offload`, with the worker settings of
    /// the production server (`VadWorkerConfig::from_engine`).
    #[cfg(feature = "models")]
    pub fn with_vad<S, L, T>(
        &self,
        builder: SessionBuilder<S, L, T>,
        offload: Offload,
        config: &EngineConfig,
    ) -> SessionBuilder<S, L, T>
    where
        S: SttProvider,
        L: LlmProvider,
        T: TtsProvider,
    {
        let model = Arc::clone(&self.silero);
        let mut worker = VadWorkerConfig::from_engine(config);
        // A diagnostic, not a setting: `BRISVOX_BENCH_VAD_MIN_SILENCE_MS` replaces the
        // detector's `min_silence` (100 ms) to measure what its window quantisation costs.
        if let Some(ms) = std::env::var("BRISVOX_BENCH_VAD_MIN_SILENCE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            worker.params.min_silence = Duration::from_millis(ms);
        }
        builder.with_vad(VadSetup::new(offload, worker, move || model.stream()))
    }

    /// Without the feature no value of this type exists ([`Self::load`] never succeeds).
    #[cfg(not(feature = "models"))]
    pub fn with_vad<S, L, T>(
        &self,
        builder: SessionBuilder<S, L, T>,
        offload: Offload,
        config: &EngineConfig,
    ) -> SessionBuilder<S, L, T>
    where
        S: SttProvider,
        L: LlmProvider,
        T: TtsProvider,
    {
        let _ = (offload, config);
        builder
    }
}

#[cfg(all(test, not(feature = "models")))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use brisvox_core::SystemClock;

    #[test]
    fn without_the_feature_asking_for_models_is_refused_not_stubbed() {
        assert!(!RealModels::available());
        let error = RealModels::load(Path::new("models"), SystemClock::shared()).unwrap_err();
        assert!(error.to_string().contains("--features models"), "{error}");
    }
}
