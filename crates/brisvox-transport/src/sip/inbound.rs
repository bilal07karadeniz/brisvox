//! Inbound SIP: routes the requests the endpoint delivers, and turns an INVITE into a call.
//!
//! An INVITE goes through: source check (done by the transport whitelist before parsing), the
//! host's admission gate (when one is set), call limit, digest challenge, SDP offer/answer, the
//! dialog, an RTP socket, `200 OK` with the answer, the wait for the ACK, and the hand-over of
//! the [`CallLink`](crate::CallLink) (with the gate's ticket) to the engine's accept queue. A CANCEL before the ACK, an ACK timeout or a full accept queue end the
//! attempt and release everything it took.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use rsipstack::dialog::dialog::{DialogState, TerminatedReason};
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::sip::{Header, Method, Request, Scheme, StatusCode};
use rsipstack::transaction::TransactionReceiver;
use rsipstack::transaction::transaction::Transaction;
use tokio::sync::OwnedSemaphorePermit;

use super::auth::{self, Verdict};
use super::call::{CallMedia, Origin, call_info, run};
use super::sdp::{LocalCrypto, Offer, SdpError, Security};
use super::srtp::{MasterKey, SrtpError, SrtpSession};
use super::{SecureDialogGuard, Shared, Signaling, is_tls, shared_timeout};
use crate::call::{WireEvent, call_pair};

/// Most `X-` headers copied into the call metadata, and the longest value copied.
const MAX_COPIED_HEADERS: usize = 16;
const MAX_HEADER_VALUE_CHARS: usize = 256;

const ALLOW: &str = "INVITE, ACK, CANCEL, BYE, OPTIONS, INFO, UPDATE";

/// Routes every transaction the endpoint hands over, until the endpoint stops.
pub(super) async fn dispatch(shared: Arc<Shared>, mut incoming: TransactionReceiver) {
    loop {
        let next = tokio::select! {
            biased;
            () = shared.endpoint_cancel.cancelled() => return,
            next = incoming.recv() => next,
        };
        let Some(tx) = next else {
            return;
        };
        route(&shared, tx);
    }
}

/// The address (and with it the transport type) of the connection a transaction came in on.
fn connection_addr(tx: &Transaction) -> Option<&rsipstack::transport::SipAddr> {
    tx.connection
        .as_ref()
        .map(|connection| connection.get_addr())
}

/// 488 for a re-INVITE or UPDATE, 403 for anything else (an ACK gets no answer), for an in-dialog
/// request that did not come over the TLS connection family of its dialog.
async fn refuse_off_tls(shared: &Arc<Shared>, tx: &mut Transaction, limit: std::time::Duration) {
    let counters = &shared.counters;
    counters.inc(&counters.insecure_dialog_requests);
    tracing::warn!(
        method = %tx.original.method,
        "sip: in-dialog request off TLS on a TLS dialog refused"
    );
    let status = match tx.original.method {
        Method::Ack => return,
        Method::Invite | Method::Update => StatusCode::NotAcceptableHere,
        _ => StatusCode::Forbidden,
    };
    let _ = bounded(shared, limit, tx.reply(status)).await;
}

fn has_to_tag(request: &Request) -> bool {
    request
        .to_header()
        .ok()
        .and_then(|to| to.tag().ok().flatten())
        .is_some()
}

fn route(shared: &Arc<Shared>, tx: Transaction) {
    let counters = &shared.counters;
    // Bounds the tasks a flood of requests can create; the peer retransmits what it drops here.
    let Ok(slot) = Arc::clone(&shared.handlers).try_acquire_owned() else {
        counters.inc(&counters.rejected_busy);
        return;
    };
    let shared = Arc::clone(shared);
    if tx.original.method == Method::Invite && !has_to_tag(&tx.original) {
        shared
            .tracker
            .clone()
            .spawn(inbound_invite(shared, tx, slot));
        return;
    }
    shared.tracker.clone().spawn(async move {
        let _slot = slot;
        handle_other(&shared, tx).await;
    });
}

/// Runs a signaling send or an in-dialog exchange for at most `limit`, and not past the end of
/// the endpoint. `None` when the limit or the endpoint's end came first: the work is dropped, the
/// caller releases whatever it holds (a handler slot above all). Over TCP a peer that stops
/// reading would otherwise keep the write, and with it the slot, forever.
async fn bounded<F: std::future::Future>(
    shared: &Shared,
    limit: std::time::Duration,
    work: F,
) -> Option<F::Output> {
    tokio::select! {
        biased;
        () = shared.endpoint_cancel.cancelled() => None,
        done = shared_timeout(&*shared.clock, limit, work) => done.ok(),
    }
}

/// Everything but a new INVITE: in-dialog requests go to their dialog; the rest is answered here.
async fn handle_other(shared: &Arc<Shared>, mut tx: Transaction) {
    let counters = &shared.counters;
    let reply_limit = shared.config.hangup_timeout;
    if has_to_tag(&tx.original) {
        if let Some(mut dialog) = shared.dialogs.match_dialog(&tx) {
            // A dialog set up over TLS takes in-dialog requests over TLS only: the `Via` of a
            // request says nothing about the connection it came on, and a cleartext re-INVITE or
            // ACK that claims to be TLS must not get an answer with our SDES key in it, nor move
            // the media. Refused before the dialog (and with it any SDP) sees it.
            if shared.secure_dialogs.contains(&dialog.id()) && !is_tls(connection_addr(&tx)) {
                refuse_off_tls(shared, &mut tx, reply_limit).await;
                return;
            }
            // An in-dialog exchange (a re-INVITE waits for its ACK) is bounded by the ACK wait.
            let limit = shared.config.ack_timeout.max(reply_limit);
            match bounded(shared, limit, dialog.handle(&mut tx)).await {
                Some(Err(error)) => tracing::debug!(%error, "sip: in-dialog request failed"),
                Some(Ok(())) => {}
                None => tracing::debug!("sip: in-dialog request timed out"),
            }
        } else {
            counters.inc(&counters.rejected_invalid);
            let _ = bounded(
                shared,
                reply_limit,
                tx.reply(StatusCode::CallTransactionDoesNotExist),
            )
            .await;
        }
        return;
    }
    let allow = Header::Allow(rsipstack::sip::Allow::new(ALLOW));
    if tx.original.method == Method::Options {
        let accept = Header::Accept(rsipstack::sip::Accept::new("application/sdp"));
        let _ = bounded(
            shared,
            reply_limit,
            tx.reply_with(StatusCode::OK, vec![allow, accept], None),
        )
        .await;
    } else if tx.original.method == Method::Cancel {
        // A CANCEL without a transaction to cancel (RFC 3261 section 9.2).
        counters.inc(&counters.rejected_invalid);
        let _ = bounded(
            shared,
            reply_limit,
            tx.reply(StatusCode::CallTransactionDoesNotExist),
        )
        .await;
    } else {
        counters.inc(&counters.rejected_invalid);
        let _ = bounded(
            shared,
            reply_limit,
            tx.reply_with(StatusCode::MethodNotAllowed, vec![allow], None),
        )
        .await;
    }
}

/// Sends a final answer to an INVITE (bounded) and lets go of it.
///
/// Nothing waits for the ACK here: dropping the transaction hands it to the endpoint's cache of
/// finished transactions, which absorbs the ACK and answers a retransmitted INVITE with the same
/// answer until its own cleanup timer fires. So a refusal holds neither the handler slot nor a
/// task past the send, and a flood of refused INVITEs cannot starve the requests that matter.
/// `permit` (the call permit, if one was taken) is released with the answer.
async fn refuse(
    shared: &Shared,
    tx: &mut Transaction,
    status: StatusCode,
    headers: Vec<Header>,
    permit: Option<OwnedSemaphorePermit>,
) {
    let limit = shared.config.hangup_timeout;
    let _ = bounded(shared, limit, tx.reply_with(status, headers, None)).await;
    drop(permit);
}

/// Option tags this endpoint understands in `Require` (RFC 3261 section 8.2.2.3): none.
const SUPPORTED_EXTENSIONS: &[&str] = &[];

/// The option tags of `Require` headers that are not supported, comma separated; `None` when
/// the request requires nothing unsupported.
fn unsupported_extensions(request: &Request) -> Option<String> {
    let tags: Vec<&str> = request
        .headers
        .iter()
        .filter_map(|h| match h {
            Header::Require(r) => Some(r.value()),
            _ => None,
        })
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|tag| {
            !tag.is_empty()
                && !SUPPORTED_EXTENSIONS
                    .iter()
                    .any(|ok| ok.eq_ignore_ascii_case(tag))
        })
        .collect();
    (!tags.is_empty()).then(|| tags.join(", "))
}

fn header_text(request: &Request, want: impl Fn(&Header) -> Option<&str>) -> Option<String> {
    request
        .headers
        .iter()
        .find_map(|h| want(h).map(str::to_owned))
}

/// What the engine learns about the call from the INVITE: parties, peer and `X-` headers
/// (never an `Authorization` header).
fn request_metadata(request: &Request, peer: Option<SocketAddr>) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();
    if let Ok(from) = request.from_header() {
        metadata.insert("from".to_owned(), from.value().to_owned());
    }
    if let Ok(to) = request.to_header() {
        metadata.insert("to".to_owned(), to.value().to_owned());
    }
    metadata.insert("request_uri".to_owned(), request.uri.to_string());
    if let Some(peer) = peer {
        metadata.insert("sip_peer".to_owned(), peer.to_string());
    }
    let mut copied = 0usize;
    for header in request.headers.iter() {
        let Header::Other(name, value) = header else {
            if let Header::PAssertedIdentity(identity) = header {
                metadata.insert(
                    "p_asserted_identity".to_owned(),
                    truncated(identity.value()),
                );
            }
            continue;
        };
        let lower = name.to_ascii_lowercase();
        if lower == "diversion" {
            metadata.insert("diversion".to_owned(), truncated(value));
        } else if lower.starts_with("x-") && copied < MAX_COPIED_HEADERS {
            copied += 1;
            metadata.insert(format!("header.{lower}"), truncated(value));
        }
    }
    metadata
}

fn truncated(value: &str) -> String {
    value.chars().take(MAX_HEADER_VALUE_CHARS).collect()
}

async fn inbound_invite(shared: Arc<Shared>, mut tx: Transaction, slot: OwnedSemaphorePermit) {
    let counters = &shared.counters;
    let clock = Arc::clone(&shared.clock);
    let peer = tx
        .connection
        .as_ref()
        .and_then(|c| c.get_remote_addr())
        .and_then(|a| a.get_socketaddr().ok());
    let busy = |tx_headers: &mut Vec<Header>, secs: u32| {
        tx_headers.push(Header::RetryAfter(rsipstack::sip::RetryAfter::new(
            secs.to_string(),
        )));
    };

    // The host's gate first (SipServer::bind_gated): a refusal is a 503 before any work, with
    // Retry-After only when the host is busy (a draining host sends the caller elsewhere). The
    // ticket is held until the call is handed over; dropping it on any early return below gives
    // the host's reservation back.
    let ticket = match shared
        .gate
        .as_ref()
        .map(|gate| gate.try_admit())
        .transpose()
    {
        Ok(ticket) => ticket,
        Err(refusal) => {
            counters.inc(&counters.rejected_busy);
            let mut headers = Vec::new();
            // The refusal's own delay, as on the websocket (busy 1 s, provider budget 2 s,
            // overloaded 5 s).
            if let Some(secs) = refusal.retry_after_secs() {
                busy(&mut headers, secs);
            }
            refuse(
                &shared,
                &mut tx,
                StatusCode::ServiceUnavailable,
                headers,
                None,
            )
            .await;
            return;
        }
    };

    // Shutting down, over the call limit or with a full accept queue: 503 before any work. The
    // accept-queue slot is reserved here and held until the call is handed over, so a burst of
    // INVITEs cannot all be answered 200 and then overflow the queue (and be hung up) later.
    let permit = Arc::clone(&shared.permits).try_acquire_owned();
    let queue_slot = if shared.cancel.is_cancelled() || permit.is_err() {
        None
    } else {
        shared.calls_tx.clone().try_reserve_owned().ok()
    };
    let Some(queue_slot) = queue_slot else {
        counters.inc(&counters.rejected_busy);
        let mut headers = Vec::new();
        busy(&mut headers, 5);
        refuse(
            &shared,
            &mut tx,
            StatusCode::ServiceUnavailable,
            headers,
            None,
        )
        .await;
        return;
    };
    let Ok(permit) = permit else {
        return;
    };

    // Who may call: digest credentials, when configured.
    if let Some(config) = &shared.config.inbound_auth {
        let verdict = {
            let mut nonces = Shared::lock(&shared.nonces);
            auth::check(config, &mut nonces, &tx.original, clock.now())
        };
        match verdict {
            Verdict::Allowed => {}
            Verdict::Challenge(header) => {
                counters.inc(&counters.rejected_unauthorized);
                refuse(
                    &shared,
                    &mut tx,
                    StatusCode::Unauthorized,
                    vec![header],
                    Some(permit),
                )
                .await;
                return;
            }
            Verdict::Forbidden => {
                counters.inc(&counters.rejected_unauthorized);
                refuse(
                    &shared,
                    &mut tx,
                    StatusCode::Forbidden,
                    Vec::new(),
                    Some(permit),
                )
                .await;
                return;
            }
        }
    }

    // Extensions the caller requires and we do not have: 420 with the tags (RFC 3261 8.2.2.3).
    if let Some(tags) = unsupported_extensions(&tx.original) {
        counters.inc(&counters.rejected_invalid);
        let header = Header::Unsupported(rsipstack::sip::headers::Unsupported::new(tags));
        refuse(
            &shared,
            &mut tx,
            StatusCode::BadExtension,
            vec![header],
            Some(permit),
        )
        .await;
        return;
    }

    // The transport the INVITE came in on: the Contact names it (the peer's BYE and re-INVITE go
    // there), and SDES keys are only accepted from TLS.
    let signaling = Signaling::of(
        tx.connection
            .as_ref()
            .and_then(|c| c.get_addr().r#type.as_ref()),
    );

    // The offer.
    let offer = match read_offer(&shared, &tx.original, signaling) {
        Ok(offer) => offer,
        Err(status) => {
            counters.inc(&counters.rejected_invalid);
            refuse(&shared, &mut tx, status, Vec::new(), Some(permit)).await;
            return;
        }
    };
    // SRTP: our key for the answer. The peer's key (in the offer) protects its media.
    let srtp = match offer.srtp().map(new_session).transpose() {
        Ok(session) => session,
        Err(_) => {
            tracing::warn!("sip: no randomness for an SRTP key, refusing the call");
            refuse(
                &shared,
                &mut tx,
                StatusCode::ServerInternalError,
                Vec::new(),
                Some(permit),
            )
            .await;
            return;
        }
    };

    // The dialog. A request without a From tag or Call-ID cannot make one: 400.
    let (state_tx, mut states) = shared.dialogs.new_dialog_state_channel();
    let sips = tx.original.uri.scheme == Some(Scheme::Sips);
    let contact = shared.contact_uri(signaling, sips);
    let dialog = match shared
        .dialogs
        .get_or_create_server_invite(&tx, state_tx, None, contact)
    {
        Ok(dialog) => dialog,
        Err(error) => {
            tracing::debug!(%error, "sip: INVITE without a usable dialog identity");
            counters.inc(&counters.rejected_invalid);
            refuse(
                &shared,
                &mut tx,
                StatusCode::BadRequest,
                Vec::new(),
                Some(permit),
            )
            .await;
            return;
        }
    };
    let call_id = tx
        .original
        .call_id_header()
        .map(|c| c.value().to_owned())
        .unwrap_or_default();
    // A TLS dialog is registered from here on, before anything else can arrive on it.
    let secure_guard = (signaling == Signaling::Tls)
        .then(|| SecureDialogGuard::new(&shared.secure_dialogs, dialog.id()));
    let mut metadata = request_metadata(&tx.original, peer);
    annotate_security(&mut metadata, signaling, srtp.as_ref());

    // The INVITE transaction runs on its own task: it takes the ACK or the CANCEL.
    let mut serving = dialog.clone();
    shared.tracker.spawn(async move {
        if let Err(error) = serving.handle(&mut tx).await {
            tracing::debug!(%error, "sip: INVITE transaction ended with an error");
        }
    });

    let outcome = answer_and_wait(
        &shared,
        &dialog,
        &mut states,
        &offer,
        srtp.as_ref().map(|session| &session.local),
    )
    .await;
    let (socket, local_rtp, session_id) = match outcome {
        Answered::Confirmed {
            socket,
            local_rtp,
            session_id,
        } => (socket, local_rtp, session_id),
        Answered::Gone => {
            shared.dialogs.remove_dialog(&dialog.id());
            return;
        }
    };
    drop(slot);

    let (mut link, ends) = call_pair();
    link.set_ticket(ticket);
    let info = call_info(&call_id, Origin::Inbound, &offer.params(), metadata);
    if ends.events.send(WireEvent::Started(info)).await.is_err() {
        return;
    }
    if shared.calls_tx.is_closed() {
        // The accept side is gone (the server is shutting down).
        counters.inc(&counters.rejected_busy);
        hang_up(&shared, &dialog).await;
        shared.dialogs.remove_dialog(&dialog.id());
        return;
    }
    // The slot was reserved before the call was answered: this cannot overflow the queue.
    drop(queue_slot.send(link));
    counters.inc(&counters.calls_inbound);
    if srtp.is_some() {
        counters.inc(&counters.srtp_calls);
    }
    run(CallMedia {
        shared: Arc::clone(&shared),
        permit,
        call_id,
        socket,
        audio: offer.params(),
        dialog,
        states,
        ends,
        local_rtp,
        session_id,
        srtp,
        signaling,
        secure_guard,
    })
    .await;
}

/// The SRTP session for an accepted crypto line: our own fresh key under the peer's tag and suite.
fn new_session(remote: &super::sdp::CryptoLine) -> Result<SrtpSession, SrtpError> {
    let key = MasterKey::generate()?;
    let local = LocalCrypto {
        tag: remote.tag,
        suite: remote.suite,
        key,
    };
    SrtpSession::new(local, remote)
}

/// What the engine learns about the call's security: the signaling transport and the SRTP suite.
pub(super) fn annotate_security(
    metadata: &mut BTreeMap<String, String>,
    signaling: Signaling,
    srtp: Option<&SrtpSession>,
) {
    metadata.insert(
        "signaling".to_owned(),
        match signaling {
            Signaling::Udp => "udp",
            Signaling::Tcp => "tcp",
            Signaling::Tls => "tls",
        }
        .to_owned(),
    );
    metadata.insert(
        "srtp".to_owned(),
        srtp.map_or("none", |s| s.local.suite.name()).to_owned(),
    );
}

/// Checks the content type and parses the offer. The error is the status to answer with.
fn read_offer(
    shared: &Shared,
    request: &Request,
    signaling: Signaling,
) -> Result<Offer, StatusCode> {
    if request.body.is_empty() {
        // An INVITE without an offer (the 200 would carry ours): not supported.
        return Err(StatusCode::NotAcceptableHere);
    }
    let content_type = header_text(request, |h| match h {
        Header::ContentType(c) => Some(c.value()),
        _ => None,
    });
    let is_sdp = content_type.as_deref().is_some_and(|c| {
        c.split(';')
            .next()
            .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/sdp"))
    });
    if !is_sdp {
        return Err(StatusCode::UnsupportedMediaType);
    }
    let security = Security {
        policy: shared.config.srtp,
        secure_signaling: signaling.is_secure(),
    };
    Offer::parse_secure(&request.body, &shared.config.codecs, security).map_err(|error| match error
    {
        SdpError::Malformed(_) => StatusCode::BadRequest,
        SdpError::NoAudio | SdpError::NoCommonCodec => StatusCode::NotAcceptableHere,
        SdpError::SrtpRefused => {
            let counters = &shared.counters;
            counters.inc(&counters.rejected_insecure);
            StatusCode::NotAcceptableHere
        }
    })
}

enum Answered {
    Confirmed {
        socket: tokio::net::UdpSocket,
        local_rtp: SocketAddr,
        session_id: u64,
    },
    /// Cancelled, refused, timed out or shut down: nothing is left to do.
    Gone,
}

/// Binds the RTP socket, answers with `200 OK` and waits for the ACK.
async fn answer_and_wait(
    shared: &Arc<Shared>,
    dialog: &InviteDialog,
    states: &mut rsipstack::dialog::dialog::DialogStateReceiver,
    offer: &Offer,
    crypto: Option<&LocalCrypto>,
) -> Answered {
    let counters = &shared.counters;
    let socket = match shared.ports.bind().await {
        Ok(socket) => socket,
        Err(error) => {
            tracing::warn!(%error, "sip: no RTP port available");
            counters.inc(&counters.rejected_busy);
            let _ = dialog.reject(Some(StatusCode::ServiceUnavailable), None);
            return Answered::Gone;
        }
    };
    let port = socket.local_addr().map_or(0, |a| a.port());
    let local_rtp = SocketAddr::new(shared.config.advertised_ip(), port);
    let session_id = shared.next_session_id();
    let answer = offer.answer_with(local_rtp, session_id, session_id, crypto);

    if shared.config.ringing || !shared.config.ring_time.is_zero() {
        let _ = dialog.ringing(None, None);
    }
    if !shared.config.ring_time.is_zero() && !ring(shared, states).await {
        counters.inc(&counters.cancelled_before_answer);
        if !shared.cancel.is_cancelled() {
            return Answered::Gone;
        }
        let _ = dialog.reject(Some(StatusCode::ServiceUnavailable), None);
        return Answered::Gone;
    }
    let sdp = Header::ContentType(rsipstack::sip::ContentType::new("application/sdp"));
    if dialog
        .accept(Some(vec![sdp]), Some(answer.into_bytes()))
        .is_err()
    {
        // The caller cancelled before we answered.
        counters.inc(&counters.cancelled_before_answer);
        return Answered::Gone;
    }

    let clock = &*shared.clock;
    let wait = clock.sleep(shared.config.ack_timeout);
    tokio::pin!(wait);
    let shutdown = shared.cancel.cancelled();
    tokio::pin!(shutdown);
    loop {
        let state = tokio::select! {
            biased;
            () = &mut shutdown => {
                hang_up(shared, dialog).await;
                return Answered::Gone;
            }
            () = &mut wait => {
                counters.inc(&counters.ack_timeouts);
                tracing::warn!("sip: no ACK for our 200, giving up on the call");
                hang_up(shared, dialog).await;
                return Answered::Gone;
            }
            state = states.recv() => state,
        };
        match state {
            Some(DialogState::Confirmed(..)) => {
                return Answered::Confirmed {
                    socket,
                    local_rtp,
                    session_id,
                };
            }
            Some(DialogState::Terminated(_, reason)) => {
                if matches!(reason, TerminatedReason::UacCancel) {
                    counters.inc(&counters.cancelled_before_answer);
                }
                return Answered::Gone;
            }
            Some(_) => {}
            None => return Answered::Gone,
        }
    }
}

/// Rings for `ring_time`; `false` when the caller cancelled (or the server stopped) meanwhile.
async fn ring(
    shared: &Shared,
    states: &mut rsipstack::dialog::dialog::DialogStateReceiver,
) -> bool {
    let wait = shared.clock.sleep(shared.config.ring_time);
    tokio::pin!(wait);
    loop {
        tokio::select! {
            biased;
            () = shared.cancel.cancelled() => return false,
            () = &mut wait => return true,
            state = states.recv() => match state {
                Some(DialogState::Terminated(..)) | None => return false,
                Some(_) => {}
            },
        }
    }
}

/// BYE (or CANCEL), bounded; failures are not interesting.
async fn hang_up(shared: &Shared, dialog: &InviteDialog) {
    let limit = shared.config.hangup_timeout;
    let _ = shared_timeout(&*shared.clock, limit, dialog.hangup()).await;
}
