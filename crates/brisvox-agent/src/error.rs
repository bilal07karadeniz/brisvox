//! Error type of `brisvox-agent`.

/// Errors produced by `brisvox-agent`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A cross-cutting failure (cancelled, timeout, closed channel).
    #[error(transparent)]
    Core(#[from] brisvox_core::Error),
    /// The LLM called a tool that is not registered for this session.
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    /// A streamed response could not be assembled (buffer cap, conflicting ids or names).
    #[error(transparent)]
    Assemble(#[from] crate::assemble::AssembleError),
}

/// `Result` alias with this crate's [`enum@Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;
