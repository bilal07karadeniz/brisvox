//! Error type of `brisvox-sim`.

/// Errors produced by `brisvox-sim`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A cross-cutting failure (cancelled, timeout, closed channel).
    #[error(transparent)]
    Core(#[from] brisvox_core::Error),
    /// Called outside a tokio runtime.
    #[error("no tokio runtime")]
    NoRuntime,
    /// A fixture file is missing, malformed or inconsistent.
    #[error("fixture: {0}")]
    Fixture(String),
    /// A simulation assertion failed (the message explains what differed).
    #[error("assertion failed: {0}")]
    Assertion(String),
    /// Tasks outlived what should have ended them.
    #[error("task leak: {alive} tasks alive, baseline {baseline}")]
    TaskLeak {
        /// Tasks alive when the wait gave up.
        alive: usize,
        /// Tasks alive when counting started.
        baseline: usize,
    },
}

/// `Result` alias with this crate's [`enum@Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;
