//! Errors of the call-control adapters.

use std::time::Duration;

use brisvox_agent::{TelephonyError, TelephonyErrorKind};

/// Where the caller is left after a warm transfer that failed halfway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerState {
    /// Nothing was changed on the caller's leg.
    Unchanged,
    /// The caller was put back on the bot's stream (the configured resume stream).
    Resumed,
    /// The caller waits in the conference with nobody to talk to and no way back to the bot was
    /// configured (or putting them back failed). The application must act (hang up, redirect).
    Parked,
}

/// Errors of [`CallControl`](super::CallControl) operations. No variant carries credentials; the
/// provider's own error text is cut, stripped of control characters and left out for 401/403
/// (some APIs echo part of the credential there).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A configuration that cannot work (for example plain `http://` to a remote host).
    #[error("invalid call-control configuration: {0}")]
    Config(String),
    /// The arguments are not acceptable; nothing was sent.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// The provider binding cannot do this (a Twilio stream with no resume configuration cannot
    /// send DTMF; a Telnyx binding without a connection id cannot place a second leg).
    #[error("{provider}: {what} is not supported with this configuration")]
    Unsupported {
        /// The provider.
        provider: &'static str,
        /// What is missing.
        what: &'static str,
    },
    /// The request got no complete answer in time. It may have been applied.
    #[error("{provider}: {what} timed out after {after:?}")]
    Timeout {
        /// The provider.
        provider: &'static str,
        /// The operation.
        what: &'static str,
        /// The limit that passed.
        after: Duration,
    },
    /// The provider could not be reached (DNS, connect, TLS). Nothing was sent.
    #[error("{provider}: unreachable: {detail}")]
    Unreachable {
        /// The provider.
        provider: &'static str,
        /// The cause, without the URL.
        detail: String,
    },
    /// The connection failed after the request left. It may have been applied.
    #[error("{provider}: connection failed: {detail}")]
    Transport {
        /// The provider.
        provider: &'static str,
        /// The cause, without the URL.
        detail: String,
    },
    /// The provider answered with an error status.
    #[error("{provider}: rejected with status {status}{}", code_suffix(.code))]
    Rejected {
        /// The provider.
        provider: &'static str,
        /// The HTTP status.
        status: u16,
        /// The provider's error code, if the body had one.
        code: Option<String>,
        /// The provider's description (bounded; empty for 401/403).
        detail: String,
    },
    /// The call does not exist (any more): hung up, or a wrong id.
    #[error("{provider}: the call is gone")]
    CallGone {
        /// The provider.
        provider: &'static str,
    },
    /// Telnyx refused a command because a media stream is active on the leg (`90045`) and the
    /// binding is not allowed to stop the stream.
    #[error("{provider}: refused while a media stream is active on the call")]
    StreamActive {
        /// The provider.
        provider: &'static str,
    },
    /// A command was refused because a media stream was active (Telnyx `90045`), the binding
    /// stopped the stream to make it possible, and the repeated command then failed. The bot's
    /// audio is gone for good (a stopped stream cannot be restarted from here), so the call is
    /// **not** "still with the bot" whatever the cause was. (A failing `streaming_stop` itself is
    /// not this variant: it is reported as it is.)
    #[error(
        "{provider}: the media stream was stopped to make the command possible and the command \
         then failed ({cause}); the bot can no longer talk on this call"
    )]
    StreamStopped {
        /// The provider.
        provider: &'static str,
        /// Why the repeated command failed.
        cause: Box<Error>,
    },
    /// The provider's answer did not have the documented shape (for example a conference id is
    /// missing). The request itself succeeded, so its effect may be in place.
    #[error("{provider}: unexpected response: {detail}")]
    Protocol {
        /// The provider.
        provider: &'static str,
        /// What was wrong.
        detail: String,
    },
    /// A warm transfer failed after it had changed the caller's leg.
    #[error("warm transfer failed ({cause}); the caller is {caller:?}")]
    WarmFailed {
        /// Why the transfer failed.
        cause: Box<Error>,
        /// Where the caller was left.
        caller: CallerState,
        /// What the failed attempt left behind (the conference, no agent leg), as a recovery
        /// handle for [`CallControl::abort_warm`](super::CallControl::abort_warm). `None` when
        /// the caller's leg is untouched or was put back on the bot.
        transfer: Option<Box<super::WarmTransfer>>,
    },
}

fn code_suffix(code: &Option<String>) -> String {
    code.as_ref()
        .map_or_else(String::new, |c| format!(" (code {c})"))
}

/// `Result` alias with this module's [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// Whether the request may have been applied although it failed (a timeout, a connection that
    /// dropped after sending, a 5xx, an unparseable success). The caller must check the call
    /// before repeating a possibly-applied command.
    pub fn outcome_unknown(&self) -> bool {
        match self {
            Self::Timeout { .. } | Self::Transport { .. } | Self::Protocol { .. } => true,
            Self::Rejected { status, .. } => *status >= 500,
            Self::WarmFailed { .. } | Self::StreamStopped { .. } => true,
            Self::Config(_)
            | Self::InvalidArgument(_)
            | Self::Unsupported { .. }
            | Self::Unreachable { .. }
            | Self::CallGone { .. }
            | Self::StreamActive { .. } => false,
        }
    }

    /// The matching [`TelephonyErrorKind`].
    pub fn kind(&self) -> TelephonyErrorKind {
        match self {
            Self::Config(_) | Self::InvalidArgument(_) => TelephonyErrorKind::InvalidArgument,
            Self::Unsupported { .. } => TelephonyErrorKind::Unsupported,
            Self::Unreachable { .. } => TelephonyErrorKind::Unavailable,
            Self::CallGone { .. } | Self::StreamActive { .. } => TelephonyErrorKind::Rejected,
            Self::WarmFailed { cause, caller, .. } => {
                // A failed warm transfer that left the caller untouched or back with the bot is a
                // definite failure; a parked caller or an unknown cause is not.
                if *caller == CallerState::Parked || cause.outcome_unknown() {
                    TelephonyErrorKind::OutcomeUnknown
                } else {
                    TelephonyErrorKind::Rejected
                }
            }
            // The stream is gone: never "nothing was done, the call continues with you".
            Self::Timeout { .. }
            | Self::Transport { .. }
            | Self::Protocol { .. }
            | Self::StreamStopped { .. } => TelephonyErrorKind::OutcomeUnknown,
            Self::Rejected { .. } => {
                if self.outcome_unknown() {
                    TelephonyErrorKind::OutcomeUnknown
                } else {
                    TelephonyErrorKind::Rejected
                }
            }
        }
    }
}

impl From<Error> for TelephonyError {
    fn from(error: Error) -> Self {
        Self::new(error.kind(), error.to_string())
    }
}
