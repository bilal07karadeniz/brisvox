//! Pieces shared by the streaming mocks (STT, TTS): a deadline-ordered outbox and stream faults.

use std::collections::VecDeque;
use std::time::Duration;

use brisvox_providers::Error;
use tokio::time::Instant;

/// Something going wrong with a streaming session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamFault {
    /// `connect` fails with `Error::Rejected { status }`.
    ConnectError {
        /// Status, e.g. 401 or 503.
        status: u16,
    },
    /// The connect takes `duration` longer. With an adapter deadline shorter than that, the
    /// connect fails with `Error::Core(Timeout)` at the deadline.
    ConnectStall {
        /// Extra delay of the connect.
        duration: Duration,
    },
    /// Right after the `after_events`-th event (0: before the first) the connection drops:
    /// `Error::Disconnected`, then the event channel closes. Acknowledgements such as TTS
    /// `Cancelled` do not count as events.
    Disconnect {
        /// Events delivered before the drop.
        after_events: usize,
    },
    /// Event number `before_event` is delayed by `duration` (the stream stalls; the session
    /// keeps accepting input and cancels meanwhile).
    Stall {
        /// Index of the delayed event.
        before_event: usize,
        /// Extra delay.
        duration: Duration,
    },
    /// After `after_events` events the provider sends something unparsable: `Error::Protocol`,
    /// then the event channel closes.
    ProtocolError {
        /// Events delivered before the violation.
        after_events: usize,
    },
}

/// Faults of one session, applied by event index.
#[derive(Debug, Clone, Default)]
pub(crate) struct FaultPlan {
    faults: Vec<StreamFault>,
    sent: usize,
}

impl FaultPlan {
    pub(crate) fn new(faults: Vec<StreamFault>) -> Self {
        Self { faults, sent: 0 }
    }

    pub(crate) fn connect_error(&self) -> Option<u16> {
        self.faults.iter().find_map(|f| match f {
            StreamFault::ConnectError { status } => Some(*status),
            _ => None,
        })
    }

    /// Extra delay of the connect, summed over the connect stalls of this session.
    pub(crate) fn connect_stall(&self) -> Duration {
        self.faults
            .iter()
            .filter_map(|f| match f {
                StreamFault::ConnectStall { duration } => Some(*duration),
                _ => None,
            })
            .sum()
    }

    /// Extra delay before the next event.
    pub(crate) fn stall(&self) -> Duration {
        self.faults
            .iter()
            .filter_map(|f| match f {
                StreamFault::Stall {
                    before_event,
                    duration,
                } if *before_event == self.sent => Some(*duration),
                _ => None,
            })
            .sum()
    }

    /// The error to send instead of the next event, if the connection drops now.
    pub(crate) fn cut(&self, provider: &'static str) -> Option<Error> {
        self.faults.iter().find_map(|f| match f {
            StreamFault::Disconnect { after_events } if *after_events == self.sent => {
                Some(Error::Disconnected { provider })
            }
            StreamFault::ProtocolError { after_events } if *after_events == self.sent => {
                Some(Error::Protocol {
                    provider,
                    detail: "mock: unparsable event".to_owned(),
                })
            }
            _ => None,
        })
    }

    pub(crate) fn delivered(&mut self) {
        self.sent = self.sent.saturating_add(1);
    }
}

/// Events waiting for their delivery time, kept in deadline order (ties: insertion order).
#[derive(Debug)]
pub(crate) struct Outbox<E> {
    queue: VecDeque<(Instant, E)>,
}

impl<E> Default for Outbox<E> {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }
}

impl<E> Outbox<E> {
    pub(crate) fn push(&mut self, at: Instant, event: E) {
        let index = self.queue.partition_point(|(t, _)| *t <= at);
        self.queue.insert(index, (at, event));
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.queue.front().map(|(t, _)| *t)
    }

    pub(crate) fn pop(&mut self) -> Option<E> {
        self.queue.pop_front().map(|(_, e)| e)
    }

    /// Drops every queued event matching `predicate`.
    pub(crate) fn remove_where(&mut self, mut predicate: impl FnMut(&E) -> bool) {
        self.queue.retain(|(_, e)| !predicate(e));
    }
}
