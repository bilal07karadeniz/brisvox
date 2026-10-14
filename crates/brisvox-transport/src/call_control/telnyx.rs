//! Telnyx call control over the Call Control REST API (`docs/providers/call-control.md`,
//! section 2). Base `https://api.telnyx.com/v2`, `Authorization: Bearer <API key>`, JSON bodies;
//! success is `200 {"data":{"result":"ok"}}`, failure `422` with an `errors` list.
//!
//! Commands used: `POST /calls/{id}/actions/{send_dtmf,transfer,hangup,streaming_stop}`,
//! `POST /conferences`, `POST /conferences/{id}/actions/join`, `POST /calls` (dial). Every
//! command carries a `command_id` derived from the operation's idempotency key
//! (`brisvox-<16 hex digits>`): Telnyx ignores a repeated command id for the same leg for 60
//! seconds, so a retried tool call cannot act twice.
//!
//! # Media streaming conflicts (UNVERIFIED)
//! `transfer`, `bridge` and conference commands may be refused with error `90045` ("This command
//! can't be issued when media streaming is used") while a media stream is active on the leg; the
//! docs do not say which commands are affected. For [`transfer_cold`](CallControl::transfer_cold)
//! and the first step of [`transfer_warm`](CallControl::transfer_warm), a `90045` makes the
//! binding issue `streaming_stop` and repeat the command once under a new command id
//! (`...r1`; the refused command was not applied) when
//! [`TelnyxConfig::stop_stream_on_conflict`] is set, which it is by default: the bot is leaving
//! the call anyway. If the stop succeeds and the retry fails, the caller is left without the bot
//! (the stream is not restarted: `streaming_start` needs the original stream parameters). That
//! failure is reported as [`Error::StreamStopped`] (outcome unknown, so the transfer tool keeps
//! its claim and tells the model not to repeat), and for a warm transfer as
//! [`Error::WarmFailed`] with [`CallerState::Parked`] wrapping it; the application must then hang
//! the caller up or move them on itself.
//! [`send_dtmf`](CallControl::send_dtmf) never stops the stream (the bot continues after the
//! tones): a `90045` there is [`Error::StreamActive`].
//!
//! A host-issued `streaming_stop` ends the media WebSocket, which the Telnyx media adapter
//! (`crate::telnyx`) would take for the end of the call. [`TelnyxConfig::on_stream_stop`] is
//! called right before the binding issues the stop, with the call it concerns, so the host can
//! mark that stream as stopped on purpose in the adapter (the adapter's own API for it lives
//! with the adapter; this hook only carries the call id there).
//!
//! # Warm transfer steps
//! 1. A conference is created with the caller's leg (`POST /conferences`, `hold_audio_url` when
//!    configured; **without it the caller hears silence** while waiting); the response must carry
//!    `data.id` (the response shape is not on the fetched pages: UNVERIFIED, a missing id is
//!    [`Error::Protocol`]).
//! 2. The destination is dialed with `POST /calls` (`link_to` the caller's leg) over
//!    [`TelnyxConfig::connection_id`], `from` = [`TelnyxConfig::caller_id`], optionally with AMD.
//!    If this fails, the caller is left parked in a lone conference: the error is
//!    [`Error::WarmFailed`] with [`CallerState::Parked`] and a recovery handle (the conference id,
//!    no agent leg), which [`BoundCall`](super::BoundCall) keeps for
//!    [`abort_warm`](CallControl::abort_warm).
//! 3. When the agent answers (a `call.answered` webhook, served by the application) and the
//!    application accepted the answer (AMD verdict: a person, not a voicemail),
//!    [`complete_warm`](CallControl::complete_warm) joins the agent leg to the conference. Until
//!    then the agent leg is not in the conference. If the agent does not answer,
//!    [`abort_warm`](CallControl::abort_warm) hangs up the agent leg.
//!
//! There is **no way back to the bot** on Telnyx: the stream was stopped to make the conference
//! command possible and `streaming_start` needs the original stream parameters, so
//! [`abort_warm`](CallControl::abort_warm) always reports [`CallerState::Parked`] and the
//! application must hang the caller up (or move them on) itself. An outcome-unknown dial may
//! have created a leg whose id is unknown; it is never joined (the join is explicit), but an
//! answered one stays up until its far end hangs up: the application sees it in the
//! `call.answered` webhook of its connection.
//!
//! The summary cannot be read to the agent with the commands this binding uses (`speak` and
//! `playback_start` were not fetched), so the transfer reports [`Briefing::NotSupported`].

use std::time::Duration;

use brisvox_agent::{Briefing, DtmfString, PhoneNumber, TransferRequest};
use brisvox_core::{Secret, SharedClock};
use reqwest::Method;
use serde_json::{Map, Value, json};

use super::amd::AmdConfig;
use super::error::{CallerState, Error, Result};
use super::http::{Body, Http, Reply, auth_header, sanitize};
use super::{CallControl, CallFuture, CallRef, Capabilities, WarmTransfer};

const PROVIDER: &str = "telnyx";
/// The REST base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.telnyx.com/v2";

/// Telnyx call-control settings.
#[derive(Debug, Clone)]
pub struct TelnyxConfig {
    /// The API key (v2), sent as a bearer token.
    pub api_key: Secret<String>,
    /// The REST base URL (a loopback `http://` URL is allowed for tests).
    pub base_url: String,
    /// Deadline of each request.
    pub request_timeout: Duration,
    /// Deadline of each connection setup.
    pub connect_timeout: Duration,
    /// The Call Control application (connection) the dialed agent leg belongs to. Required for
    /// warm transfers.
    pub connection_id: Option<String>,
    /// The `from` of transfers and of the agent leg: E.164. For a cold transfer the default is
    /// the original call's `to`; a warm transfer requires it.
    pub caller_id: Option<PhoneNumber>,
    /// Ring time of the destination (`timeout_secs`), 5 to 600.
    pub transfer_timeout_secs: u32,
    /// Duration of each DTMF tone (`duration_millis`), 100 to 500 ms.
    pub dtmf_duration: Duration,
    /// Stop the media stream and retry once when a transfer is refused with `90045`.
    pub stop_stream_on_conflict: bool,
    /// `hold_audio_url` of the warm-transfer conference (audio the caller hears while waiting).
    pub hold_audio_url: Option<String>,
    /// AMD on the agent leg.
    pub agent_leg_amd: Option<AmdConfig>,
    /// Called with the call right before the binding issues a `streaming_stop` (the `90045`
    /// retry path), so the host can mark that stream as stopped on purpose and the media adapter
    /// does not read the end of the stream as the end of the call. `None` by default.
    pub on_stream_stop: Option<StreamStopHook>,
}

/// A callback invoked before the binding stops a call's media stream on purpose; see
/// [`TelnyxConfig::on_stream_stop`]. It runs on the task that issues the transfer, so it must be
/// quick and must not block.
#[derive(Clone)]
pub struct StreamStopHook(std::sync::Arc<dyn Fn(&CallRef) + Send + Sync>);

impl StreamStopHook {
    /// Wraps `hook`.
    pub fn new(hook: impl Fn(&CallRef) + Send + Sync + 'static) -> Self {
        Self(std::sync::Arc::new(hook))
    }

    /// Runs the hook for `call` (what the binding does right before `streaming_stop`).
    pub fn invoke(&self, call: &CallRef) {
        (self.0)(call);
    }

    /// The hook that tells the Telnyx media adapter (`crate::telnyx`) the stream of the call is
    /// about to be stopped on purpose, so the stream close that `streaming_stop` causes
    /// is neither reported as a caller hang-up nor answered with a REST hang-up. Pass the
    /// server's [`crate::telnyx::TelnyxServer::stream_stopper`].
    #[cfg(feature = "telnyx")]
    pub fn from_stopper(stopper: crate::telnyx::StreamStopper) -> Self {
        Self::new(move |call: &CallRef| {
            if !stopper.expect_stop(call.as_str()) {
                tracing::debug!("stream stop announced for a call this server does not stream");
            }
        })
    }
}

impl std::fmt::Debug for StreamStopHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StreamStopHook(..)")
    }
}

impl TelnyxConfig {
    /// Settings with the production base URL, 10 s request and 3 s connect deadlines, a 30 s
    /// ring time, 250 ms tones and stream stopping on conflict.
    pub fn new(api_key: Secret<String>) -> Self {
        Self {
            api_key,
            base_url: DEFAULT_BASE_URL.to_owned(),
            request_timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(3),
            connection_id: None,
            caller_id: None,
            transfer_timeout_secs: 30,
            dtmf_duration: Duration::from_millis(250),
            stop_stream_on_conflict: true,
            hold_audio_url: None,
            agent_leg_amd: None,
            on_stream_stop: None,
        }
    }
}

/// Telnyx call control (REST). Cheap to share behind an `Arc`.
pub struct TelnyxCallControl {
    inner: std::sync::Arc<Inner>,
}

struct Inner {
    config: TelnyxConfig,
    http: Http,
}

impl std::fmt::Debug for TelnyxCallControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelnyxCallControl")
            .field("config", &self.inner.config)
            .finish_non_exhaustive()
    }
}

impl Clone for TelnyxCallControl {
    fn clone(&self) -> Self {
        Self {
            inner: std::sync::Arc::clone(&self.inner),
        }
    }
}

impl TelnyxCallControl {
    /// Builds the client. The key goes into a sensitive `Authorization` header and is not kept
    /// anywhere else.
    ///
    /// # Errors
    /// [`Error::Config`] for a bad base URL, timeout, tone duration or connection id, a plain
    /// `http://` base URL on a remote host, or a key that is not valid in a header.
    pub fn new(config: TelnyxConfig, clock: SharedClock) -> Result<Self> {
        if !(5..=600).contains(&config.transfer_timeout_secs) {
            return Err(Error::Config(
                "transfer_timeout_secs must be 5 to 600".to_owned(),
            ));
        }
        if !(100..=500).contains(&config.dtmf_duration.as_millis()) {
            return Err(Error::Config(
                "dtmf_duration must be 100 to 500 ms".to_owned(),
            ));
        }
        if let Some(id) = &config.connection_id
            && CallRef::new(id.as_str()).is_err()
        {
            return Err(Error::Config("connection_id is not a valid id".to_owned()));
        }
        if let Some(amd) = &config.agent_leg_amd {
            amd.telnyx_fields()
                .map_err(|e| Error::Config(e.to_string()))?;
        }
        let header = auth_header(&format!("Bearer {}", config.api_key.expose()))?;
        let http = Http::new(
            PROVIDER,
            &config.base_url,
            header,
            clock,
            config.request_timeout,
            config.connect_timeout,
        )?;
        Ok(Self {
            inner: std::sync::Arc::new(Inner { config, http }),
        })
    }

    /// The configuration in use (the key prints as `[REDACTED]`).
    pub fn config(&self) -> &TelnyxConfig {
        &self.inner.config
    }
}

fn protocol(detail: &str) -> Error {
    Error::Protocol {
        provider: PROVIDER,
        detail: detail.to_owned(),
    }
}

/// The command id of an operation: `brisvox-<key>` plus a suffix for follow-up commands.
fn command_id(idempotency: u64, suffix: &str) -> String {
    format!("brisvox-{idempotency:016x}{suffix}")
}

/// The JSON of a 2xx reply; the mapped error otherwise.
fn ok_body(reply: &Reply) -> Result<Value> {
    if (200..300).contains(&reply.status) {
        return Ok(serde_json::from_slice(&reply.body).unwrap_or(Value::Null));
    }
    Err(rejection(reply))
}

/// Maps an error reply: `errors[0].code/title/detail`. `90018` (call already ended) is
/// [`Error::CallGone`], `90045` (media streaming in use) [`Error::StreamActive`]. The text is
/// left out for 401 and 403.
fn rejection(reply: &Reply) -> Error {
    let body: Value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
    let first = body
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|errors| errors.first());
    let code = first.and_then(|e| e.get("code")).and_then(|c| match c {
        Value::String(text) => Some(sanitize(text, 16)),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    });
    match code.as_deref() {
        Some("90018") => return Error::CallGone { provider: PROVIDER },
        Some("90045") => return Error::StreamActive { provider: PROVIDER },
        _ => {}
    }
    if reply.status == 404 {
        return Error::CallGone { provider: PROVIDER };
    }
    let detail = if matches!(reply.status, 401 | 403) {
        String::new()
    } else {
        first
            .and_then(|e| e.get("detail").or_else(|| e.get("title")))
            .and_then(Value::as_str)
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
    async fn post(&self, path: &str, body: &Value, what: &'static str) -> Result<Value> {
        let bytes = serde_json::to_vec(body)
            .map_err(|_| Error::InvalidArgument("the request cannot be encoded".to_owned()))?;
        let reply = self
            .http
            .request(Method::POST, path, Body::Json(bytes), what)
            .await?;
        ok_body(&reply)
    }

    async fn action(
        &self,
        call: &CallRef,
        action: &str,
        body: &Value,
        what: &'static str,
    ) -> Result<Value> {
        self.post(
            &format!("/calls/{}/actions/{action}", call.as_str()),
            body,
            what,
        )
        .await
    }

    /// Runs a command that a media stream may block: `POST path` with `build(command_id)`. On
    /// `90045`, and when allowed, stops the stream and repeats the command once under a new
    /// command id. A failure of the repeated command is [`Error::StreamStopped`]: the stream is
    /// gone whatever the cause, which the caller must not mistake for "nothing was done".
    /// (A failed `streaming_stop` is returned as it is: the stream is then still up, or the
    /// error is already an unknown outcome.)
    async fn with_stream_stop(
        &self,
        call: &CallRef,
        idempotency: u64,
        path: &str,
        what: &'static str,
        build: impl Fn(String) -> Value,
    ) -> Result<Value> {
        match self
            .post(path, &build(command_id(idempotency, "")), what)
            .await
        {
            Err(Error::StreamActive { .. }) if self.config.stop_stream_on_conflict => {
                tracing::info!(
                    what,
                    "refused while streaming; stopping the stream and retrying once"
                );
                if let Some(hook) = &self.config.on_stream_stop {
                    (hook.0)(call);
                }
                self.action(
                    call,
                    "streaming_stop",
                    &json!({ "command_id": command_id(idempotency, ".stop") }),
                    "stop stream",
                )
                .await?;
                self.post(path, &build(command_id(idempotency, ".r1")), what)
                    .await
                    .map_err(|cause| Error::StreamStopped {
                        provider: PROVIDER,
                        cause: Box::new(cause),
                    })
            }
            other => other,
        }
    }

    fn dial_body(
        &self,
        request: &TransferRequest,
        caller: &CallRef,
        idempotency: u64,
    ) -> Result<Value> {
        let connection = self
            .config
            .connection_id
            .as_ref()
            .ok_or(Error::Unsupported {
                provider: PROVIDER,
                what: "a warm transfer without connection_id",
            })?;
        let from = self.config.caller_id.as_ref().ok_or(Error::Unsupported {
            provider: PROVIDER,
            what: "a warm transfer without caller_id",
        })?;
        let mut body = Map::new();
        body.insert("connection_id".to_owned(), json!(connection));
        body.insert("to".to_owned(), json!(request.destination.as_str()));
        body.insert("from".to_owned(), json!(from.as_str()));
        body.insert("link_to".to_owned(), json!(caller.as_str()));
        body.insert(
            "timeout_secs".to_owned(),
            json!(self.config.transfer_timeout_secs),
        );
        body.insert(
            "command_id".to_owned(),
            json!(command_id(idempotency, ".dial")),
        );
        if let Some(amd) = &self.config.agent_leg_amd {
            for (key, value) in amd.telnyx_fields()? {
                body.insert(key.to_owned(), value);
            }
        }
        Ok(Value::Object(body))
    }

    async fn warm(&self, call: CallRef, request: TransferRequest) -> Result<WarmTransfer> {
        let name = format!("brisvox-{:016x}", request.idempotency);
        // Validate everything that can be validated before the first request.
        let dial = self.dial_body(&request, &call, request.idempotency)?;
        let hold_audio = self.config.hold_audio_url.clone();
        let created = self
            .with_stream_stop(
                &call,
                request.idempotency,
                "/conferences",
                "create conference",
                |_| {
                    let mut body = json!({
                        "call_control_id": call.as_str(),
                        "name": name,
                        "start_conference_on_create": true,
                    });
                    if let (Some(url), Some(map)) = (&hold_audio, body.as_object_mut()) {
                        map.insert("hold_audio_url".to_owned(), json!(url));
                    }
                    body
                },
            )
            .await
            .map_err(|error| match error {
                // The bot's stream is gone and no conference exists (or its id is unknown): the
                // caller is parked with nothing to recover from, and the model must be told so.
                Error::StreamStopped { .. } => Error::WarmFailed {
                    cause: Box::new(error),
                    caller: CallerState::Parked,
                    transfer: None,
                },
                other => other,
            })?;
        let conference = created
            .pointer("/data/id")
            .and_then(Value::as_str)
            .and_then(|id| CallRef::new(id).ok())
            .ok_or_else(|| protocol("the created conference has no usable data.id"))?;
        let handle = |agent_leg: Option<CallRef>| WarmTransfer {
            provider: PROVIDER,
            caller: call.clone(),
            agent_leg,
            conference: conference.as_str().to_owned(),
            briefing: if request.summary.as_deref().is_some_and(|s| !s.is_empty()) {
                Briefing::NotSupported
            } else {
                Briefing::None
            },
            summary: None,
        };
        let dialed = match self.post("/calls", &dial, "call agent").await {
            Ok(dialed) => dialed,
            Err(cause) => {
                return Err(Error::WarmFailed {
                    cause: Box::new(cause),
                    caller: CallerState::Parked,
                    transfer: Some(Box::new(handle(None))),
                });
            }
        };
        let Some(agent_leg) = dialed
            .pointer("/data/call_control_id")
            .and_then(Value::as_str)
            .and_then(|id| CallRef::new(id).ok())
        else {
            // The dial was accepted but its leg is unknown: the caller waits in the conference.
            return Err(Error::WarmFailed {
                cause: Box::new(protocol(
                    "the dialed call has no usable data.call_control_id",
                )),
                caller: CallerState::Parked,
                transfer: Some(Box::new(handle(None))),
            });
        };
        Ok(handle(Some(agent_leg)))
    }
}

impl CallControl for TelnyxCallControl {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    fn capabilities(&self) -> Capabilities {
        let config = &self.inner.config;
        Capabilities {
            send_dtmf: true,
            warm_transfer: config.connection_id.is_some() && config.caller_id.is_some(),
            warm_briefing: false,
        }
    }

    fn send_dtmf<'a>(
        &'a self,
        call: &'a CallRef,
        digits: &'a DtmfString,
        idempotency: u64,
    ) -> CallFuture<'a, ()> {
        Box::pin(async move {
            let duration =
                u64::try_from(self.inner.config.dtmf_duration.as_millis()).unwrap_or(250);
            let body = json!({
                "digits": digits.as_str(),
                "duration_millis": duration,
                "command_id": command_id(idempotency, ""),
            });
            self.inner
                .action(call, "send_dtmf", &body, "send dtmf")
                .await
                .map(|_| ())
        })
    }

    fn transfer_cold<'a>(
        &'a self,
        call: &'a CallRef,
        request: &'a TransferRequest,
    ) -> CallFuture<'a, ()> {
        Box::pin(async move {
            let config = &self.inner.config;
            self.inner
                .with_stream_stop(
                    call,
                    request.idempotency,
                    &format!("/calls/{}/actions/transfer", call.as_str()),
                    "cold transfer",
                    |id| {
                        let mut body = json!({
                            "to": request.destination.as_str(),
                            "timeout_secs": config.transfer_timeout_secs,
                            "command_id": id,
                        });
                        if let (Some(from), Some(map)) = (&config.caller_id, body.as_object_mut()) {
                            map.insert("from".to_owned(), json!(from.as_str()));
                        }
                        body
                    },
                )
                .await
                .map(|_| ())
        })
    }

    fn transfer_warm<'a>(
        &'a self,
        call: &'a CallRef,
        request: &'a TransferRequest,
    ) -> CallFuture<'a, WarmTransfer> {
        let inner = std::sync::Arc::clone(&self.inner);
        let call = call.clone();
        let request = request.clone();
        Box::pin(async move {
            // Its own task: dropping the caller's future must not leave the caller in a lone
            // conference between the two steps. Every step has a deadline.
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
            let body = json!({
                "call_control_id": leg.as_str(),
                "start_conference_on_enter": true,
                "end_conference_on_exit": false,
            });
            self.inner
                .post(
                    &format!("/conferences/{}/actions/join", transfer.conference),
                    &body,
                    "join agent",
                )
                .await
                .map(|_| ())
        })
    }

    fn abort_warm<'a>(&'a self, transfer: &'a WarmTransfer) -> CallFuture<'a, CallerState> {
        Box::pin(async move {
            if let Some(leg) = &transfer.agent_leg {
                match self
                    .inner
                    .action(
                        leg,
                        "hangup",
                        &json!({ "command_id": command_id(stable_hash(&transfer.conference), ".abort") }),
                        "end agent leg",
                    )
                    .await
                {
                    Ok(_) | Err(Error::CallGone { .. }) => {}
                    Err(error) => return Err(error),
                }
            }
            // The stopped media stream cannot be restarted from here: the caller stays in the
            // conference. Say so instead of hiding it.
            Ok(CallerState::Parked)
        })
    }

    fn hangup<'a>(&'a self, call: &'a CallRef, idempotency: u64) -> CallFuture<'a, ()> {
        Box::pin(async move {
            let body = json!({ "command_id": command_id(idempotency, ".hangup") });
            self.inner
                .action(call, "hangup", &body, "hang up")
                .await
                .map(|_| ())
        })
    }
}

/// A stable 64-bit hash (FNV-1a) of `text`, for command ids of follow-up commands.
fn stable_hash(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_ids_are_stable_and_suffixed() {
        assert_eq!(command_id(0xab, ""), "brisvox-00000000000000ab");
        assert_eq!(command_id(0xab, ".r1"), "brisvox-00000000000000ab.r1");
    }

    #[test]
    fn rejections_map_documented_codes() {
        let reply = |status: u16, body: &str| Reply {
            status,
            body: body.as_bytes().to_vec(),
        };
        assert!(matches!(
            rejection(&reply(
                422,
                r#"{"errors":[{"code":"90018","title":"Call has already ended"}]}"#
            )),
            Error::CallGone { .. }
        ));
        assert!(matches!(
            rejection(&reply(422, r#"{"errors":[{"code":90045,"title":"x"}]}"#)),
            Error::StreamActive { .. }
        ));
        match rejection(&reply(
            422,
            r#"{"errors":[{"code":"10004","title":"Missing","detail":"to is required"}]}"#,
        )) {
            Error::Rejected {
                status,
                code,
                detail,
                ..
            } => {
                assert_eq!(
                    (status, code.as_deref(), detail.as_str()),
                    (422, Some("10004"), "to is required")
                );
            }
            other => panic!("{other:?}"),
        }
        match rejection(&reply(
            401,
            r#"{"errors":[{"code":"10009","detail":"key sk-secret invalid"}]}"#,
        )) {
            Error::Rejected { detail, .. } => assert!(detail.is_empty()),
            other => panic!("{other:?}"),
        }
        assert!(matches!(rejection(&reply(404, "")), Error::CallGone { .. }));
    }
}
