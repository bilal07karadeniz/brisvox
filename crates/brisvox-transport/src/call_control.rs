//! Call control over REST: DTMF toward the far end, cold and warm
//! transfer, hang-up and answering-machine detection, for Twilio and Telnyx. Cargo feature
//! `call-control` (off by default). Protocol facts: `docs/providers/call-control.md` (fetched
//! 2026-10-08); design and wiring: `docs/TELEPHONY.md`.
//!
//! The media WebSocket ([`twilio`](crate::twilio)) carries audio, marks and inbound DTMF. It
//! cannot send DTMF, transfer or hang up with a REST-grade guarantee, so those go through the
//! provider's REST API, next to the stream. [`CallControl`] is the provider-neutral port:
//!
//! | operation | Twilio ([`TwilioCallControl`]) | Telnyx ([`TelnyxCallControl`]) |
//! |---|---|---|
//! | [`send_dtmf`](CallControl::send_dtmf) | update the call with `<Play digits>` then a new `<Connect><Stream>` (needs [`twilio::ResumeStream`] and an adapter that resumes the session) | `send_dtmf` command |
//! | [`transfer_cold`](CallControl::transfer_cold) | update the call with `<Dial>` TwiML | `transfer` command |
//! | [`transfer_warm`](CallControl::transfer_warm) | caller into a conference, agent leg created waiting silently; [`complete_warm`](CallControl::complete_warm) updates it to read the summary and join | conference with the caller, agent leg dialed (`link_to`); [`complete_warm`](CallControl::complete_warm) joins it |
//! | [`hangup`](CallControl::hangup) | update with `Status=completed` | `hangup` command |
//!
//! # Module placement (decision)
//! This is a module of the transport crate, not a new crate: it is the same kind of thing as
//! the Twilio adapter (it speaks a provider's protocol for a call), shares the crate's error
//! and feature conventions, and a new crate would only add a manifest. Its dependency on
//! `brisvox-agent` is for vocabulary types ([`DtmfString`],
//! [`PhoneNumber`](brisvox_agent::PhoneNumber),
//! [`TransferRequest`],
//! [`AnsweredBy`](brisvox_agent::AnsweredBy)) and the [`TelephonyOps`](brisvox_agent::TelephonyOps)
//! binding ([`BoundCall`]); the crate graph allows it (agent is below transport). The TLS setup
//! duplicates `brisvox_providers::net::tls_config` so transport does not depend on its
//! sibling, the providers crate.
//!
//! # Rules every operation follows
//! - Every request has a deadline on the injected [`Clock`](brisvox_core::Clock) covering the
//!   send, the headers and the bounded body; no redirect is followed; no proxy; `http://` only on
//!   loopback (credentials never travel in cleartext).
//! - Credentials are [`Secret`](brisvox_core::Secret)s and header values are marked sensitive;
//!   they appear in no error, log line or `Debug` output.
//! - **Nothing is retried automatically** except the one documented Telnyx case (`90045`, a
//!   rejected command, which is retried after `streaming_stop` under a new command id): a
//!   timeout or a dropped connection may mean the command was applied, and `Error::outcome_unknown`
//!   tells the caller so. After a Twilio timeout, [`TwilioCallControl::call_status`] reads the call
//!   before anything is repeated; Telnyx ignores a repeated `command_id` for 60 seconds.
//! - At-most-once is also the caller's job: the agent tools run as SideEffect tools with an
//!   idempotency key, which every request here carries as `idempotency` (the Telnyx `command_id`).

mod amd;
mod bound;
mod error;
mod http;
pub mod telnyx;
pub mod twilio;

use std::future::Future;
use std::pin::Pin;

use brisvox_agent::{Briefing, DtmfString, TransferRequest};

pub use amd::{
    AmdConfig, AmdMode, TelnyxAmdEvent, TwilioAmdResult, answered_by_from_twilio,
    parse_telnyx_amd_event, parse_twilio_amd_callback,
};
pub use bound::{BoundCall, SettleFn};
pub use error::{CallerState, Error, Result};
pub use http::form_decode;
pub use telnyx::{StreamStopHook, TelnyxCallControl, TelnyxConfig};
pub use twilio::{StreamRestartHook, TwilioAuth, TwilioCallControl, TwilioConfig};

/// The provider's identifier of one call leg: a Twilio `CallSid` or a Telnyx
/// `call_control_id`. Validated so it can be part of a URL path.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CallRef(String);

/// Longest call id accepted (Twilio SIDs are 34 characters; Telnyx ids are longer tokens).
const MAX_CALL_REF: usize = 256;

impl CallRef {
    /// Wraps a provider call id.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] if it is empty, longer than 256 characters or has characters
    /// outside `A-Z a-z 0-9 - _ : = .` (so it cannot alter a URL path).
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        let valid = !id.is_empty()
            && id.len() <= MAX_CALL_REF
            && id.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'=' | b'.')
            });
        if valid {
            Ok(Self(id))
        } else {
            Err(Error::InvalidArgument("not a valid call id".to_owned()))
        }
    }

    /// The id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for CallRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CallRef({})", self.0)
    }
}

/// What a binding can do with its current configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// [`CallControl::send_dtmf`] works.
    pub send_dtmf: bool,
    /// [`CallControl::transfer_warm`] works.
    pub warm_transfer: bool,
    /// A warm transfer reads the summary to the receiving party.
    pub warm_briefing: bool,
}

/// A warm transfer in progress: the caller waits in a conference and the agent leg is being
/// called. Returned by [`CallControl::transfer_warm`]; hand it to
/// [`CallControl::complete_warm`] when the agent answered (and the application accepted the
/// answer: a person, not a voicemail), or to [`CallControl::abort_warm`] when not.
///
/// A transfer that failed halfway comes back inside [`Error::WarmFailed`] with
/// [`agent_leg`](Self::agent_leg) `None` (the leg was never created, or the request that would
/// have created it has an unknown outcome): it is the recovery handle for
/// [`CallControl::abort_warm`].
#[derive(Clone, PartialEq, Eq)]
pub struct WarmTransfer {
    /// `"twilio"` or `"telnyx"`.
    pub provider: &'static str,
    /// The caller's leg.
    pub caller: CallRef,
    /// The leg that was called; `None` in a recovery handle (see above).
    pub agent_leg: Option<CallRef>,
    /// Twilio: the conference friendly name. Telnyx: the conference id.
    pub conference: String,
    /// What happens to the summary.
    pub briefing: Briefing,
    /// The summary to read to the agent when the legs are joined (Twilio; `None` where the
    /// binding cannot deliver it). Bounded and sanitized by the tool. Kept out of `Debug`: it can
    /// hold personal data.
    pub summary: Option<String>,
}

impl std::fmt::Debug for WarmTransfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WarmTransfer")
            .field("provider", &self.provider)
            .field("caller", &self.caller)
            .field("agent_leg", &self.agent_leg)
            .field("conference", &self.conference)
            .field("briefing", &self.briefing)
            .field(
                "summary_chars",
                &self.summary.as_ref().map(|s| s.chars().count()),
            )
            .finish()
    }
}

/// Boxed future of a [`CallControl`] method (keeps the trait object safe).
pub type CallFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// The provider-neutral call-control port. Implementations are cheap to clone around in an
/// `Arc`; all methods take `&self` and are safe to call from several tasks.
///
/// `idempotency` is a stable 64-bit key of the logical operation (the agent tool engine's
/// idempotency digest). Providers with a command id use it so a repeated request is ignored;
/// for the others it only names the conference.
pub trait CallControl: Send + Sync + std::fmt::Debug {
    /// `"twilio"` or `"telnyx"`.
    fn provider(&self) -> &'static str;

    /// What this binding can do as configured.
    fn capabilities(&self) -> Capabilities;

    /// Plays DTMF tones toward the far end of `call`.
    ///
    /// # Errors
    /// [`Error::Unsupported`] when the binding cannot (Twilio without a resume stream), plus the
    /// usual transport and provider errors.
    fn send_dtmf<'a>(
        &'a self,
        call: &'a CallRef,
        digits: &'a DtmfString,
        idempotency: u64,
    ) -> CallFuture<'a, ()>;

    /// Connects the caller of `call` to `request.destination` and leaves the call.
    ///
    /// # Errors
    /// Transport and provider errors; [`Error::outcome_unknown`] says whether to check the call.
    fn transfer_cold<'a>(
        &'a self,
        call: &'a CallRef,
        request: &'a TransferRequest,
    ) -> CallFuture<'a, ()>;

    /// Starts a warm (conference-based) transfer. The steps are documented on each binding; the
    /// operation runs to completion even if the returned future is dropped, so the caller is
    /// never left half-moved by a cancelled tool.
    ///
    /// # Errors
    /// [`Error::Unsupported`] when the binding lacks what it needs; [`Error::WarmFailed`] when it
    /// failed after changing the caller's leg (the error says where the caller was left).
    fn transfer_warm<'a>(
        &'a self,
        call: &'a CallRef,
        request: &'a TransferRequest,
    ) -> CallFuture<'a, WarmTransfer>;

    /// Finishes a warm transfer after the agent leg answered **and the application accepted the
    /// answer** (a person, not a voicemail: check the AMD verdict first). Until this is called
    /// the agent leg is never joined to the caller's conference, on either provider. Twilio:
    /// updates the agent leg with TwiML that reads the summary and joins the conference. Telnyx:
    /// joins the agent leg to the conference. The agent leg must be answered (an update of a
    /// ringing Twilio call is rejected).
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] for a recovery handle without an agent leg; transport and
    /// provider errors.
    fn complete_warm<'a>(&'a self, transfer: &'a WarmTransfer) -> CallFuture<'a, ()>;

    /// Gives up a warm transfer (the agent did not answer, declined or was a voicemail): ends the
    /// agent leg (when there is one) and, where possible, puts the caller back on the bot. The
    /// returned [`CallerState`] says where the caller was left: [`CallerState::Resumed`] on the
    /// bot, or [`CallerState::Parked`] when there is no way back (Twilio without a resume
    /// stream; Telnyx always, because a stopped media stream cannot be restarted from here) and
    /// the application must act (hang up, redirect).
    ///
    /// # Errors
    /// Transport and provider errors. The steps stop at the first error, so a retry repeats
    /// them safely.
    fn abort_warm<'a>(&'a self, transfer: &'a WarmTransfer) -> CallFuture<'a, CallerState>;

    /// Ends `call`.
    ///
    /// # Errors
    /// [`Error::CallGone`] if it already ended, plus the usual errors.
    fn hangup<'a>(&'a self, call: &'a CallRef, idempotency: u64) -> CallFuture<'a, ()>;
}
