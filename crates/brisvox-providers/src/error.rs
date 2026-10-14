//! Error type of `brisvox-providers`.

/// Errors produced by provider adapters (and their simulation mocks).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A cross-cutting failure (cancelled, timeout, closed channel).
    #[error(transparent)]
    Core(#[from] brisvox_core::Error),
    /// The provider refused the request (HTTP status or WebSocket close code), e.g. 429 or 500.
    #[error("{provider}: rejected with status {status}")]
    Rejected {
        /// Provider name.
        provider: &'static str,
        /// Status or close code.
        status: u16,
    },
    /// The connection dropped mid-stream.
    #[error("{provider}: disconnected")]
    Disconnected {
        /// Provider name.
        provider: &'static str,
    },
    /// The provider sent something that violates its protocol.
    #[error("{provider}: protocol error: {detail}")]
    Protocol {
        /// Provider name.
        provider: &'static str,
        /// What was wrong. Must never contain credentials.
        detail: String,
    },
    /// The resilience layer ([`crate::resilience`]) refused the attempt without contacting a
    /// provider: every circuit breaker of the stage was open, or the retry budget was spent.
    #[error("{provider}: unavailable ({reason})")]
    Unavailable {
        /// Name of the stage's primary provider.
        provider: &'static str,
        /// Why, e.g. `"circuit open"` or `"retry budget spent"`.
        reason: &'static str,
    },
}

/// `Result` alias with this crate's [`enum@Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;
