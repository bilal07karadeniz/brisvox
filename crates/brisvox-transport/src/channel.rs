//! Bounded channels whose overflow policy is decided per item (the transport's queues).
//!
//! Every item says what a full channel does with it ([`Overflow`]): the sender waits, the oldest
//! queued item of the same class is evicted, or the new item is dropped. Losses are counted in
//! [`ChannelStats`]. Buffers are allocated once, at construction. The lock is a `std::sync::Mutex`
//! held only for queue operations, never across an `.await`; waiting uses `tokio::sync::Notify`.
//!
//! Only [`Sender::send`] needs the item type to say its policy ([`Overflowing`]); receiving and
//! cloning work for any item type.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use brisvox_core::Error as CoreError;
use tokio::sync::Notify;

use crate::error::Result;

/// What a full channel does with an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Overflow {
    /// The sender waits for space. Nothing is lost and order is kept.
    Backpressure,
    /// The oldest queued item of the same [`Overflowing::class`] is evicted and the new item is
    /// queued. If no item of that class is queued, the new item is dropped instead.
    DropOldest,
    /// The new item is dropped.
    DropNewest,
    /// For items that supersede earlier ones (cumulative reports) or must get through (the final
    /// event). Queued items the new one supersedes ([`Overflowing::supersedes`]) are removed
    /// first, full or not, so the queue holds at most the latest of each kind and order is kept.
    /// If the channel is still full, the oldest queued item whose own policy is
    /// [`Overflow::DropOldest`] is evicted; if there is none, the sender waits for space (every
    /// queued item is lossless).
    Displace,
}

/// Items declare their overflow policy, so one channel can carry lossless control items next to
/// droppable real-time data.
pub trait Overflowing {
    /// The policy applied when the channel is full.
    fn overflow(&self) -> Overflow;

    /// Eviction class for [`Overflow::DropOldest`]: only items of the same class evict each other.
    fn class(&self) -> u8 {
        0
    }

    /// Whether this item makes `queued` redundant, for [`Overflow::Displace`]. Default: never.
    fn supersedes(&self, queued: &Self) -> bool {
        let _ = queued;
        false
    }
}

/// Items lost to overflow policies, per channel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChannelStats {
    /// Queued items evicted by [`Overflow::DropOldest`], or by [`Overflow::Displace`] (replaced
    /// or evicted).
    pub dropped_oldest: u64,
    /// New items dropped: [`Overflow::DropNewest`], or `DropOldest` with nothing of its class
    /// queued.
    pub dropped_newest: u64,
}

struct State<T> {
    queue: VecDeque<T>,
    senders: usize,
    receiver_alive: bool,
    stats: ChannelStats,
}

struct Shared<T> {
    state: Mutex<State<T>>,
    capacity: usize,
    /// Wakes the receiver: an item was queued, or the last sender left.
    readable: Notify,
    /// Wakes waiting senders: space was freed, or the receiver left.
    space: Notify,
}

impl<T> Shared<T> {
    /// Critical sections never panic, so a poisoned lock still guards consistent data.
    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Outcome of one attempt to queue an item.
enum Offer<T> {
    /// Queued, or dropped by a lossy policy (the sender has nothing more to do).
    Done,
    /// Full and the policy is backpressure: the item comes back to wait for space.
    Full(T),
    /// The receiver is gone.
    Closed,
}

/// Sending half. Cheap to clone; the receiver sees the channel end once every clone is dropped.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

/// Receiving half. Not cloneable: one consumer per channel.
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
}

/// Creates a channel holding at most `capacity` items (at least one).
pub fn bounded<T: Overflowing>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let capacity = capacity.max(1);
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            queue: VecDeque::with_capacity(capacity),
            senders: 1,
            receiver_alive: true,
            stats: ChannelStats::default(),
        }),
        capacity,
        readable: Notify::new(),
        space: Notify::new(),
    });
    (
        Sender {
            shared: Arc::clone(&shared),
        },
        Receiver { shared },
    )
}

impl<T: Overflowing> Sender<T> {
    /// Queues `item` under its overflow policy. Only a [`Overflow::Backpressure`] item can wait
    /// (for space); the lossy policies return at once.
    ///
    /// Cancel-safe: dropping the future while it waits leaves the channel unchanged.
    ///
    /// # Errors
    /// [`CoreError::ChannelClosed`] if the receiver was dropped.
    pub async fn send(&self, item: T) -> Result<()> {
        let mut pending = item;
        loop {
            // Register before the check, so a receiver freeing space in between is not missed.
            let space = self.shared.space.notified();
            tokio::pin!(space);
            space.as_mut().enable();
            match self.offer(pending) {
                Offer::Done => return Ok(()),
                Offer::Closed => return Err(CoreError::ChannelClosed("transport channel").into()),
                Offer::Full(back) => {
                    pending = back;
                    space.await;
                }
            }
        }
    }

    fn offer(&self, item: T) -> Offer<T> {
        let mut state = self.shared.lock();
        if !state.receiver_alive {
            return Offer::Closed;
        }
        if item.overflow() == Overflow::Displace {
            // Cumulative items replace what they supersede, whether or not the channel is full.
            let before = state.queue.len();
            state.queue.retain(|queued| !item.supersedes(queued));
            let removed = before.saturating_sub(state.queue.len());
            state.stats.dropped_oldest = state
                .stats
                .dropped_oldest
                .saturating_add(u64::try_from(removed).unwrap_or(u64::MAX));
        }
        if state.queue.len() < self.shared.capacity {
            state.queue.push_back(item);
            drop(state);
            self.shared.readable.notify_waiters();
            return Offer::Done;
        }
        match item.overflow() {
            Overflow::Backpressure => Offer::Full(item),
            Overflow::DropNewest => {
                state.stats.dropped_newest = state.stats.dropped_newest.saturating_add(1);
                Offer::Done
            }
            Overflow::Displace => {
                let evictable = state
                    .queue
                    .iter()
                    .position(|queued| queued.overflow() == Overflow::DropOldest);
                match evictable {
                    Some(index) => {
                        state.queue.remove(index);
                        state.stats.dropped_oldest = state.stats.dropped_oldest.saturating_add(1);
                        state.queue.push_back(item);
                        drop(state);
                        self.shared.readable.notify_waiters();
                        Offer::Done
                    }
                    None => Offer::Full(item),
                }
            }
            Overflow::DropOldest => {
                let class = item.class();
                match state
                    .queue
                    .iter()
                    .position(|queued| queued.class() == class)
                {
                    Some(index) => {
                        state.queue.remove(index);
                        state.stats.dropped_oldest = state.stats.dropped_oldest.saturating_add(1);
                        state.queue.push_back(item);
                        drop(state);
                        self.shared.readable.notify_waiters();
                    }
                    None => {
                        state.stats.dropped_newest = state.stats.dropped_newest.saturating_add(1);
                    }
                }
                Offer::Done
            }
        }
    }

    /// Items lost to overflow so far, on this channel.
    pub fn stats(&self) -> ChannelStats {
        self.shared.lock().stats
    }
}

impl<T> Sender<T> {
    /// Removes every queued item for which `discard` returns true, and returns how many. Used to
    /// pull stale items out of a queue ahead of a clear. Does not count as overflow loss.
    pub fn purge(&self, mut discard: impl FnMut(&T) -> bool) -> usize {
        let removed = {
            let mut state = self.shared.lock();
            let before = state.queue.len();
            state.queue.retain(|queued| !discard(queued));
            before.saturating_sub(state.queue.len())
        };
        if removed > 0 {
            self.shared.space.notify_waiters();
        }
        removed
    }
}

impl<T> Receiver<T> {
    /// The next item in queue order, waiting if the channel is empty. `None` once it is empty
    /// and every sender is gone.
    pub async fn recv(&mut self) -> Option<T> {
        loop {
            let readable = self.shared.readable.notified();
            tokio::pin!(readable);
            readable.as_mut().enable();
            {
                let mut state = self.shared.lock();
                if let Some(item) = state.queue.pop_front() {
                    drop(state);
                    self.shared.space.notify_waiters();
                    return Some(item);
                }
                if state.senders == 0 {
                    return None;
                }
            }
            readable.await;
        }
    }

    /// Items lost to overflow so far, on this channel.
    pub fn stats(&self) -> ChannelStats {
        self.shared.lock().stats
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        {
            let mut state = self.shared.lock();
            state.senders = state.senders.saturating_add(1);
        }
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let last = {
            let mut state = self.shared.lock();
            state.senders = state.senders.saturating_sub(1);
            state.senders == 0
        };
        if last {
            self.shared.readable.notify_waiters();
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let queued = {
            let mut state = self.shared.lock();
            state.receiver_alive = false;
            std::mem::take(&mut state.queue)
        };
        // Release the items outside the lock.
        drop(queued);
        self.shared.space.notify_waiters();
    }
}
