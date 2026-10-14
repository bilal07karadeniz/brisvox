//! [`CallRatesCell`]: the call's audio rates, shared by the pump (which learns them) and the
//! stages that need them (the TTS stage resamples to the output rate).
//!
//! The transport reports the rates once, in its `Started` event, which the pump reads after the
//! pipeline has started; the stages are built before that. The cell is the hand-over: the stage
//! reads [`CallRatesCell::get`] when it needs a rate (the configured fallback until `Started`
//! was read, and for transports that report none), the pump publishes with
//! [`CallRatesCell::publish`]. It is a `tokio::sync::watch` underneath, so a reader that must
//! wait for the real rates can ([`CallRatesCell::subscribe`]); reads never block.

use std::sync::Arc;

use brisvox_transport::CallRates;
use tokio::sync::watch;

/// The call's rates; cheap to clone, clones share the value. See the [module docs](self).
#[derive(Debug, Clone)]
pub struct CallRatesCell {
    tx: Arc<watch::Sender<Option<CallRates>>>,
    fallback: CallRates,
}

impl CallRatesCell {
    /// A cell that reads as `fallback` until rates are published.
    pub fn new(fallback: CallRates) -> Self {
        Self {
            tx: Arc::new(watch::channel(None).0),
            fallback,
        }
    }

    /// The call's rates: what the transport reported, or the fallback before that and when it
    /// reports none.
    pub fn get(&self) -> CallRates {
        (*self.tx.borrow()).unwrap_or(self.fallback)
    }

    /// The rates the transport reported, `None` before `Started` was read.
    pub fn reported(&self) -> Option<CallRates> {
        *self.tx.borrow()
    }

    /// Publishes the rates the transport reported (the pump calls this).
    pub fn publish(&self, rates: CallRates) {
        self.tx.send_replace(Some(rates));
    }

    /// A receiver that sees every published value (and the current one), for a task that waits
    /// for the real rates.
    pub fn subscribe(&self) -> watch::Receiver<Option<CallRates>> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use brisvox_core::frame::SampleRate;

    use super::*;

    #[test]
    fn reads_the_fallback_until_rates_are_published_and_clones_share_them() {
        let fallback = CallRates::symmetric(SampleRate::Hz16000);
        let cell = CallRatesCell::new(fallback);
        let clone = cell.clone();
        assert_eq!(cell.get(), fallback);
        assert_eq!(cell.reported(), None);
        let twilio = CallRates::symmetric(SampleRate::Hz8000);
        clone.publish(twilio);
        assert_eq!(cell.get(), twilio);
        assert_eq!(cell.reported(), Some(twilio));
        assert_eq!(*cell.subscribe().borrow(), Some(twilio));
    }
}
