//! The wire side of one call: the channels between a protocol adapter (websocket, Twilio, the
//! loopback) and the transport core.
//!
//! The core never touches a socket. An adapter owns the [`CallEnds`] of a call: it reports what
//! the client sent as [`WireEvent`]s and plays back what the core sends as [`WireOut`]s. The
//! core holds the matching [`CallLink`]. Both directions are bounded: input audio is dropped
//! oldest-first when the core falls behind, everything else waits.

use std::time::Duration;

use brisvox_core::TurnEpoch;
use brisvox_core::frame::{AudioFrame, DtmfDigit};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::admission::AdmissionTicket;
use crate::channel::{self, Overflow, Overflowing};
use crate::event::{AUDIO_CLASS, CallInfo};

/// Capacity of the adapter-to-core queue (frames of input audio, plus events).
pub const WIRE_EVENTS_CAPACITY: usize = 64;

/// Capacity of the core-to-adapter queue. Kept small on purpose: every queued frame is latency
/// the caller hears (see `docs/ARCHITECTURE.md`, Transport).
pub const WIRE_OUT_CAPACITY: usize = 8;

/// What a protocol adapter reports about a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireEvent {
    /// The call started. Sent once, before anything else.
    Started(CallInfo),
    /// Caller audio. The frame's epoch is ignored: the core re-tags it with its current epoch.
    Audio(AudioFrame),
    /// The caller pressed a DTMF key.
    Dtmf {
        /// The key.
        digit: DtmfDigit,
        /// When the adapter received it.
        at: Instant,
    },
    /// The client echoed a mark the core sent. An echo of a mark the core no longer waits for (cut
    /// by a clear, given up, repeated, or never sent) is ignored.
    MarkAck {
        /// The mark id.
        id: String,
    },
    /// How far the client is behind what was sent to it: audio sent but not yet played, as the
    /// client reports it (Twilio: the distance between the last echoed mark and the end of the
    /// audio sent). A level, not a counter: a new report replaces the queued one. The core
    /// forwards it as [`TransportEvent::PlaybackLead`](crate::TransportEvent::PlaybackLead).
    Lead {
        /// Audio the client holds that the caller has not heard yet.
        lead: Duration,
    },
    /// A non-fatal protocol fault.
    Fault {
        /// Rendered description.
        message: String,
    },
    /// The client hung up.
    Hangup,
}

/// What the core asks an adapter to play or do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireOut {
    /// Audio to play to the client, in order.
    Audio(AudioFrame),
    /// Barge-in: drop everything buffered for epochs before `epoch`, including audio already
    /// queued in the adapter, and tell the client to clear its own buffers. Sent in order with the
    /// audio, so audio sent before it is cleared too. Marks the adapter still buffers are dropped
    /// with the audio; the core already reports every unechoed mark as cut and ignores their
    /// echoes, so an adapter may drop or echo them (a protocol that echoes them on a clear, like
    /// Twilio's, needs no special handling).
    Clear {
        /// The epoch now current.
        epoch: TurnEpoch,
    },
    /// Ask the client to echo this mark once the audio ahead of it has played.
    Mark {
        /// The mark id.
        id: String,
    },
    /// The session ended: hang up the client.
    Hangup,
}

impl Overflowing for WireEvent {
    fn overflow(&self) -> Overflow {
        match self {
            Self::Audio(_) => Overflow::DropOldest,
            Self::Lead { .. } => Overflow::Displace,
            _ => Overflow::Backpressure,
        }
    }

    fn class(&self) -> u8 {
        match self {
            Self::Audio(_) => AUDIO_CLASS,
            _ => 0,
        }
    }

    fn supersedes(&self, queued: &Self) -> bool {
        matches!((self, queued), (Self::Lead { .. }, Self::Lead { .. }))
    }
}

impl Overflowing for WireOut {
    fn overflow(&self) -> Overflow {
        Overflow::Backpressure
    }
}

/// The core's end of one call. Hand it to [`TransportSession::open`](crate::TransportSession::open).
pub struct CallLink {
    pub(crate) events: channel::Receiver<WireEvent>,
    pub(crate) outgoing: channel::Sender<WireOut>,
    pub(crate) ended: CancellationToken,
    /// The host's reservation, when the transport admitted the call through an
    /// [`AdmissionGate`](crate::admission::AdmissionGate).
    pub(crate) ticket: Option<AdmissionTicket>,
    /// The transport that carried the call when it differs from its listener's (the WebRTC
    /// listener's WebSocket fallback says `webrtc-ws`); `None` otherwise.
    pub(crate) transport: Option<&'static str>,
    /// The transport's call id (the `call_id` of its `started` message), for logs that tie a
    /// session to the call the client sees; `None` when the transport does not label it.
    pub(crate) call_id: Option<String>,
}

impl CallLink {
    /// Takes the [`AdmissionTicket`] the transport's gate issued for this call (`None` without a
    /// gate, or once taken). A ticket still in the link when it is opened or dropped is dropped
    /// with it, which gives the reservation back: a host takes it before it starts the call.
    pub fn take_ticket(&mut self) -> Option<AdmissionTicket> {
        self.ticket.take()
    }

    /// Puts a ticket in the link (the transport, right before it hands the call over).
    #[cfg_attr(not(any(feature = "websocket", feature = "sip")), allow(dead_code))]
    pub(crate) fn set_ticket(&mut self, ticket: Option<AdmissionTicket>) {
        self.ticket = ticket;
    }

    /// The transport that carried the call, when it is not the listener's own (see the field).
    pub fn transport(&self) -> Option<&'static str> {
        self.transport
    }

    /// The transport's id of the call, when the adapter labelled it (the websocket adapter does).
    pub fn call_id(&self) -> Option<&str> {
        self.call_id.as_deref()
    }

    /// Labels the call with the transport's id (the adapter, before it hands the call over).
    #[cfg_attr(not(feature = "websocket"), allow(dead_code))]
    pub(crate) fn set_call_id(&mut self, call_id: &str) {
        self.call_id = Some(call_id.to_owned());
    }

    /// Labels the transport that carried the call (the adapter, before it hands the call over).
    #[cfg_attr(not(feature = "websocket"), allow(dead_code))]
    pub(crate) fn set_transport(&mut self, transport: &'static str) {
        self.transport = Some(transport);
    }
}

/// The adapter's end of one call.
pub struct CallEnds {
    /// Reports what the client sent.
    pub events: channel::Sender<WireEvent>,
    /// What the core asked the client to hear.
    pub outgoing: channel::Receiver<WireOut>,
    /// Cancelled once the core has ended the call (after it queued `WireOut::Hangup`). An adapter
    /// that is blocked on its socket or its client must stop waiting, release what it holds and
    /// return: the core no longer reads from `events` or writes to `outgoing`.
    pub ended: CancellationToken,
}

/// Creates both ends of a call's wire.
pub fn call_pair() -> (CallLink, CallEnds) {
    let (events_tx, events_rx) = channel::bounded(WIRE_EVENTS_CAPACITY);
    let (outgoing_tx, outgoing_rx) = channel::bounded(WIRE_OUT_CAPACITY);
    let ended = CancellationToken::new();
    (
        CallLink {
            events: events_rx,
            outgoing: outgoing_tx,
            ended: ended.clone(),
            ticket: None,
            transport: None,
            call_id: None,
        },
        CallEnds {
            events: events_tx,
            outgoing: outgoing_rx,
            ended,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_new_lead_replaces_a_queued_one_and_never_waits() {
        // Capacity 2 with a lossless event queued: leads must neither pile up nor make the
        // adapter's reader wait, which would end the call as "engine not taking input".
        let (tx, mut rx) = channel::bounded::<WireEvent>(2);
        tx.send(WireEvent::Fault {
            message: "x".to_owned(),
        })
        .await
        .unwrap();
        let sends = async {
            for ms in 0..10u64 {
                tx.send(WireEvent::Lead {
                    lead: Duration::from_millis(ms),
                })
                .await
                .unwrap();
            }
        };
        tokio::time::timeout(Duration::from_secs(1), sends)
            .await
            .expect("a lead report waited for space");
        assert_eq!(tx.stats().dropped_oldest, 9);
        assert!(matches!(rx.recv().await, Some(WireEvent::Fault { .. })));
        assert_eq!(
            rx.recv().await,
            Some(WireEvent::Lead {
                lead: Duration::from_millis(9)
            })
        );
    }
}
