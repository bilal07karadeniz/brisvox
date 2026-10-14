//! The playout estimate of an RTVI call.
//!
//! RTVI has no playback acknowledgement: the JS client plays what it receives and tells nobody
//! (`docs/providers/rtvi.md`, "Minimum a server must implement"). The core releases audio at real
//! time, so the client's buffer is short, and the adapter estimates when what it sent has been
//! played: the audio sent so far ends at `ends = max(ends, now) + duration` on the injected
//! clock, as if the client started playing each frame the moment it arrived and played it at
//! real time. From that:
//!
//! - an engine mark is "played" when the estimate reaches the mark's position;
//! - the lead (audio sent but not yet played) is `ends - now`;
//! - the bot is "speaking" from the first frame of a burst until the estimate runs out.
//!
//! This is an estimate: network delay and the client's own buffering make the real playout
//! later, and a client whose tab is throttled may play later still. A barge-in clears the
//! estimate (the adapter cannot clear audio the browser already holds, see the module docs).

use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

use brisvox_core::clock::deadline_after;

/// Most marks waiting for their playout time. The core keeps at most 256 outstanding marks; past
/// the bound the oldest is acknowledged at once (a mark that is never answered would stall the
/// engine, an early answer only makes the estimate optimistic).
pub(crate) const MAX_PENDING_MARKS: usize = 256;

/// What is due at an instant.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Due {
    /// Mark ids whose playout time has come, oldest first.
    pub(crate) marks: Vec<String>,
    /// The bot stopped speaking: the estimate ran out.
    pub(crate) stopped_speaking: bool,
}

/// The playout estimate and the marks waiting on it.
#[derive(Debug, Default)]
pub(crate) struct Playout {
    ends: Option<Instant>,
    speaking: bool,
    marks: VecDeque<(Instant, String)>,
}

impl Playout {
    /// Audio of `duration` was sent at `now`. Returns `true` if the bot started speaking.
    pub(crate) fn audio_sent(&mut self, now: Instant, duration: Duration) -> bool {
        let start = self.ends.map_or(now, |ends| ends.max(now));
        self.ends = Some(deadline_after(start, duration));
        !std::mem::replace(&mut self.speaking, true)
    }

    /// Audio sent but not yet played, as estimated.
    pub(crate) fn lead(&self, now: Instant) -> Duration {
        self.ends
            .map_or(Duration::ZERO, |ends| ends.saturating_duration_since(now))
    }

    /// An engine mark sent at `now`: due when the audio ahead of it has played. Returns the id of
    /// the oldest mark if the bound forced it out (to be acknowledged at once).
    pub(crate) fn mark(&mut self, now: Instant, id: String) -> Option<String> {
        let due = self.ends.map_or(now, |ends| ends.max(now));
        self.marks.push_back((due, id));
        if self.marks.len() > MAX_PENDING_MARKS {
            self.marks.pop_front().map(|(_, id)| id)
        } else {
            None
        }
    }

    /// A barge-in: forget the estimate and the waiting marks (the core reports them as cut and
    /// ignores any acknowledgement). Returns `true` if the bot was speaking.
    pub(crate) fn clear(&mut self) -> bool {
        self.ends = None;
        self.marks.clear();
        std::mem::take(&mut self.speaking)
    }

    /// The next instant something becomes due, if anything is pending.
    pub(crate) fn next_due(&self) -> Option<Instant> {
        let mark = self.marks.front().map(|(due, _)| *due);
        let stop = if self.speaking { self.ends } else { None };
        match (mark, stop) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Takes what is due at `now`.
    pub(crate) fn take_due(&mut self, now: Instant) -> Due {
        let mut due = Due::default();
        while let Some((at, _)) = self.marks.front() {
            if *at > now {
                break;
            }
            if let Some((_, id)) = self.marks.pop_front() {
                due.marks.push(id);
            }
        }
        if self.speaking && self.ends.is_none_or(|ends| ends <= now) {
            self.speaking = false;
            due.stopped_speaking = true;
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brisvox_core::SystemClock;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_lead_is_the_audio_sent_and_not_yet_played() {
        let t0 = SystemClock::shared().now();
        let mut p = Playout::default();
        assert!(p.audio_sent(t0, ms(20)), "the first frame starts speaking");
        assert!(!p.audio_sent(t0, ms(20)), "the next one does not");
        assert_eq!(p.lead(t0), ms(40));
        assert_eq!(p.lead(t0 + ms(30)), ms(10));
        assert_eq!(p.lead(t0 + ms(100)), Duration::ZERO);
    }

    #[test]
    fn a_gap_restarts_the_estimate_at_the_next_frame() {
        let t0 = SystemClock::shared().now();
        let mut p = Playout::default();
        p.audio_sent(t0, ms(20));
        // 80 ms of silence, then more audio: it ends 20 ms after it was sent, not after t0 + 20.
        p.audio_sent(t0 + ms(100), ms(20));
        assert_eq!(p.lead(t0 + ms(100)), ms(20));
    }

    #[test]
    fn a_mark_is_due_when_the_audio_ahead_of_it_has_played() {
        let t0 = SystemClock::shared().now();
        let mut p = Playout::default();
        p.audio_sent(t0, ms(60));
        assert_eq!(p.mark(t0, "end".to_owned()), None);
        assert_eq!(p.next_due(), Some(t0 + ms(60)));
        assert_eq!(p.take_due(t0 + ms(59)), Due::default());
        let due = p.take_due(t0 + ms(60));
        assert_eq!(due.marks, vec!["end".to_owned()]);
        assert!(due.stopped_speaking);
        assert_eq!(p.next_due(), None);
    }

    #[test]
    fn a_mark_with_nothing_ahead_is_due_at_once() {
        let t0 = SystemClock::shared().now();
        let mut p = Playout::default();
        p.mark(t0, "m".to_owned());
        assert_eq!(p.take_due(t0).marks, vec!["m".to_owned()]);
    }

    #[test]
    fn speaking_continues_while_audio_keeps_coming() {
        let t0 = SystemClock::shared().now();
        let mut p = Playout::default();
        p.audio_sent(t0, ms(20));
        p.audio_sent(t0 + ms(10), ms(20));
        // The estimate ends at t0 + 40: not due at +30.
        assert!(!p.take_due(t0 + ms(30)).stopped_speaking);
        assert!(p.take_due(t0 + ms(40)).stopped_speaking);
    }

    #[test]
    fn clear_forgets_everything_and_reports_speaking() {
        let t0 = SystemClock::shared().now();
        let mut p = Playout::default();
        p.audio_sent(t0, ms(100));
        p.mark(t0, "m".to_owned());
        assert!(p.clear());
        assert!(!p.clear());
        assert_eq!(p.lead(t0), Duration::ZERO);
        assert_eq!(p.take_due(t0 + ms(500)), Due::default());
        assert_eq!(p.next_due(), None);
    }

    #[test]
    fn the_mark_bound_acknowledges_the_oldest_early() {
        let t0 = SystemClock::shared().now();
        let mut p = Playout::default();
        p.audio_sent(t0, ms(10_000));
        for n in 0..MAX_PENDING_MARKS {
            assert_eq!(p.mark(t0, format!("m{n}")), None);
        }
        assert_eq!(p.mark(t0, "extra".to_owned()), Some("m0".to_owned()));
    }
}
