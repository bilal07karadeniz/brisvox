//! Errors shared by every brisvox crate.
//!
//! Each crate defines its own `Error` enum (via `thiserror`) and wraps this one with `#[from]`,
//! so the cross-cutting failure modes below keep one spelling across the engine.

use std::time::Duration;

/// Cross-cutting failures that any stage of a session can hit.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The task was cancelled through its `CancellationToken`. Not a fault: callers unwind.
    #[error("cancelled")]
    Cancelled,
    /// A bounded wait (network/IO/queue) exceeded its explicit timeout.
    #[error("{what} timed out after {after:?}")]
    Timeout {
        /// The operation that timed out, e.g. `"stt connect"`.
        what: &'static str,
        /// The timeout that elapsed.
        after: Duration,
    },
    /// The other end of an internal channel is gone (its task finished or was cancelled).
    #[error("channel closed: {0}")]
    ChannelClosed(&'static str),
    /// An audio buffer violates the frame format (e.g. not whole PCM16 samples).
    #[error("invalid audio: {0}")]
    InvalidAudio(&'static str),
    /// A sample rate the audio path does not support.
    #[error("unsupported sample rate: {0} Hz")]
    UnsupportedSampleRate(u32),
    /// A character that is not a DTMF key.
    #[error("invalid DTMF digit: {0:?}")]
    InvalidDtmfDigit(char),
    /// A pipeline cannot be built as described.
    #[error("invalid pipeline: {0}")]
    InvalidPipeline(&'static str),
    /// A processor task panicked; the pipeline was cancelled.
    #[error("processor {0} panicked")]
    ProcessorPanicked(&'static str),
}

/// `Result` alias with [`enum@Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_stable() {
        let e = Error::Timeout {
            what: "stt connect",
            after: Duration::from_millis(1500),
        };
        assert_eq!(e.to_string(), "stt connect timed out after 1.5s");
        assert_eq!(
            Error::ChannelClosed("vad->turn").to_string(),
            "channel closed: vad->turn"
        );
    }
}
