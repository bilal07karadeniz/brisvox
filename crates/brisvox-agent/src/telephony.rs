//! Telephony for the agent: the vocabulary the LLM-facing side shares with
//! the call-control adapters, the tools the model calls, DTMF input and the voicemail flow.
//! Protocol facts: `docs/providers/call-control.md`; design and wiring: `docs/TELEPHONY.md`.
//!
//! This module does no IO and knows no provider. The agent talks to a call through one small
//! trait object, [`TelephonyOps`], which the application binds to a live call (the transport
//! crate's `call_control::BoundCall` is the ready-made binding for Twilio and Telnyx REST). The
//! dependency points the right way: the transport crate depends on this module for its
//! vocabulary ([`DtmfString`], [`PhoneNumber`], [`TransferRequest`], [`AnsweredBy`]), never the
//! other way around.
//!
//! - [`tools`]: `transfer_call`, `send_dtmf` and `end_call` (all SideEffect), registered with
//!   [`register_telephony_tools`] under a [`TelephonyPolicy`] (an allow-list of named
//!   destinations: the model never produces a phone number).
//! - [`dtmf`]: caller key presses to a context note ([`dtmf::render_dtmf_note`]) and a collector
//!   with terminator, digit limit and inter-digit timeout ([`dtmf::DtmfCollector`]).
//! - [`voicemail`]: answering-machine handling: the leave-message [`Flow`](crate::flows::Flow)
//!   and the pure [`voicemail::VoicemailDriver`] that turns AMD verdicts into flow transitions.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

pub mod dtmf;
pub mod tools;
pub mod voicemail;

pub use tools::{Destination, PolicyError, TelephonyPolicy, register_telephony_tools};

/// Longest digit string one `send_dtmf` may carry by default (Twilio `SendDigits` allows 32).
pub const DEFAULT_MAX_DTMF_DIGITS: usize = 32;

/// A validated DTMF digit string: `0-9`, `A-D`, `*`, `#` and the pauses `w` (0.5 s) and `W`
/// (1 s), at least one key, at most the limit it was parsed with. Lowercase `a-d` are
/// normalized to uppercase; `w` and `W` keep their case because they differ.
///
/// This is the intersection of what Twilio (`<Play digits>`) and Telnyx (`send_dtmf`) accept, so
/// a parsed string is valid for both.
#[derive(Clone, PartialEq, Eq)]
pub struct DtmfString(String);

impl DtmfString {
    /// Parses `text` with the default limit [`DEFAULT_MAX_DTMF_DIGITS`].
    ///
    /// # Errors
    /// [`TelephonyErrorKind::InvalidArgument`] for an empty string, only pauses, a character
    /// outside the alphabet, or more characters than the limit.
    pub fn parse(text: &str) -> Result<Self, TelephonyError> {
        Self::parse_with_limit(text, DEFAULT_MAX_DTMF_DIGITS)
    }

    /// Parses `text`, allowing at most `max_len` characters (pauses included).
    ///
    /// # Errors
    /// As [`Self::parse`].
    pub fn parse_with_limit(text: &str, max_len: usize) -> Result<Self, TelephonyError> {
        let mut out = String::with_capacity(text.len());
        let mut keys = 0usize;
        for ch in text.chars() {
            match ch {
                '0'..='9' | 'A'..='D' | '*' | '#' => {
                    keys = keys.saturating_add(1);
                    out.push(ch);
                }
                'a'..='d' => {
                    keys = keys.saturating_add(1);
                    out.push(ch.to_ascii_uppercase());
                }
                'w' | 'W' => out.push(ch),
                _ => {
                    return Err(TelephonyError::invalid(
                        "DTMF digits may only contain 0-9, A-D, *, #, w and W",
                    ));
                }
            }
            if out.len() > max_len {
                return Err(TelephonyError::invalid(format!(
                    "at most {max_len} DTMF characters per request"
                )));
            }
        }
        if keys == 0 {
            return Err(TelephonyError::invalid(
                "DTMF digits need at least one key (0-9, A-D, *, #)",
            ));
        }
        Ok(Self(out))
    }

    /// The normalized digits.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Number of characters, pauses included.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Never true: a parsed string has at least one key.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for DtmfString {
    /// Digits can be a PIN or a card number typed into an IVR: only the length is printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DtmfString(len={})", self.0.len())
    }
}

/// An E.164 phone number: `+`, then 8 to 15 digits, the first one not `0`.
///
/// The `Debug` and [`Self::redacted`] forms keep only the country prefix and the last two digits
/// (phone numbers are personal data; logs and errors must not carry them whole).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PhoneNumber(String);

impl PhoneNumber {
    /// Parses `text` (surrounding whitespace is ignored).
    ///
    /// # Errors
    /// [`TelephonyErrorKind::InvalidArgument`] if it is not E.164.
    pub fn parse(text: &str) -> Result<Self, TelephonyError> {
        let text = text.trim();
        let digits = text.strip_prefix('+').unwrap_or_default();
        let valid = (8..=15).contains(&digits.len())
            && digits.bytes().all(|b| b.is_ascii_digit())
            && !digits.starts_with('0');
        if valid {
            Ok(Self(text.to_owned()))
        } else {
            Err(TelephonyError::invalid(
                "a phone number must be E.164: + and 8 to 15 digits, not starting with 0",
            ))
        }
    }

    /// The number, for the provider request. Do not log it; use [`Self::redacted`].
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `+90******33`: the first three characters and the last two digits.
    pub fn redacted(&self) -> String {
        let head: String = self.0.chars().take(3).collect();
        let tail: String = self
            .0
            .chars()
            .rev()
            .take(2)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("{head}******{tail}")
    }
}

impl fmt::Debug for PhoneNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PhoneNumber({})", self.redacted())
    }
}

/// How a call is handed to another party.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransferMode {
    /// Connect the caller to the destination and leave the call (the bot is gone).
    Cold,
    /// Park the caller, call the destination, optionally brief it, then connect both (conference
    /// based; see `docs/TELEPHONY.md`).
    Warm,
}

impl TransferMode {
    /// `"cold"` or `"warm"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Warm => "warm",
        }
    }
}

/// A transfer the tool asks the application to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRequest {
    /// Where to. Always from the policy's allow-list, never from the model.
    pub destination: PhoneNumber,
    /// The configured name of the destination (safe to log).
    pub destination_name: String,
    /// Cold or warm.
    pub mode: TransferMode,
    /// What to tell the receiving party before connecting it (warm only; sanitized, bounded).
    pub summary: Option<String>,
    /// Why the caller is transferred (for logs and records; sanitized, bounded).
    pub reason: Option<String>,
    /// Stable per call id: the digest of the tool engine's
    /// [`IdempotencyKey`](crate::tools::IdempotencyKey). Providers that take a command id use it
    /// so a repeated request is ignored.
    pub idempotency: u64,
}

/// Whether the receiving party of a warm transfer heard the summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Briefing {
    /// No summary was given or it is not part of this transfer mode.
    None,
    /// The summary is read to the receiving party before it is connected.
    Whisper,
    /// A summary was given but the provider binding cannot deliver it.
    NotSupported,
}

/// What a started transfer reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferReport {
    /// The mode that was started.
    pub mode: TransferMode,
    /// What happens to the summary.
    pub briefing: Briefing,
}

/// The party that answered an outbound call, as an answering-machine detector reports it.
///
/// The variants follow Twilio's `AnsweredBy` values (`docs/providers/call-control.md`, 1.7);
/// Telnyx results map onto them in the transport crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnsweredBy {
    /// A person.
    Human,
    /// A machine was detected and its greeting is still playing (Twilio `machine_start`, Telnyx
    /// detection result `machine`). Do not speak yet.
    MachineStart,
    /// The greeting ended with a beep (`machine_end_beep`, Telnyx `beep_detected`). Speak now.
    MachineEndBeep,
    /// The greeting ended with silence (`machine_end_silence`).
    MachineEndSilence,
    /// The greeting ended some other way, or the timeout cut the analysis (`machine_end_other`,
    /// Telnyx greeting result `ended`).
    MachineEndOther,
    /// A fax machine.
    Fax,
    /// The detector could not decide (`unknown`, Telnyx `not_sure`, silence).
    Unknown,
}

impl AnsweredBy {
    /// Whether a machine answered (any `Machine*` variant).
    pub const fn is_machine(self) -> bool {
        matches!(
            self,
            Self::MachineStart
                | Self::MachineEndBeep
                | Self::MachineEndSilence
                | Self::MachineEndOther
        )
    }

    /// Whether the machine's greeting is over, so a message can be spoken.
    pub const fn greeting_finished(self) -> bool {
        matches!(
            self,
            Self::MachineEndBeep | Self::MachineEndSilence | Self::MachineEndOther
        )
    }

    /// A stable lowercase label for logs and metrics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::MachineStart => "machine_start",
            Self::MachineEndBeep => "machine_end_beep",
            Self::MachineEndSilence => "machine_end_silence",
            Self::MachineEndOther => "machine_end_other",
            Self::Fax => "fax",
            Self::Unknown => "unknown",
        }
    }
}

/// Why a telephony operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TelephonyErrorKind {
    /// The operation is not available on this call or transport.
    Unsupported,
    /// The arguments are not acceptable (nothing was sent).
    InvalidArgument,
    /// The provider refused the request: it was not applied and the call is unchanged.
    Rejected,
    /// The provider could not be reached: nothing was applied.
    Unavailable,
    /// The request may or may not have been applied (a timeout or a connection that dropped after
    /// the request left). The caller must not repeat it blindly.
    OutcomeUnknown,
}

/// A failed telephony operation. The message is for the model and the log: it never carries
/// credentials or whole phone numbers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct TelephonyError {
    kind: TelephonyErrorKind,
    message: String,
}

impl TelephonyError {
    /// An error of `kind` with `message`.
    pub fn new(kind: TelephonyErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// [`TelephonyErrorKind::InvalidArgument`].
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(TelephonyErrorKind::InvalidArgument, message)
    }

    /// [`TelephonyErrorKind::Unsupported`].
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(TelephonyErrorKind::Unsupported, message)
    }

    /// What kind of failure.
    pub const fn kind(&self) -> TelephonyErrorKind {
        self.kind
    }

    /// The description.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Boxed future of a [`TelephonyOps`] method (keeps the trait object safe).
pub type OpsFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, TelephonyError>> + Send + 'a>>;

/// What the telephony tools do on a live call. The application implements it for the call a
/// session serves; the tools hold it as `Arc<dyn TelephonyOps>`.
///
/// Implementations must bound every network await with a timeout and must not block the thread.
/// The tools run as SideEffect tools, so each method is called at most once per tool call id.
pub trait TelephonyOps: Send + Sync {
    /// Resolves when what the assistant has said so far was played to the caller, or the
    /// implementation gave up waiting (it bounds its own wait). `transfer_call` awaits this
    /// before it asks for the transfer, so the hand-off sentence is not cut off. The default
    /// returns at once.
    fn settle_speech(&self) -> OpsFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    /// Plays DTMF tones toward the far end of the call. `idempotency` is stable per tool call.
    fn send_dtmf<'a>(&'a self, digits: &'a DtmfString, idempotency: u64) -> OpsFuture<'a, ()>;

    /// Starts a transfer. Returns when the provider accepted the request, not when the other
    /// party answered.
    fn transfer<'a>(&'a self, request: &'a TransferRequest) -> OpsFuture<'a, TransferReport>;

    /// Records that the call must end once the assistant's next response was played (the
    /// farewell). It must not cut the farewell: the actual hang-up happens after the
    /// utterance-end mark, as in the Twilio adapter's rule for "Hang-up".
    fn end_call(&self) -> OpsFuture<'_, ()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dtmf_strings_normalize_and_validate() {
        assert_eq!(DtmfString::parse("1a#wW2").unwrap().as_str(), "1A#wW2");
        assert!(DtmfString::parse("").is_err());
        assert!(DtmfString::parse("ww").is_err());
        assert!(DtmfString::parse("12e").is_err());
        assert!(DtmfString::parse("1 2").is_err());
        assert!(DtmfString::parse_with_limit("123", 2).is_err());
        assert!(DtmfString::parse_with_limit("12", 2).is_ok());
        assert_eq!(
            format!("{:?}", DtmfString::parse("1234").unwrap()),
            "DtmfString(len=4)"
        );
    }

    #[test]
    fn phone_numbers_are_e164_and_redacted() {
        let number = PhoneNumber::parse(" +905551112233 ").unwrap();
        assert_eq!(number.as_str(), "+905551112233");
        assert_eq!(number.redacted(), "+90******33");
        assert!(!format!("{number:?}").contains("5551112"));
        for bad in [
            "905551112233",
            "+0555111223",
            "+1234567",
            "+1234567890123456",
            "+90 555",
            "",
        ] {
            assert!(PhoneNumber::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn answered_by_classification() {
        assert!(AnsweredBy::MachineStart.is_machine());
        assert!(!AnsweredBy::MachineStart.greeting_finished());
        assert!(AnsweredBy::MachineEndBeep.greeting_finished());
        assert!(!AnsweredBy::Human.is_machine());
        assert!(!AnsweredBy::Unknown.is_machine());
        assert_eq!(AnsweredBy::MachineEndOther.as_str(), "machine_end_other");
    }
}
