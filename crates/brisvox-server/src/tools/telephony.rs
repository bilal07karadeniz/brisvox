//! The telephony tools of the server's calls (cargo feature `call-control`): the
//! agent crate's `transfer_call`, `send_dtmf` and `end_call` (`docs/TELEPHONY.md`) over the
//! carriers' REST call control (`brisvox_transport::call_control`).
//!
//! # Which call
//! The tools act on the call their session serves. A call's carrier id is in its `Started`
//! info (Twilio `call_sid`, Telnyx `call_control_id`), which the transport session records
//! ([`CallStarted`]); the tools bind to it the first time they act. A call whose transport has
//! no configured call control (the generic websocket, RTVI, SIP, or a carrier not in
//! `BRISVOX_CALL_CONTROL`) still has the tools, so the definition's tool set is the same on every
//! listener: `transfer_call` and `send_dtmf` answer "not available on this call", and
//! `end_call` ends the call through the transport.
//!
//! # Speech, transfer and hang-up
//! The runner feeds each call's engine events to `CallTelephony::observe`, which tracks
//! whether the bot is speaking:
//! - `transfer_call` waits for the hand-off sentence first ([`TelephonyOps::settle_speech`]):
//!   until the bot is not speaking and nothing new was produced for [`SETTLE_QUIET`], bounded by
//!   the policy's `settle_timeout`.
//! - `end_call` only records the request. The runner then waits for the farewell (the end of
//!   the next bot speech, at most [`FAREWELL_TIMEOUT`](super::FAREWELL_TIMEOUT)), hangs the call up through the carrier
//!   when bound ([`HANGUP_TIMEOUT`], or until the session is cancelled) and stops the call. The
//!   engine reports the end of a response's speech once per response, after its tool loop, so a
//!   farewell spoken in the same response as the `end_call` call is the speech waited for.
//!
//! # The hub, warm transfers and voicemail
//! A call with a carrier binding enters the [`CallHub`] when it starts, under the carrier and
//! the carrier's id of the call. The callback listener ([`crate::callbacks`]) finds the call
//! there:
//! - **Warm transfers** (`[telephony] warm`, offered only when the callback listener runs and
//!   every binding can do one) are finished by `complete` or `abort` requests
//!   ([`CallHub::complete_warm`], [`CallHub::abort_warm`]). A completed transfer hands the call
//!   off (the bot's session stops, the call is not hung up); an aborted one puts the caller back
//!   on the bot, who says so, or, with no way back, hangs up. A transfer still pending when the
//!   call's session ends is aborted and the caller hung up (`CallTelephony::finish`).
//! - **Voicemail** (`[telephony.voicemail]`): the verdict of the carrier's detector arrives as
//!   the stream parameter `answered_by` or as a callback ([`CallHub::deliver_verdict`]) and drives
//!   a `VoicemailDriver`. The decisions are `CallAction`s the runner carries out
//!   (`CallTelephony::next_action`): speak the fixed message, then hang up; hang up; hand the
//!   call off.
//!
//! The carriers' call control is linked to the media listeners that stream the same calls
//! ([`crate::hosting::StreamLinks`]): DTMF over Twilio and the stream stop of a Telnyx transfer
//! retry are announced to the adapters, which keep the call's session across the stream's end
//! (`docs/TELEPHONY.md` 5).

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use brisvox_agent::telephony::voicemail::{
    Fallback, HintStrength, Step, VoicemailDriver, VoicemailNode, VoicemailPolicy, voicemail_hint,
};
use brisvox_agent::telephony::{
    DtmfString, OpsFuture, PhoneNumber, TelephonyError, TelephonyOps, TelephonyPolicy,
    TransferMode, TransferReport, TransferRequest, register_telephony_tools,
};
use brisvox_agent::tools::{RegistryError, ToolRegistry};
use brisvox_agent::{AnsweredBy, Language};
use brisvox_core::{Secret, SharedClock};
use brisvox_engine::{BotLine, EngineEvent, Offload, SayReceiver, SaySender, say_input};
use brisvox_transport::call_control::{
    BoundCall, CallControl, CallRef, CallerState, StreamRestartHook, StreamStopHook,
    TelnyxCallControl, TelnyxConfig, TwilioAuth, TwilioCallControl, TwilioConfig,
    answered_by_from_twilio,
    twilio::{ResumeStream, StreamUrlFn},
};
use brisvox_transport::twilio::stream_token::{self, StreamUrlSigner};
use brisvox_transport::{CallInfo, CallStarted};
use tokio::sync::{Notify, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{
    CallAction, Carrier, ENV_TELNYX_API_BASE, ENV_TELNYX_API_KEY, ENV_TELNYX_CALLER_ID,
    ENV_TELNYX_CONNECTION_ID, ENV_TELNYX_HOLD_URL, ENV_TWILIO_ACCOUNT_SID, ENV_TWILIO_API_BASE,
    ENV_TWILIO_CALLER_ID, ENV_TWILIO_HOLD_URL, ENV_TWILIO_STATUS_CALLBACK,
};
use crate::config::{AgentConfig, Env, VoicemailSettings, VoicemailUnknown};
use crate::hosting::{ENV_TWILIO_AUTH_TOKEN, StreamLinks};
use crate::settings::var;
use crate::{Error, Result};

/// How long the bot must have produced nothing before `transfer_call` goes ahead: the
/// hand-off sentence's audio follows its text by the TTS's first-byte latency.
pub const SETTLE_QUIET: Duration = Duration::from_secs(1);
/// Longest wait for the carrier's hang-up request (the binding bounds each request too).
pub const HANGUP_TIMEOUT: Duration = Duration::from_secs(12);
/// Longest wait for the carrier in a warm-transfer callback (`complete` or `abort`); the binding
/// bounds each of its requests too, and the abort makes two.
pub const WARM_STEP_TIMEOUT: Duration = Duration::from_secs(25);
/// Longest wait for the cleanup of a warm transfer that is still pending when its call ends.
pub const CLEANUP_TIMEOUT: Duration = Duration::from_secs(20);
/// Most actions waiting for a call's runner; a newer one is dropped (it cannot happen in
/// practice: a call has a handful of events in its life).
const MAX_QUEUED_ACTIONS: usize = 8;
/// Capacity of the call's queue of lines the bot speaks on its own account.
const SAY_CAPACITY: usize = 4;
/// How long a verdict for a call that has not started yet waits for it (an asynchronous detector
/// can be faster than the media stream's connection).
pub const PENDING_VERDICT_TTL: Duration = Duration::from_secs(60);
/// Most verdicts waiting for their calls; a newer one pushes out the oldest.
const MAX_PENDING_VERDICTS: usize = 256;

/// The carriers' call control the telephony tools use, one per carrier.
#[derive(Clone, Default)]
pub struct CallControls {
    twilio: Option<Arc<dyn CallControl>>,
    telnyx: Option<Arc<dyn CallControl>>,
    /// The calls the callback listener can reach.
    hub: CallHub,
    /// Whether the callback listener runs (a warm transfer needs it to finish).
    callbacks: bool,
}

/// What became of a verdict handed to [`CallHub::deliver_verdict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictDelivery {
    /// The call is live and has the verdict.
    Delivered,
    /// No such call yet: the verdict waits for it ([`PENDING_VERDICT_TTL`]).
    Queued,
}

impl fmt::Debug for CallControls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallControls")
            .field("twilio", &self.twilio.is_some())
            .field("telnyx", &self.telnyx.is_some())
            .field("callbacks", &self.callbacks)
            .finish()
    }
}

impl CallControls {
    /// None configured.
    pub fn new() -> Self {
        Self::default()
    }

    /// Twilio calls (`CallInfo` metadata `transport = "twilio"`) use `control`.
    #[must_use]
    pub fn with_twilio(mut self, control: Arc<dyn CallControl>) -> Self {
        self.twilio = Some(control);
        self
    }

    /// Telnyx calls (`transport = "telnyx"`) use `control`.
    #[must_use]
    pub fn with_telnyx(mut self, control: Arc<dyn CallControl>) -> Self {
        self.telnyx = Some(control);
        self
    }

    /// Says whether a callback listener serves this hub (`CallControls::hub`): without one
    /// nothing can finish a warm transfer, so warm transfers are not offered.
    #[must_use]
    pub const fn with_callbacks(mut self, callbacks: bool) -> Self {
        self.callbacks = callbacks;
        self
    }

    /// The registry of live calls the callback listener reaches.
    pub fn hub(&self) -> CallHub {
        self.hub.clone()
    }

    /// Whether no carrier is configured.
    pub const fn is_empty(&self) -> bool {
        self.twilio.is_none() && self.telnyx.is_none()
    }

    /// Every configured binding.
    fn all(&self) -> impl Iterator<Item = &Arc<dyn CallControl>> {
        self.twilio.iter().chain(self.telnyx.iter())
    }

    /// The bindings `carriers` names, from the environment (`docs/OPERATIONS.md`). `links`
    /// connects them to the listeners that stream the same calls (see [`StreamLinks`]): without
    /// a Twilio listener Twilio cannot send DTMF, and the announcements are no-ops.
    pub(crate) fn from_env(
        env: &dyn Env,
        carriers: &[Carrier],
        clock: &SharedClock,
        links: &StreamLinks,
    ) -> Result<Self> {
        let mut controls = Self::new();
        controls.hub = CallHub::with_clock(clock.clone());
        for carrier in carriers {
            match carrier {
                Carrier::Twilio => {
                    let sid = required(env, ENV_TWILIO_ACCOUNT_SID, "Twilio")?;
                    let token = required(env, ENV_TWILIO_AUTH_TOKEN, "Twilio")?;
                    let mut config =
                        TwilioConfig::new(sid, TwilioAuth::AuthToken(Secret::new(token)));
                    config.caller_id = caller_id(env, ENV_TWILIO_CALLER_ID)?;
                    if let Some(base) = var(env, ENV_TWILIO_API_BASE)? {
                        config.base_url = base;
                    }
                    config.conference_wait_url = url_var(env, ENV_TWILIO_HOLD_URL)?;
                    config.status_callback = url_var(env, ENV_TWILIO_STATUS_CALLBACK)?;
                    if let Some(url) = links.twilio_stream_url() {
                        // The in-process adapter keeps a call's session across the stream that
                        // DTMF and a warm transfer end, and takes the stream that follows.
                        // With per-call stream URLs the new stream's URL is signed for the
                        // call, so the adapter accepts it once (`twilio::stream_token`).
                        let url_for_call: Option<StreamUrlFn> = match links.twilio_stream_secret() {
                            Some(secret) => {
                                let signer = StreamUrlSigner::new(
                                    secret,
                                    clock.clone(),
                                    stream_token::DEFAULT_TTL,
                                )
                                .map_err(|e| Error::Config(format!("Twilio stream secret: {e}")))?;
                                let base = url.to_owned();
                                Some(StreamUrlFn::new(move |call: &CallRef| {
                                    signer.url(&base, call.as_str())
                                }))
                            }
                            None => None,
                        };
                        config.resume_stream = Some(ResumeStream {
                            url: url.to_owned(),
                            url_for_call,
                            name: Some("brisvox".to_owned()),
                            parameters: Vec::new(),
                        });
                        config.adapter_resumes_session = true;
                        let announce_links = links.clone();
                        let retract_links = links.clone();
                        config.on_stream_restart = Some(
                            StreamRestartHook::new(move |call, wait| match announce_links
                                .twilio_resumer()
                            {
                                Some(resumer) => {
                                    if !resumer.expect_resume(call.as_str(), wait) {
                                        tracing::debug!(target: "brisvox::server", "stream restart announced for a call the Twilio listener does not stream");
                                    }
                                }
                                None => {
                                    tracing::warn!(target: "brisvox::server", "stream restart announced before the Twilio listener was bound");
                                }
                            })
                            .with_retraction(move |call| {
                                if let Some(resumer) = retract_links.twilio_resumer() {
                                    resumer.cancel_expectation(call.as_str());
                                }
                            }),
                        );
                    }
                    let control = TwilioCallControl::new(config, clock.clone())
                        .map_err(|e| Error::Config(format!("Twilio call control: {e}")))?;
                    controls = controls.with_twilio(Arc::new(control));
                }
                Carrier::Telnyx => {
                    let key = required(env, ENV_TELNYX_API_KEY, "Telnyx")?;
                    let mut config = TelnyxConfig::new(Secret::new(key));
                    config.caller_id = caller_id(env, ENV_TELNYX_CALLER_ID)?;
                    if let Some(base) = var(env, ENV_TELNYX_API_BASE)? {
                        config.base_url = base;
                    }
                    config.connection_id = var(env, ENV_TELNYX_CONNECTION_ID)?;
                    config.hold_audio_url = url_var(env, ENV_TELNYX_HOLD_URL)?;
                    // The `streaming_stop` of the transfer retry ends the stream on purpose: the
                    // adapter must not read its close as a hang-up.
                    let links = links.clone();
                    config.on_stream_stop = Some(StreamStopHook::new(move |call| {
                        match links.telnyx_stopper() {
                            Some(stopper) => {
                                if !stopper.expect_stop(call.as_str()) {
                                    tracing::debug!(target: "brisvox::server", "stream stop announced for a call the Telnyx listener does not stream");
                                }
                            }
                            None => {
                                tracing::warn!(target: "brisvox::server", "stream stop announced before the Telnyx listener was bound");
                            }
                        }
                    }));
                    let control = TelnyxCallControl::new(config, clock.clone())
                        .map_err(|e| Error::Config(format!("Telnyx call control: {e}")))?;
                    controls = controls.with_telnyx(Arc::new(control));
                }
            }
        }
        Ok(controls)
    }

    /// Whether every configured binding can send DTMF (the tool set is the same for every
    /// call, so `send_dtmf` is offered only when it works on all of them).
    fn send_dtmf(&self) -> bool {
        self.all().all(|c| c.capabilities().send_dtmf)
    }

    /// Whether warm transfers can be offered: the callback listener runs (it finishes them) and
    /// every configured binding can do one.
    fn warm_transfer(&self) -> bool {
        self.callbacks && self.all().all(|c| c.capabilities().warm_transfer)
    }

    /// The carrier and id of the call `info` describes, if its transport has a binding.
    fn key_of(&self, info: &CallInfo) -> Option<(Carrier, Arc<dyn CallControl>, String)> {
        let (carrier, control, key) = match info.metadata.get("transport").map(String::as_str) {
            Some("twilio") => (Carrier::Twilio, self.twilio.as_ref()?, "call_sid"),
            Some("telnyx") => (Carrier::Telnyx, self.telnyx.as_ref()?, "call_control_id"),
            _ => return None,
        };
        let id = info.metadata.get(key)?;
        Some((carrier, Arc::clone(control), id.clone()))
    }

    /// The binding of the call `info` describes, if its transport has one.
    fn bind(&self, info: &CallInfo) -> Option<(Carrier, Arc<BoundCall>)> {
        let (carrier, control, id) = self.key_of(info)?;
        match CallRef::new(id.as_str()) {
            Ok(call) => Some((carrier, Arc::new(BoundCall::new(control, call)))),
            Err(error) => {
                tracing::warn!(target: "brisvox::server", %error, "the call's carrier id is unusable: no call control");
                None
            }
        }
    }
}

/// The value of `name`, required by `carrier`'s call control.
fn required(env: &dyn Env, name: &str, carrier: &str) -> Result<String> {
    var(env, name)?.ok_or_else(|| {
        Error::Config(format!(
            "{} names {carrier}: {name} is required",
            super::ENV_CALL_CONTROL
        ))
    })
}

/// An optional E.164 caller id.
fn caller_id(env: &dyn Env, name: &str) -> Result<Option<PhoneNumber>> {
    var(env, name)?
        .map(|v| {
            PhoneNumber::parse(&v)
                .map_err(|e| Error::Config(format!("{name}: not an E.164 number: {}", e.message())))
        })
        .transpose()
}

/// An optional `http(s)://` URL.
fn url_var(env: &dyn Env, name: &str) -> Result<Option<String>> {
    let Some(value) = var(env, name)? else {
        return Ok(None);
    };
    if value.starts_with("https://") || value.starts_with("http://") {
        Ok(Some(value))
    } else {
        Err(Error::Config(format!(
            "{name}: expected an http:// or https:// URL"
        )))
    }
}

/// Runs `future`, giving up after `after` on `clock`.
async fn within<F: Future>(clock: &SharedClock, after: Duration, future: F) -> Option<F::Output> {
    tokio::select! {
        biased;
        output = future => Some(output),
        () = clock.sleep(after) => None,
    }
}

// -----------------------------------------------------------------------------------------------
// The hub: live calls the callback listener can reach

/// Why a request to a call through the [`CallHub`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubError {
    /// No live call of that carrier has that id (it ended, or never was).
    NoSuchCall,
    /// The call has no warm transfer in progress.
    NoWarmTransfer,
    /// The call's agent has no `[telephony.voicemail]` section.
    NoVoicemail,
    /// The carrier refused or failed; the carrier's error, without credentials.
    Provider(String),
    /// The carrier did not answer in time.
    Timeout,
}

impl fmt::Display for HubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchCall => f.write_str("no live call with that id"),
            Self::NoWarmTransfer => f.write_str("the call has no warm transfer in progress"),
            Self::NoVoicemail => f.write_str("the call's agent has no voicemail handling"),
            Self::Provider(detail) => write!(f, "the carrier failed: {detail}"),
            Self::Timeout => f.write_str("the carrier did not answer in time"),
        }
    }
}

impl std::error::Error for HubError {}

/// The live calls that have a carrier binding, by carrier and the carrier's id of the call. The
/// callback listener ([`crate::callbacks`]) finds the call a warm-transfer callback or an AMD
/// verdict is about here. A call enters when it starts and leaves when its session ends.
/// Cheap to clone; clones share the registry.
#[derive(Clone, Default)]
pub struct CallHub {
    calls: Arc<Mutex<LiveCalls>>,
    /// Verdicts for calls that have not started, oldest first.
    pending: Arc<Mutex<VecDeque<PendingVerdict>>>,
    /// Dates the pending verdicts; without one nothing is queued.
    clock: Option<SharedClock>,
}

/// A verdict that arrived before its call.
struct PendingVerdict {
    carrier: Carrier,
    id: String,
    verdict: AnsweredBy,
    at: Instant,
}

/// The live calls by carrier and carrier id.
type LiveCalls = HashMap<(Carrier, String), Arc<CallOps>>;

impl fmt::Debug for CallHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallHub")
            .field("calls", &self.len())
            .finish()
    }
}

impl CallHub {
    /// A hub that dates the verdicts it queues with `clock`.
    #[must_use]
    pub fn with_clock(clock: SharedClock) -> Self {
        Self {
            clock: Some(clock),
            ..Self::default()
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LiveCalls> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, VecDeque<PendingVerdict>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The verdicts that waited for the call `(carrier, id)`, oldest first; expired ones are
    /// forgotten.
    fn take_pending(&self, carrier: Carrier, id: &str) -> Vec<AnsweredBy> {
        let Some(clock) = &self.clock else {
            return Vec::new();
        };
        let now = clock.now();
        let mut pending = self.lock_pending();
        pending.retain(|p| now.saturating_duration_since(p.at) <= PENDING_VERDICT_TTL);
        let mut taken = Vec::new();
        pending.retain(|p| {
            let mine = p.carrier == carrier && p.id == id;
            if mine {
                taken.push(p.verdict);
            }
            !mine
        });
        taken
    }

    /// Verdicts waiting for their calls.
    pub fn pending_verdicts(&self) -> usize {
        self.lock_pending().len()
    }

    /// Calls registered right now.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no call is registered.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn find(&self, carrier: Carrier, id: &str) -> Option<Arc<CallOps>> {
        self.lock().get(&(carrier, id.to_owned())).cloned()
    }

    /// Registers `ops` under `(carrier, id)`; `None` when a live call has that id already.
    fn register(&self, carrier: Carrier, id: String, ops: Arc<CallOps>) -> Option<HubGuard> {
        let mut calls = self.lock();
        let key = (carrier, id);
        if calls.contains_key(&key) {
            return None;
        }
        calls.insert(key.clone(), Arc::clone(&ops));
        Some(HubGuard {
            hub: self.clone(),
            key,
            ops,
        })
    }

    /// The receiving party of the call's warm transfer accepted: joins them to the caller. The
    /// bot leaves the call afterwards.
    ///
    /// # Errors
    /// [`HubError::NoSuchCall`], [`HubError::NoWarmTransfer`] (none in progress, or it already
    /// ended), [`HubError::Provider`] (the carrier refused: the transfer is kept, the request
    /// can be repeated or the transfer aborted) and [`HubError::Timeout`].
    pub async fn complete_warm(&self, carrier: Carrier, id: &str) -> Result<(), HubError> {
        self.find(carrier, id)
            .ok_or(HubError::NoSuchCall)?
            .complete_warm()
            .await
    }

    /// The warm transfer failed (nobody answered, declined, a machine): ends the receiving
    /// party's leg and puts the caller back on the bot. Returns where the caller was left; when
    /// that is [`CallerState::Parked`] there is no way back to the bot and the call is hung up.
    ///
    /// # Errors
    /// As [`Self::complete_warm`].
    pub async fn abort_warm(&self, carrier: Carrier, id: &str) -> Result<CallerState, HubError> {
        self.find(carrier, id)
            .ok_or(HubError::NoSuchCall)?
            .abort_warm()
            .await
    }

    /// A detector's verdict on who answered the call, for the call's voicemail handling. A
    /// verdict for a call that is not live (an asynchronous detector can beat the media stream
    /// to the server) waits for it ([`PENDING_VERDICT_TTL`]; at most 256 wait, the oldest is
    /// pushed out).
    ///
    /// # Errors
    /// [`HubError::NoSuchCall`] (this hub queues nothing: it has no clock), or
    /// [`HubError::NoVoicemail`] when the call's agent has no `[telephony.voicemail]` section.
    pub fn deliver_verdict(
        &self,
        carrier: Carrier,
        id: &str,
        verdict: AnsweredBy,
    ) -> Result<VerdictDelivery, HubError> {
        if let Some(ops) = self.find(carrier, id) {
            return ops
                .deliver_verdict(verdict)
                .map(|()| VerdictDelivery::Delivered);
        }
        let clock = self.clock.as_ref().ok_or(HubError::NoSuchCall)?;
        let at = clock.now();
        let mut pending = self.lock_pending();
        pending.retain(|p| at.saturating_duration_since(p.at) <= PENDING_VERDICT_TTL);
        if pending.len() >= MAX_PENDING_VERDICTS {
            pending.pop_front();
        }
        pending.push_back(PendingVerdict {
            carrier,
            id: id.to_owned(),
            verdict,
            at,
        });
        Ok(VerdictDelivery::Queued)
    }
}

/// A call's entry in the hub; dropping it removes the entry.
pub(crate) struct HubGuard {
    hub: CallHub,
    key: (Carrier, String),
    ops: Arc<CallOps>,
}

impl Drop for HubGuard {
    fn drop(&mut self) {
        let mut calls = self.hub.lock();
        if calls
            .get(&self.key)
            .is_some_and(|ops| Arc::ptr_eq(ops, &self.ops))
        {
            calls.remove(&self.key);
        }
    }
}

// -----------------------------------------------------------------------------------------------
// One agent's telephony

/// What the voicemail handling of an agent's calls needs.
#[derive(Clone)]
struct VoicemailConfig {
    policy: VoicemailPolicy,
    message: Arc<str>,
}

impl VoicemailConfig {
    fn from_settings(settings: &VoicemailSettings) -> Self {
        let base = if settings.detect_only {
            VoicemailPolicy::detect_only()
        } else {
            VoicemailPolicy::default()
        };
        let policy = VoicemailPolicy {
            on_unknown: match settings.on_unknown {
                VoicemailUnknown::TreatAsHuman => Fallback::TreatAsHuman,
                VoicemailUnknown::TreatAsMachine => Fallback::TreatAsMachine,
                VoicemailUnknown::HangUp => Fallback::HangUp,
            },
            ..base
        };
        Self {
            policy,
            message: Arc::from(settings.message.as_str()),
        }
    }
}

/// The line the bot speaks when a warm transfer failed and the caller is back on the bot.
fn warm_failed_line(agent: &AgentConfig) -> Arc<str> {
    if matches!(agent.language(), Language::Tr) {
        Arc::from(
            "Üzgünüm, aradığım kişiye şu anda ulaşılamadı. Size başka nasıl yardımcı olabilirim?",
        )
    } else {
        Arc::from(
            "I'm sorry, I couldn't reach them just now. Is there anything else I can help you with?",
        )
    }
}

/// The telephony tools of one agent: the bindings and the definition's policy.
pub(crate) struct AgentTelephony {
    controls: CallControls,
    policy: TelephonyPolicy,
    /// The tools the agent names (`agent.tools`).
    names: Arc<[Arc<str>]>,
    /// The agent's `[telephony.voicemail]`, if it has one.
    voicemail: Option<VoicemailConfig>,
    /// What the bot says when the caller is back after a failed warm transfer.
    warm_failed_line: Arc<str>,
}

impl AgentTelephony {
    /// `agent`'s telephony tools over `controls`.
    pub(crate) fn new(controls: CallControls, agent: &AgentConfig) -> Self {
        let mut policy = agent.telephony_policy();
        if !controls.send_dtmf() {
            policy = policy.with_dtmf(false);
        }
        if policy.allow_warm && !controls.warm_transfer() {
            tracing::warn!(
                target: "brisvox::server",
                agent = agent.name(),
                "telephony.warm is on, but warm transfers need the callback listener ({}) and a carrier binding with a caller id (Telnyx: a connection id too): cold transfers only",
                crate::callbacks::ENV_CALLBACK_ADDR
            );
            policy = policy.with_warm(false);
        }
        Self {
            controls,
            policy,
            names: agent.tool_names().map(Arc::from).collect(),
            voicemail: agent
                .voicemail()
                .map(|v| VoicemailConfig::from_settings(&v)),
            warm_failed_line: warm_failed_line(agent),
        }
    }

    /// Whether the agent has voicemail handling.
    pub(crate) const fn has_voicemail(&self) -> bool {
        self.voicemail.is_some()
    }

    /// Registers the tools over a call that never acts (the startup check of the definition).
    pub(crate) fn register_probe(
        &self,
        registry: &mut ToolRegistry,
        clock: &SharedClock,
    ) -> std::result::Result<(), RegistryError> {
        let ops: Arc<dyn TelephonyOps> = Arc::new(Unbound);
        register_telephony_tools(registry, &ops, &self.policy, clock)
    }

    /// The telephony state of one call.
    pub(crate) fn call(&self, started: CallStarted, clock: SharedClock) -> CallTelephony {
        let (speech, watch) = watch::channel(Speech {
            speaking: false,
            last_activity: clock.now(),
            stops: 0,
        });
        // A call speaks on its own account for voicemail and after a failed warm transfer.
        let (say, say_receiver) = if self.voicemail.is_some() || self.policy.allow_warm {
            let (tx, rx) = say_input(SAY_CAPACITY);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        CallTelephony {
            ops: Arc::new(CallOps {
                started,
                controls: self.controls.clone(),
                bound: OnceLock::new(),
                speech: watch,
                settle_max: self.policy.settle_timeout,
                allow_warm: self.policy.allow_warm,
                clock,
                end_requested: AtomicBool::new(false),
                end: Notify::new(),
                actions: Mutex::new(VecDeque::new()),
                wake: Notify::new(),
                warm_gate: tokio::sync::Mutex::new(()),
                voicemail: self.voicemail.clone().map(|config| VoicemailState {
                    config,
                    inner: Mutex::new(VoicemailInner::default()),
                }),
                warm_failed_line: Arc::clone(&self.warm_failed_line),
                say,
            }),
            speech,
            say_receiver: Mutex::new(say_receiver),
            registration: Mutex::new(None),
        }
    }

    /// `app` plus the telephony tools over `call`, limited to the agent's tools. Registration
    /// compiles the tools' schemas, so it runs on `offload`.
    pub(crate) async fn registry_for_call(
        &self,
        app: &Arc<ToolRegistry>,
        call: &CallTelephony,
        clock: &SharedClock,
        offload: &Offload,
        timeout: Duration,
    ) -> Result<Arc<ToolRegistry>> {
        let app = Arc::clone(app);
        let ops: Arc<dyn TelephonyOps> = call.ops.clone();
        let policy = self.policy.clone();
        let names = Arc::clone(&self.names);
        let clock = clock.clone();
        let done = offload
            .run(timeout, move || {
                let mut registry = app.filtered(|_| true);
                register_telephony_tools(&mut registry, &ops, &policy, &clock)?;
                Ok::<_, RegistryError>(
                    registry.filtered(|spec| names.iter().any(|n| **n == *spec.name)),
                )
            })
            .await
            .map_err(|e| Error::Call(format!("telephony tools: {e}")))?;
        done.value
            .map(Arc::new)
            .map_err(|e| Error::Call(format!("telephony tools: {e}")))
    }
}

/// Whether the bot is speaking, as the call's engine events say.
#[derive(Debug, Clone, Copy)]
struct Speech {
    /// Between `BotStartedSpeaking` and `BotStoppedSpeaking` (or a barge-in).
    speaking: bool,
    /// The last bot event (text produced, speech started or stopped).
    last_activity: Instant,
    /// `BotStoppedSpeaking` events so far.
    stops: u64,
}

/// One call's telephony state: what the tools act through, and the runner's side of it.
pub(crate) struct CallTelephony {
    ops: Arc<CallOps>,
    speech: watch::Sender<Speech>,
    /// The receiving end of the lines the bot speaks on its own account, until the runner takes
    /// it for the call's engine.
    say_receiver: Mutex<Option<SayReceiver>>,
    /// The call's entry in the hub while it lives.
    registration: Mutex<Option<HubGuard>>,
}

impl fmt::Debug for CallTelephony {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallTelephony")
            .field("bound", &self.ops.bound.get())
            .field(
                "end_requested",
                &self.ops.end_requested.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl CallTelephony {
    /// Takes one of the call's engine events into account.
    pub(crate) fn observe(&self, event: &EngineEvent) {
        let now = self.ops.clock.now();
        if let EngineEvent::UserTranscript {
            text,
            is_final: true,
        } = event
        {
            self.ops.voicemail_hint(text);
        }
        self.speech.send_if_modified(|speech| {
            match event {
                EngineEvent::BotStartedSpeaking { .. } => speech.speaking = true,
                EngineEvent::BotStoppedSpeaking { .. } => {
                    speech.speaking = false;
                    speech.stops = speech.stops.saturating_add(1);
                }
                EngineEvent::BotInterrupted { .. } => speech.speaking = false,
                EngineEvent::BotLlmStarted { .. }
                | EngineEvent::BotLlmText { .. }
                | EngineEvent::BotTtsText { .. } => {}
                _ => return false,
            }
            speech.last_activity = now;
            true
        });
    }

    /// Resolves once `end_call` asked for the call to end (at once if it already did).
    pub(crate) async fn end_requested(&self) {
        if self.ops.end_requested.load(Ordering::Acquire) {
            return;
        }
        self.ops.end.notified().await;
    }

    /// The bot-speech count to compare [`Self::farewell_played`] against, taken when the end
    /// was requested.
    pub(crate) fn speech_stops(&self) -> u64 {
        self.speech.borrow().stops
    }

    /// Whether a bot speech ended after `stops` was taken and the bot is silent now.
    pub(crate) fn farewell_played(&self, stops: u64) -> bool {
        let speech = *self.speech.borrow();
        !speech.speaking && speech.stops > stops
    }

    /// The receiving end of the lines the bot speaks on its own account (voicemail, a failed
    /// warm transfer), for the call's engine (`SessionBuilder::with_say_input`). Once.
    pub(crate) fn take_say_receiver(&self) -> Option<SayReceiver> {
        self.say_receiver
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Makes the bot speak `text` now, cutting what it says. `false` when the engine takes no
    /// lines (no receiver was handed to it, or the call is over).
    pub(crate) fn say(&self, text: &Arc<str>) -> bool {
        match &self.ops.say {
            Some(say) => say
                .try_send(BotLine::interrupting(Arc::clone(text)))
                .is_ok(),
            None => false,
        }
    }

    /// Enters the call in the hub once it started (so callbacks can reach it) and applies what
    /// its start says about who answered (`answered_by` stream parameter). Waits for the call's
    /// `Started` event: race it with the call's end.
    pub(crate) async fn register(&self) {
        let info = self.ops.started.wait().await;
        if let Some(value) = info.metadata.get("custom.answered_by") {
            let verdict = answered_by_from_twilio(value);
            if let Err(error) = self.ops.deliver_verdict(verdict) {
                tracing::debug!(target: "brisvox::server", %error, "the stream's answered_by is not used");
            }
        }
        let Some((carrier, bound)) = self.ops.binding() else {
            return;
        };
        let id = bound.call().as_str().to_owned();
        let carrier = *carrier;
        let hub = &self.ops.controls.hub;
        match hub.register(carrier, id.clone(), Arc::clone(&self.ops)) {
            Some(guard) => {
                *self
                    .registration
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(guard);
                // Verdicts that beat the call here.
                for verdict in hub.take_pending(carrier, &id) {
                    if let Err(error) = self.ops.deliver_verdict(verdict) {
                        tracing::debug!(target: "brisvox::server", %error, "a queued verdict is not used");
                    }
                }
            }
            None => {
                tracing::warn!(target: "brisvox::server", "another live call has this carrier id: callbacks cannot reach this one");
            }
        }
    }

    /// The next thing the runner must do for this call, as the callbacks, the voicemail handling
    /// and the tools decide. Cancel-safe.
    pub(crate) async fn next_action(&self) -> CallAction {
        loop {
            if let Some(action) = self.ops.pop_action() {
                return action;
            }
            let deadline = self.ops.voicemail_deadline();
            tokio::select! {
                biased;
                () = self.ops.wake.notified() => {}
                () = sleep_until(&self.ops.clock, deadline) => self.ops.voicemail_timeout(),
            }
        }
    }

    /// The voicemail message was played (its speech ended): the call is done.
    pub(crate) fn message_played(&self) {
        self.ops.voicemail_message_played();
    }

    /// Hangs the call up through its carrier, when it has one (bounded, never fails: the caller
    /// stops the call through the transport either way). Gives up at once when `cancel` fires
    /// (drain timeout, shutdown): the session then stops the call itself.
    pub(crate) async fn hang_up(&self, idempotency: u64, cancel: &CancellationToken) {
        let Some(bound) = self.ops.bound() else {
            return;
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                tracing::debug!(target: "brisvox::server", "carrier hang-up abandoned: the session was cancelled");
            }
            done = bound.hangup(idempotency) => {
                if let Err(error) = done {
                    tracing::warn!(target: "brisvox::server", %error, "carrier hang-up failed; stopping the stream");
                }
            }
            () = self.ops.clock.sleep(HANGUP_TIMEOUT) => {
                tracing::warn!(target: "brisvox::server", "carrier hang-up timed out; stopping the stream");
            }
        }
    }

    /// The call-control of this call for the out-of-process agent's `transfer`: the
    /// call's own operations, when its transport has a carrier binding. Waits up to `wait` for
    /// the call's start (a carrier sends it at once); `None` for a call without a binding
    /// (generic websocket, RTVI, SIP) or when the start does not come in time.
    pub(crate) async fn link_ops(&self, wait: Duration) -> Option<Arc<dyn TelephonyOps>> {
        within(&self.ops.clock, wait, self.ops.started.wait()).await?;
        self.ops.binding()?;
        let ops: Arc<dyn TelephonyOps> = self.ops.clone();
        Some(ops)
    }

    /// Cleans up when the call's session ended: a warm transfer still pending (nobody finished
    /// it) is aborted and the caller, who has nobody to talk to now, is hung up. A transfer that
    /// was completed is not touched. Bounded by [`CLEANUP_TIMEOUT`].
    pub(crate) async fn finish(&self, idempotency: u64) {
        let ops = &self.ops;
        let Some(bound) = ops.bound() else {
            return;
        };
        let _gate = ops.warm_gate.lock().await;
        if bound.warm_transfer().is_none() {
            return;
        }
        tracing::warn!(target: "brisvox::server", "the call ended with a warm transfer pending: aborting it and hanging the caller up");
        let cleanup = async {
            if let Err(error) = bound.abort_warm().await {
                tracing::warn!(target: "brisvox::server", %error, "aborting the pending warm transfer failed");
            }
            if let Err(error) = bound.hangup(idempotency).await {
                tracing::warn!(target: "brisvox::server", %error, "hanging up after the pending warm transfer failed");
            }
        };
        if within(&ops.clock, CLEANUP_TIMEOUT, cleanup).await.is_none() {
            tracing::warn!(target: "brisvox::server", "cleaning up the pending warm transfer timed out");
        }
    }
}

/// Sleeps until `deadline` on `clock`; never resolves without one.
async fn sleep_until(clock: &SharedClock, deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => clock.sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// What the telephony tools of one call act through.
struct CallOps {
    started: CallStarted,
    controls: CallControls,
    /// The call's binding, resolved the first time a tool acts after the call started.
    bound: OnceLock<Option<(Carrier, Arc<BoundCall>)>>,
    speech: watch::Receiver<Speech>,
    settle_max: Duration,
    /// Warm transfers are on for this agent (`[telephony] warm`, the callback listener and a
    /// binding that can do them). Checked for the agent link's `transfer`, whose destination
    /// and mode come from the agent process, not from the definition's tool.
    allow_warm: bool,
    clock: SharedClock,
    end_requested: AtomicBool,
    end: Notify,
    /// What the runner must do next (bounded by [`MAX_QUEUED_ACTIONS`], newest dropped).
    actions: Mutex<VecDeque<CallAction>>,
    /// Woken when an action was queued.
    wake: Notify,
    /// Serializes the warm-transfer callbacks and the cleanup of one call.
    warm_gate: tokio::sync::Mutex<()>,
    /// The call's voicemail handling, when its agent has any.
    voicemail: Option<VoicemailState>,
    /// What the bot says when the caller is back after a failed warm transfer.
    warm_failed_line: Arc<str>,
    /// Lines the bot speaks on its own account.
    say: Option<SaySender>,
}

/// The state of a call's voicemail handling: created with the call, the driver at the first
/// verdict (an inbound call, which has none, is never touched).
struct VoicemailState {
    config: VoicemailConfig,
    inner: Mutex<VoicemailInner>,
}

#[derive(Default)]
struct VoicemailInner {
    driver: Option<VoicemailDriver>,
    /// A hint heard before any verdict, to hand to the driver when it starts.
    early_hint: Option<(HintStrength, Instant)>,
}

impl CallOps {
    /// The call's carrier and binding (`None` before the call started or when its transport
    /// has none).
    fn binding(&self) -> Option<&(Carrier, Arc<BoundCall>)> {
        if let Some(bound) = self.bound.get() {
            return bound.as_ref();
        }
        let info = self.started.get()?;
        self.bound.get_or_init(|| self.controls.bind(info)).as_ref()
    }

    /// The call's binding (`None` before the call started or when its transport has none).
    fn bound(&self) -> Option<&Arc<BoundCall>> {
        self.binding().map(|(_, bound)| bound)
    }

    fn push_action(&self, action: CallAction) {
        {
            let mut actions = self.actions.lock().unwrap_or_else(PoisonError::into_inner);
            if actions.len() >= MAX_QUEUED_ACTIONS {
                tracing::warn!(target: "brisvox::server", ?action, "too many pending call actions: dropped");
                return;
            }
            actions.push_back(action);
        }
        self.wake.notify_one();
    }

    fn pop_action(&self) -> Option<CallAction> {
        self.actions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }

    /// Waits until the bot is silent and quiet for [`SETTLE_QUIET`], at most `settle_max`.
    async fn settle(&self) {
        let deadline = self.clock.now() + self.settle_max;
        let mut speech = self.speech.clone();
        loop {
            let now = self.clock.now();
            if now >= deadline {
                return;
            }
            let state = *speech.borrow_and_update();
            let quiet = now.saturating_duration_since(state.last_activity);
            if !state.speaking && quiet >= SETTLE_QUIET {
                return;
            }
            let wake = if state.speaking {
                deadline
            } else {
                (now + (SETTLE_QUIET - quiet)).min(deadline)
            };
            tokio::select! {
                changed = speech.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                () = self.clock.sleep_until(wake) => {}
            }
        }
    }

    // ---- warm transfer (the callback listener's side)

    async fn complete_warm(&self) -> Result<(), HubError> {
        let bound = self.bound().ok_or(HubError::NoSuchCall)?;
        let _gate = self.warm_gate.lock().await;
        if bound.warm_transfer().is_none() {
            return Err(HubError::NoWarmTransfer);
        }
        match within(&self.clock, WARM_STEP_TIMEOUT, bound.complete_warm()).await {
            Some(Ok(())) => {
                // The caller and the receiving party talk now: the bot leaves.
                self.push_action(CallAction::HandOff);
                Ok(())
            }
            Some(Err(error)) => Err(HubError::Provider(error.to_string())),
            None => Err(HubError::Timeout),
        }
    }

    async fn abort_warm(&self) -> Result<CallerState, HubError> {
        let bound = self.bound().ok_or(HubError::NoSuchCall)?;
        let _gate = self.warm_gate.lock().await;
        if bound.warm_transfer().is_none() {
            return Err(HubError::NoWarmTransfer);
        }
        match within(&self.clock, WARM_STEP_TIMEOUT, bound.abort_warm()).await {
            Some(Ok(state)) => {
                match state {
                    // The caller is back on the bot (the adapter holds what the bot says until
                    // the new stream is up): tell them.
                    CallerState::Resumed => self.push_action(CallAction::Say {
                        text: Arc::clone(&self.warm_failed_line),
                        then_hang_up: false,
                    }),
                    // No way back to the bot: nobody is left to talk to the caller.
                    CallerState::Parked => self.push_action(CallAction::HangUp),
                    CallerState::Unchanged => {}
                }
                Ok(state)
            }
            Some(Err(error)) => Err(HubError::Provider(error.to_string())),
            None => Err(HubError::Timeout),
        }
    }

    // ---- voicemail

    fn voicemail_state(&self) -> Option<&VoicemailState> {
        self.voicemail.as_ref()
    }

    /// A detector's verdict: starts the driver at the first one.
    fn deliver_verdict(&self, verdict: AnsweredBy) -> Result<(), HubError> {
        let vm = self.voicemail_state().ok_or(HubError::NoVoicemail)?;
        let now = self.clock.now();
        let step = {
            let mut inner = vm.inner.lock().unwrap_or_else(PoisonError::into_inner);
            let inner = &mut *inner;
            let driver = inner.driver.get_or_insert_with(|| {
                let mut driver = VoicemailDriver::new(vm.config.policy, now);
                if let Some((strength, at)) = inner.early_hint.take() {
                    driver.on_voicemail_hint(strength, at);
                }
                driver
            });
            driver.on_verdict(verdict, now)
        };
        tracing::info!(target: "brisvox::server", ?verdict, ?step, "answering-machine verdict");
        self.apply_step(vm, step);
        Ok(())
    }

    /// What the callee said (a final transcript) may be a voicemail greeting.
    fn voicemail_hint(&self, text: &str) {
        let Some(vm) = self.voicemail_state() else {
            return;
        };
        let Some(strength) = voicemail_hint(text, None) else {
            return;
        };
        let now = self.clock.now();
        let step = {
            let mut inner = vm.inner.lock().unwrap_or_else(PoisonError::into_inner);
            match inner.driver.as_mut() {
                Some(driver) => driver.on_voicemail_hint(strength, now),
                None => {
                    // Before any verdict: remember the strongest hint for the driver.
                    let keep = inner
                        .early_hint
                        .is_none_or(|(old, _)| strength == HintStrength::Phrase && old != strength);
                    if keep {
                        inner.early_hint = Some((strength, now));
                    }
                    Step::Stay
                }
            }
        };
        self.apply_step(vm, step);
    }

    /// The instant the voicemail driver wants a timeout call at, if a wait is running.
    fn voicemail_deadline(&self) -> Option<Instant> {
        let vm = self.voicemail_state()?;
        let inner = vm.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.driver.as_ref().and_then(VoicemailDriver::deadline)
    }

    fn voicemail_timeout(&self) {
        let Some(vm) = self.voicemail_state() else {
            return;
        };
        let now = self.clock.now();
        let step = {
            let mut inner = vm.inner.lock().unwrap_or_else(PoisonError::into_inner);
            match inner.driver.as_mut() {
                Some(driver) => driver.on_timeout(now),
                None => Step::Stay,
            }
        };
        self.apply_step(vm, step);
    }

    fn voicemail_message_played(&self) {
        let Some(vm) = self.voicemail_state() else {
            return;
        };
        let mut inner = vm.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(driver) = inner.driver.as_mut() {
            driver.message_played();
            tracing::info!(target: "brisvox::server", outcome = ?driver.outcome(), "voicemail handling done");
        }
    }

    /// Turns a driver step into what the runner does.
    fn apply_step(&self, vm: &VoicemailState, step: Step) {
        match step {
            Step::Stay => {}
            Step::Enter(VoicemailNode::Human | VoicemailNode::WaitGreeting) => {
                // A person: the conversation goes on. A greeting still playing: the driver's
                // deadlines decide when to speak.
            }
            Step::Enter(VoicemailNode::LeaveMessage) => self.push_action(CallAction::Say {
                text: Arc::clone(&vm.config.message),
                then_hang_up: true,
            }),
            Step::HangUp => self.push_action(CallAction::HangUp),
        }
    }
}

/// The answer of a carrier operation on a call without a binding.
fn not_on_this_call(what: &str) -> TelephonyError {
    TelephonyError::unsupported(format!("{what} is not available on this call"))
}

impl TelephonyOps for CallOps {
    fn settle_speech(&self) -> OpsFuture<'_, ()> {
        Box::pin(async move {
            self.settle().await;
            Ok(())
        })
    }

    fn send_dtmf<'a>(&'a self, digits: &'a DtmfString, idempotency: u64) -> OpsFuture<'a, ()> {
        match self.bound() {
            Some(bound) => TelephonyOps::send_dtmf(bound.as_ref(), digits, idempotency),
            None => Box::pin(async { Err(not_on_this_call("sending DTMF")) }),
        }
    }

    fn transfer<'a>(&'a self, request: &'a TransferRequest) -> OpsFuture<'a, TransferReport> {
        if request.mode == TransferMode::Warm && !self.allow_warm {
            return Box::pin(async { Err(not_on_this_call("a warm transfer")) });
        }
        match self.bound() {
            Some(bound) => Box::pin(async move {
                let report = TelephonyOps::transfer(bound.as_ref(), request).await?;
                if report.mode == TransferMode::Cold {
                    // The caller is on the destination's line: the bot has nothing left to do
                    // (and must not hang the call up).
                    self.push_action(CallAction::HandOff);
                }
                Ok(report)
            }),
            None => Box::pin(async { Err(not_on_this_call("a transfer")) }),
        }
    }

    fn end_call(&self) -> OpsFuture<'_, ()> {
        Box::pin(async move {
            self.end_requested.store(true, Ordering::Release);
            self.end.notify_one();
            Ok(())
        })
    }
}

/// The calls of the startup check: never acted on.
struct Unbound;

impl TelephonyOps for Unbound {
    fn send_dtmf<'a>(&'a self, _digits: &'a DtmfString, _idempotency: u64) -> OpsFuture<'a, ()> {
        Box::pin(async { Err(not_on_this_call("sending DTMF")) })
    }

    fn transfer<'a>(&'a self, _request: &'a TransferRequest) -> OpsFuture<'a, TransferReport> {
        Box::pin(async { Err(not_on_this_call("a transfer")) })
    }

    fn end_call(&self) -> OpsFuture<'_, ()> {
        Box::pin(async { Err(not_on_this_call("ending the call")) })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use brisvox_agent::telephony::{TelephonyErrorKind, TransferMode};
    use brisvox_core::{SimClock, TurnEpoch};
    use brisvox_transport::call_control::{CallFuture, CallerState, Capabilities, WarmTransfer};

    use super::*;

    /// Records every request and accepts it.
    #[derive(Debug, Default)]
    struct Fake {
        log: Mutex<Vec<String>>,
        /// The carrier never answers a hang-up.
        hangup_stalls: bool,
        /// Warm transfers work; where `abort_warm` leaves the caller.
        warm: Option<CallerState>,
    }

    impl Fake {
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        fn record(&self, entry: String) {
            self.log.lock().unwrap().push(entry);
        }
    }

    impl CallControl for Fake {
        fn provider(&self) -> &'static str {
            "twilio"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                send_dtmf: false,
                warm_transfer: self.warm.is_some(),
                warm_briefing: false,
            }
        }

        fn send_dtmf<'a>(
            &'a self,
            call: &'a CallRef,
            digits: &'a DtmfString,
            _idempotency: u64,
        ) -> CallFuture<'a, ()> {
            self.record(format!("dtmf {} {}", call.as_str(), digits.as_str()));
            Box::pin(async { Ok(()) })
        }

        fn transfer_cold<'a>(
            &'a self,
            call: &'a CallRef,
            request: &'a TransferRequest,
        ) -> CallFuture<'a, ()> {
            self.record(format!(
                "transfer {} {}",
                call.as_str(),
                request.destination_name
            ));
            Box::pin(async { Ok(()) })
        }

        fn transfer_warm<'a>(
            &'a self,
            call: &'a CallRef,
            _request: &'a TransferRequest,
        ) -> CallFuture<'a, WarmTransfer> {
            self.record(format!("warm {}", call.as_str()));
            let transfer = WarmTransfer {
                provider: "twilio",
                caller: call.clone(),
                agent_leg: CallRef::new("CAagent").ok(),
                conference: "brisvox-test".to_owned(),
                briefing: brisvox_agent::Briefing::None,
                summary: None,
            };
            Box::pin(async { Ok(transfer) })
        }

        fn complete_warm<'a>(&'a self, _transfer: &'a WarmTransfer) -> CallFuture<'a, ()> {
            self.record("complete".to_owned());
            Box::pin(async { Ok(()) })
        }

        fn abort_warm<'a>(&'a self, _transfer: &'a WarmTransfer) -> CallFuture<'a, CallerState> {
            self.record("abort".to_owned());
            let state = self.warm.unwrap_or(CallerState::Unchanged);
            Box::pin(async move { Ok(state) })
        }

        fn hangup<'a>(&'a self, call: &'a CallRef, _idempotency: u64) -> CallFuture<'a, ()> {
            self.record(format!("hangup {}", call.as_str()));
            if self.hangup_stalls {
                return Box::pin(std::future::pending());
            }
            Box::pin(async { Ok(()) })
        }
    }

    fn info(transport: &str, id_key: &str, id: &str) -> CallInfo {
        let mut metadata = BTreeMap::new();
        metadata.insert("transport".to_owned(), transport.to_owned());
        metadata.insert(id_key.to_owned(), id.to_owned());
        CallInfo {
            call_id: "stream-1".to_owned(),
            metadata,
        }
    }

    /// An agent with every telephony tool and one destination, over `fake` for Twilio calls.
    fn agent_telephony(fake: &Arc<Fake>) -> AgentTelephony {
        let control: Arc<dyn CallControl> = fake.clone();
        let controls = CallControls::new().with_twilio(control);
        let mut policy = TelephonyPolicy::new()
            .destination("sales", "+15555550100", "the sales team")
            .unwrap();
        if !controls.send_dtmf() {
            policy = policy.with_dtmf(false);
        }
        AgentTelephony {
            controls,
            policy,
            names: ["transfer_call", "send_dtmf", "end_call"]
                .into_iter()
                .map(Arc::from)
                .collect(),
            voicemail: None,
            warm_failed_line: Arc::from("sorry"),
        }
    }

    fn request() -> TransferRequest {
        TransferRequest {
            destination: PhoneNumber::parse("+15555550100").unwrap(),
            destination_name: "sales".to_owned(),
            mode: TransferMode::Cold,
            summary: None,
            reason: None,
            idempotency: 7,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_tools_act_on_the_carrier_call_the_transport_started() {
        let clock = SimClock::shared();
        let fake = Arc::new(Fake::default());
        let agent = agent_telephony(&fake);
        let app = Arc::new(ToolRegistry::new());

        // A Twilio call: its call SID is what the carrier is asked about.
        let call = agent.call(
            CallStarted::from_info(info("twilio", "call_sid", "CA0001")),
            clock.clone(),
        );
        let registry = agent
            .registry_for_call(
                &app,
                &call,
                &clock,
                &Offload::Inline(clock.clone()),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        // No `send_dtmf`: the binding cannot send tones.
        let names: Vec<&str> = registry.specs().map(|s| &*s.name).collect();
        assert_eq!(names, ["end_call", "transfer_call"]);
        call.ops.transfer(&request()).await.unwrap();
        assert_eq!(fake.log(), ["transfer CA0001 sales"]);
        call.hang_up(9, &CancellationToken::new()).await;
        assert_eq!(fake.log(), ["transfer CA0001 sales", "hangup CA0001"]);

        // A websocket call, a Telnyx call without Telnyx call control, a call not started yet:
        // nothing reaches a carrier.
        for started in [
            CallStarted::from_info(info("websocket", "call_sid", "CA0002")),
            CallStarted::from_info(info("telnyx", "call_control_id", "v3:abc")),
            CallStarted::default(),
        ] {
            let call = agent.call(started, clock.clone());
            let error = call.ops.transfer(&request()).await.unwrap_err();
            assert_eq!(error.kind(), TelephonyErrorKind::Unsupported, "{error:?}");
            let digits = DtmfString::parse("12#").unwrap();
            let error = call.ops.send_dtmf(&digits, 1).await.unwrap_err();
            assert_eq!(error.kind(), TelephonyErrorKind::Unsupported, "{error:?}");
            call.hang_up(1, &CancellationToken::new()).await;
        }
        assert_eq!(fake.log().len(), 2, "{:?}", fake.log());
    }

    #[tokio::test(start_paused = true)]
    async fn a_transfer_waits_until_the_hand_off_sentence_was_played() {
        let clock = SimClock::shared();
        let fake = Arc::new(Fake::default());
        let agent = agent_telephony(&fake);
        let call = agent.call(CallStarted::default(), clock.clone());
        let epoch = TurnEpoch::ZERO;
        call.observe(&EngineEvent::BotStartedSpeaking { epoch });
        let ops = Arc::clone(&call.ops);
        let start = clock.now();
        let settled = tokio::spawn(async move {
            ops.settle_speech().await.unwrap();
        });
        // Speaking for 3 s: still waiting.
        clock.sleep(Duration::from_secs(3)).await;
        assert!(!settled.is_finished());
        call.observe(&EngineEvent::BotStoppedSpeaking { epoch });
        settled.await.unwrap();
        // The speech plus the quiet second.
        assert_eq!(clock.now() - start, Duration::from_secs(3) + SETTLE_QUIET);

        // A bot that never stops: the wait ends at the policy's bound.
        call.observe(&EngineEvent::BotStartedSpeaking { epoch });
        let start = clock.now();
        call.ops.settle_speech().await.unwrap();
        assert_eq!(clock.now() - start, agent.policy.settle_timeout);
    }

    #[tokio::test(start_paused = true)]
    async fn end_call_waits_for_the_farewell() {
        let clock = SimClock::shared();
        let fake = Arc::new(Fake::default());
        let agent = agent_telephony(&fake);
        let call = agent.call(CallStarted::default(), clock.clone());
        let epoch = TurnEpoch::ZERO;
        call.observe(&EngineEvent::BotStartedSpeaking { epoch });
        call.observe(&EngineEvent::BotStoppedSpeaking { epoch });

        call.ops.end_call().await.unwrap();
        // Requested before anyone waited: the wait returns at once.
        call.end_requested().await;
        let stops = call.speech_stops();
        assert!(
            !call.farewell_played(stops),
            "the earlier speech is not the farewell"
        );
        call.observe(&EngineEvent::BotStartedSpeaking { epoch });
        assert!(!call.farewell_played(stops));
        call.observe(&EngineEvent::BotStoppedSpeaking { epoch });
        assert!(call.farewell_played(stops));
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_hang_up_gives_way_to_the_sessions_cancellation() {
        let clock = SimClock::shared();
        let fake = Arc::new(Fake {
            hangup_stalls: true,
            ..Fake::default()
        });
        let agent = agent_telephony(&fake);
        let call = Arc::new(agent.call(
            CallStarted::from_info(info("twilio", "call_sid", "CA0001")),
            clock.clone(),
        ));
        let cancel = CancellationToken::new();
        let start = clock.now();
        let hanging = tokio::spawn({
            let call = Arc::clone(&call);
            let cancel = cancel.clone();
            async move { call.hang_up(3, &cancel).await }
        });
        clock.sleep(Duration::from_secs(1)).await;
        assert!(!hanging.is_finished(), "the carrier has not answered");
        // Shutdown: the hang-up is abandoned at once, not at its bound.
        cancel.cancel();
        hanging.await.unwrap();
        assert_eq!(clock.now() - start, Duration::from_secs(1));
        assert!(clock.now() - start < HANGUP_TIMEOUT);
        assert_eq!(fake.log(), ["hangup CA0001"]);
    }

    // ---- the hub, warm-transfer callbacks, voicemail

    /// An agent over `fake` (Twilio calls) with warm transfers on and, when given, voicemail.
    fn agent_with(fake: &Arc<Fake>, voicemail: Option<VoicemailConfig>) -> AgentTelephony {
        let mut agent = agent_telephony(fake);
        agent.policy = agent.policy.clone().with_warm(true);
        agent.voicemail = voicemail;
        agent
    }

    fn voicemail_config(policy: VoicemailPolicy) -> VoicemailConfig {
        VoicemailConfig {
            policy,
            message: Arc::from("Please call us back."),
        }
    }

    fn warm_request() -> TransferRequest {
        TransferRequest {
            mode: TransferMode::Warm,
            summary: Some("a refund".to_owned()),
            ..request()
        }
    }

    /// A started Twilio call in the hub.
    async fn registered(
        agent: &AgentTelephony,
        clock: &SharedClock,
        sid: &str,
    ) -> (CallTelephony, CallHub) {
        let call = agent.call(
            CallStarted::from_info(info("twilio", "call_sid", sid)),
            clock.clone(),
        );
        call.register().await;
        (call, agent.controls.hub())
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_is_in_the_hub_while_its_telephony_lives() {
        let clock = SimClock::shared();
        let fake = Arc::new(Fake::default());
        let agent = agent_with(&fake, None);
        let (call, hub) = registered(&agent, &clock, "CA0001").await;
        assert_eq!(hub.len(), 1);
        // A second call with the same id is not reachable (and does not displace the first).
        let (twin, _) = registered(&agent, &clock, "CA0001").await;
        assert_eq!(hub.len(), 1);
        drop(twin);
        assert_eq!(hub.len(), 1, "the twin never owned the entry");
        // Nothing is registered for a call without a binding, or one that did not start.
        let websocket = agent.call(
            CallStarted::from_info(info("websocket", "call_sid", "CA0002")),
            clock.clone(),
        );
        websocket.register().await;
        assert_eq!(hub.len(), 1);
        drop(call);
        assert!(hub.is_empty());
        assert_eq!(
            hub.complete_warm(Carrier::Twilio, "CA0001").await,
            Err(HubError::NoSuchCall)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn completing_a_warm_transfer_joins_the_agent_and_hands_the_call_off() {
        let clock = SimClock::shared();
        let fake = Arc::new(Fake {
            warm: Some(CallerState::Resumed),
            ..Fake::default()
        });
        let agent = agent_with(&fake, None);
        let (call, hub) = registered(&agent, &clock, "CA0001").await;
        // No warm transfer yet.
        assert_eq!(
            hub.complete_warm(Carrier::Twilio, "CA0001").await,
            Err(HubError::NoWarmTransfer)
        );
        // Another carrier's id space.
        assert_eq!(
            hub.complete_warm(Carrier::Telnyx, "CA0001").await,
            Err(HubError::NoSuchCall)
        );

        call.ops.transfer(&warm_request()).await.unwrap();
        assert_eq!(fake.log(), ["warm CA0001"]);
        assert!(
            call.ops.pop_action().is_none(),
            "a warm transfer does not end the bot's call"
        );
        hub.complete_warm(Carrier::Twilio, "CA0001").await.unwrap();
        assert_eq!(fake.log(), ["warm CA0001", "complete"]);
        assert_eq!(call.next_action().await, CallAction::HandOff);
        // Done: a second callback has nothing to finish.
        assert_eq!(
            hub.complete_warm(Carrier::Twilio, "CA0001").await,
            Err(HubError::NoWarmTransfer)
        );
        assert_eq!(
            hub.abort_warm(Carrier::Twilio, "CA0001").await,
            Err(HubError::NoWarmTransfer)
        );
        // The call ending afterwards leaves a completed transfer alone.
        call.finish(1).await;
        assert_eq!(fake.log(), ["warm CA0001", "complete"]);
    }

    #[tokio::test(start_paused = true)]
    async fn aborting_a_warm_transfer_tells_the_caller_or_hangs_up() {
        let clock = SimClock::shared();
        // The caller is back on the bot: the bot apologizes.
        let fake = Arc::new(Fake {
            warm: Some(CallerState::Resumed),
            ..Fake::default()
        });
        let agent = agent_with(&fake, None);
        let (call, hub) = registered(&agent, &clock, "CA0001").await;
        call.ops.transfer(&warm_request()).await.unwrap();
        let state = hub.abort_warm(Carrier::Twilio, "CA0001").await.unwrap();
        assert_eq!(state, CallerState::Resumed);
        assert_eq!(
            call.next_action().await,
            CallAction::Say {
                text: Arc::from("sorry"),
                then_hang_up: false,
            }
        );
        assert_eq!(fake.log(), ["warm CA0001", "abort"]);

        // No way back to the bot: the caller is hung up.
        let fake = Arc::new(Fake {
            warm: Some(CallerState::Parked),
            ..Fake::default()
        });
        let agent = agent_with(&fake, None);
        let (call, hub) = registered(&agent, &clock, "CA0003").await;
        call.ops.transfer(&warm_request()).await.unwrap();
        let state = hub.abort_warm(Carrier::Twilio, "CA0003").await.unwrap();
        assert_eq!(state, CallerState::Parked);
        assert_eq!(call.next_action().await, CallAction::HangUp);
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_that_ends_with_a_warm_transfer_pending_aborts_it_and_hangs_the_caller_up() {
        let clock = SimClock::shared();
        let fake = Arc::new(Fake {
            warm: Some(CallerState::Parked),
            ..Fake::default()
        });
        let agent = agent_with(&fake, None);
        let (call, _hub) = registered(&agent, &clock, "CA0001").await;
        call.ops.transfer(&warm_request()).await.unwrap();
        call.finish(5).await;
        assert_eq!(fake.log(), ["warm CA0001", "abort", "hangup CA0001"]);
        // Nothing pending: nothing to do.
        call.finish(6).await;
        assert_eq!(fake.log().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_cold_transfer_hands_the_call_off_without_hanging_it_up() {
        let clock = SimClock::shared();
        let fake = Arc::new(Fake::default());
        let agent = agent_with(&fake, None);
        let (call, _hub) = registered(&agent, &clock, "CA0001").await;
        call.ops.transfer(&request()).await.unwrap();
        assert_eq!(call.next_action().await, CallAction::HandOff);
        assert_eq!(fake.log(), ["transfer CA0001 sales"], "no hang-up");
    }

    #[tokio::test(start_paused = true)]
    async fn a_machine_that_finished_its_greeting_gets_the_message_then_the_call_ends() {
        let clock = SimClock::shared();
        let fake = Arc::new(Fake::default());
        let agent = agent_with(&fake, Some(voicemail_config(VoicemailPolicy::default())));
        let (call, hub) = registered(&agent, &clock, "CA0001").await;
        // A callback that is not about a machine's end: the greeting is still playing.
        hub.deliver_verdict(Carrier::Twilio, "CA0001", AnsweredBy::MachineStart)
            .unwrap();
        assert!(call.ops.pop_action().is_none());
        hub.deliver_verdict(Carrier::Twilio, "CA0001", AnsweredBy::MachineEndBeep)
            .unwrap();
        let expected = CallAction::Say {
            text: Arc::from("Please call us back."),
            then_hang_up: true,
        };
        assert_eq!(call.next_action().await, expected);
        call.message_played();
        // The message was spoken once.
        assert!(call.ops.pop_action().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_person_a_fax_and_an_unknown_verdict_follow_the_policy() {
        let clock = SimClock::shared();
        let fake = Arc::new(Fake::default());
        // A person: the conversation goes on.
        let agent = agent_with(&fake, Some(voicemail_config(VoicemailPolicy::default())));
        let (call, hub) = registered(&agent, &clock, "CA0001").await;
        hub.deliver_verdict(Carrier::Twilio, "CA0001", AnsweredBy::Human)
            .unwrap();
        assert!(call.ops.pop_action().is_none());
        // A fax machine: hang up.
        let (call, hub) = registered(&agent, &clock, "CA0002").await;
        hub.deliver_verdict(Carrier::Twilio, "CA0002", AnsweredBy::Fax)
            .unwrap();
        assert_eq!(call.next_action().await, CallAction::HangUp);
        // Unknown: a person by default, a hang-up when the policy says.
        let (call, hub) = registered(&agent, &clock, "CA0003").await;
        hub.deliver_verdict(Carrier::Twilio, "CA0003", AnsweredBy::Unknown)
            .unwrap();
        assert!(call.ops.pop_action().is_none());
        let strict = VoicemailPolicy {
            on_unknown: Fallback::HangUp,
            ..VoicemailPolicy::default()
        };
        let agent = agent_with(&fake, Some(voicemail_config(strict)));
        let (call, hub) = registered(&agent, &clock, "CA0004").await;
        hub.deliver_verdict(Carrier::Twilio, "CA0004", AnsweredBy::Unknown)
            .unwrap();
        assert_eq!(call.next_action().await, CallAction::HangUp);
        // A detector that never reports a greeting's end: a machine is hung up on.
        let agent = agent_with(
            &fake,
            Some(voicemail_config(VoicemailPolicy::detect_only())),
        );
        let (call, hub) = registered(&agent, &clock, "CA0005").await;
        hub.deliver_verdict(Carrier::Twilio, "CA0005", AnsweredBy::MachineStart)
            .unwrap();
        assert_eq!(call.next_action().await, CallAction::HangUp);
    }

    #[tokio::test(start_paused = true)]
    async fn a_greeting_that_never_ends_gets_the_message_after_the_driver_gives_up_waiting() {
        let clock = SimClock::shared();
        let fake = Arc::new(Fake::default());
        let agent = agent_with(&fake, Some(voicemail_config(VoicemailPolicy::default())));
        let (call, hub) = registered(&agent, &clock, "CA0001").await;
        hub.deliver_verdict(Carrier::Twilio, "CA0001", AnsweredBy::MachineStart)
            .unwrap();
        let start = clock.now();
        // `next_action` sleeps until the driver's deadline (the greeting timeout), then acts.
        let action = call.next_action().await;
        assert_eq!(
            clock.now() - start,
            voicemail_config(VoicemailPolicy::default())
                .policy
                .greeting_timeout
        );
        assert!(
            matches!(
                action,
                CallAction::Say {
                    then_hang_up: true,
                    ..
                }
            ),
            "{action:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_voicemail_phrase_heard_before_the_verdict_upgrades_a_human_verdict() {
        let clock = SimClock::shared();
        let fake = Arc::new(Fake::default());
        let agent = agent_with(&fake, Some(voicemail_config(VoicemailPolicy::default())));
        let (call, hub) = registered(&agent, &clock, "CA0001").await;
        call.observe(&EngineEvent::UserTranscript {
            text: Arc::from("Hello, please leave a message after the beep."),
            is_final: true,
        });
        // An interim transcript is not heard.
        call.observe(&EngineEvent::UserTranscript {
            text: Arc::from("leave a message"),
            is_final: false,
        });
        hub.deliver_verdict(Carrier::Twilio, "CA0001", AnsweredBy::Human)
            .unwrap();
        // The driver waits for the greeting to end (its `hint_wait`), then speaks.
        let start = clock.now();
        let action = call.next_action().await;
        assert!(
            matches!(
                action,
                CallAction::Say {
                    then_hang_up: true,
                    ..
                }
            ),
            "{action:?}"
        );
        assert_eq!(
            clock.now() - start,
            voicemail_config(VoicemailPolicy::default())
                .policy
                .hint_wait
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_start_of_a_call_can_carry_the_verdict() {
        let clock = SimClock::shared();
        let fake = Arc::new(Fake::default());
        let agent = agent_with(&fake, Some(voicemail_config(VoicemailPolicy::default())));
        let mut started = info("twilio", "call_sid", "CA0001");
        started.metadata.insert(
            "custom.answered_by".to_owned(),
            "machine_end_beep".to_owned(),
        );
        let call = agent.call(CallStarted::from_info(started), clock.clone());
        call.register().await;
        assert!(matches!(
            call.next_action().await,
            CallAction::Say {
                then_hang_up: true,
                ..
            }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_verdict_for_an_agent_without_voicemail_is_refused() {
        let clock = SimClock::shared();
        let fake = Arc::new(Fake::default());
        let agent = agent_with(&fake, None);
        let (_call, hub) = registered(&agent, &clock, "CA0001").await;
        assert_eq!(
            hub.deliver_verdict(Carrier::Twilio, "CA0001", AnsweredBy::Human),
            Err(HubError::NoVoicemail)
        );
        assert_eq!(
            hub.deliver_verdict(Carrier::Twilio, "CA9999", AnsweredBy::Human),
            Err(HubError::NoSuchCall)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_verdict_that_beats_its_call_waits_for_it() {
        let clock = SimClock::shared();
        let fake = Arc::new(Fake::default());
        let mut agent = agent_with(&fake, Some(voicemail_config(VoicemailPolicy::default())));
        agent.controls.hub = CallHub::with_clock(clock.clone());
        let hub = agent.controls.hub();
        // The detector was faster than the media stream: queued, not refused.
        assert_eq!(
            hub.deliver_verdict(Carrier::Twilio, "CA0001", AnsweredBy::MachineEndBeep),
            Ok(VerdictDelivery::Queued)
        );
        assert_eq!(hub.pending_verdicts(), 1);
        // Another call's start does not take it.
        let (_other, _) = registered(&agent, &clock, "CA0002").await;
        assert_eq!(hub.pending_verdicts(), 1);
        // Its own start does, and the machine gets the message.
        let (call, _) = registered(&agent, &clock, "CA0001").await;
        assert_eq!(hub.pending_verdicts(), 0);
        assert!(matches!(
            call.next_action().await,
            CallAction::Say {
                then_hang_up: true,
                ..
            }
        ));
        // A live call gets its verdicts at once.
        assert_eq!(
            hub.deliver_verdict(Carrier::Twilio, "CA0001", AnsweredBy::Fax),
            Ok(VerdictDelivery::Delivered)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn queued_verdicts_expire_and_are_bounded() {
        let clock = SimClock::shared();
        let fake = Arc::new(Fake::default());
        let mut agent = agent_with(&fake, Some(voicemail_config(VoicemailPolicy::default())));
        agent.controls.hub = CallHub::with_clock(clock.clone());
        let hub = agent.controls.hub();
        hub.deliver_verdict(Carrier::Twilio, "CA0001", AnsweredBy::MachineEndBeep)
            .unwrap();
        clock
            .sleep(PENDING_VERDICT_TTL + Duration::from_secs(1))
            .await;
        // Too old: the call that starts now never hears of it.
        let (call, _) = registered(&agent, &clock, "CA0001").await;
        assert!(call.ops.pop_action().is_none());
        assert_eq!(hub.pending_verdicts(), 0);
        // A flood of verdicts for calls that never come is bounded.
        for i in 0..MAX_PENDING_VERDICTS + 10 {
            hub.deliver_verdict(Carrier::Telnyx, &format!("v3:{i}"), AnsweredBy::Human)
                .unwrap();
        }
        assert_eq!(hub.pending_verdicts(), MAX_PENDING_VERDICTS);
        // A hub without a clock queues nothing.
        let plain = CallHub::default();
        assert_eq!(
            plain.deliver_verdict(Carrier::Twilio, "CA1", AnsweredBy::Human),
            Err(HubError::NoSuchCall)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_actions_queue_is_bounded() {
        let clock = SimClock::shared();
        let fake = Arc::new(Fake::default());
        let agent = agent_with(&fake, None);
        let (call, _hub) = registered(&agent, &clock, "CA0001").await;
        for _ in 0..MAX_QUEUED_ACTIONS + 5 {
            call.ops.push_action(CallAction::HangUp);
        }
        let mut n = 0;
        while call.ops.pop_action().is_some() {
            n += 1;
        }
        assert_eq!(n, MAX_QUEUED_ACTIONS);
    }
}
