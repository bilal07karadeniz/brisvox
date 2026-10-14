//! Observability of the server (operator view in `docs/OPERATIONS.md`, conventions in
//! `docs/TRACING.md`): Prometheus metrics, one OpenTelemetry trace per turn, and redaction of
//! log lines.
//!
//! # Shape
//! One [`Observability`] per process, shared as an `Arc`. Each call gets a [`CallObserver`]
//! ([`Observability::call`]): it is the [`TimelineSink`](brisvox_core::TimelineSink) of the call's
//! timeline recorder, so every finished turn lands in the latency histograms and becomes one
//! trace, and its provider and engine counters are folded in when the call ends. The
//! [`EngineRunner`](crate::runner::EngineRunner) wires this up
//! ([`with_observability`](crate::runner::EngineRunner::with_observability)); the health
//! listener serves `GET /metrics` from the manager the observability is attached to
//! ([`SessionManager::attach_observability`](crate::session::SessionManager::attach_observability)).
//!
//! # Where the data comes from
//! - Stage latency histograms, turn outcomes: the per-stage marks of `brisvox_core::timeline`, once per finished turn.
//! - Sessions, admission, pool pressure, provider budgets: read from the session
//!   manager at scrape time. They cost nothing between scrapes.
//! - Breaker states, the process retry budget and failovers: the breakers and budget
//!   are read at scrape time; each call's failovers, retries and budget refusals are folded in
//!   at every finished turn and when the call ends.
//! - Provider errors and engine trouble counters: the engine's [`CallStats`](brisvox_engine::session::CallStats),
//!   once when a call ends.
//!
//! Nothing here runs per audio frame, per token or per chunk.
//!
//! # Features
//! The `otlp` feature (off by default) adds `otlp::OtlpExporter`, which posts the traces as
//! OTLP/HTTP JSON to a collector. Everything else is always compiled and has no dependency
//! beyond the ones the server already has.

pub mod call;
pub mod exposition;
pub mod metrics;
#[cfg(feature = "otlp")]
pub mod otlp;
pub mod redact;
pub mod registry;
pub mod trace;

use std::sync::{Arc, OnceLock};

use brisvox_core::SharedClock;
use tokio_util::sync::CancellationToken;

pub use call::{CallGuard, CallObserver};
pub use redact::{RedactingMakeWriter, Redactor};
pub use registry::{
    BreakerSnapshot, BudgetSnapshot, Metrics, ProviderStage, ResilienceSnapshot, ResilienceSource,
    Stage,
};
pub use trace::{MemoryExporter, TraceExporter, TurnTrace};

use crate::config::Env;
use crate::session::SessionManager;
use crate::settings::{flag_var, var};
use crate::{Error, Result};

/// Environment variable: serve `GET /metrics` on the health listener (default `true`).
pub const ENV_METRICS: &str = "BRISVOX_METRICS";
/// Environment variable: where turn traces go: `off` (default) or `otlp` (feature `otlp`).
pub const ENV_TRACES: &str = "BRISVOX_TRACES";

/// The process's metrics, trace exporter and id source. See the [module docs](self).
pub struct Observability {
    metrics: Metrics,
    exporter: Option<Arc<dyn TraceExporter>>,
    ids: trace::IdGen,
    resilience: OnceLock<Arc<dyn ResilienceSource>>,
    smart_turn_pool: OnceLock<brisvox_inference::PoolHandle>,
    metrics_endpoint: bool,
}

impl std::fmt::Debug for Observability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Observability")
            .field("traces", &self.exporter.is_some())
            .field("metrics_endpoint", &self.metrics_endpoint)
            .finish_non_exhaustive()
    }
}

impl Default for Observability {
    fn default() -> Self {
        Self::new()
    }
}

impl Observability {
    /// Metrics only (no trace exporter), `/metrics` on, random ids.
    pub fn new() -> Self {
        Self {
            metrics: Metrics::new(),
            exporter: None,
            ids: trace::IdGen::random(),
            resilience: OnceLock::new(),
            smart_turn_pool: OnceLock::new(),
            metrics_endpoint: true,
        }
    }

    /// Sends every turn's trace to `exporter`.
    #[must_use]
    pub fn with_exporter(mut self, exporter: Arc<dyn TraceExporter>) -> Self {
        self.exporter = Some(exporter);
        self
    }

    /// Takes trace and span ids from `ids` (tests use a fixed seed).
    #[must_use]
    pub fn with_ids(mut self, ids: trace::IdGen) -> Self {
        self.ids = ids;
        self
    }

    /// Turns the `GET /metrics` answer on or off (the metrics are collected either way).
    #[must_use]
    pub const fn with_metrics_endpoint(mut self, on: bool) -> Self {
        self.metrics_endpoint = on;
        self
    }

    /// The metrics.
    pub const fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// Whether `GET /metrics` answers.
    pub const fn metrics_endpoint(&self) -> bool {
        self.metrics_endpoint
    }

    pub(crate) fn exporter(&self) -> Option<&Arc<dyn TraceExporter>> {
        self.exporter.as_ref()
    }

    pub(crate) const fn ids(&self) -> &trace::IdGen {
        &self.ids
    }

    /// Tells scrapes where the resilience layer's breakers and budget are. Set once; `false`
    /// when a source was already set.
    pub fn set_resilience_source(&self, source: Arc<dyn ResilienceSource>) -> bool {
        self.resilience.set(source).is_ok()
    }

    /// Tells scrapes which pool is the Smart Turn executor's: its queue depth, refused
    /// and timed-out analyses are exported as `brisvox_smart_turn_*`. Set once, and only when
    /// the process runs an executor (without one the series are absent); `false` when a pool was
    /// already set.
    pub fn set_smart_turn_pool(&self, pool: brisvox_inference::PoolHandle) -> bool {
        self.smart_turn_pool.set(pool).is_ok()
    }

    /// The observer for one call: use [`CallObserver::recorder`] as the call's timeline.
    pub fn call(
        self: &Arc<Self>,
        session: u64,
        agent: &str,
        clock: SharedClock,
    ) -> Arc<CallObserver> {
        CallObserver::new(
            Arc::clone(self),
            trace::TurnIdentity {
                session,
                agent: Arc::from(agent),
            },
            clock,
        )
    }

    /// The Prometheus text exposition for `manager` (content type
    /// [`METRICS_CONTENT_TYPE`]).
    pub fn render(&self, manager: &SessionManager) -> String {
        let smart_turn = self
            .smart_turn_pool
            .get()
            .map(brisvox_inference::PoolHandle::metrics);
        self.metrics
            .render(manager, self.resilience.get(), smart_turn.as_ref())
    }
}

/// The content type of [`Observability::render`].
pub const METRICS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// The observability settings of the environment (`docs/OPERATIONS.md`).
#[derive(Debug, Clone)]
pub struct ObservabilitySettings {
    /// Serve `/metrics` ([`ENV_METRICS`], default `true`; the setting only hides the endpoint).
    pub metrics: bool,
    /// Where traces go.
    pub traces: TraceSink,
}

/// Where the turn traces go.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub enum TraceSink {
    /// Nowhere (the default).
    #[default]
    Off,
    /// An OTLP/HTTP collector.
    #[cfg(feature = "otlp")]
    Otlp(otlp::OtlpConfig),
}

impl Default for ObservabilitySettings {
    fn default() -> Self {
        Self {
            metrics: true,
            traces: TraceSink::Off,
        }
    }
}

impl ObservabilitySettings {
    /// Reads `BRISVOX_METRICS`, `BRISVOX_TRACES` and, for `otlp`, the `BRISVOX_OTLP_*` variables.
    ///
    /// # Errors
    /// [`Error::Config`] naming the variable when a value does not parse, or when traces are
    /// asked for in a build without the feature that provides them.
    pub fn from_env(env: &dyn Env) -> Result<Self> {
        let metrics = flag_var(env, ENV_METRICS)?.unwrap_or(true);
        let traces = match var(env, ENV_TRACES)?
            .as_deref()
            .map(str::to_ascii_lowercase)
        {
            None => TraceSink::Off,
            Some(v) if matches!(v.as_str(), "off" | "none" | "false" | "0") => TraceSink::Off,
            #[cfg(feature = "otlp")]
            Some(v) if v == "otlp" => TraceSink::Otlp(otlp::OtlpConfig::from_env(env)?),
            #[cfg(not(feature = "otlp"))]
            Some(v) if v == "otlp" => {
                return Err(Error::Config(format!(
                    "{ENV_TRACES}=otlp, but this binary is built without the `otlp` feature"
                )));
            }
            Some(other) => {
                return Err(Error::Config(format!(
                    "{ENV_TRACES}: {other:?} is not off or otlp"
                )));
            }
        };
        Ok(Self { metrics, traces })
    }

    /// Builds the observability these settings describe. Starts the exporter's task on the
    /// current tokio runtime (an `otlp` sink needs one) and stops it with `cancel`.
    ///
    /// # Errors
    /// [`Error::Config`] when an `otlp` sink is asked for outside a tokio runtime.
    #[cfg_attr(not(feature = "otlp"), allow(unused_variables))]
    pub fn start(
        &self,
        clock: &SharedClock,
        cancel: &CancellationToken,
    ) -> Result<Arc<Observability>> {
        let obs = Observability::new().with_metrics_endpoint(self.metrics);
        let obs = match &self.traces {
            TraceSink::Off => obs,
            #[cfg(feature = "otlp")]
            TraceSink::Otlp(config) => {
                let counters = Arc::clone(&obs.metrics().traces);
                let exporter =
                    otlp::OtlpExporter::spawn(config, clock.clone(), cancel.clone(), counters)?;
                obs.with_exporter(exporter)
            }
        };
        Ok(Arc::new(obs))
    }
}

#[cfg(test)]
mod tests;
