//! `brisvox-bench`: benchmark harnesses.
//!
//! - [`sim_load`]: in-process load simulation, N concurrent simulated calls with mock providers,
//!   reporting the engine's internal overhead and resource use.
//! - [`infer_load`]: inference pool under N sessions x one job per 32 ms.
//! - [`resample_bench`]: resampler cost per frame, delay and quality per rate pair.
//! - [`fixture_load`]: whole simulated calls driven by the real-speech fixtures, English, Turkish
//!   and mixed, with response latency per fixture and per language.
//!
//! - [`v2v`]: the voice-to-voice latency benchmark, a websocket client that replays fixtures
//!   against any server (sim mode: the engine over mock providers in process, or the mock bot as
//!   a self-check; live mode: a URL) and reports p50/p90/p99 per turn over N runs.
//!
//! - [`capacity`]: the capacity benchmark, a ramp of N concurrent calls (each the v2v client)
//!   against any websocket URL, finding the largest N that keeps p95 V2V within an SLO, with
//!   the target process's CPU and memory.
//!
//! - `model_cpu` (feature `models`): CPU time per Smart Turn analysis and per Silero window for
//!   each ONNX Runtime session configuration and VAD backend.
//!
//! Comparisons against the Pipecat baseline use the same client.

pub mod capacity;
pub mod fixture_load;
pub mod infer_load;
#[cfg(feature = "models")]
pub mod model_cpu;
pub mod resample_bench;
pub mod resources;
pub mod sim_load;
pub mod spec_eval;
pub mod stats;
pub mod v2v;

/// Benchmark harness errors.
#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    /// The runtime or a sampler thread could not be created, or a report could not be written.
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    /// The configuration cannot be run.
    #[error("invalid configuration: {0}")]
    Config(String),
    /// A call could not be made or measured (connection, handshake, cancellation).
    #[error("call: {0}")]
    Call(String),
}
