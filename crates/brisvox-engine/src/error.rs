//! Error type of `brisvox-engine`.

/// Errors produced by `brisvox-engine`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A cross-cutting failure (cancelled, timeout, closed channel).
    #[error(transparent)]
    Core(#[from] brisvox_core::Error),
    /// The inference pool rejected, lost or timed out a job.
    #[error(transparent)]
    Inference(#[from] brisvox_inference::Error),
    /// Audio conversion failed (the resampler rejected a buffer or a configuration).
    #[error(transparent)]
    Audio(#[from] brisvox_audio::Error),
    /// The conversation flow of a call could not start.
    #[error("flow: {0}")]
    Flow(#[from] brisvox_agent::FlowError),
    /// An [`EngineConfig`](crate::EngineConfig) that cannot work; the message names the rule.
    #[error("invalid engine configuration: {0}")]
    Config(&'static str),
}

/// `Result` alias with this crate's [`enum@Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;
