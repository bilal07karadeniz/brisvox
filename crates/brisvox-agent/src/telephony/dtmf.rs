//! Caller key presses (DTMF) as input for the model.
//!
//! # How a key press reaches the model
//! 1. The transport adapter turns the provider's `dtmf` message into a `WireEvent::Dtmf` and the
//!    pump into a `DataFrame::Dtmf` (`brisvox_core::DtmfFrame`, one per key, `at` = when it was
//!    received).
//! 2. The turn driver re-tags the frame with the current turn epoch and forwards it downstream
//!    (`TurnDriver`, `Frame::Data(DataFrame::Dtmf)`); DTMF is never sent to STT.
//! 3. The LLM runner feeds each frame's digit to a [`DtmfCollector`]. When the
//!    collector completes (terminator, digit limit or inter-digit timeout) the runner pushes
//!    [`dtmf_message`] into the [`Context`](crate::Context) as a user message and starts a round,
//!    exactly as for a final transcript. The collector's [`DtmfCollector::next_deadline`] is the
//!    timer the runner must arm (on the injected clock) so a half-typed entry is flushed.
//!
//! # Order of keys
//! Telnyx documents that its `dtmf` frames may arrive out of order and carry an `occurred_at`
//! to order them by. [`DtmfFrame`](brisvox_core::frame::DtmfFrame) has no such field, so the
//! ordering belongs to the transport adapter: it must emit the frames in `occurred_at` order
//! (a short reorder buffer) and stamp `at` accordingly. The collector then only has to be
//! robust against a non-monotonic `at` (it never moves its inter-digit deadline backwards).
//! Open requirement for the Telnyx media adapter; the Twilio adapter has no such issue.
//!
//! A key press is a user turn: it can interrupt the bot if the application's interruption policy
//! says so (`docs/providers/call-control.md`, 3.1); this module only shapes the text.

use std::time::Duration;

use brisvox_core::frame::DtmfDigit;
use tokio::time::Instant;

use crate::context::Message;
use crate::segment::Language;

/// Default longest entry before the collector completes: 16 keys.
pub const DEFAULT_MAX_DIGITS: usize = 16;
/// Default time without a key after which a non-empty entry completes: 3 seconds.
pub const DEFAULT_INTER_DIGIT_TIMEOUT: Duration = Duration::from_secs(3);

/// The keys as text: digits and letters as they are, `*` and `#` too, separated by spaces so a
/// text-to-speech or a model reads them one by one.
pub fn digits_text(digits: &[DtmfDigit]) -> String {
    let mut text = String::with_capacity(digits.len().saturating_mul(2));
    for (index, digit) in digits.iter().enumerate() {
        if index > 0 {
            text.push(' ');
        }
        text.push(digit.as_char());
    }
    text
}

/// A short note for the model saying which keys the caller pressed, or `None` for no keys.
/// Digits are the only variable content, so the note cannot carry an injection.
pub fn render_dtmf_note(digits: &[DtmfDigit], language: Language) -> Option<String> {
    if digits.is_empty() {
        return None;
    }
    let keys = digits_text(digits);
    Some(match language {
        Language::En => format!("[The caller pressed the keys: {keys}]"),
        Language::Tr => format!("[Arayan şu tuşlara bastı: {keys}]"),
    })
}

/// The note as a user message for the context (`None` for no keys).
pub fn dtmf_message(digits: &[DtmfDigit], language: Language) -> Option<Message> {
    render_dtmf_note(digits, language).map(Message::user)
}

/// Why a [`DtmfCollector`] completed an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectReason {
    /// The terminator key was pressed (it is not part of the entry).
    Terminator,
    /// The terminator key was pressed with nothing pending ("# to skip", "* to go back"): the
    /// entry is that one key. Only reported when
    /// [`DtmfCollectorConfig::report_lone_terminator`] is set.
    LoneTerminator,
    /// The entry reached the digit limit.
    MaxDigits,
    /// No key for the inter-digit timeout.
    Timeout,
}

/// A completed entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collected {
    /// The keys, in order, without the terminator (for [`CollectReason::LoneTerminator`] the
    /// terminator itself).
    pub digits: Vec<DtmfDigit>,
    /// Why the entry completed.
    pub reason: CollectReason,
}

impl Collected {
    /// The keys as one string without separators (`"1234"`), for a tool or an account lookup.
    pub fn as_string(&self) -> String {
        self.digits.iter().map(|d| d.as_char()).collect()
    }
}

/// What a key press did to the collector.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pushed {
    /// An older entry that had already timed out when this key arrived.
    pub expired: Option<Collected>,
    /// The entry this key completed.
    pub completed: Option<Collected>,
}

/// Collector settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DtmfCollectorConfig {
    /// Keys after which an entry completes. At least 1.
    pub max_digits: usize,
    /// The key that completes an entry and is not part of it (`#` by default). With `None` only
    /// the limit and the timeout complete an entry.
    pub terminator: Option<DtmfDigit>,
    /// Time without a key after which a non-empty entry completes.
    pub inter_digit_timeout: Duration,
    /// Whether the terminator pressed with no pending keys is an entry of its own
    /// ([`CollectReason::LoneTerminator`]) instead of being dropped. On by default: a caller who
    /// presses only `#` or `*` to skip a prompt or go back must reach the model.
    pub report_lone_terminator: bool,
}

impl Default for DtmfCollectorConfig {
    fn default() -> Self {
        Self {
            max_digits: DEFAULT_MAX_DIGITS,
            terminator: Some(DtmfDigit::Pound),
            inter_digit_timeout: DEFAULT_INTER_DIGIT_TIMEOUT,
            report_lone_terminator: true,
        }
    }
}

/// Groups key presses into entries ("1 2 3 4 #" is one entry). Pure: time comes from the
/// `Instant`s the caller passes, never from a clock of its own, so it is deterministic under
/// simulation. Memory is bounded by [`DtmfCollectorConfig::max_digits`].
#[derive(Debug, Clone)]
pub struct DtmfCollector {
    config: DtmfCollectorConfig,
    digits: Vec<DtmfDigit>,
    last_key: Option<Instant>,
}

impl DtmfCollector {
    /// A collector with `config` (a zero `max_digits` is raised to 1).
    pub fn new(config: DtmfCollectorConfig) -> Self {
        let config = DtmfCollectorConfig {
            max_digits: config.max_digits.max(1),
            ..config
        };
        Self {
            config,
            digits: Vec::with_capacity(config.max_digits),
            last_key: None,
        }
    }

    /// Whether keys are waiting for the entry to complete.
    pub fn is_pending(&self) -> bool {
        !self.digits.is_empty()
    }

    /// The instant at which the pending entry completes by timeout (arm a timer for it), or
    /// `None` when nothing is pending.
    pub fn next_deadline(&self) -> Option<Instant> {
        if self.digits.is_empty() {
            return None;
        }
        self.last_key
            .and_then(|at| at.checked_add(self.config.inter_digit_timeout))
    }

    /// Adds the key pressed at `at`. An older entry that had timed out by `at` is returned in
    /// [`Pushed::expired`] first, and the key starts a new entry.
    pub fn push(&mut self, digit: DtmfDigit, at: Instant) -> Pushed {
        let mut pushed = Pushed {
            expired: self.poll(at),
            completed: None,
        };
        if self.config.terminator == Some(digit) {
            if !self.digits.is_empty() {
                pushed.completed = self.take(CollectReason::Terminator);
            } else if self.config.report_lone_terminator {
                pushed.completed = Some(Collected {
                    digits: vec![digit],
                    reason: CollectReason::LoneTerminator,
                });
            }
            return pushed;
        }
        self.digits.push(digit);
        // `at` can be earlier than the previous key's (frames reordered upstream): the deadline
        // must not move backwards.
        self.last_key = Some(self.last_key.map_or(at, |last| last.max(at)));
        if self.digits.len() >= self.config.max_digits {
            pushed.completed = self.take(CollectReason::MaxDigits);
        }
        pushed
    }

    /// Completes the pending entry if `now` is at or past its deadline.
    pub fn poll(&mut self, now: Instant) -> Option<Collected> {
        let deadline = self.next_deadline()?;
        if now >= deadline {
            self.take(CollectReason::Timeout)
        } else {
            None
        }
    }

    /// Discards the pending keys (a barge-in that should not count, a node change).
    pub fn reset(&mut self) {
        self.digits.clear();
        self.last_key = None;
    }

    fn take(&mut self, reason: CollectReason) -> Option<Collected> {
        self.last_key = None;
        if self.digits.is_empty() {
            return None;
        }
        Some(Collected {
            digits: std::mem::take(&mut self.digits),
            reason,
        })
    }
}

impl Default for DtmfCollector {
    fn default() -> Self {
        Self::new(DtmfCollectorConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(text: &str) -> Vec<DtmfDigit> {
        text.chars()
            .map(|c| DtmfDigit::try_from(c).unwrap())
            .collect()
    }

    #[test]
    fn note_lists_the_keys_in_both_languages() {
        assert_eq!(
            render_dtmf_note(&keys("12#"), Language::En).as_deref(),
            Some("[The caller pressed the keys: 1 2 #]")
        );
        assert_eq!(
            render_dtmf_note(&keys("5"), Language::Tr).as_deref(),
            Some("[Arayan şu tuşlara bastı: 5]")
        );
        assert_eq!(render_dtmf_note(&[], Language::En), None);
        assert!(matches!(
            dtmf_message(&keys("9"), Language::En),
            Some(Message::User { .. })
        ));
    }

    #[test]
    fn collector_completes_on_terminator_limit_and_timeout() {
        #[allow(clippy::disallowed_methods)] // tests need some Instant value
        let t0 = Instant::now();
        let ms = Duration::from_millis;

        let mut c = DtmfCollector::default();
        assert!(c.push(DtmfDigit::D1, t0).completed.is_none());
        assert!(c.push(DtmfDigit::D2, t0 + ms(100)).completed.is_none());
        let done = c.push(DtmfDigit::Pound, t0 + ms(200)).completed.unwrap();
        assert_eq!(done.as_string(), "12");
        assert_eq!(done.reason, CollectReason::Terminator);
        assert!(!c.is_pending());

        // A lone terminator is an entry of its own (skip / back).
        let lone = c.push(DtmfDigit::Pound, t0 + ms(300)).completed.unwrap();
        assert_eq!(
            (lone.as_string().as_str(), lone.reason),
            ("#", CollectReason::LoneTerminator)
        );
        assert!(!c.is_pending());

        // ... unless configured away.
        let mut quiet = DtmfCollector::new(DtmfCollectorConfig {
            report_lone_terminator: false,
            ..DtmfCollectorConfig::default()
        });
        assert_eq!(quiet.push(DtmfDigit::Pound, t0), Pushed::default());

        let mut c = DtmfCollector::new(DtmfCollectorConfig {
            max_digits: 2,
            ..DtmfCollectorConfig::default()
        });
        assert!(c.push(DtmfDigit::D7, t0).completed.is_none());
        let done = c.push(DtmfDigit::D8, t0 + ms(10)).completed.unwrap();
        assert_eq!(
            (done.as_string().as_str(), done.reason),
            ("78", CollectReason::MaxDigits)
        );

        let mut c = DtmfCollector::default();
        c.push(DtmfDigit::D4, t0);
        assert_eq!(c.next_deadline(), Some(t0 + DEFAULT_INTER_DIGIT_TIMEOUT));
        assert!(c.poll(t0 + ms(2999)).is_none());
        let done = c.poll(t0 + ms(3000)).unwrap();
        assert_eq!(
            (done.as_string().as_str(), done.reason),
            ("4", CollectReason::Timeout)
        );
        assert_eq!(c.next_deadline(), None);
    }

    #[test]
    fn a_non_monotonic_key_time_does_not_move_the_deadline_back() {
        #[allow(clippy::disallowed_methods)] // tests need some Instant value
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut c = DtmfCollector::default();
        c.push(DtmfDigit::D1, t0 + ms(500));
        // A key stamped earlier than the previous one (arrived out of order).
        c.push(DtmfDigit::D2, t0 + ms(100));
        assert_eq!(
            c.next_deadline(),
            Some(t0 + ms(500) + DEFAULT_INTER_DIGIT_TIMEOUT)
        );
        let done = c.push(DtmfDigit::Pound, t0 + ms(600)).completed.unwrap();
        assert_eq!(done.as_string(), "12");
    }

    #[test]
    fn a_stale_entry_is_returned_before_the_new_key_starts_another() {
        #[allow(clippy::disallowed_methods)] // tests need some Instant value
        let t0 = Instant::now();
        let mut c = DtmfCollector::default();
        c.push(DtmfDigit::D1, t0);
        let pushed = c.push(DtmfDigit::D2, t0 + Duration::from_secs(10));
        assert_eq!(pushed.expired.unwrap().as_string(), "1");
        assert!(pushed.completed.is_none());
        assert!(c.is_pending());
        c.reset();
        assert!(!c.is_pending());
    }
}
