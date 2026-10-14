//! The telephony tools: `transfer_call`, `send_dtmf` and `end_call`, all SideEffect (never run
//! speculatively, never run twice for a call id, and not dropped by the engine on a timeout or an
//! interruption).
//!
//! Safety rules (`docs/providers/call-control.md`, 1.5.3 "Tool contract"):
//! - **The model never produces a phone number.** `transfer_call` takes the *name* of a
//!   destination from the [`TelephonyPolicy`] allow-list (the schema is an `enum` of the names).
//!   A prompt injection cannot dial a premium-rate number. With no destination configured the
//!   tool is not registered at all.
//! - **One transfer per call.** A second `transfer_call` is refused while one is running or has
//!   been accepted. A transfer that failed definitively (rejected or unreachable provider, nothing
//!   applied) releases the claim so the model can try another destination; one whose outcome is
//!   unknown keeps it.
//! - **The hand-off sentence is played first.** `transfer_call` awaits
//!   [`TelephonyOps::settle_speech`] (bounded by [`TelephonyPolicy::settle_timeout`] on the
//!   injected clock) before it asks for the transfer.
//! - Phone numbers are logged redacted; the tool results carry the destination *name*.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use brisvox_core::SharedClock;
use futures::future::{Either, select};
use serde_json::{Value, json};

use super::{
    DEFAULT_MAX_DTMF_DIGITS, DtmfString, PhoneNumber, TelephonyError, TelephonyErrorKind,
    TelephonyOps, TransferMode, TransferRequest,
};
use crate::tools::{
    RegistryError, Tool, ToolCallCtx, ToolError, ToolFuture, ToolKind, ToolOutput, ToolRegistry,
    ToolSpec,
};

/// Default wait for the assistant's speech to be played before a transfer: 8 seconds.
pub const DEFAULT_SETTLE_TIMEOUT: Duration = Duration::from_secs(8);
/// Default time the provider calls of one transfer may take in total: 40 seconds, four
/// sequential requests at the transport's default 10-second request deadline (the worst chain:
/// Telnyx refuses the conference command while streaming, stops the stream, repeats it, and
/// dials; Twilio parks the caller, creates the agent leg and rolls the caller back). Set
/// [`TelephonyPolicy::provider_budget`] to `requests * request_timeout` of the binding in use.
pub const DEFAULT_PROVIDER_BUDGET: Duration = Duration::from_secs(40);
/// Slack added on top of the settle wait and the provider budget in the derived tool timeout.
pub const TRANSFER_TIMEOUT_MARGIN: Duration = Duration::from_secs(2);
/// Longest `summary` of a warm transfer, in characters (Twilio `<Say>` and the 4000-character
/// TwiML limit leave room for this).
pub const MAX_SUMMARY_CHARS: usize = 600;
/// Longest `reason`, in characters.
pub const MAX_REASON_CHARS: usize = 200;
/// Longest destination name.
pub const MAX_DESTINATION_NAME: usize = 32;

/// Why a [`TelephonyPolicy`] cannot be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PolicyError {
    /// The name is empty, longer than [`MAX_DESTINATION_NAME`] or has characters outside
    /// `[A-Za-z0-9_-]`.
    #[error("invalid destination name {0:?}: use 1 to {MAX_DESTINATION_NAME} of A-Z a-z 0-9 _ -")]
    InvalidName(String),
    /// Two destinations share a name.
    #[error("destination {0:?} is configured twice")]
    Duplicate(String),
    /// The number is not E.164.
    #[error("destination {name:?} has an invalid number: {reason}")]
    InvalidNumber {
        /// The destination.
        name: String,
        /// Why.
        reason: String,
    },
}

/// A place the model may transfer the call to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// The name the model uses (`"sales"`, `"duty_manager"`).
    pub name: String,
    /// The number.
    pub number: PhoneNumber,
    /// When to use it, shown to the model in the tool description.
    pub description: String,
}

/// What the telephony tools may do on this deployment.
#[derive(Debug, Clone)]
pub struct TelephonyPolicy {
    destinations: Vec<Destination>,
    /// Whether `mode: "warm"` is offered (needs a provider binding that supports it).
    pub allow_warm: bool,
    /// Whether `send_dtmf` is registered.
    pub allow_dtmf: bool,
    /// Longest digit string of one `send_dtmf` (pauses included).
    pub max_dtmf_digits: usize,
    /// How long `transfer_call` waits for the hand-off sentence to be played.
    pub settle_timeout: Duration,
    /// Total time the provider requests of one transfer may take (see
    /// [`DEFAULT_PROVIDER_BUDGET`]).
    pub provider_budget: Duration,
    /// The `transfer_call` tool timeout. `None` derives it as `settle_timeout +
    /// provider_budget + TRANSFER_TIMEOUT_MARGIN`, so the model never gets a "timeout, do not
    /// repeat" for a transfer that is still within the transport's own deadlines. An explicit
    /// value is used as given (a shorter one risks exactly that answer).
    pub transfer_timeout: Option<Duration>,
}

impl Default for TelephonyPolicy {
    fn default() -> Self {
        Self {
            destinations: Vec::new(),
            allow_warm: false,
            allow_dtmf: true,
            max_dtmf_digits: DEFAULT_MAX_DTMF_DIGITS,
            settle_timeout: DEFAULT_SETTLE_TIMEOUT,
            provider_budget: DEFAULT_PROVIDER_BUDGET,
            transfer_timeout: None,
        }
    }
}

impl TelephonyPolicy {
    /// A policy with no destination (so no `transfer_call`), DTMF allowed, warm transfers off.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a destination.
    ///
    /// # Errors
    /// [`PolicyError`] for a bad or repeated name or a number that is not E.164.
    pub fn destination(
        mut self,
        name: &str,
        number: &str,
        description: &str,
    ) -> Result<Self, PolicyError> {
        let valid_name = !name.is_empty()
            && name.len() <= MAX_DESTINATION_NAME
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !valid_name {
            return Err(PolicyError::InvalidName(name.to_owned()));
        }
        if self.destinations.iter().any(|d| d.name == name) {
            return Err(PolicyError::Duplicate(name.to_owned()));
        }
        let number = PhoneNumber::parse(number).map_err(|e| PolicyError::InvalidNumber {
            name: name.to_owned(),
            reason: e.message().to_owned(),
        })?;
        self.destinations.push(Destination {
            name: name.to_owned(),
            number,
            description: clean(description, 120),
        });
        Ok(self)
    }

    /// Allows (or forbids) warm transfers.
    #[must_use]
    pub fn with_warm(mut self, allow: bool) -> Self {
        self.allow_warm = allow;
        self
    }

    /// Allows (or forbids) the `send_dtmf` tool.
    #[must_use]
    pub fn with_dtmf(mut self, allow: bool) -> Self {
        self.allow_dtmf = allow;
        self
    }

    /// The `transfer_call` tool timeout in effect: the explicit [`Self::transfer_timeout`], or
    /// `settle_timeout + provider_budget + TRANSFER_TIMEOUT_MARGIN`.
    pub fn effective_transfer_timeout(&self) -> Duration {
        self.transfer_timeout.unwrap_or_else(|| {
            self.settle_timeout
                .saturating_add(self.provider_budget)
                .saturating_add(TRANSFER_TIMEOUT_MARGIN)
        })
    }

    /// The configured destinations, in the order they were added.
    pub fn destinations(&self) -> &[Destination] {
        &self.destinations
    }
}

/// Registers the telephony tools over `ops`:
/// - `transfer_call` (SideEffect) when the policy has a destination;
/// - `send_dtmf` (SideEffect) when the policy allows DTMF;
/// - `end_call` (SideEffect), always.
///
/// `clock` times the wait for the hand-off sentence ([`TelephonyPolicy::settle_timeout`]).
///
/// # Errors
/// [`RegistryError`] if a tool name is taken. A tool registered before the failure stays.
pub fn register_telephony_tools(
    registry: &mut ToolRegistry,
    ops: &Arc<dyn TelephonyOps>,
    policy: &TelephonyPolicy,
    clock: &SharedClock,
) -> Result<(), RegistryError> {
    if !policy.destinations.is_empty() {
        registry.register(
            transfer_spec(policy),
            TransferCall {
                ops: Arc::clone(ops),
                destinations: policy.destinations.clone(),
                allow_warm: policy.allow_warm,
                settle_timeout: policy.settle_timeout,
                clock: Arc::clone(clock),
                claimed: AtomicBool::new(false),
            },
        )?;
    }
    if policy.allow_dtmf {
        registry.register(
            dtmf_spec(policy),
            SendDtmf {
                ops: Arc::clone(ops),
                max_len: policy.max_dtmf_digits,
            },
        )?;
    }
    registry.register(
        ToolSpec::new(
            "end_call",
            "Ends the call. Call it when the caller is done or asks to hang up, then say one \
             short goodbye: the call ends after that sentence was spoken.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            ToolKind::SideEffect,
        )
        .with_filler_after(None),
        EndCall {
            ops: Arc::clone(ops),
        },
    )?;
    Ok(())
}

fn transfer_spec(policy: &TelephonyPolicy) -> ToolSpec {
    let names: Vec<&str> = policy
        .destinations
        .iter()
        .map(|d| d.name.as_str())
        .collect();
    let list: Vec<String> = policy
        .destinations
        .iter()
        .map(|d| {
            if d.description.is_empty() {
                d.name.clone()
            } else {
                format!("{} ({})", d.name, d.description)
            }
        })
        .collect();
    let modes: Vec<&str> = if policy.allow_warm {
        vec!["cold", "warm"]
    } else {
        vec!["cold"]
    };
    let mut description = format!(
        "Transfers the call to a person. Destinations: {}. First tell the caller you are \
         transferring them, then call this tool once. Use mode \"cold\" to connect the caller \
         directly",
        list.join(", ")
    );
    if policy.allow_warm {
        description.push_str(
            " and \"warm\" to brief the person first (give a short summary, no secrets or full \
             card numbers)",
        );
    }
    description.push('.');
    ToolSpec::new(
        "transfer_call",
        description,
        json!({
            "type": "object",
            "properties": {
                "destination": { "type": "string", "enum": names },
                "mode": { "type": "string", "enum": modes, "default": "cold" },
                "summary": { "type": "string", "maxLength": MAX_SUMMARY_CHARS },
                "reason": { "type": "string", "maxLength": MAX_REASON_CHARS },
            },
            "required": ["destination"],
            "additionalProperties": false,
        }),
        ToolKind::SideEffect,
    )
    .with_timeout(policy.effective_transfer_timeout())
    .with_filler_after(None)
}

fn dtmf_spec(policy: &TelephonyPolicy) -> ToolSpec {
    ToolSpec::new(
        "send_dtmf",
        "Presses phone keys toward the other end of the call, for example to navigate an automatic \
         menu. Digits: 0-9, A-D, * and #; w is a half-second pause, W a one-second pause.",
        json!({
            "type": "object",
            "properties": {
                "digits": {
                    "type": "string",
                    "pattern": format!("^[0-9A-Da-d*#wW]{{1,{}}}$", policy.max_dtmf_digits.max(1)),
                },
            },
            "required": ["digits"],
            "additionalProperties": false,
        }),
        ToolKind::SideEffect,
    )
    .with_filler_after(None)
    // Pressing the same key again is another key press, not a repeated request.
    .with_repeatable(true)
}

/// `text` without control characters, whitespace collapsed, at most `max` characters.
fn clean(text: &str, max: usize) -> String {
    let collapsed: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    collapsed.chars().take(max).collect()
}

fn optional_text(args: &Value, key: &str, max: usize) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(|text| clean(text, max))
        .filter(|text| !text.is_empty())
}

/// The model-facing text of a failure.
fn failure(action: &str, error: &TelephonyError) -> ToolError {
    let advice = match error.kind() {
        TelephonyErrorKind::OutcomeUnknown => {
            "It is not known whether it happened: do not repeat it; tell the caller."
        }
        TelephonyErrorKind::Unsupported => "It is not available on this call; tell the caller.",
        TelephonyErrorKind::InvalidArgument
        | TelephonyErrorKind::Rejected
        | TelephonyErrorKind::Unavailable => "Nothing was done; the call continues with you.",
    };
    ToolError::new(format!("{action} failed: {} {advice}", error.message()))
}

struct TransferCall {
    ops: Arc<dyn TelephonyOps>,
    destinations: Vec<Destination>,
    allow_warm: bool,
    settle_timeout: Duration,
    clock: SharedClock,
    claimed: AtomicBool,
}

impl TransferCall {
    /// Waits for the hand-off sentence to be played, at most `settle_timeout`.
    async fn settle(&self) {
        let settled = Box::pin(self.ops.settle_speech());
        let timeout = Box::pin(self.clock.sleep(self.settle_timeout));
        match select(settled, timeout).await {
            Either::Left((Ok(()), _)) => {}
            Either::Left((Err(error), _)) => {
                tracing::warn!(%error, "waiting for the hand-off sentence failed; transferring anyway");
            }
            Either::Right(((), _)) => {
                tracing::warn!(
                    "the hand-off sentence was not confirmed played in time; transferring anyway"
                );
            }
        }
    }
}

impl Tool for TransferCall {
    fn call<'a>(&'a self, args: Value, ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async move {
            let name = args
                .get("destination")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::new("missing destination"))?;
            let destination = self
                .destinations
                .iter()
                .find(|d| d.name == name)
                .ok_or_else(|| ToolError::new("unknown destination"))?;
            let mode = match args.get("mode").and_then(Value::as_str) {
                None | Some("cold") => TransferMode::Cold,
                Some("warm") if self.allow_warm => TransferMode::Warm,
                Some(_) => return Err(ToolError::new("this transfer mode is not available")),
            };
            if self
                .claimed
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Err(ToolError::new(
                    "a transfer was already started for this call; do not start another",
                ));
            }
            let request = TransferRequest {
                destination: destination.number.clone(),
                destination_name: destination.name.clone(),
                mode,
                summary: optional_text(&args, "summary", MAX_SUMMARY_CHARS),
                reason: optional_text(&args, "reason", MAX_REASON_CHARS),
                idempotency: ctx.idempotency_key().digest(),
            };
            self.settle().await;
            tracing::info!(
                destination = request.destination_name,
                number = %request.destination.redacted(),
                mode = mode.as_str(),
                "transfer requested"
            );
            match self.ops.transfer(&request).await {
                Ok(report) => Ok(ToolOutput::Json(json!({
                    "status": "initiated",
                    "mode": report.mode.as_str(),
                    "destination": request.destination_name,
                    "summary_delivered": matches!(report.briefing, super::Briefing::Whisper),
                    "instruction": "Tell the caller briefly that they are being connected. \
                                    Do not call any more tools.",
                }))),
                Err(error) => {
                    if error.kind() != TelephonyErrorKind::OutcomeUnknown {
                        self.claimed.store(false, Ordering::Release);
                    }
                    tracing::warn!(kind = ?error.kind(), "transfer failed");
                    Err(failure("transfer", &error))
                }
            }
        })
    }
}

struct SendDtmf {
    ops: Arc<dyn TelephonyOps>,
    max_len: usize,
}

impl Tool for SendDtmf {
    fn call<'a>(&'a self, args: Value, ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async move {
            let text = args
                .get("digits")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::new("missing digits"))?;
            let digits = DtmfString::parse_with_limit(text, self.max_len)
                .map_err(|e| ToolError::new(e.message()))?;
            match self
                .ops
                .send_dtmf(&digits, ctx.idempotency_key().digest())
                .await
            {
                Ok(()) => Ok(ToolOutput::Json(
                    json!({ "status": "sent", "keys": digits.len() }),
                )),
                Err(error) => Err(failure("sending the keys", &error)),
            }
        })
    }
}

struct EndCall {
    ops: Arc<dyn TelephonyOps>,
}

impl Tool for EndCall {
    fn call<'a>(&'a self, _args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async move {
            match self.ops.end_call().await {
                Ok(()) => Ok(ToolOutput::Json(json!({
                    "status": "ending",
                    "instruction": "Say one short goodbye sentence now. The call ends after it \
                                    was spoken. Do not call any more tools.",
                }))),
                Err(error) => Err(failure("ending the call", &error)),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_control_characters_and_bounds() {
        assert_eq!(clean("a\n\tb   c\u{0}d", 100), "a b c d");
        assert_eq!(clean("abcdef", 3), "abc");
        assert_eq!(clean("   ", 3), "");
    }

    #[test]
    fn the_transfer_timeout_covers_the_settle_wait_and_the_provider_budget() {
        let policy = TelephonyPolicy::new();
        assert_eq!(
            policy.effective_transfer_timeout(),
            DEFAULT_SETTLE_TIMEOUT + DEFAULT_PROVIDER_BUDGET + TRANSFER_TIMEOUT_MARGIN
        );
        assert!(policy.effective_transfer_timeout() > Duration::from_secs(8 + 4 * 10));
        let mut policy = TelephonyPolicy::new();
        policy.settle_timeout = Duration::from_secs(1);
        policy.provider_budget = Duration::from_secs(9);
        assert_eq!(
            policy.effective_transfer_timeout(),
            Duration::from_secs(10) + TRANSFER_TIMEOUT_MARGIN
        );
        policy.transfer_timeout = Some(Duration::from_secs(5));
        assert_eq!(policy.effective_transfer_timeout(), Duration::from_secs(5));
    }

    #[test]
    fn policy_rejects_bad_destinations() {
        let policy = TelephonyPolicy::new()
            .destination("sales", "+905551112233", "Sales")
            .unwrap();
        assert!(matches!(
            policy.clone().destination("sales", "+905551112244", ""),
            Err(PolicyError::Duplicate(_))
        ));
        assert!(matches!(
            policy.clone().destination("bad name", "+905551112244", ""),
            Err(PolicyError::InvalidName(_))
        ));
        assert!(matches!(
            policy.destination("x", "0555", ""),
            Err(PolicyError::InvalidNumber { .. })
        ));
    }
}
