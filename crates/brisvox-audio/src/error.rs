//! Error type of `brisvox-audio`.

/// Errors produced by `brisvox-audio`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A cross-cutting failure (cancelled, timeout, closed channel).
    #[error(transparent)]
    Core(#[from] brisvox_core::Error),
    /// The output buffer cannot hold the result.
    #[error("output buffer too small: need {needed} bytes, have {available}")]
    BufferTooSmall {
        /// Bytes required.
        needed: usize,
        /// Bytes available.
        available: usize,
    },
    /// A PCM16 buffer with a partial sample.
    #[error("PCM16 buffer has an odd length ({0} bytes)")]
    OddPcmLength(usize),
    /// The resampler library rejected a configuration or buffer.
    #[error("resampler: {0}")]
    Resampler(String),
}

/// `Result` alias with this crate's [`enum@Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;
