//! Error type of `brisvox-server`.

/// Errors produced by `brisvox-server`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A cross-cutting failure (cancelled, timeout, closed channel).
    #[error(transparent)]
    Core(#[from] brisvox_core::Error),
    /// Configuration is missing or invalid.
    #[error("config error: {0}")]
    Config(String),
    /// The tracing subscriber could not be installed (e.g. one is already set).
    #[error("telemetry init failed: {0}")]
    Telemetry(String),
    /// Admission control refused a new session.
    #[error("server at capacity")]
    AtCapacity,
    /// The server is draining or shut down and admits no new sessions.
    #[error("server is shutting down")]
    ShuttingDown,
    /// Admission control refused a new session because the inference pool is under pressure.
    #[error("server overloaded")]
    Overloaded,
    /// Admission control refused a new session because a provider is at its concurrency limit.
    #[error("provider {provider} at its limit of {limit} concurrent calls")]
    ProviderBudget {
        /// The exhausted provider.
        provider: String,
        /// Its configured limit.
        limit: usize,
    },
    /// A hosted call failed (transport, engine configuration, pipeline). The text names the
    /// step and never carries credentials.
    #[error("call failed: {0}")]
    Call(String),
}

impl From<crate::config::ConfigError> for Error {
    fn from(e: crate::config::ConfigError) -> Self {
        Self::Config(e.to_string())
    }
}

/// `Result` alias with this crate's [`enum@Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;
