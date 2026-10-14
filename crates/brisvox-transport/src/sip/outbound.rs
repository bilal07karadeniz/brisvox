//! Outbound calls: INVITE with an SDP offer, digest authentication when challenged, 18x/200/ACK,
//! and the hand-over of the answered call as a [`CallLink`].

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::Secret;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::sip::uri::ParamsExt;
use rsipstack::sip::{Header, Param, Scheme, StatusCode, StatusCodeKind, Transport, Uri};
use rsipstack::transport::SipAddr;
use tokio_util::sync::CancellationToken;

use super::call::{CallMedia, Origin, call_info, run};
use super::inbound::annotate_security;
use super::sdp::{self, LocalCrypto, SdpError};
use super::srtp::{MasterKey, SrtpPolicy, SrtpSession, SrtpSuite};
use super::{SecureDialogGuard, Shared, Signaling, shared_timeout};
use crate::call::{CallLink, WireEvent, call_pair};

/// Longest header name or value accepted in [`DialRequest::headers`].
const MAX_HEADER_LEN: usize = 1024;

/// Most extra headers in one [`DialRequest`].
const MAX_EXTRA_HEADERS: usize = 32;

/// Digest credentials for an outbound call (answers a 401 or 407 challenge).
#[derive(Debug, Clone)]
pub struct DialCredential {
    /// The digest user name (a trunk's credential, a SIP account).
    pub username: String,
    /// The password; never logged or printed.
    pub password: Secret<String>,
    /// The realm to answer for, when it should not be taken from the challenge.
    pub realm: Option<String>,
}

/// An outbound call.
///
/// `Debug` shows the names of [`headers`](Self::headers) but never their values: they may carry
/// authentication tokens (`X-Telnyx-Token`).
#[derive(Clone)]
pub struct DialRequest {
    /// The callee, as a SIP URI: `sip:+15551234567@trunk.example.com` (add `;transport=tcp` for
    /// TCP, `;transport=tls` or a `sips:` URI for TLS). Twilio wants E.164 with a leading `+`
    /// in the user part. A TLS target is verified against `SipTlsConfig::client_ca_pem`, and with
    /// SRTP enabled the media of a TLS call is SRTP (SDES, `AES_CM_128_HMAC_SHA1_80`).
    pub to: String,
    /// The caller, as a SIP URI: the identity (caller id) the trunk expects in `From`.
    pub from: String,
    /// Where to send the INVITE when it should not go to the host of `to` (an outbound proxy, or
    /// a numeric address: host names in `to` are resolved by the system resolver, with no SRV
    /// lookup).
    pub destination: Option<SocketAddr>,
    /// Credentials for a digest challenge.
    pub credential: Option<DialCredential>,
    /// Extra headers (`P-Charge-Info`, `X-Telnyx-Token`, ...). Names and values must not contain
    /// line breaks; `Authorization`, `Via`, `Contact`, `Call-ID` and the like are set by the
    /// stack and are refused here. Values are redacted in `Debug` output, but the stack logs
    /// whole SIP messages at `info` level (`rsipstack` target): a token in a header reaches the
    /// logs unless that target is filtered to `warn` (see the module docs).
    pub headers: Vec<(String, String)>,
    /// How long the callee may take to answer before the call is cancelled.
    pub ring_timeout: Duration,
    /// Makes a retried request safe: a second request with the same key is refused with
    /// [`DialError::DuplicateKey`] instead of placing a second call. The key is remembered
    /// (the last 1024) from the moment the INVITE goes out.
    pub dial_key: Option<String>,
}

impl std::fmt::Debug for DialRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("DialRequest")
            .field("to", &self.to)
            .field("from", &self.from)
            .field("destination", &self.destination)
            .field("credential", &self.credential)
            .field("header_names", &names)
            .field("ring_timeout", &self.ring_timeout)
            .field("dial_key", &self.dial_key)
            .finish()
    }
}

impl DialRequest {
    /// A request to call `to` as `from`, ringing for at most 60 seconds.
    pub fn new(to: impl Into<String>, from: impl Into<String>) -> Self {
        Self {
            to: to.into(),
            from: from.into(),
            destination: None,
            credential: None,
            headers: Vec::new(),
            ring_timeout: Duration::from_secs(60),
            dial_key: None,
        }
    }
}

/// Why [`SipHandle::dial`](super::SipHandle::dial) did not produce a call.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DialError {
    /// The request is unusable (bad URI, bad header, zero timeout); the message names the rule.
    #[error("invalid dial request: {0}")]
    InvalidRequest(String),
    /// The server is at `max_calls`.
    #[error("at the call limit")]
    AtCapacity,
    /// The server is shutting down.
    #[error("the server is shutting down")]
    ShuttingDown,
    /// A request with this dial key was already made.
    #[error("dial key already used")]
    DuplicateKey,
    /// The callee or the network refused the call with this final SIP status.
    #[error("call rejected: {status} {reason}")]
    Rejected {
        /// The SIP status code (486 busy, 603 decline, 404, 401 without credentials, ...).
        status: u16,
        /// The reason phrase of the status.
        reason: String,
    },
    /// Nobody answered within the ring timeout; the call was cancelled.
    #[error("no answer within the ring timeout")]
    NoAnswer,
    /// The caller cancelled; the call was cancelled.
    #[error("cancelled")]
    Cancelled,
    /// The callee answered with an SDP this endpoint cannot use; the call was ended.
    #[error("unusable SDP answer: {0}")]
    BadAnswer(SdpError),
    /// Signaling failed (no route, transaction timeout, ...); the rendered cause.
    #[error("sip error: {0}")]
    Sip(String),
    /// A socket could not be opened.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Headers a caller may not set: the stack owns them.
fn is_reserved(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "via"
            | "from"
            | "to"
            | "call-id"
            | "cseq"
            | "contact"
            | "content-type"
            | "content-length"
            | "max-forwards"
            | "authorization"
            | "proxy-authorization"
            | "route"
            | "record-route"
    )
}

fn check_headers(headers: &[(String, String)]) -> Result<Vec<Header>, DialError> {
    let bad = |what: String| Err(DialError::InvalidRequest(what));
    if headers.len() > MAX_EXTRA_HEADERS {
        return bad(format!("more than {MAX_EXTRA_HEADERS} extra headers"));
    }
    let mut out = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        let token_ok = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
        if !token_ok || name.len() > MAX_HEADER_LEN {
            return bad(format!("header name {name:?} is not a token"));
        }
        if is_reserved(name) {
            return bad(format!("header {name} is set by the stack"));
        }
        if value.len() > MAX_HEADER_LEN || value.contains(['\r', '\n']) {
            return bad(format!("header {name} has an unusable value"));
        }
        out.push(Header::Other(name.clone(), value.clone()));
    }
    Ok(out)
}

fn parse_uri(text: &str, what: &str) -> Result<Uri, DialError> {
    if text.contains(['\r', '\n', ' ', '<', '>']) {
        return Err(DialError::InvalidRequest(format!(
            "{what} is not a SIP URI"
        )));
    }
    let uri = Uri::try_from(text)
        .map_err(|_| DialError::InvalidRequest(format!("{what} is not a SIP URI")))?;
    if uri.scheme.is_none() {
        return Err(DialError::InvalidRequest(format!(
            "{what} needs a sip: scheme"
        )));
    }
    Ok(uri)
}

/// Removes the dial key again when the INVITE never went out.
struct KeyGuard<'a> {
    shared: &'a Shared,
    key: Option<String>,
}

impl KeyGuard<'_> {
    /// The INVITE went out: the key stays.
    fn keep(&mut self) {
        self.key = None;
    }
}

impl Drop for KeyGuard<'_> {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            Shared::lock(&self.shared.dial_keys).forget(&key);
        }
    }
}

pub(super) async fn dial(
    shared: &Arc<Shared>,
    request: DialRequest,
    cancel: &CancellationToken,
) -> Result<CallLink, DialError> {
    if shared.cancel.is_cancelled() {
        return Err(DialError::ShuttingDown);
    }
    if request.ring_timeout.is_zero() {
        return Err(DialError::InvalidRequest(
            "ring_timeout must be positive".to_owned(),
        ));
    }
    let mut callee = parse_uri(&request.to, "to")?;
    let caller = parse_uri(&request.from, "from")?;
    let extra = check_headers(&request.headers)?;

    let mut key_guard = KeyGuard { shared, key: None };
    if let Some(key) = &request.dial_key {
        if !Shared::lock(&shared.dial_keys).insert(key) {
            return Err(DialError::DuplicateKey);
        }
        key_guard.key = Some(key.clone());
    }
    let permit = Arc::clone(&shared.permits)
        .try_acquire_owned()
        .map_err(|_| DialError::AtCapacity)?;

    // The signaling transport, and with it whether the media may be SRTP: SDES keys only go out
    // on TLS. `optional` offers SRTP when the call is TLS; `required` refuses anything else.
    let sips = callee.scheme == Some(Scheme::Sips);
    let transport = match callee.transport().cloned() {
        Some(transport) => transport,
        None if sips => Transport::Tls,
        None => Transport::Udp,
    };
    if sips && callee.transport().is_none() {
        // The stack picks the connection type from the `transport` parameter, not the scheme.
        callee.params.push(Param::Transport(Transport::Tls));
    }
    let signaling = Signaling::of(Some(&transport));
    let srtp_wanted = match (shared.config.srtp, signaling.is_secure()) {
        (SrtpPolicy::Off, _) | (SrtpPolicy::Optional, false) => false,
        (SrtpPolicy::Optional | SrtpPolicy::Required, true) => true,
        (SrtpPolicy::Required, false) => {
            return Err(DialError::InvalidRequest(
                "srtp is required: call a sips: or ;transport=tls target".to_owned(),
            ));
        }
    };
    if signaling == Signaling::Tls && shared.tls_addr.is_none() {
        return Err(DialError::InvalidRequest(
            "TLS targets need the SIPS listener (tls) to be configured".to_owned(),
        ));
    }
    let local_crypto = if srtp_wanted {
        let key = MasterKey::generate()
            .map_err(|_| DialError::Sip("no randomness for an SRTP key".to_owned()))?;
        Some(LocalCrypto {
            tag: 1,
            suite: SrtpSuite::AesCm128HmacSha1_80,
            key,
        })
    } else {
        None
    };

    let socket = shared.ports.bind().await?;
    let port = socket.local_addr()?.port();
    let local_rtp = SocketAddr::new(shared.config.advertised_ip(), port);
    let session_id = shared.next_session_id();
    let offer = sdp::build_offer(
        local_rtp,
        &shared.config.codecs,
        session_id,
        local_crypto.as_ref(),
    );

    // Through an explicit proxy address the stack would verify the certificate against the
    // proxy's IP address. For a callee named by a domain the connection is made here instead,
    // verified for that domain (`TlsRuntime::connect_named`), and handed to the stack.
    if let (Signaling::Tls, Some(proxy), Some(runtime)) =
        (signaling, request.destination, shared.tls_runtime.as_ref())
        && let rsipstack::sip::Host::Domain(domain) = &callee.host_with_port.host
    {
        let name = domain.to_string();
        let connecting = runtime.connect_named(shared, proxy, &name);
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(DialError::Cancelled),
            () = shared.cancel.cancelled() => return Err(DialError::ShuttingDown),
            result = connecting => result.map_err(|reason| {
                DialError::Sip(format!("TLS to {proxy} for {name}: {reason}"))
            })?,
        }
    }

    let contact = shared
        .contact_uri(signaling, sips)
        .ok_or_else(|| DialError::Sip("cannot build a Contact URI".to_owned()))?;
    let destination = request.destination.map(|addr| {
        let mut sip_addr = SipAddr::from(addr);
        sip_addr.r#type = Some(transport);
        sip_addr
    });
    let option = InviteOption {
        caller,
        callee,
        destination,
        content_type: Some("application/sdp".to_owned()),
        offer: Some(offer.into_bytes()),
        contact,
        credential: request.credential.as_ref().map(|c| Credential {
            username: c.username.clone(),
            password: c.password.expose().clone(),
            realm: c.realm.clone(),
            auth_username: None,
        }),
        headers: (!extra.is_empty()).then_some(extra),
        ..InviteOption::default()
    };

    let (state_tx, states) = shared.dialogs.new_dialog_state_channel();
    key_guard.keep();
    let clock = &*shared.clock;
    // Dropping the INVITE future abandons the call: rsipstack sends the CANCEL (or ends a
    // 2xx that crosses it with ACK and BYE).
    let invite = shared.dialogs.do_invite(option, state_tx);
    let (dialog, response) = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(DialError::Cancelled),
        () = shared.cancel.cancelled() => return Err(DialError::ShuttingDown),
        () = clock.sleep(request.ring_timeout) => return Err(DialError::NoAnswer),
        result = invite => result.map_err(|e| DialError::Sip(e.to_string()))?,
    };
    let Some(response) = response else {
        return Err(DialError::Sip("no final response".to_owned()));
    };
    if response.status_code.kind() != StatusCodeKind::Successful {
        shared.dialogs.remove_dialog(&dialog.id());
        let status = response.status_code.clone();
        return Err(DialError::Rejected {
            status: status.code(),
            reason: reason_of(&status),
        });
    }

    // A TLS dialog is registered from the answer on: its in-dialog requests must come over TLS.
    let secure_guard = (signaling == Signaling::Tls)
        .then(|| SecureDialogGuard::new(&shared.secure_dialogs, dialog.id()));
    let (audio, remote_crypto) =
        match sdp::read_answer(&response.body, &shared.config.codecs, local_crypto.as_ref()) {
            Ok(answer) => answer,
            Err(error) => {
                // Acknowledged already; end the session we cannot use.
                let limit = shared.config.hangup_timeout;
                let _ = shared_timeout(clock, limit, dialog.hangup()).await;
                shared.dialogs.remove_dialog(&dialog.id());
                return Err(DialError::BadAnswer(error));
            }
        };

    let call_id = response
        .call_id_header()
        .map(|c| c.value().to_owned())
        .unwrap_or_default();
    let srtp = match (local_crypto, remote_crypto) {
        (Some(local), Some(remote)) => match SrtpSession::new(local, &remote) {
            Ok(session) => Some(session),
            Err(_) => {
                let limit = shared.config.hangup_timeout;
                let _ = shared_timeout(clock, limit, dialog.hangup()).await;
                shared.dialogs.remove_dialog(&dialog.id());
                return Err(DialError::Sip("no usable SRTP key".to_owned()));
            }
        },
        _ => None,
    };
    let mut metadata = std::collections::BTreeMap::new();
    annotate_security(&mut metadata, signaling, srtp.as_ref());
    metadata.insert("to".to_owned(), request.to.clone());
    metadata.insert("from".to_owned(), request.from.clone());
    if let Some(destination) = request.destination {
        metadata.insert("sip_peer".to_owned(), destination.to_string());
    }
    let (link, ends) = call_pair();
    let info = call_info(&call_id, Origin::Outbound, &audio, metadata);
    // The queue is empty, so `Started` goes in at once; the engine sees it first.
    if ends.events.send(WireEvent::Started(info)).await.is_err() {
        return Err(DialError::Sip("call channel closed".to_owned()));
    }
    shared.counters.inc(&shared.counters.calls_outbound);
    if srtp.is_some() {
        shared.counters.inc(&shared.counters.srtp_calls);
    }
    shared.tracker.spawn(run(CallMedia {
        shared: Arc::clone(shared),
        permit,
        call_id,
        socket,
        audio,
        dialog,
        states,
        ends,
        local_rtp,
        session_id,
        srtp,
        signaling,
        secure_guard,
    }));
    Ok(link)
}

fn reason_of(status: &StatusCode) -> String {
    let text = status.to_string();
    // `Display` renders "486 Busy Here"; keep the phrase.
    text.split_once(' ')
        .map_or(text.clone(), |(_, phrase)| phrase.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(name: &str, value: &str) -> Vec<(String, String)> {
        vec![(name.to_owned(), value.to_owned())]
    }

    #[test]
    fn extra_headers_are_checked_against_injection_and_reserved_names() {
        assert!(check_headers(&h("P-Charge-Info", "sip:1@example.com")).is_ok());
        assert!(check_headers(&h("X-Telnyx-Token", "abc")).is_ok());
        for (name, value) in [
            ("Bad Name", "x"),
            ("X-A:B", "x"),
            ("", "x"),
            ("X-Ok", "a\r\nVia: evil"),
            ("X-Ok", "a\nb"),
            ("Via", "SIP/2.0/UDP evil"),
            ("authorization", "Digest x"),
            ("Call-ID", "x"),
            ("Contact", "<sip:evil@example.com>"),
        ] {
            assert!(
                check_headers(&h(name, value)).is_err(),
                "{name}: {value:?} passed"
            );
        }
        let many: Vec<(String, String)> = (0..=MAX_EXTRA_HEADERS)
            .map(|i| (format!("X-{i}"), "v".to_owned()))
            .collect();
        assert!(check_headers(&many).is_err());
    }

    #[test]
    fn uris_are_checked() {
        assert!(parse_uri("sip:+15551234567@trunk.example.com", "to").is_ok());
        assert!(parse_uri("sip:bob@127.0.0.1:5070;transport=udp", "to").is_ok());
        assert!(parse_uri("sip:bob@example.com\r\nVia: x", "to").is_err());
        assert!(parse_uri("<sip:bob@example.com>", "to").is_err());
        assert!(parse_uri("", "to").is_err());
        assert!(parse_uri("bob", "to").is_err());
    }

    #[test]
    fn credentials_never_show_in_debug_output() {
        let mut request = DialRequest::new("sip:b@example.com", "sip:a@example.com");
        request.credential = Some(DialCredential {
            username: "trunk".to_owned(),
            password: Secret::new("hunter2".to_owned()),
            realm: None,
        });
        request
            .headers
            .push(("X-Telnyx-Token".to_owned(), "tok-s3cret".to_owned()));
        let text = format!("{request:?}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(!text.contains("tok-s3cret"), "{text}");
        assert!(text.contains("trunk"));
        assert!(text.contains("X-Telnyx-Token"));
    }
}
