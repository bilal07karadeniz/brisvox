//! Error type of `brisvox-inference`.

/// Errors produced by `brisvox-inference`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A cross-cutting failure (cancelled, timeout, closed channel).
    #[error(transparent)]
    Core(#[from] brisvox_core::Error),
    /// The pool's bounded job queue is full; the job was rejected, not queued.
    #[error("inference pool saturated ({capacity} queued jobs)")]
    PoolSaturated {
        /// Queue capacity that was reached.
        capacity: usize,
    },
    /// The job panicked; only its own request fails, the worker thread survives.
    #[error("inference job panicked")]
    JobPanicked,
    /// An on-device model could not be loaded or run.
    #[error("model: {0}")]
    Model(String),
    /// A pool worker thread could not be started.
    #[error("cannot spawn inference thread: {0}")]
    Spawn(#[source] std::io::Error),
}

/// `Result` alias with this crate's [`enum@Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;
