//! Which calls this server is streaming or has streamed: a second connection for the same
//! `call_control_id` is refused.
//!
//! Telnyx may reconnect a TeXML stream whose WebSocket dropped (`enableReconnect`, default `true`)
//! and deliver a fresh `connected` / `start` for the same call. The engine's session for the call
//! ended with the first connection, so a second connection could only be an orphan: it is refused
//! (the stream then stays down) instead of being handed to the engine as a new call. The ended
//! calls are remembered in a bounded list, oldest forgotten first. A call that was never handed to
//! the engine (the accept queue was full, the server stopped, the format was refused) is released
//! without being remembered, so Telnyx's retry for it is not refused.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::time::Instant;

/// Ended calls remembered.
pub(crate) const REMEMBERED_ENDED: usize = 1024;

#[derive(Debug, Default)]
struct Inner {
    live: HashMap<String, Arc<StopMark>>,
    ended: VecDeque<String>,
    ended_set: HashSet<String>,
}

/// "The host is stopping this call's stream on purpose" (`streaming_stop`), valid until a
/// deadline. Shared by the registry (which the host reaches through
/// [`StreamStopper`](super::StreamStopper)) and the connection that serves the call.
#[derive(Debug, Default)]
pub(crate) struct StopMark(Mutex<Option<Instant>>);

impl StopMark {
    fn lock(&self) -> MutexGuard<'_, Option<Instant>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records that a stop is expected until `until`.
    fn expect_until(&self, until: Instant) {
        *self.lock() = Some(until);
    }

    /// Whether a stop is expected at `now`.
    pub(crate) fn expected(&self, now: Instant) -> bool {
        self.lock().is_some_and(|until| now <= until)
    }
}

/// The registry; cheap to share behind an `Arc`.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    inner: Mutex<Inner>,
}

impl Registry {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Claims `key`. `None` if the call is streaming or has streamed. The claim is held by the
    /// returned guard; dropping it moves the call to the ended list, but only after
    /// [`Claim::served`]: a claim dropped before that never reached the engine and is forgotten.
    pub(crate) fn claim(self: &Arc<Self>, key: &str) -> Option<Claim> {
        let mut inner = self.lock();
        if inner.live.contains_key(key) || inner.ended_set.contains(key) {
            return None;
        }
        let stop = Arc::new(StopMark::default());
        inner.live.insert(key.to_owned(), Arc::clone(&stop));
        Some(Claim {
            registry: Arc::clone(self),
            key: key.to_owned(),
            served: false,
            stop,
        })
    }

    /// Records that the host is about to stop the stream of the live call `key`, until `until`.
    /// `false` when no such call is streaming.
    pub(crate) fn expect_stop(&self, key: &str, until: Instant) -> bool {
        let mark = self.lock().live.get(key).cloned();
        mark.is_some_and(|mark| {
            mark.expect_until(until);
            true
        })
    }

    fn release(&self, key: &str, served: bool) {
        let mut inner = self.lock();
        if inner.live.remove(key).is_none() || !served {
            return;
        }
        if inner.ended.len() >= REMEMBERED_ENDED
            && let Some(oldest) = inner.ended.pop_front()
        {
            inner.ended_set.remove(&oldest);
        }
        inner.ended.push_back(key.to_owned());
        inner.ended_set.insert(key.to_owned());
    }

    /// Calls streaming right now.
    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.lock().live.len()
    }
}

/// A call's claim on its key.
#[derive(Debug)]
pub(crate) struct Claim {
    registry: Arc<Registry>,
    key: String,
    /// Whether the call was handed to the engine.
    served: bool,
    /// Whether the host announced a stop of the stream.
    stop: Arc<StopMark>,
}

impl Claim {
    /// The call was handed to the engine: from now on its key is remembered as ended when the
    /// claim is dropped.
    pub(crate) fn served(&mut self) {
        self.served = true;
    }

    /// The host's announcement of a stream stop for this call.
    pub(crate) fn stop_mark(&self) -> Arc<StopMark> {
        Arc::clone(&self.stop)
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.registry.release(&self.key, self.served);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_claimed_once_while_live_and_after() {
        let registry = Arc::new(Registry::default());
        let mut first = registry.claim("call-1").unwrap();
        first.served();
        assert_eq!(registry.live(), 1);
        assert!(registry.claim("call-1").is_none(), "live");
        assert!(registry.claim("call-2").is_some(), "another call is fine");
        drop(first);
        assert!(
            registry.claim("call-1").is_none(),
            "ended calls stay refused"
        );
    }

    #[test]
    fn the_ended_list_is_bounded_and_forgets_the_oldest() {
        let registry = Arc::new(Registry::default());
        for i in 0..REMEMBERED_ENDED + 5 {
            let mut claim = registry.claim(&format!("c{i}")).unwrap();
            claim.served();
            drop(claim);
        }
        let inner = registry.lock();
        assert_eq!(inner.ended.len(), REMEMBERED_ENDED);
        assert_eq!(inner.ended_set.len(), REMEMBERED_ENDED);
        drop(inner);
        // The oldest was forgotten, the newest is remembered.
        assert!(registry.claim("c0").is_some());
        assert!(
            registry
                .claim(&format!("c{}", REMEMBERED_ENDED + 4))
                .is_none()
        );
    }

    #[test]
    fn a_stop_is_expected_only_until_its_deadline_and_only_for_live_calls() {
        let registry = Arc::new(Registry::default());
        let claim = registry.claim("call-1").unwrap();
        let mark = claim.stop_mark();
        let now = brisvox_core::SystemClock::shared().now();
        assert!(!mark.expected(now), "nothing announced");
        assert!(!registry.expect_stop("call-2", now), "unknown call");
        assert!(registry.expect_stop("call-1", now + std::time::Duration::from_secs(5)));
        assert!(mark.expected(now));
        assert!(!mark.expected(now + std::time::Duration::from_secs(6)));
        drop(claim);
        assert!(!registry.expect_stop("call-1", now), "ended call");
    }

    #[test]
    fn a_claim_dropped_before_the_hand_over_is_forgotten_not_remembered() {
        let registry = Arc::new(Registry::default());
        drop(registry.claim("call-1").unwrap());
        assert_eq!(registry.live(), 0);
        assert!(registry.claim("call-1").is_some(), "the retry is welcome");
    }
}
