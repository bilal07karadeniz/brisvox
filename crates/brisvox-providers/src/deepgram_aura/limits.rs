//! Client-side accounting of the two server limits that are fatal or silently swallowing
//! (docs/providers/deepgram-aura.md, "Limits"): at most 20 `Flush` messages per 60 s (a flush
//! over the limit never gets a `Flushed`) and 2400 characters per minute (over it the server
//! closes the socket with 1008 `DATA-0001`, an inference, UNVERIFIED). Both are sliding windows
//! on the injected clock; the session waits (never drops) when a send would exceed a limit.

use std::collections::VecDeque;
use std::time::Duration;

use brisvox_core::clock::deadline_after;
use tokio::time::Instant;

/// Smallest wait ever reported (a zero wait would spin the gate timer).
const MIN_WAIT: Duration = Duration::from_millis(1);

/// Times of the `Flush` messages sent within the window.
#[derive(Debug)]
pub(super) struct FlushWindow {
    times: VecDeque<Instant>,
    limit: usize,
    window: Duration,
}

impl FlushWindow {
    pub(super) fn new(limit: usize, window: Duration) -> Self {
        Self {
            times: VecDeque::with_capacity(limit.min(64)),
            limit,
            window,
        }
    }

    fn prune(&mut self, now: Instant) {
        while self
            .times
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= self.window)
        {
            self.times.pop_front();
        }
    }

    /// Flushes sent within the window ending `now`.
    pub(super) fn count(&mut self, now: Instant) -> usize {
        self.prune(now);
        self.times.len()
    }

    /// The configured limit.
    pub(super) fn limit(&self) -> usize {
        self.limit
    }

    /// `None` if a flush may be sent now, otherwise how long until a slot frees up.
    pub(super) fn wait(&mut self, now: Instant) -> Option<Duration> {
        self.prune(now);
        if self.times.len() < self.limit {
            return None;
        }
        let oldest_counted = self.times.get(self.times.len().saturating_sub(self.limit));
        let frees_at = oldest_counted.map_or(now, |t| deadline_after(*t, self.window));
        Some(frees_at.saturating_duration_since(now).max(MIN_WAIT))
    }

    /// Notes a flush sent at `now`.
    pub(super) fn record(&mut self, now: Instant) {
        self.prune(now);
        self.times.push_back(now);
    }
}

/// Characters sent within the window.
#[derive(Debug)]
pub(super) struct CharWindow {
    sent: VecDeque<(Instant, usize)>,
    limit: usize,
    window: Duration,
}

impl CharWindow {
    pub(super) fn new(limit: usize, window: Duration) -> Self {
        Self {
            sent: VecDeque::new(),
            limit,
            window,
        }
    }

    fn prune(&mut self, now: Instant) {
        while self
            .sent
            .front()
            .is_some_and(|(t, _)| now.saturating_duration_since(*t) >= self.window)
        {
            self.sent.pop_front();
        }
    }

    /// `None` if `chars` more characters fit now, otherwise how long until enough of the
    /// earlier ones have left the window. A message larger than the whole budget is sent once
    /// the window is empty (the configuration keeps segments below the budget).
    pub(super) fn wait(&mut self, now: Instant, chars: usize) -> Option<Duration> {
        self.prune(now);
        let mut used: usize = self.sent.iter().map(|(_, c)| *c).sum();
        if used == 0 || used.saturating_add(chars) <= self.limit {
            return None;
        }
        for (at, count) in &self.sent {
            used = used.saturating_sub(*count);
            if used.saturating_add(chars) <= self.limit || used == 0 {
                let frees_at = deadline_after(*at, self.window);
                return Some(frees_at.saturating_duration_since(now).max(MIN_WAIT));
            }
        }
        Some(self.window)
    }

    /// Notes `chars` characters sent at `now`.
    pub(super) fn record(&mut self, now: Instant, chars: usize) {
        self.prune(now);
        self.sent.push_back((now, chars));
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn the_flush_window_blocks_the_slot_after_the_limit_until_the_oldest_leaves() {
        let base = Instant::now();
        let mut w = FlushWindow::new(3, Duration::from_millis(1000));
        for ms in [0, 100, 200] {
            assert_eq!(w.wait(at(base, ms)), None);
            w.record(at(base, ms));
        }
        assert_eq!(w.count(at(base, 300)), 3);
        assert_eq!(
            w.wait(at(base, 300)),
            Some(Duration::from_millis(700)),
            "the oldest flush (t=0) leaves the window at t=1000"
        );
        assert_eq!(w.wait(at(base, 1000)), None);
        assert_eq!(w.count(at(base, 1000)), 2);
        assert_eq!(w.limit(), 3);
    }

    #[test]
    fn the_char_window_waits_for_enough_old_characters_to_expire() {
        let base = Instant::now();
        let mut w = CharWindow::new(100, Duration::from_millis(1000));
        assert_eq!(w.wait(at(base, 0), 60), None);
        w.record(at(base, 0), 60);
        assert_eq!(w.wait(at(base, 10), 40), None, "exactly the budget fits");
        w.record(at(base, 10), 40);
        // 100 used: 30 more need the first 60 to leave (t=1000).
        assert_eq!(w.wait(at(base, 500), 30), Some(Duration::from_millis(500)));
        assert_eq!(w.wait(at(base, 1000), 30), None);
    }

    #[test]
    fn a_huge_window_saturates_instead_of_overflowing() {
        // `Instant + Duration` panics on overflow; a window that is "forever" must not.
        let base = Instant::now();
        let mut flushes = FlushWindow::new(1, Duration::MAX);
        flushes.record(base);
        assert!(flushes.wait(at(base, 1)).is_some());
        let mut chars = CharWindow::new(10, Duration::MAX);
        chars.record(base, 10);
        assert!(chars.wait(at(base, 1), 5).is_some());
        assert_eq!(chars.wait(at(base, 1), 0), None);
    }

    #[test]
    fn a_message_above_the_whole_budget_goes_out_once_the_window_is_empty() {
        let base = Instant::now();
        let mut w = CharWindow::new(10, Duration::from_millis(1000));
        assert_eq!(w.wait(base, 50), None);
        w.record(base, 5);
        assert!(w.wait(at(base, 1), 50).is_some());
        assert_eq!(w.wait(at(base, 1000), 50), None);
    }
}
