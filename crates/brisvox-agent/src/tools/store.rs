//! The per-session idempotency store of the tool engine (TOOLS.md item 6).
//!
//! Each SideEffect call is recorded under its [`IdempotencyKey`] while it runs and after it
//! settles. The store is bounded twice: by entry count and by the bytes of the recorded results.
//! Eviction removes the oldest entry that is not in flight, so a running call is never evicted and
//! its duplicate can never start while it runs.

use std::collections::{HashMap, VecDeque};

use crate::context::Message;

use super::IdempotencyKey;

/// The state of one recorded SideEffect call.
#[derive(Debug, Clone)]
pub(super) enum Entry {
    /// Its tool is running; its result is not known yet.
    InFlight,
    /// It settled; `message` is the result given to the LLM and `bytes` its size in the store.
    Done { message: Message, bytes: usize },
    /// Its task ended without recording a result (the tool panicked or its task was aborted), so
    /// whether the effect happened is unknown. It is never run again under the same key.
    Lost,
}

/// Outcome of trying to start a SideEffect call.
#[derive(Debug, Clone)]
pub(super) enum Begin {
    /// Recorded as in flight; the caller runs the call.
    Started,
    /// Already settled: the recorded result, and the call is not run again.
    Recorded(Message),
    /// Already running: a duplicate submitted while the first run is in flight.
    InFlight,
    /// Ended without a result earlier: the outcome is unknown and the call is not run again.
    Lost,
    /// Every entry is in flight, so none can be evicted to make room.
    Full,
}

/// Bounded record of one session's SideEffect calls.
#[derive(Debug)]
pub(super) struct Store {
    capacity: usize,
    max_bytes: usize,
    bytes: usize,
    entries: HashMap<IdempotencyKey, Entry>,
    /// Insertion order, for eviction. Holds exactly the keys of `entries`.
    order: VecDeque<IdempotencyKey>,
}

impl Store {
    /// A store of at most `capacity` entries (at least 1) and `max_bytes` recorded result bytes.
    pub(super) fn new(capacity: usize, max_bytes: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            max_bytes,
            bytes: 0,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Records `key` as in flight, unless it is already recorded, evicting the oldest settled
    /// entry when the store is full.
    pub(super) fn begin(&mut self, key: &IdempotencyKey) -> Begin {
        match self.entries.get(key) {
            Some(Entry::Done { message, .. }) => return Begin::Recorded(message.clone()),
            Some(Entry::InFlight) => return Begin::InFlight,
            Some(Entry::Lost) => return Begin::Lost,
            None => {}
        }
        while self.entries.len() >= self.capacity {
            if !self.evict_settled() {
                return Begin::Full;
            }
        }
        self.entries.insert(key.clone(), Entry::InFlight);
        self.order.push_back(key.clone());
        Begin::Started
    }

    /// The entry of `key`, if it is still recorded.
    pub(super) fn get(&self, key: &IdempotencyKey) -> Option<&Entry> {
        self.entries.get(key)
    }

    /// Records the settled result of an in-flight `key`, then evicts the oldest settled entries
    /// while the recorded bytes exceed the budget.
    pub(super) fn finish(&mut self, key: &IdempotencyKey, message: Message) {
        if !self.entries.contains_key(key) {
            return;
        }
        let bytes = message.text().len();
        self.entries
            .insert(key.clone(), Entry::Done { message, bytes });
        self.bytes = self.bytes.saturating_add(bytes);
        while self.bytes > self.max_bytes && self.evict_settled() {}
    }

    /// Records that the task of an in-flight `key` ended without a result.
    pub(super) fn lost(&mut self, key: &IdempotencyKey) {
        if self.entries.contains_key(key) {
            self.entries.insert(key.clone(), Entry::Lost);
        }
    }

    /// Number of recorded entries (in flight or settled).
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Bytes of the recorded results.
    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }

    /// Removes the oldest entry that is not in flight. Returns whether one existed.
    fn evict_settled(&mut self) -> bool {
        let position = self
            .order
            .iter()
            .position(|key| !matches!(self.entries.get(key), Some(Entry::InFlight)));
        let Some(at) = position else {
            return false;
        };
        let Some(key) = self.order.remove(at) else {
            return false;
        };
        if let Some(Entry::Done { bytes, .. }) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(bytes);
        }
        true
    }
}
