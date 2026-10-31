//! Dedicated CPU inference pool for brisvox. Work heavier than ~50 µs (Silero VAD, Smart Turn,
//! heavy DSP) runs here, never on tokio worker threads.

pub mod denoise;
pub mod error;
pub mod gate;
pub mod mel;
pub mod pool;
#[cfg(feature = "silero")]
pub mod silero;
#[cfg(feature = "silero-tract")]
pub mod silero_tract;
pub mod smart_turn;
pub mod vad;

pub use denoise::{Backend as DenoiseBackend, BoxedDenoiser, Denoiser};
pub use error::{Error, Result};
pub use gate::{GateConfig, Gated, NoiseFloorHandle, RollingGate};
pub use pool::{Done, InferencePool, JobTiming, PoolConfig, PoolHandle, PoolMetrics, Priority};
pub use vad::{DenoisedChunk, SpeechModel, VadDetector, VadEvent, VadParams, VadSignal, VadStream};
