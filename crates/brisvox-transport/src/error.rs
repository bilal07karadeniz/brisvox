//! Error type of `brisvox-transport`.

/// Errors produced by `brisvox-transport`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A cross-cutting failure (cancelled, timeout, closed channel).
    #[error(transparent)]
    Core(#[from] brisvox_core::Error),
    /// The remote peer disconnected.
    #[error("peer disconnected")]
    Disconnected,
    /// A socket could not be bound or inspected.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A configuration that cannot work (for example `ping_timeout` not longer than
    /// `ping_interval`); the message names the rule.
    #[error("invalid configuration: {0}")]
    Config(String),
    /// A websocket failure on the wire (connect, handshake, read, write); the rendered cause.
    #[error("websocket error: {0}")]
    WebSocket(String),
    /// A WebRTC failure (SDP, ICE, DTLS, Opus); the rendered cause, never credentials.
    #[error("webrtc error: {0}")]
    WebRtc(String),
    /// A SIP signaling failure (bind, transaction, dialog); the rendered cause, never credentials.
    #[error("sip error: {0}")]
    Sip(String),
    /// The peer broke the websocket control protocol (`docs/WEBSOCKET_PROTOCOL.md`).
    #[error("protocol violation: {0}")]
    Protocol(String),
    /// The server reported a fatal error to the client.
    #[error("server error {code}: {message}")]
    Remote {
        /// The server's error code.
        code: String,
        /// The server's description.
        message: String,
    },
    /// A WAV file could not be read.
    #[error(transparent)]
    Wav(#[from] crate::wav::WavError),
}

/// `Result` alias with this crate's [`enum@Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;
