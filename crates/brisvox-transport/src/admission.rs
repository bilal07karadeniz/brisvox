//! An admission gate a host puts in front of a transport's accept loop.
//!
//! A transport limits the connections it serves on its own (the websocket servers'
//! `max_calls`), but a process that hosts several transports admits calls through one
//! process-wide limit and drains them together (the server binary's session manager). The
//! transport cannot depend on that manager, so the host hands it an [`AdmissionGate`]: the
//! websocket-based servers (`WebSocketServer`, `TwilioServer`, `TelnyxServer`, `RtviServer`)
//! ask the gate right after they accept a TCP connection, before the upgrade:
//!
//! - **Refused** (a [`Refusal`]: busy, overloaded, provider budget or shutting down): the connection is answered
//!   with HTTP 503 and closed, no upgrade, no work spent on it.
//! - **Admitted**: the gate returns an [`AdmissionTicket`] (the host's reservation, opaque to the
//!   transport). The connection holds it through the upgrade and the protocol's start, and the
//!   transport puts it in the call's [`CallLink`](crate::CallLink), where the host takes it back
//!   with [`CallLink::take_ticket`](crate::CallLink::take_ticket) when it accepts the call. A
//!   connection that fails before it becomes a call drops the ticket, which gives the
//!   reservation back.
//!
//! The SIP server (`SipServer::bind_gated`, feature `sip`) asks the gate for every new inbound
//! INVITE before any other work: a refusal is a `503` (with `Retry-After` unless the host is
//! shutting down), and the ticket of an admitted INVITE travels in the call's link the same
//! way.
//!
//! Without a gate a transport admits on its own limits only, as before.

use std::any::Any;
use std::fmt;
use std::sync::Arc;

/// Why a gate refused a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The host is at its call limit; trying again later may work (HTTP 503 with
    /// `Retry-After: 1`).
    Busy,
    /// The host's inference pool is under pressure: a call it took now would degrade
    /// the calls it has. HTTP 503 with a longer `Retry-After` (the pressure clears over a
    /// window of seconds).
    Overloaded,
    /// A provider the call would use is at its concurrency budget; trying again later
    /// may work. HTTP 503 with `Retry-After`.
    ProviderBudget,
    /// The host is draining for a shutdown and takes no new calls (HTTP 503 without
    /// `Retry-After`: the client should go to another instance).
    ShuttingDown,
}

impl Refusal {
    /// Seconds for the `Retry-After` header of a websocket-based transport's 503; `None` when
    /// retrying this instance is pointless (it is shutting down).
    pub const fn retry_after_secs(self) -> Option<u32> {
        match self {
            Self::Busy => Some(1),
            Self::ProviderBudget => Some(2),
            Self::Overloaded => Some(5),
            Self::ShuttingDown => None,
        }
    }

    /// A short reason for logs and the refusal's body.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Overloaded => "server overloaded",
            Self::ProviderBudget => "provider capacity reached",
            Self::Busy => "server busy",
            Self::ShuttingDown => "server shutting down",
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The host's reservation for one admitted call, opaque to the transport. Dropping it gives the
/// reservation back (whatever the host's value does when dropped).
pub struct AdmissionTicket(Box<dyn Any + Send>);

impl AdmissionTicket {
    /// Wraps the host's reservation.
    pub fn new<T: Any + Send>(reservation: T) -> Self {
        Self(Box::new(reservation))
    }

    /// The host's reservation back, or the ticket itself if it holds another type.
    ///
    /// # Errors
    /// The unchanged ticket when it does not hold a `T`.
    pub fn downcast<T: Any>(self) -> Result<T, Self> {
        self.0.downcast::<T>().map(|b| *b).map_err(Self)
    }
}

impl fmt::Debug for AdmissionTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AdmissionTicket")
    }
}

/// Decides whether a transport takes a new call; see the [module docs](self).
///
/// Called on the transport's accept task for every new connection: it must not block or wait
/// (take a short lock at most).
pub trait AdmissionGate: Send + Sync + 'static {
    /// Reserves room for one call, or says why there is none.
    ///
    /// # Errors
    /// The [`Refusal`] the transport answers the connection with.
    fn try_admit(&self) -> Result<AdmissionTicket, Refusal>;
}

/// A gate shared by the transports of one host.
pub type SharedGate = Arc<dyn AdmissionGate>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ticket_gives_back_what_it_holds_and_only_that() {
        let ticket = AdmissionTicket::new(7_u32);
        let ticket = ticket.downcast::<String>().unwrap_err();
        assert_eq!(ticket.downcast::<u32>().unwrap(), 7);
        assert_eq!(format!("{:?}", AdmissionTicket::new(())), "AdmissionTicket");
        assert_eq!(Refusal::Busy.to_string(), "server busy");
        assert_eq!(Refusal::ShuttingDown.as_str(), "server shutting down");
        assert_eq!(Refusal::Overloaded.as_str(), "server overloaded");
        assert_eq!(
            Refusal::ProviderBudget.as_str(),
            "provider capacity reached"
        );
        assert_eq!(Refusal::Busy.retry_after_secs(), Some(1));
        assert_eq!(Refusal::Overloaded.retry_after_secs(), Some(5));
        assert_eq!(Refusal::ProviderBudget.retry_after_secs(), Some(2));
        assert_eq!(Refusal::ShuttingDown.retry_after_secs(), None);
    }
}
