//! Twilio call control over the REST API (`docs/providers/call-control.md`, section 1).
//!
//! Endpoints (base `https://api.twilio.com/2010-04-01`, HTTP Basic, form-encoded bodies):
//! - update a live call: `POST /Accounts/{AccountSid}/Calls/{CallSid}.json` with `Twiml=` (new
//!   instructions) or `Status=completed` (hang up);
//! - create a call (the agent leg of a warm transfer): `POST /Accounts/{AccountSid}/Calls.json`;
//! - fetch a call: `GET /Accounts/{AccountSid}/Calls/{CallSid}.json`.
//!
//! # What an update does to the media stream (UNVERIFIED)
//! Twilio documents neither what a REST update does to an active bidirectional
//! `<Connect><Stream>` nor whether `<Connect action>` fires when the server closes the socket
//! (`call-control.md`, 1.3). This binding is built on the safe reading: the update replaces the
//! call's TwiML, the old stream ends in some way, and anything that must continue after the
//! update (DTMF, a failed warm transfer) starts a **new** stream through
//! [`ResumeStream`]. The transport's Twilio adapter must therefore treat a stream that ends
//! within a few seconds of a REST update as expected, not as a hang-up, and map the new stream
//! (its `custom.resume` parameter) back onto the live session. The in-process adapter
//! (`crate::twilio`) does both when the host announces the stream's end first: set
//! [`TwilioConfig::on_stream_restart`] to [`StreamRestartHook::from_resumer`] (with the adapter
//! server's `stream_resumer()`) and [`TwilioConfig::adapter_resumes_session`] to `true`.
//! [`send_dtmf`](CallControl::send_dtmf) is only offered when
//! [`TwilioConfig::adapter_resumes_session`] says the application's adapter does (otherwise the
//! old stream's end would be handled as a hang-up and the conversation lost behind a "sent"
//! tool result).
//!
//! # Warm transfer steps
//! 1. The caller's call is updated with `<Dial><Conference startConferenceOnEnter="false"
//!    endConferenceOnExit="true" beep="false">` named `brisvox-<key>`: the caller waits (hold music
//!    or [`TwilioConfig::conference_wait_url`]) and the bot's stream ends.
//! 2. A second call to the destination is created (`From` = [`TwilioConfig::caller_id`]) whose
//!    TwiML is only a silent `<Pause>` of [`TwilioConfig::agent_accept_window_secs`]: the agent
//!    leg is **not** in the conference and nothing is read to it yet. AMD can run on this leg
//!    ([`TwilioConfig::agent_leg_amd`]); with the synchronous mode the verdict is fetchable with
//!    [`TwilioCallControl::call_status`] (`answered_by`) once the leg is answered, with
//!    `async_amd` it arrives at the callback URL.
//! 3. When the leg was answered by a person (the application decides: AMD verdict, status
//!    callback, call status), [`complete_warm`](CallControl::complete_warm) updates the agent
//!    leg with TwiML that reads the summary (`<Say>`, the caller does not hear it) and joins the
//!    conference with `startConferenceOnEnter="true"`. This explicit step is the accept gate: a
//!    voicemail on the agent's phone is never joined to the caller and never hears the summary,
//!    and if the application does nothing the leg simply ends when the pause ends. (A
//!    `<Gather>` accept prompt would need a webhook; brisvox serves none.)
//! 4. When the agent does not answer, declines or is a machine,
//!    [`abort_warm`](CallControl::abort_warm) ends the agent leg and resumes the caller.
//! 5. If step 2 fails definitively, the caller is put back on the bot through [`ResumeStream`]
//!    (when configured) and the error says where the caller was left. If its outcome is
//!    **unknown** (timeout, connection loss, 5xx) the caller is **not** resumed (the leg may
//!    exist) and the error reports [`CallerState::Parked`] with a recovery handle; the possibly
//!    existing leg has no instructions to join anything, so it cannot reach the empty
//!    conference, it only rings and then ends after the pause. Its SID is unknown; look it up in
//!    the Twilio console or by the `To`/`From` pair if it matters.
//!
//! Both outcomes of the agent's answer are events (status callbacks), not return values:
//! brisvox serves no webhooks, so the application's HTTP app tells the session.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use brisvox_agent::{AnsweredBy, Briefing, DtmfString, PhoneNumber, TransferRequest};
use brisvox_core::{Secret, SharedClock};
use reqwest::Method;
use serde_json::Value;

use super::amd::{AmdConfig, answered_by_from_twilio};
use super::error::{CallerState, Error, Result};
use super::http::{Body, Http, Reply, auth_header, form_encode, sanitize};
use super::{CallControl, CallFuture, CallRef, Capabilities, WarmTransfer};

const PROVIDER: &str = "twilio";
/// The REST base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.twilio.com/2010-04-01";
/// Longest `Twiml` the Create Call page documents (4000 characters). The Update page states no
/// limit (UNVERIFIED), so the same one is applied everywhere.
pub const MAX_TWIML_CHARS: usize = 4000;

/// How requests are authenticated (HTTP Basic).
#[derive(Debug, Clone)]
pub enum TwilioAuth {
    /// The account SID as the user and the Auth Token as the password.
    AuthToken(Secret<String>),
    /// An API key SID as the user and its secret as the password (what Twilio recommends for
    /// applications).
    ApiKey {
        /// The `SK...` key SID.
        sid: String,
        /// The key's secret.
        secret: Secret<String>,
    },
}

/// A new `<Connect><Stream>` the call is sent back to after a REST update that interrupted the
/// bot's stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeStream {
    /// The `wss://` URL of the brisvox Twilio adapter. Used as it is unless
    /// [`url_for_call`](Self::url_for_call) is set.
    pub url: String,
    /// Makes the stream URL of one call (a signed per-call URL, `crate::twilio::stream_token`)
    /// instead of the fixed [`url`](Self::url): the adapter then accepts the new stream only if
    /// its URL was issued for that call and has not been used. `None` from the function (a call
    /// SID that cannot be signed) fails the update.
    pub url_for_call: Option<StreamUrlFn>,
    /// The stream `name` attribute.
    pub name: Option<String>,
    /// Extra `<Parameter>`s. The binding adds `resume` itself (`dtmf` or `warm_failed`), which
    /// arrives as `CallInfo.metadata["custom.resume"]`.
    pub parameters: Vec<(String, String)>,
}

/// Makes the stream URL of a call ([`ResumeStream::url_for_call`]). Two values are equal when
/// they are clones of the same function.
#[derive(Clone)]
pub struct StreamUrlFn(Arc<UrlFn>);

type UrlFn = dyn Fn(&CallRef) -> Option<String> + Send + Sync;

impl StreamUrlFn {
    /// Wraps `make`, which returns the URL for a call (`None`: it cannot be made).
    pub fn new(make: impl Fn(&CallRef) -> Option<String> + Send + Sync + 'static) -> Self {
        Self(Arc::new(make))
    }

    /// The stream URL for `call`.
    pub fn url(&self, call: &CallRef) -> Option<String> {
        (self.0)(call)
    }
}

impl std::fmt::Debug for StreamUrlFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StreamUrlFn(..)")
    }
}

impl PartialEq for StreamUrlFn {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for StreamUrlFn {}

/// A callback invoked right before the binding replaces a call's TwiML in a way that ends the
/// media stream and brings the call back on a new one ([`ResumeStream`]): sending DTMF and the
/// first step of a warm transfer. It gets the call and how long the engine's session should wait
/// for the new stream, so the host can tell its media adapter that the stream's end is expected
/// and must not be read as a hang-up ([`TwilioConfig::on_stream_restart`]). It runs on the task
/// that issues the update, so it must be quick and must not block.
#[derive(Clone)]
pub struct StreamRestartHook {
    announce: Arc<RestartFn>,
    retract: Option<Arc<RetractFn>>,
}

type RestartFn = dyn Fn(&CallRef, Duration) + Send + Sync;
type RetractFn = dyn Fn(&CallRef) + Send + Sync;

impl StreamRestartHook {
    /// Wraps `hook`.
    pub fn new(hook: impl Fn(&CallRef, Duration) + Send + Sync + 'static) -> Self {
        Self {
            announce: Arc::new(hook),
            retract: None,
        }
    }

    /// Adds the callback that withdraws the announcement of a call whose update was definitively
    /// refused (the stream does not end after all). Without it a refused update leaves the
    /// announcement armed until its window runs out.
    #[must_use]
    pub fn with_retraction(mut self, retract: impl Fn(&CallRef) + Send + Sync + 'static) -> Self {
        self.retract = Some(Arc::new(retract));
        self
    }

    /// Runs the hook for `call` (what the binding does right before its update).
    pub fn invoke(&self, call: &CallRef, wait: Duration) {
        (self.announce)(call, wait);
    }

    /// Withdraws the announcement made by [`invoke`](Self::invoke) for `call` (what the binding
    /// does when the update that should have ended the stream was definitively refused).
    pub fn retract(&self, call: &CallRef) {
        if let Some(retract) = &self.retract {
            retract(call);
        }
    }

    /// The hook that tells the Twilio media adapter (`crate::twilio`) that the stream of the
    /// call is about to end on purpose, so the engine's session is kept for the call's next
    /// stream. Pass the server's [`crate::twilio::TwilioServer::stream_resumer`].
    #[cfg(feature = "twilio")]
    pub fn from_resumer(resumer: crate::twilio::StreamResumer) -> Self {
        let canceller = resumer.clone();
        Self::new(move |call: &CallRef, wait: Duration| {
            if !resumer.expect_resume(call.as_str(), wait) {
                tracing::debug!("stream restart announced for a call this server does not stream");
            }
        })
        .with_retraction(move |call: &CallRef| {
            canceller.cancel_expectation(call.as_str());
        })
    }
}

impl std::fmt::Debug for StreamRestartHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StreamRestartHook(..)")
    }
}

/// Added to the agent leg's ring time and accept window for how long the engine's session waits
/// for a caller who is put back on the bot after a warm transfer.
const WARM_RESUME_MARGIN: Duration = Duration::from_secs(60);

/// How long the engine's session waits for the new stream after a DTMF update (the update, the
/// tones and the new stream's connection all fit in it).
pub const DTMF_RESUME_WAIT: Duration = Duration::from_secs(30);

/// Twilio call-control settings.
#[derive(Debug, Clone)]
pub struct TwilioConfig {
    /// The account SID (`AC...`), part of every URL.
    pub account_sid: String,
    /// The credentials.
    pub auth: TwilioAuth,
    /// The REST base URL (a loopback `http://` URL is allowed for tests).
    pub base_url: String,
    /// Deadline of each request.
    pub request_timeout: Duration,
    /// Deadline of each connection setup.
    pub connect_timeout: Duration,
    /// Where to send the call back after an update that ended the stream. Needed by
    /// [`send_dtmf`](CallControl::send_dtmf); optional for warm transfers (without it a failed
    /// warm transfer leaves the caller parked).
    pub resume_stream: Option<ResumeStream>,
    /// The `From` of the agent leg of a warm transfer and the `callerId` of a cold transfer when
    /// set: a Twilio number or a verified number. Required for warm transfers.
    pub caller_id: Option<PhoneNumber>,
    /// Ring time of the dialed party, 5 to 600 seconds.
    pub dial_timeout_secs: u32,
    /// `<Dial action>` of cold transfers: where Twilio reports the outcome of the dial.
    pub dial_action_url: Option<String>,
    /// `StatusCallback` of the agent leg (events `initiated ringing answered completed`).
    pub status_callback: Option<String>,
    /// The `<Say language>` of the summary read to the receiving party.
    pub whisper_language: String,
    /// `waitUrl` of the conference the caller waits in.
    pub conference_wait_url: Option<String>,
    /// AMD on the agent leg.
    pub agent_leg_amd: Option<AmdConfig>,
    /// How long a created agent leg waits (silently) for
    /// [`complete_warm`](CallControl::complete_warm) before its TwiML ends and the call hangs
    /// up, 5 to 600 seconds.
    pub agent_accept_window_secs: u32,
    /// Set to `true` only when the application's Twilio media adapter treats the end of the old
    /// stream after a REST update as expected and maps the new stream (parameter
    /// `custom.resume`) back to the live session. Required, with
    /// [`resume_stream`](Self::resume_stream), for [`send_dtmf`](CallControl::send_dtmf): the
    /// update restarts the stream.
    pub adapter_resumes_session: bool,
    /// Called with the call right before an update that ends the stream and brings the call back
    /// on a new one (DTMF, the first step of a warm transfer). Without it the adapter reads the
    /// old stream's end as a hang-up; with the in-process adapter set it from
    /// [`StreamRestartHook::from_resumer`]. `None` by default.
    pub on_stream_restart: Option<StreamRestartHook>,
}

impl TwilioConfig {
    /// Settings with the production base URL, 10 s request and 3 s connect deadlines, a 30 s
    /// ring time and `en-US` for the summary.
    pub fn new(account_sid: impl Into<String>, auth: TwilioAuth) -> Self {
        Self {
            account_sid: account_sid.into(),
            auth,
            base_url: DEFAULT_BASE_URL.to_owned(),
            request_timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(3),
            resume_stream: None,
            caller_id: None,
            dial_timeout_secs: 30,
            dial_action_url: None,
            status_callback: None,
            whisper_language: "en-US".to_owned(),
            conference_wait_url: None,
            agent_leg_amd: None,
            agent_accept_window_secs: 120,
            adapter_resumes_session: false,
            on_stream_restart: None,
        }
    }
}

/// The status of a call as Twilio reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TwilioCallStatus {
    /// `queued`.
    Queued,
    /// `ringing`.
    Ringing,
    /// `in-progress`.
    InProgress,
    /// `completed`.
    Completed,
    /// `busy`.
    Busy,
    /// `failed`.
    Failed,
    /// `no-answer`.
    NoAnswer,
    /// `canceled`.
    Canceled,
    /// A status not in the documented list.
    Other(String),
}

impl TwilioCallStatus {
    fn parse(text: &str) -> Self {
        match text {
            "queued" => Self::Queued,
            "ringing" => Self::Ringing,
            "in-progress" => Self::InProgress,
            "completed" => Self::Completed,
            "busy" => Self::Busy,
            "failed" => Self::Failed,
            "no-answer" => Self::NoAnswer,
            "canceled" => Self::Canceled,
            other => Self::Other(sanitize(other, 32)),
        }
    }

    /// Whether the call is over.
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Busy | Self::Failed | Self::NoAnswer | Self::Canceled
        )
    }
}

/// What [`TwilioCallControl::call_status`] reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSnapshot {
    /// The call status.
    pub status: TwilioCallStatus,
    /// The AMD verdict, when the call had AMD and Twilio has one (`answered_by`).
    pub answered_by: Option<AnsweredBy>,
}

/// Twilio call control (REST). Cheap to share behind an `Arc`.
#[derive(Debug, Clone)]
pub struct TwilioCallControl {
    inner: Arc<Inner>,
}

struct Inner {
    config: TwilioConfig,
    http: Http,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TwilioCallControl")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl TwilioCallControl {
    /// Builds the client. The credentials go into a sensitive `Authorization` header and are not
    /// kept anywhere else.
    ///
    /// # Errors
    /// [`Error::Config`] for a bad account SID, base URL or timeout, a plain `http://` base URL on
    /// a remote host, or credentials that are not valid in a header.
    pub fn new(config: TwilioConfig, clock: SharedClock) -> Result<Self> {
        if config.account_sid.is_empty()
            || !config
                .account_sid
                .bytes()
                .all(|b| b.is_ascii_alphanumeric())
        {
            return Err(Error::Config(
                "account_sid must be letters and digits only".to_owned(),
            ));
        }
        if !(5..=600).contains(&config.dial_timeout_secs) {
            return Err(Error::Config(
                "dial_timeout_secs must be 5 to 600".to_owned(),
            ));
        }
        if !(5..=600).contains(&config.agent_accept_window_secs) {
            return Err(Error::Config(
                "agent_accept_window_secs must be 5 to 600".to_owned(),
            ));
        }
        if let Some(amd) = &config.agent_leg_amd {
            amd.validate().map_err(|e| Error::Config(e.to_string()))?;
        }
        let (user, password) = match &config.auth {
            TwilioAuth::AuthToken(token) => (config.account_sid.as_str(), token.expose().as_str()),
            TwilioAuth::ApiKey { sid, secret } => (sid.as_str(), secret.expose().as_str()),
        };
        let credentials = format!("{user}:{password}");
        let header = auth_header(&format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(credentials.as_bytes())
        ))?;
        let http = Http::new(
            PROVIDER,
            &config.base_url,
            header,
            clock,
            config.request_timeout,
            config.connect_timeout,
        )?;
        Ok(Self {
            inner: Arc::new(Inner { config, http }),
        })
    }

    /// The configuration in use (credentials print as `[REDACTED]`).
    pub fn config(&self) -> &TwilioConfig {
        &self.inner.config
    }

    /// Reads the call. Use it after a timeout to find out whether an update took effect before
    /// repeating anything.
    ///
    /// # Errors
    /// [`Error::CallGone`] for an unknown call, plus the usual errors.
    pub async fn call_status(&self, call: &CallRef) -> Result<CallSnapshot> {
        self.inner.call_status(call).await
    }
}

fn protocol(detail: &str) -> Error {
    Error::Protocol {
        provider: PROVIDER,
        detail: detail.to_owned(),
    }
}

/// The JSON body of a 2xx reply; the mapped error otherwise.
fn ok_body(reply: &Reply) -> Result<Value> {
    if (200..300).contains(&reply.status) {
        // An empty 2xx body (DELETE answers 204) is fine for callers that ignore the body.
        return Ok(serde_json::from_slice(&reply.body).unwrap_or(Value::Null));
    }
    Err(rejection(reply))
}

/// Maps an error reply. Twilio's REST errors carry `code` and `message` (the page samples show
/// `error_code` and `error_message`: both are read). The text is left out for 401 and 403.
fn rejection(reply: &Reply) -> Error {
    if reply.status == 404 {
        return Error::CallGone { provider: PROVIDER };
    }
    let body: Value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
    let code = ["code", "error_code"]
        .iter()
        .filter_map(|key| body.get(*key))
        .find_map(|value| match value {
            Value::String(text) => Some(sanitize(text, 16)),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        });
    let detail = if matches!(reply.status, 401 | 403) {
        String::new()
    } else {
        ["message", "error_message"]
            .iter()
            .filter_map(|key| body.get(*key).and_then(Value::as_str))
            .next()
            .map(|text| sanitize(text, 200))
            .unwrap_or_default()
    };
    Error::Rejected {
        provider: PROVIDER,
        status: reply.status,
        code,
        detail,
    }
}

impl Inner {
    fn calls_path(&self) -> String {
        format!("/Accounts/{}/Calls.json", self.config.account_sid)
    }

    fn call_path(&self, call: &CallRef) -> String {
        format!(
            "/Accounts/{}/Calls/{}.json",
            self.config.account_sid,
            call.as_str()
        )
    }

    async fn call_status(&self, call: &CallRef) -> Result<CallSnapshot> {
        let reply = self
            .http
            .request(Method::GET, &self.call_path(call), Body::None, "fetch call")
            .await?;
        let body = ok_body(&reply)?;
        let status = body
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| protocol("the call has no status"))?;
        Ok(CallSnapshot {
            status: TwilioCallStatus::parse(status),
            answered_by: body
                .get("answered_by")
                .and_then(Value::as_str)
                .map(answered_by_from_twilio),
        })
    }

    /// Ends the agent leg of a warm transfer that is given up. Success means the leg is over:
    /// ended now, gone (404), or in a final state already. Twilio documents neither the error
    /// for updating a finished call (UNVERIFIED) nor `completed` for a call that is still
    /// ringing (`canceled` is its status for queued and ringing calls), so a 4xx refusal is
    /// answered by reading the leg: a final status is the goal reached, a queued or ringing leg
    /// is ended with `Status=canceled`. Anything else returns the original refusal, so a retry
    /// repeats the sequence.
    async fn end_agent_leg(&self, leg: &CallRef) -> Result<()> {
        match self
            .update_call(leg, &[("Status", "completed".to_owned())], "end agent leg")
            .await
        {
            Ok(()) | Err(Error::CallGone { .. }) => Ok(()),
            Err(refusal @ Error::Rejected { status, .. })
                if (400..500).contains(&status) && !matches!(status, 401 | 403) =>
            {
                match self.call_status(leg).await {
                    Ok(snapshot) if snapshot.status.is_final() => Ok(()),
                    Err(Error::CallGone { .. }) => Ok(()),
                    Ok(snapshot)
                        if matches!(
                            snapshot.status,
                            TwilioCallStatus::Queued | TwilioCallStatus::Ringing
                        ) =>
                    {
                        match self
                            .update_call(
                                leg,
                                &[("Status", "canceled".to_owned())],
                                "cancel agent leg",
                            )
                            .await
                        {
                            Ok(()) | Err(Error::CallGone { .. }) => Ok(()),
                            Err(_) => Err(refusal),
                        }
                    }
                    Ok(_) | Err(_) => Err(refusal),
                }
            }
            Err(error) => Err(error),
        }
    }

    async fn update_call(
        &self,
        call: &CallRef,
        params: &[(&str, String)],
        what: &'static str,
    ) -> Result<()> {
        let reply = self
            .http
            .request(
                Method::POST,
                &self.call_path(call),
                Body::Form(form_encode(params)),
                what,
            )
            .await?;
        ok_body(&reply).map(|_| ())
    }

    async fn update_twiml(&self, call: &CallRef, twiml: String, what: &'static str) -> Result<()> {
        check_twiml(&twiml)?;
        self.update_call(call, &[("Twiml", twiml)], what).await
    }

    async fn create_call(&self, params: &[(&str, String)], what: &'static str) -> Result<CallRef> {
        let reply = self
            .http
            .request(
                Method::POST,
                &self.calls_path(),
                Body::Form(form_encode(params)),
                what,
            )
            .await?;
        let body = ok_body(&reply)?;
        let sid = body
            .get("sid")
            .and_then(Value::as_str)
            .ok_or_else(|| protocol("the created call has no sid"))?;
        CallRef::new(sid).map_err(|_| protocol("the created call has an unusable sid"))
    }

    /// The TwiML that puts `call` back on the bot; `None` without a `resume_stream` (or when its
    /// per-call URL cannot be made).
    fn resume_twiml(&self, call: &CallRef, kind: &str) -> Option<String> {
        self.config
            .resume_stream
            .as_ref()
            .and_then(|stream| connect_stream(stream, call, kind))
    }

    /// The warm-transfer sequence. Runs on its own task (see `transfer_warm`).
    async fn warm(&self, call: CallRef, request: TransferRequest) -> Result<WarmTransfer> {
        let from = self.config.caller_id.as_ref().ok_or(Error::Unsupported {
            provider: PROVIDER,
            what: "a warm transfer without caller_id (the From of the agent leg)",
        })?;
        let name = conference_name(request.idempotency);
        let caller_twiml = caller_conference_twiml(&self.config, &name);
        let summary = request
            .summary
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let agent_twiml = agent_hold_twiml(self.config.agent_accept_window_secs);
        // The TwiML of the later join is checked now: a summary that is too long must fail before
        // the caller is touched.
        check_twiml(&agent_join_twiml(&self.config, &name, summary.as_deref()))?;
        check_twiml(&caller_twiml)?;
        check_twiml(&agent_twiml)?;
        let mut agent_params = vec![
            ("To", request.destination.as_str().to_owned()),
            ("From", from.as_str().to_owned()),
            ("Twiml", agent_twiml),
            ("Timeout", self.config.dial_timeout_secs.to_string()),
        ];
        if let Some(url) = &self.config.status_callback {
            agent_params.push(("StatusCallback", url.clone()));
            for event in ["initiated", "ringing", "answered", "completed"] {
                agent_params.push(("StatusCallbackEvent", event.to_owned()));
            }
        }
        if let Some(amd) = &self.config.agent_leg_amd {
            agent_params.extend(amd.twilio_params()?);
        }
        let handle = |agent_leg: Option<CallRef>| WarmTransfer {
            provider: PROVIDER,
            caller: call.clone(),
            agent_leg,
            conference: name.clone(),
            briefing: if summary.is_some() {
                Briefing::Whisper
            } else {
                Briefing::None
            },
            summary: summary.clone(),
        };

        if let Some(hook) = &self.config.on_stream_restart {
            // The caller may come back on the bot (a failed or aborted transfer): the engine's
            // session waits for the agent leg to ring, be answered and be accepted.
            let wait = Duration::from_secs(u64::from(self.config.agent_accept_window_secs))
                .saturating_add(Duration::from_secs(u64::from(
                    self.config.dial_timeout_secs,
                )))
                .saturating_add(WARM_RESUME_MARGIN);
            hook.invoke(&call, wait);
        }
        if let Err(cause) = self.update_twiml(&call, caller_twiml, "park caller").await {
            // A definite refusal changed nothing: the stream stays, so the announcement of its
            // end is withdrawn. An unknown outcome may have parked the caller: hand back a
            // recovery handle (the announcement stays, the stream may still end).
            if !cause.outcome_unknown()
                && let Some(hook) = &self.config.on_stream_restart
            {
                hook.retract(&call);
            }
            return Err(if cause.outcome_unknown() {
                Error::WarmFailed {
                    cause: Box::new(cause),
                    caller: CallerState::Parked,
                    transfer: Some(Box::new(handle(None))),
                }
            } else {
                cause
            });
        }
        let agent_leg = match self.create_call(&agent_params, "call agent").await {
            Ok(leg) => leg,
            Err(cause) => {
                // An outcome-unknown create may have created the leg: do not resume the caller
                // behind it (the application decides with the recovery handle).
                let caller = if cause.outcome_unknown() {
                    CallerState::Parked
                } else {
                    self.rollback_caller(&call).await
                };
                let transfer = (caller == CallerState::Parked).then(|| Box::new(handle(None)));
                return Err(Error::WarmFailed {
                    cause: Box::new(cause),
                    caller,
                    transfer,
                });
            }
        };
        Ok(handle(Some(agent_leg)))
    }

    /// Puts the caller back on the bot after a failed warm transfer.
    async fn rollback_caller(&self, call: &CallRef) -> CallerState {
        let Some(twiml) = self.resume_twiml(call, "warm_failed") else {
            return CallerState::Parked;
        };
        match self.update_twiml(call, twiml, "resume caller").await {
            Ok(()) => CallerState::Resumed,
            Err(error) => {
                tracing::warn!(%error, "could not put the caller back on the bot");
                CallerState::Parked
            }
        }
    }
}

fn check_twiml(twiml: &str) -> Result<()> {
    if twiml.chars().count() > MAX_TWIML_CHARS {
        return Err(Error::InvalidArgument(format!(
            "the TwiML is longer than {MAX_TWIML_CHARS} characters (shorten the summary)"
        )));
    }
    Ok(())
}

/// The friendly name of a warm transfer's conference. Names are namespaced to the account and
/// must not carry personal data: only the idempotency key goes in.
fn conference_name(idempotency: u64) -> String {
    format!("brisvox-{idempotency:016x}")
}

/// Escapes text and attribute values for XML.
fn xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

fn connect_stream(stream: &ResumeStream, call: &CallRef, kind: &str) -> Option<String> {
    let url = match &stream.url_for_call {
        Some(make) => make.url(call)?,
        None => stream.url.clone(),
    };
    let mut out = String::from("<Connect><Stream url=\"");
    out.push_str(&xml(&url));
    out.push('"');
    if let Some(name) = &stream.name {
        out.push_str(" name=\"");
        out.push_str(&xml(name));
        out.push('"');
    }
    out.push_str("><Parameter name=\"resume\" value=\"");
    out.push_str(&xml(kind));
    out.push_str("\"/>");
    for (name, value) in &stream.parameters {
        out.push_str("<Parameter name=\"");
        out.push_str(&xml(name));
        out.push_str("\" value=\"");
        out.push_str(&xml(value));
        out.push_str("\"/>");
    }
    out.push_str("</Stream></Connect>");
    Some(out)
}

fn dtmf_twiml(digits: &DtmfString, stream: &ResumeStream, call: &CallRef) -> Option<String> {
    Some(format!(
        "<Response><Play digits=\"{}\"/>{}</Response>",
        xml(digits.as_str()),
        connect_stream(stream, call, "dtmf")?
    ))
}

fn dial_twiml(config: &TwilioConfig, number: &PhoneNumber) -> String {
    let mut attrs = String::new();
    if let Some(caller_id) = &config.caller_id {
        attrs.push_str(&format!(" callerId=\"{}\"", xml(caller_id.as_str())));
    }
    attrs.push_str(&format!(
        " answerOnBridge=\"true\" timeout=\"{}\"",
        config.dial_timeout_secs
    ));
    if let Some(action) = &config.dial_action_url {
        attrs.push_str(&format!(" action=\"{}\"", xml(action)));
    }
    format!(
        "<Response><Dial{attrs}>{}</Dial></Response>",
        xml(number.as_str())
    )
}

fn caller_conference_twiml(config: &TwilioConfig, name: &str) -> String {
    let wait = config
        .conference_wait_url
        .as_ref()
        .map(|url| format!(" waitUrl=\"{}\"", xml(url)))
        .unwrap_or_default();
    format!(
        "<Response><Dial><Conference beep=\"false\" startConferenceOnEnter=\"false\" \
         endConferenceOnExit=\"true\" participantLabel=\"caller\"{wait}>{}</Conference></Dial></Response>",
        xml(name)
    )
}

/// TwiML of the agent leg while it waits for the accept decision: silence, in pauses of at most
/// a minute (the maximum of `<Pause length>` is not documented, so none is relied on). When the
/// TwiML ends the call ends.
fn agent_hold_twiml(window_secs: u32) -> String {
    let mut out = String::from("<Response>");
    let mut left = window_secs;
    while left > 0 {
        let step = left.min(60);
        out.push_str(&format!("<Pause length=\"{step}\"/>"));
        left -= step;
    }
    out.push_str("</Response>");
    out
}

/// TwiML that makes an accepted agent leg read the summary and join the conference.
fn agent_join_twiml(config: &TwilioConfig, name: &str, summary: Option<&str>) -> String {
    let say = summary
        .map(|text| {
            format!(
                "<Say language=\"{}\">{}</Say>",
                xml(&config.whisper_language),
                xml(text)
            )
        })
        .unwrap_or_default();
    format!(
        "<Response>{say}<Dial><Conference beep=\"false\" startConferenceOnEnter=\"true\" \
         endConferenceOnExit=\"false\" participantLabel=\"agent\">{}</Conference></Dial></Response>",
        xml(name)
    )
}

impl CallControl for TwilioCallControl {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            send_dtmf: self.inner.config.resume_stream.is_some()
                && self.inner.config.adapter_resumes_session,
            warm_transfer: self.inner.config.caller_id.is_some(),
            warm_briefing: true,
        }
    }

    fn send_dtmf<'a>(
        &'a self,
        call: &'a CallRef,
        digits: &'a DtmfString,
        _idempotency: u64,
    ) -> CallFuture<'a, ()> {
        Box::pin(async move {
            let config = &self.inner.config;
            let stream = config
                .resume_stream
                .as_ref()
                .filter(|_| config.adapter_resumes_session)
                .ok_or(Error::Unsupported {
                    provider: PROVIDER,
                    what: "DTMF toward the far end (Twilio cannot send it over the stream; the \
                           REST route restarts the stream, so it needs a resume_stream and an \
                           adapter that resumes the session: adapter_resumes_session)",
                })?;
            let twiml = dtmf_twiml(digits, stream, call)
                .ok_or_else(|| protocol("the per-call stream URL could not be made"))?;
            check_twiml(&twiml)?;
            if let Some(hook) = &config.on_stream_restart {
                hook.invoke(call, DTMF_RESUME_WAIT);
            }
            let result = self.inner.update_twiml(call, twiml, "send dtmf").await;
            if let (Err(cause), Some(hook)) = (&result, &config.on_stream_restart)
                && !cause.outcome_unknown()
            {
                // Refused for certain: the stream does not end, so a hang-up must not be read as
                // the announced restart.
                hook.retract(call);
            }
            result
        })
    }

    fn transfer_cold<'a>(
        &'a self,
        call: &'a CallRef,
        request: &'a TransferRequest,
    ) -> CallFuture<'a, ()> {
        Box::pin(async move {
            let twiml = dial_twiml(&self.inner.config, &request.destination);
            self.inner.update_twiml(call, twiml, "cold transfer").await
        })
    }

    fn transfer_warm<'a>(
        &'a self,
        call: &'a CallRef,
        request: &'a TransferRequest,
    ) -> CallFuture<'a, WarmTransfer> {
        let inner = Arc::clone(&self.inner);
        let call = call.clone();
        let request = request.clone();
        Box::pin(async move {
            // Its own task: dropping the caller's future must not leave the caller parked
            // between step 1 and step 2. Every step has a deadline, so the task is bounded.
            tokio::spawn(async move { inner.warm(call, request).await })
                .await
                .unwrap_or_else(|_| Err(protocol("the warm transfer task ended unexpectedly")))
        })
    }

    fn complete_warm<'a>(&'a self, transfer: &'a WarmTransfer) -> CallFuture<'a, ()> {
        Box::pin(async move {
            let leg = transfer.agent_leg.as_ref().ok_or_else(|| {
                Error::InvalidArgument("the transfer has no agent leg to join".to_owned())
            })?;
            let twiml = agent_join_twiml(
                &self.inner.config,
                &transfer.conference,
                transfer.summary.as_deref(),
            );
            self.inner
                .update_twiml(leg, twiml, "join agent to the conference")
                .await
        })
    }

    fn abort_warm<'a>(&'a self, transfer: &'a WarmTransfer) -> CallFuture<'a, CallerState> {
        Box::pin(async move {
            let inner = Arc::clone(&self.inner);
            let transfer = transfer.clone();
            // Its own task, like the transfer: a dropped caller must not leave the sequence half
            // done. It stops at the first error so a retry repeats it safely (the caller is only
            // resumed once the agent leg is certainly gone).
            tokio::spawn(async move {
                if let Some(leg) = &transfer.agent_leg {
                    // The agent leg is over (ended now, already final, or gone): the goal is
                    // reached. Any other failure stops here and a retry repeats the sequence.
                    inner.end_agent_leg(leg).await?;
                }
                match inner.resume_twiml(&transfer.caller, "warm_failed") {
                    Some(twiml) => {
                        inner
                            .update_twiml(&transfer.caller, twiml, "resume caller")
                            .await?;
                        Ok(CallerState::Resumed)
                    }
                    None => Ok(CallerState::Parked),
                }
            })
            .await
            .unwrap_or_else(|_| Err(protocol("the abort task ended unexpectedly")))
        })
    }

    fn hangup<'a>(&'a self, call: &'a CallRef, _idempotency: u64) -> CallFuture<'a, ()> {
        Box::pin(async move {
            self.inner
                .update_call(call, &[("Status", "completed".to_owned())], "hang up")
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escapes_markup() {
        assert_eq!(xml("a&b<c>\"d'"), "a&amp;b&lt;c&gt;&quot;d&apos;");
    }

    #[test]
    fn status_parsing() {
        assert_eq!(
            TwilioCallStatus::parse("in-progress"),
            TwilioCallStatus::InProgress
        );
        assert!(TwilioCallStatus::parse("no-answer").is_final());
        assert!(!TwilioCallStatus::parse("ringing").is_final());
        assert_eq!(
            TwilioCallStatus::parse("weird"),
            TwilioCallStatus::Other("weird".to_owned())
        );
    }
}
