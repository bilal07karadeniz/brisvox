//! Which calls this server streams, and the sessions that wait between two streams of one call.
//!
//! Twilio's REST call control cannot change a call without replacing its TwiML, and replacing
//! the TwiML ends the `<Connect><Stream>`: sending DTMF (`<Play digits>`) and a warm transfer
//! both end the stream, and the call then comes back on a **new** stream when the new TwiML says
//! `<Connect><Stream>` again (`call_control::twilio::ResumeStream`, `custom.resume`). Without
//! this registry the old stream's end is a hang-up and the conversation is lost.
//!
//! The protocol between the host and the adapter (`StreamResumer::expect_resume`):
//! 1. The host announces, right before its REST update, that the stream of call `X` is about to
//!    end on purpose and for how long the engine's session should wait for the call to come
//!    back (`linger`).
//! 2. The old connection sees `stop`, a close frame or a vanished socket within
//!    [`EXPECTED_WITHIN`] of the announcement: it does **not** tell the engine the caller hung
//!    up. It keeps the engine's session (`CallEnds`) and parks it here; what the engine sends
//!    meanwhile is held for the new stream.
//! 3. A new connection of the same `callSid` whose `start` carries the `resume` parameter is
//!    handed to the parked session, which goes on with the new socket. The new stream may
//!    arrive before the old one finished closing: the registry keeps it until the old
//!    connection parks.
//! 4. A session nobody resumed within `linger` ends as a disconnect.
//!
//! A connection with the `resume` parameter and no announced call is refused: it could only be
//! an orphan, and answering it as a new call would start a second conversation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_core::SharedClock;
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::protocol::StartInfo;
use crate::telephony::Ws;

/// How long after the host's announcement the end of the stream counts as the host's doing (the
/// REST call, Twilio's processing and the close of the socket all fit in it).
pub const EXPECTED_WITHIN: Duration = Duration::from_secs(30);

/// Longest time a parked session waits for its call to come back. The announcement's `linger`
/// is cut to it.
pub const MAX_LINGER: Duration = Duration::from_secs(15 * 60);

/// The `start.customParameters` key that marks a stream as the continuation of a call.
pub const RESUME_PARAMETER: &str = "resume";

/// A new stream of a call whose session is parked (or about to be).
pub(crate) struct Handoff {
    pub(crate) ws: Ws,
    pub(crate) start: Box<StartInfo>,
}

#[derive(Default)]
struct SlotState {
    /// Announced: the stream's end is expected until this instant; the session then waits for
    /// the given time.
    expect: Option<(Instant, Duration)>,
    /// The old stream's end was taken as the host's doing and its session is about to park (the
    /// connection is still closing): a continuation that arrives now is held for it.
    ending: bool,
    /// A continuation that arrived before the old stream parked its session.
    pending: Option<Handoff>,
    /// The parked session waits for a continuation here.
    parked: Option<oneshot::Sender<Handoff>>,
}

/// One live call.
#[derive(Default)]
pub(crate) struct Slot {
    state: Mutex<SlotState>,
}

impl Slot {
    fn lock(&self) -> MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the end of the stream was announced and `now` is within the window. Consumes the
    /// announcement and returns how long the session should wait.
    pub(crate) fn take_expected(&self, now: Instant) -> Option<Duration> {
        let mut state = self.lock();
        match state.expect.take() {
            Some((until, linger)) if now <= until => {
                state.ending = true;
                Some(linger)
            }
            _ => None,
        }
    }

    /// Parks the session: a continuation that is already here, or a receiver for the one to
    /// come.
    pub(crate) fn park(&self) -> Parked {
        let mut state = self.lock();
        state.ending = false;
        if let Some(handoff) = state.pending.take() {
            return Parked::Ready(Box::new(handoff));
        }
        let (tx, rx) = oneshot::channel();
        state.parked = Some(tx);
        Parked::Wait(rx)
    }
}

/// What parking a session found.
pub(crate) enum Parked {
    /// The continuation had arrived already.
    Ready(Box<Handoff>),
    /// Wait for it.
    Wait(oneshot::Receiver<Handoff>),
}

#[derive(Default)]
pub(crate) struct Registry {
    live: Mutex<HashMap<String, Arc<Slot>>>,
}

impl Registry {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Arc<Slot>>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers the call `key`; `None` for an empty key or a call that is live already (that
    /// connection is not resumable).
    pub(crate) fn claim(self: &Arc<Self>, key: &str) -> Option<Claim> {
        if key.is_empty() {
            return None;
        }
        let mut live = self.lock();
        if live.contains_key(key) {
            return None;
        }
        let slot = Arc::new(Slot::default());
        live.insert(key.to_owned(), Arc::clone(&slot));
        Some(Claim {
            registry: Arc::clone(self),
            key: key.to_owned(),
            slot,
        })
    }

    /// Announces the end of the stream of `key`; whether the call is live.
    pub(crate) fn expect_resume(&self, key: &str, until: Instant, linger: Duration) -> bool {
        let slot = self.lock().get(key).cloned();
        slot.is_some_and(|slot| {
            slot.lock().expect = Some((until, linger.min(MAX_LINGER)));
            true
        })
    }

    /// Whether some call waits for a continuation: its stream's end is announced, taken as
    /// announced, or its session is parked.
    pub(crate) fn awaiting_resume(&self) -> bool {
        self.lock().values().any(|slot| {
            let state = slot.lock();
            state.expect.is_some() || state.ending || state.parked.is_some()
        })
    }

    /// Withdraws the announcement of `key` (the host's update was refused, so the stream does
    /// not end). Does nothing once the stream's end was taken as the host's doing. Whether an
    /// announcement was withdrawn.
    pub(crate) fn cancel_expect(&self, key: &str) -> bool {
        let slot = self.lock().get(key).cloned();
        slot.is_some_and(|slot| slot.lock().expect.take().is_some())
    }

    /// Hands a continuation to the call `key`: to its parked session, or (when the end of the
    /// old stream was announced and has not happened yet) to the slot until the session parks.
    /// Gives the handoff back when there is nothing to resume.
    pub(crate) fn resume(
        &self,
        key: &str,
        handoff: Handoff,
        now: Instant,
    ) -> Result<(), Box<Handoff>> {
        let Some(slot) = self.lock().get(key).cloned() else {
            return Err(Box::new(handoff));
        };
        let mut state = slot.lock();
        if let Some(parked) = state.parked.take() {
            return parked.send(handoff).map_err(Box::new);
        }
        let announced = state.ending || state.expect.is_some_and(|(until, _)| now <= until);
        if announced && state.pending.is_none() {
            state.pending = Some(handoff);
            return Ok(());
        }
        Err(Box::new(handoff))
    }

    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.lock().len()
    }
}

/// A call's entry; dropping it forgets the call (and any continuation that waits).
pub(crate) struct Claim {
    registry: Arc<Registry>,
    key: String,
    slot: Arc<Slot>,
}

impl Claim {
    pub(crate) fn slot(&self) -> Arc<Slot> {
        Arc::clone(&self.slot)
    }

    pub(crate) fn park(&self) -> Parked {
        self.slot.park()
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut live = self.registry.lock();
        if live
            .get(&self.key)
            .is_some_and(|slot| Arc::ptr_eq(slot, &self.slot))
        {
            live.remove(&self.key);
        }
    }
}

/// The host's way to announce that it is about to end a call's stream on purpose and expects the
/// call to come back on a new one (see the module docs). Cheap to clone; get it from
/// [`TwilioServer::stream_resumer`](super::TwilioServer::stream_resumer).
#[derive(Clone)]
pub struct StreamResumer {
    registry: Arc<Registry>,
    clock: SharedClock,
}

impl StreamResumer {
    pub(crate) fn new(registry: Arc<Registry>, clock: SharedClock) -> Self {
        Self { registry, clock }
    }

    /// Announces that the stream of the call `call_sid` ends within [`EXPECTED_WITHIN`] because
    /// the host replaces the call's TwiML, and that the call comes back on a new stream (its
    /// `<Parameter name="resume">`). Until then the engine's session of the call stays up and
    /// waits at most `linger` (cut to [`MAX_LINGER`]); what it produces meanwhile is held
    /// for the new stream. Returns whether this server streams the call right now.
    pub fn expect_resume(&self, call_sid: &str, linger: Duration) -> bool {
        let until = self.clock.now() + EXPECTED_WITHIN;
        self.registry.expect_resume(call_sid, until, linger)
    }

    /// Withdraws the announcement made by [`expect_resume`](Self::expect_resume) for the call
    /// `call_sid`: call it when the host's update was definitively refused, so the stream does
    /// not end after all. Without it a hang-up within [`EXPECTED_WITHIN`] would be read as the
    /// announced restart and the engine would not hear of it until the linger ran out. Does
    /// nothing when the stream's end was already taken as announced. Returns whether an
    /// announcement was withdrawn.
    pub fn cancel_expectation(&self, call_sid: &str) -> bool {
        self.registry.cancel_expect(call_sid)
    }
}

impl std::fmt::Debug for StreamResumer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StreamResumer(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_is_claimed_once_and_forgotten_on_drop() {
        let registry = Arc::new(Registry::default());
        assert!(registry.claim("").is_none(), "no call sid, no resume");
        let claim = registry.claim("CA1").unwrap();
        assert!(registry.claim("CA1").is_none(), "live");
        assert_eq!(registry.live(), 1);
        drop(claim);
        assert_eq!(registry.live(), 0);
        assert!(registry.claim("CA1").is_some(), "free again");
    }

    #[test]
    fn an_announcement_is_valid_once_and_only_in_its_window() {
        let registry = Arc::new(Registry::default());
        let claim = registry.claim("CA1").unwrap();
        let slot = claim.slot();
        let now = brisvox_core::SystemClock::shared().now();
        assert!(slot.take_expected(now).is_none(), "nothing announced");
        assert!(
            !registry.expect_resume("CA2", now, Duration::ZERO),
            "unknown call"
        );
        assert!(registry.expect_resume(
            "CA1",
            now + Duration::from_secs(5),
            Duration::from_secs(9)
        ));
        assert_eq!(slot.take_expected(now), Some(Duration::from_secs(9)));
        assert!(slot.take_expected(now).is_none(), "consumed");
        assert!(registry.expect_resume("CA1", now + Duration::from_secs(5), Duration::ZERO));
        assert!(
            slot.take_expected(now + Duration::from_secs(6)).is_none(),
            "expired"
        );
    }

    #[test]
    fn only_a_call_that_waits_for_a_stream_is_awaiting() {
        let registry = Arc::new(Registry::default());
        assert!(!registry.awaiting_resume(), "no call");
        let claim = registry.claim("CA1").unwrap();
        assert!(
            !registry.awaiting_resume(),
            "a live call that nobody announced"
        );
        let now = brisvox_core::SystemClock::shared().now();
        assert!(registry.expect_resume("CA1", now + Duration::from_secs(5), Duration::ZERO));
        assert!(registry.awaiting_resume(), "announced");
        assert!(registry.cancel_expect("CA1"));
        assert!(!registry.awaiting_resume(), "withdrawn");
        assert!(registry.expect_resume("CA1", now + Duration::from_secs(5), Duration::ZERO));
        assert!(claim.slot().take_expected(now).is_some());
        assert!(registry.awaiting_resume(), "ending");
        let _parked = claim.park();
        assert!(registry.awaiting_resume(), "parked");
    }

    #[test]
    fn a_withdrawn_announcement_is_gone() {
        let registry = Arc::new(Registry::default());
        let claim = registry.claim("CA1").unwrap();
        let now = brisvox_core::SystemClock::shared().now();
        assert!(!registry.cancel_expect("CA2"), "unknown call");
        assert!(!registry.cancel_expect("CA1"), "nothing announced");
        assert!(registry.expect_resume("CA1", now + Duration::from_secs(5), Duration::ZERO));
        assert!(registry.cancel_expect("CA1"));
        assert!(claim.slot().take_expected(now).is_none(), "withdrawn");
        // Once the end was taken as announced there is nothing left to withdraw.
        assert!(registry.expect_resume("CA1", now + Duration::from_secs(5), Duration::ZERO));
        assert!(claim.slot().take_expected(now).is_some());
        assert!(!registry.cancel_expect("CA1"));
    }

    #[test]
    fn the_linger_is_cut_to_the_maximum() {
        let registry = Arc::new(Registry::default());
        let claim = registry.claim("CA1").unwrap();
        let now = brisvox_core::SystemClock::shared().now();
        assert!(registry.expect_resume("CA1", now, Duration::from_secs(10_000)));
        assert_eq!(claim.slot().take_expected(now), Some(MAX_LINGER));
    }
}
