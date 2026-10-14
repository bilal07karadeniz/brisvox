//! Per-context state of a Cartesia session: what has been sent, what is still owed by the
//! server, how much audio arrived, and the word-timestamp time base.

use std::time::Duration;

use brisvox_core::frame::SampleRate;
use tokio::time::Instant;

use crate::tts::ContextId;

/// A chunk's first word counts as "restarted" (chunk-relative times) only when it starts this
/// much (seconds) before the audio that preceded the chunk: a cumulative server cannot start a
/// word before the audio of the earlier chunks has ended, up to rounding.
const RESTART_TOLERANCE: f64 = 0.02;

/// Converts the server's word times into offsets from the context's first audio sample.
///
/// The docs do not say whether the times of a continuation chunk restart at zero (UNVERIFIED),
/// so both are handled. Chunk boundaries are the manual-flush acknowledgements (`flush_done`),
/// which the session requests after every text chunk; the audio received at a boundary is the
/// discriminator (a cumulative server cannot start a chunk's first word before it).
///
/// The docs also leave the order of `timestamps` and `flush_done` open (UNVERIFIED). The
/// normal order (timestamps of a chunk, then its `flush_done`) is decided exactly. If the
/// timestamps of the chunk before the last boundary arrive after it (`prev_unfilled`), a
/// cumulative server is still classified correctly (its times are not shifted either way) and a
/// restarting server is shifted by the earlier chunk's start. Unresolvable ambiguity: a chunk
/// without any timestamps followed by a restarting server is shifted by the earlier start;
/// offsets never go backwards, so the damage is bounded. Word times only drive truncation, so no input here is an error: bad times are clamped.
///
/// A server context that was closed or expired and is re-opened for the same engine context
/// starts at zero again; [`Self::reopen`] makes the audio already delivered the new origin, so
/// offsets stay continuous across server contexts.
#[derive(Debug, Default)]
pub(super) struct WordClock {
    /// Audio (from the context's first sample) when the current server context began.
    origin: Duration,
    /// Audio received since the origin when the current chunk began (the last boundary).
    cur_start: Duration,
    /// Start of the chunk before the last boundary if no timestamps arrived for it yet.
    prev_unfilled: Option<Duration>,
    /// Timestamps were attributed to the current chunk.
    cur_has_ts: bool,
    /// The shift decided for the current chunk by its first `timestamps` message (not set for
    /// messages that arrived late for the chunk before the last boundary).
    shift: Option<Duration>,
    /// The shift of the message being converted.
    message_shift: Duration,
    /// Last offset handed out: offsets never go backwards.
    last_offset: Duration,
}

impl WordClock {
    /// A `flush_done` arrived: the next audio belongs to a new chunk. `audio_total` is all audio
    /// received for the engine context so far.
    pub(super) fn boundary(&mut self, audio_total: Duration) {
        self.prev_unfilled = if self.cur_has_ts {
            None
        } else {
            Some(self.cur_start)
        };
        self.cur_start = audio_total.saturating_sub(self.origin);
        self.cur_has_ts = false;
        self.shift = None;
    }

    /// A new server context begins for this engine context after `audio_total` of audio.
    pub(super) fn reopen(&mut self, audio_total: Duration) {
        self.origin = audio_total;
        self.cur_start = Duration::ZERO;
        self.prev_unfilled = None;
        self.cur_has_ts = false;
        self.shift = None;
    }

    /// Called with the first start time of a `timestamps` message, before [`Self::offset`].
    pub(super) fn begin_message(&mut self, first_start: f64) {
        if let Some(shift) = self.shift {
            self.message_shift = shift;
            return;
        }
        if !first_start.is_finite() {
            self.message_shift = Duration::ZERO;
            return;
        }
        let behind_current = first_start + RESTART_TOLERANCE < self.cur_start.as_secs_f64();
        let (shift, to_current) = match self.prev_unfilled.take() {
            None => {
                if behind_current {
                    (self.cur_start, true)
                } else {
                    (Duration::ZERO, true)
                }
            }
            Some(previous) => {
                if !behind_current {
                    (Duration::ZERO, true)
                } else if first_start + RESTART_TOLERANCE >= previous.as_secs_f64() {
                    // Cumulative times that belong to the chunk before the last boundary.
                    (Duration::ZERO, false)
                } else {
                    (previous, false)
                }
            }
        };
        self.cur_has_ts |= to_current;
        self.message_shift = shift;
        if to_current {
            self.shift = Some(shift);
        }
    }

    /// The offset of a word that starts at `start` seconds in the server's time base. Total: a
    /// missing, non-finite or absurd time yields the previous offset, a negative one zero.
    pub(super) fn offset(&mut self, start: Option<f64>) -> Duration {
        let raw = match start {
            Some(seconds) if seconds.is_finite() => {
                Duration::try_from_secs_f64(seconds.max(0.0)).ok()
            }
            _ => None,
        };
        let absolute = raw
            .and_then(|r| r.checked_add(self.message_shift))
            .and_then(|r| r.checked_add(self.origin))
            .unwrap_or(self.last_offset);
        let offset = absolute.max(self.last_offset);
        self.last_offset = offset;
        offset
    }
}

/// Most text kept for re-sending after the loss of a server context (a few sentences).
pub(super) const MAX_PENDING_TEXT: usize = 8 * 1024;

/// One engine context ([`ContextId`]) that has text in flight or is waiting for its engine-side
/// end. It survives server-side closes and expiries (a new server context is opened on the next
/// text) so the audio count and the word time base stay continuous. It ends with the engine's
/// `Flush` or cancel.
#[derive(Debug)]
pub(super) struct Live {
    /// The engine's id.
    pub(super) id: ContextId,
    /// The id of the current server context on the wire.
    pub(super) server_id: String,
    /// The current server context is believed open (not closed, expired or lost with the
    /// socket).
    pub(super) server_open: bool,
    /// Flush markers sent whose `flush_done` has not arrived.
    pub(super) markers_out: u32,
    /// Text was sent without a marker and the server has not answered yet.
    pub(super) first_pending: bool,
    /// The closing request was sent; the server's `done` is owed.
    pub(super) finish_sent: bool,
    /// The engine asked for the end (`Flush`): `Flushed` follows the `done`. False when the
    /// adapter closed the context itself because the input was dropped.
    pub(super) engine_flush: bool,
    /// The closing request must follow as soon as the pending re-open has happened.
    pub(super) finish_wanted: bool,
    /// The current server context produced output (so a later error is not a bad request).
    pub(super) established: bool,
    /// Text sent to the current server context since its last confirmed output; re-sent to a
    /// new server context if the old one is lost.
    pub(super) pending_text: String,
    /// `pending_text` overflowed its cap and cannot be re-sent.
    pub(super) resend_lost: bool,
    /// Consecutive re-opens without output.
    pub(super) attempts: u8,
    /// Consecutive times the server closed the context (`done`) with text unanswered; reset by
    /// a `flush_done` (a confirmed answer), so a server that never confirms cannot loop us.
    pub(super) lost_dones: u8,
    /// When to re-open after a refusal that is expected to pass (`concurrency_limited`).
    pub(super) retry_at: Option<Instant>,
    /// Bytes of audio received for this engine context (all server contexts).
    pub(super) audio_bytes: u64,
    /// When the server last sent something for the current server context.
    pub(super) last_activity: Instant,
    /// A trailing odd byte of PCM16 audio, completed by the next chunk.
    pub(super) carry: Option<u8>,
    /// Word time base.
    pub(super) words: WordClock,
}

impl Live {
    pub(super) fn new(id: ContextId, server_id: String, now: Instant) -> Self {
        Self {
            id,
            server_id,
            server_open: true,
            markers_out: 0,
            first_pending: false,
            finish_sent: false,
            engine_flush: false,
            finish_wanted: false,
            established: false,
            pending_text: String::new(),
            resend_lost: false,
            attempts: 0,
            lost_dones: 0,
            retry_at: None,
            audio_bytes: 0,
            last_activity: now,
            carry: None,
            words: WordClock::default(),
        }
    }

    /// Starts a new server context `server_id` for this engine context. The audio count and the
    /// word base of the engine context carry over; `pending_text` is kept (the caller decides).
    pub(super) fn reopen(&mut self, server_id: String, audio: Duration, now: Instant) {
        self.server_id = server_id;
        self.server_open = true;
        self.markers_out = 0;
        self.first_pending = false;
        self.established = false;
        self.carry = None;
        self.retry_at = None;
        self.last_activity = now;
        self.words.reopen(audio);
    }

    /// Remembers sent text for a possible re-send.
    pub(super) fn remember_text(&mut self, text: &str) {
        if self.resend_lost {
            return;
        }
        if self.pending_text.len().saturating_add(text.len()) > MAX_PENDING_TEXT {
            self.pending_text.clear();
            self.resend_lost = true;
        } else {
            self.pending_text.push_str(text);
        }
    }

    /// Everything sent to the server context has been answered.
    pub(super) fn clear_pending(&mut self) {
        self.pending_text.clear();
        self.resend_lost = false;
    }

    /// The server owes this context a response (so a silent server is a stalled server), or a
    /// re-open is scheduled.
    pub(super) fn awaiting_server(&self) -> bool {
        self.retry_at.is_some()
            || (self.server_open
                && (self.markers_out > 0 || self.first_pending || self.finish_sent))
    }

    /// The server context is open, owes nothing and has been quiet for `expiry`: the server has
    /// expired it (or will refuse more input), so the next text must open a new one. Only
    /// meaningful with flush markers (`markers_known`): without them an open context cannot be
    /// told from one still generating.
    pub(super) fn expired(&self, now: Instant, expiry: Duration, markers_known: bool) -> bool {
        markers_known
            && self.server_open
            && !self.awaiting_server()
            && now.saturating_duration_since(self.last_activity) >= expiry
    }

    /// Duration of the audio received so far.
    pub(super) fn audio_duration(&self, rate: SampleRate, bytes_per_sample: u64) -> Duration {
        let bytes_per_second = u128::from(rate.hz()) * u128::from(bytes_per_sample);
        if bytes_per_second == 0 {
            return Duration::ZERO;
        }
        let nanos = u128::from(self.audio_bytes) * 1_000_000_000 / bytes_per_second;
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // tests build `Instant`s directly
mod tests {
    use super::*;

    fn feed(clock: &mut WordClock, words: &[f64]) -> Vec<Duration> {
        clock.begin_message(words.first().copied().unwrap_or(0.0));
        words.iter().map(|&s| clock.offset(Some(s))).collect()
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn cumulative_times_pass_through() {
        let mut c = WordClock::default();
        assert_eq!(feed(&mut c, &[0.1, 0.5]), [ms(100), ms(500)]);
        c.boundary(ms(1000));
        // The next chunk continues after the previous one ended.
        assert_eq!(feed(&mut c, &[1.0, 1.4]), [ms(1000), ms(1400)]);
    }

    #[test]
    fn restarted_times_get_the_chunk_start_added() {
        let mut c = WordClock::default();
        assert_eq!(feed(&mut c, &[0.1, 0.5]), [ms(100), ms(500)]);
        c.boundary(ms(1000));
        // Times restart near zero: chunk-relative, base = audio received at the boundary.
        assert_eq!(feed(&mut c, &[0.05, 0.4]), [ms(1050), ms(1400)]);
        c.boundary(ms(2000));
        // The decision is made per chunk: this one is chunk-relative again.
        assert_eq!(feed(&mut c, &[0.0]), [ms(2000)]);
    }

    #[test]
    fn a_short_first_chunk_and_an_early_word_in_the_next_are_told_apart() {
        // The first chunk's last word ends at 0.30 s but its audio lasts 0.35 s; the next
        // chunk's first word is at 0.26 s into that chunk: restarted, not cumulative.
        let mut c = WordClock::default();
        feed(&mut c, &[0.0, 0.2]);
        c.boundary(ms(350));
        assert_eq!(feed(&mut c, &[0.26]), [ms(610)]);
    }

    #[test]
    fn later_messages_of_a_chunk_keep_the_chunks_decision() {
        let mut c = WordClock::default();
        feed(&mut c, &[0.1]);
        c.boundary(ms(1000));
        assert_eq!(feed(&mut c, &[0.0]), [ms(1000)]);
        // A second message of the same chunk: still chunk-relative, even if it starts after the
        // previous audio's length.
        assert_eq!(feed(&mut c, &[1.3]), [ms(2300)]);
    }

    #[test]
    fn cumulative_timestamps_that_arrive_after_their_flush_done_are_not_shifted() {
        let mut c = WordClock::default();
        // Chunk 0 is answered normally.
        feed(&mut c, &[0.1, 0.5]);
        c.boundary(ms(1000));
        // Chunk 1: audio, then flush_done, then its timestamps (cumulative: 1.0 and 1.4).
        c.boundary(ms(2000));
        assert_eq!(feed(&mut c, &[1.0, 1.4]), [ms(1000), ms(1400)]);
        // Chunk 2's timestamps arrive in time.
        assert_eq!(feed(&mut c, &[2.0]), [ms(2000)]);
    }

    #[test]
    fn restarted_timestamps_that_arrive_after_their_flush_done_get_their_own_chunk_start() {
        let mut c = WordClock::default();
        feed(&mut c, &[0.1, 0.5]);
        c.boundary(ms(1000));
        // Chunk 1's flush_done arrives before its timestamps (relative to chunk 1).
        c.boundary(ms(2000));
        assert_eq!(feed(&mut c, &[0.05, 0.4]), [ms(1050), ms(1400)]);
    }

    #[test]
    fn bad_times_are_clamped_never_rejected() {
        let mut c = WordClock::default();
        c.begin_message(1.0);
        assert_eq!(c.offset(Some(1.0)), ms(1000));
        assert_eq!(c.offset(Some(0.9)), ms(1000), "never backwards");
        assert_eq!(c.offset(Some(-0.001)), ms(1000), "negative");
        assert_eq!(c.offset(Some(f64::NAN)), ms(1000));
        assert_eq!(c.offset(Some(f64::INFINITY)), ms(1000));
        assert_eq!(c.offset(Some(1e300)), ms(1000));
        assert_eq!(c.offset(None), ms(1000), "missing");
        assert_eq!(c.offset(Some(1.5)), ms(1500));
        let mut fresh = WordClock::default();
        assert_eq!(fresh.offset(Some(-0.001)), Duration::ZERO);
    }

    #[test]
    fn without_boundaries_times_are_taken_as_cumulative() {
        let mut c = WordClock::default();
        feed(&mut c, &[0.1]);
        // No flush marker acknowledged: no chunk start is known, so the times stay as sent.
        assert_eq!(feed(&mut c, &[0.0]), [ms(100)]);
    }

    #[test]
    fn a_reopened_server_context_continues_after_the_audio_already_delivered() {
        let mut c = WordClock::default();
        feed(&mut c, &[0.1, 0.5]);
        c.boundary(ms(800));
        // The server context expired; the new one starts at zero.
        c.reopen(ms(800));
        assert_eq!(feed(&mut c, &[0.02, 0.22]), [ms(820), ms(1020)]);
        c.boundary(ms(1200));
        // Inside the new server context the usual decisions apply, relative to its origin.
        assert_eq!(feed(&mut c, &[0.45]), [ms(1250)]);
        c.boundary(ms(1600));
        assert_eq!(feed(&mut c, &[0.0]), [ms(1600)]);
    }

    #[test]
    fn audio_duration_follows_the_format() {
        let mut live = Live::new(ContextId(1), "x".to_owned(), Instant::now());
        live.audio_bytes = 32_000;
        assert_eq!(live.audio_duration(SampleRate::Hz16000, 2), ms(1000));
        live.audio_bytes = 8_000;
        assert_eq!(live.audio_duration(SampleRate::Hz8000, 1), ms(1000));
    }

    #[test]
    fn awaiting_server_tracks_markers_first_response_finish_and_retry() {
        let now = Instant::now();
        let mut live = Live::new(ContextId(1), "x".to_owned(), now);
        assert!(!live.awaiting_server());
        live.markers_out = 1;
        assert!(live.awaiting_server());
        live.markers_out = 0;
        live.first_pending = true;
        assert!(live.awaiting_server());
        live.first_pending = false;
        live.finish_sent = true;
        assert!(live.awaiting_server());
        live.finish_sent = false;
        live.server_open = false;
        live.markers_out = 1;
        assert!(
            !live.awaiting_server(),
            "a closed server context owes nothing"
        );
        live.retry_at = Some(now);
        assert!(live.awaiting_server(), "a scheduled re-open is awaited");
    }

    #[test]
    fn a_quiet_open_context_expires_only_with_markers() {
        let now = Instant::now();
        let live = Live::new(ContextId(1), "x".to_owned(), now);
        let later = now + Duration::from_secs(2);
        assert!(live.expired(later, Duration::from_secs(1), true));
        assert!(!live.expired(later, Duration::from_secs(1), false));
        assert!(!live.expired(now, Duration::from_secs(1), true));
    }

    #[test]
    fn pending_text_is_capped() {
        let mut live = Live::new(ContextId(1), "x".to_owned(), Instant::now());
        live.remember_text("hello");
        assert_eq!(live.pending_text, "hello");
        live.remember_text(&"x".repeat(MAX_PENDING_TEXT));
        assert!(live.resend_lost && live.pending_text.is_empty());
        live.clear_pending();
        assert!(!live.resend_lost);
    }
}
