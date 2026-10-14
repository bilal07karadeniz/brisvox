//! Per-context state of an ElevenLabs session: what has been sent, how much of it the server
//! has aligned, how much audio arrived, and the character-to-word conversion.

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use tokio::time::Instant;

use crate::tts::ContextId;

/// Number of characters that are not whitespace.
pub(super) fn count_spoken<S: AsRef<str>>(chars: &[S]) -> u64 {
    chars
        .iter()
        .flat_map(|c| c.as_ref().chars())
        .filter(|c| !c.is_whitespace())
        .count() as u64
}

/// Groups the characters of alignment messages into whitespace-separated words.
///
/// The alignment has no word level (docs): a word starts at the first non-whitespace
/// character after whitespace, with that character's start time, and ends at the next
/// whitespace. A word may span messages, so the unfinished one is kept between calls;
/// [`Self::finish`] ends it. Offsets never go backwards: a bad time (missing, negative,
/// non-finite, absurd) is clamped, never an error.
#[derive(Debug, Default)]
pub(super) struct Words {
    word: String,
    start: Duration,
    last: Duration,
}

impl Words {
    /// Feeds one message's alignment. `origin` is the audio delivered before the message (the
    /// alignment times are relative to the message's own audio). `emit` receives each word that
    /// ended, with its offset from the context's first audio sample.
    pub(super) fn feed<S: AsRef<str>>(
        &mut self,
        origin: Duration,
        chars: &[S],
        starts_ms: &[f64],
        mut emit: impl FnMut(Arc<str>, Duration),
    ) {
        let mut previous = 0.0_f64;
        for (position, chunk) in chars.iter().enumerate() {
            let start_ms = starts_ms.get(position).copied().unwrap_or(previous);
            if start_ms.is_finite() {
                previous = start_ms;
            }
            for c in chunk.as_ref().chars() {
                if c.is_whitespace() {
                    self.finish_with(&mut emit);
                } else {
                    if self.word.is_empty() {
                        self.start = self.offset(origin, start_ms);
                    }
                    self.word.push(c);
                }
            }
        }
    }

    /// Ends the unfinished word, if any.
    pub(super) fn finish(&mut self, mut emit: impl FnMut(Arc<str>, Duration)) {
        self.finish_with(&mut emit);
    }

    fn finish_with(&mut self, emit: &mut impl FnMut(Arc<str>, Duration)) {
        if !self.word.is_empty() {
            emit(Arc::from(self.word.as_str()), self.start);
            self.word.clear();
        }
    }

    fn offset(&mut self, origin: Duration, start_ms: f64) -> Duration {
        let within = if start_ms.is_finite() {
            Duration::try_from_secs_f64(start_ms.max(0.0) / 1000.0).ok()
        } else {
            None
        };
        let absolute = within
            .and_then(|w| origin.checked_add(w))
            .unwrap_or(self.last);
        let offset = absolute.max(self.last);
        self.last = offset;
        offset
    }
}

/// Most engine contexts kept at once. The server allows 5 contexts per connection (docs); one
/// is left as headroom for a context that is closing.
pub(super) const MAX_LIVE: usize = 4;

/// Characters the alignment may omit before a quiet server is taken for stalled (emoji, markup
/// and the like that the server does not align).
pub(super) const SHORTFALL_MIN: u64 = 4;

/// The factor by which the end-of-output grace is stretched for a context without alignment.
pub(super) const UNALIGNED_GRACE_FACTOR: u32 = 5;

/// One engine context ([`ContextId`]) that has text in flight or is waiting for its engine-side
/// end. It survives server-side closes (a new server context is opened on the next text) so the
/// audio count and the word time base stay continuous. It ends with the engine's `Flush` or
/// cancel.
#[derive(Debug)]
pub(super) struct Live {
    /// The engine's id.
    pub(super) id: ContextId,
    /// The id of the current server context on the wire.
    pub(super) server_id: String,
    /// The current server context is believed open (not closed by the server or lost with the
    /// socket).
    pub(super) server_open: bool,
    /// The closing request was sent; the server's `isFinal` is expected.
    pub(super) finish_sent: bool,
    /// The engine asked for the end (`Flush`): `Flushed` follows. False when the adapter closed
    /// the context itself because the input was dropped.
    pub(super) engine_flush: bool,
    /// Characters (not whitespace) of the text sent.
    pub(super) chars_sent: u64,
    /// Characters of `alignment` received.
    pub(super) chars_aligned: u64,
    /// Characters of `normalizedAlignment` received.
    pub(super) chars_normalized: u64,
    /// Any alignment object has been received.
    pub(super) aligned_seen: bool,
    /// A plain `alignment` object (not only `normalizedAlignment`) has been received: its
    /// count, not the normalized one, measures coverage from then on.
    pub(super) plain_seen: bool,
    /// The alignment covered fewer characters than were sent and the server went quiet after
    /// responding: the alignment is not trusted to be complete for this context any more, and
    /// completion falls back to the first response after the last text.
    pub(super) degraded: bool,
    /// Audio or alignment arrived since the last text was sent.
    pub(super) responded: bool,
    /// Bytes of audio received for this engine context (all server contexts).
    pub(super) audio_bytes: u64,
    /// `audio_bytes` when the current server context was opened: the origin of the alignment
    /// clock when `sync_alignment` makes the times run from the context's start.
    pub(super) server_audio_bytes: u64,
    /// A trailing odd byte of PCM16 audio, completed by the next chunk.
    pub(super) carry: Option<u8>,
    /// When something was last sent to the current server context (keepalive clock).
    pub(super) last_send: Instant,
    /// When the server last sent something for the current server context (or the closing
    /// request went out): the grace clock.
    pub(super) last_activity: Instant,
    /// Word grouping.
    pub(super) words: Words,
}

impl Live {
    pub(super) fn new(id: ContextId, server_id: String, now: Instant) -> Self {
        Self {
            id,
            server_id,
            server_open: true,
            finish_sent: false,
            engine_flush: false,
            chars_sent: 0,
            chars_aligned: 0,
            chars_normalized: 0,
            aligned_seen: false,
            plain_seen: false,
            degraded: false,
            responded: false,
            audio_bytes: 0,
            server_audio_bytes: 0,
            carry: None,
            last_send: now,
            last_activity: now,
            words: Words::default(),
        }
    }

    /// Starts a new server context `server_id` for this engine context. Counters, audio count
    /// and word state carry over.
    pub(super) fn reopen(&mut self, server_id: String, now: Instant) {
        self.server_id = server_id;
        self.server_open = true;
        self.server_audio_bytes = self.audio_bytes;
        self.carry = None;
        self.last_send = now;
        self.last_activity = now;
    }

    /// Characters of the alignment that the words are built from and that measure coverage: the
    /// plain `alignment` once one was seen, the normalized one only while there is no plain one.
    ///
    /// The normalized text is the spelled-out one ("15" becomes "fifteen") and can be longer
    /// than the input, so it must never stand in for the plain count when that exists: an
    /// overshoot would declare text complete that is still unspoken.
    fn chars_covered(&self) -> u64 {
        if self.plain_seen {
            self.chars_aligned
        } else {
            self.chars_normalized
        }
    }

    /// The alignment received so far covers every character sent (and is trusted to).
    pub(super) fn covered(&self, by_alignment: bool) -> bool {
        by_alignment
            && self.aligned_seen
            && !self.degraded
            && self.chars_sent > 0
            && self.chars_covered() >= self.chars_sent
    }

    /// The server still owes output for text already sent.
    ///
    /// With alignment (`by_alignment`, once any was seen, and not [`Self::degraded`]) the output
    /// is complete when the aligned characters (see [`Self::chars_covered`]) cover the
    /// characters sent. Otherwise the docs offer no marker, so the first response after the
    /// last text counts as complete.
    pub(super) fn owed(&self, by_alignment: bool) -> bool {
        if self.chars_sent == 0 {
            return false;
        }
        if by_alignment && self.aligned_seen && !self.degraded {
            self.chars_covered() < self.chars_sent
        } else {
            !self.responded
        }
    }

    /// The server went quiet while the alignment still falls short, but it did respond to the
    /// last text and the shortfall is small ([`Self::shortfall_tolerated`]): take that as a
    /// healthy server whose alignment omits a few characters (the docs leave the 1:1 mapping
    /// open) and stop measuring coverage for this context. `false`: nothing to degrade, the
    /// silence is a real stall (no response yet, or most of the text is still unaligned).
    pub(super) fn degrade(&mut self, by_alignment: bool) -> bool {
        if by_alignment
            && self.aligned_seen
            && !self.degraded
            && self.responded
            && self.owed(true)
            && self.shortfall_tolerated()
        {
            self.degraded = true;
            true
        } else {
            false
        }
    }

    /// The alignment misses at most [`SHORTFALL_MIN`] characters or a quarter of the text sent.
    fn shortfall_tolerated(&self) -> bool {
        let missing = self.chars_sent.saturating_sub(self.chars_covered());
        missing <= SHORTFALL_MIN.max(self.chars_sent / 4)
    }

    /// The server owes this context output, so a silent server is a stalled server.
    pub(super) fn awaiting_server(&self, by_alignment: bool) -> bool {
        self.server_open && self.owed(by_alignment)
    }

    /// How long the context must be quiet after its closing request before it is taken as done
    /// without an `isFinal`. Without any alignment the first audio chunk is the only sign of
    /// progress there is (the server may queue the rest for longer than `base`), so the wait is
    /// longer.
    pub(super) fn grace(&self, base: Duration) -> Duration {
        if self.aligned_seen {
            base
        } else {
            base.saturating_mul(UNALIGNED_GRACE_FACTOR)
        }
    }

    /// Where the alignment clock of the next message starts, as an offset from the first audio
    /// sample of the engine context.
    ///
    /// Measured live (2026-10-13, see docs/providers/elevenlabs.md): without `sync_alignment`
    /// the times are relative to the message's own audio (the docs: "relative to the returned
    /// chunk"), so the origin is all the audio delivered before the message; with
    /// `sync_alignment=true` they run from the start of the server context (each message
    /// continues where the previous one's alignment ended), so the origin is the audio that
    /// was delivered before this server context opened. Adding the full audio count in that
    /// mode counted the earlier messages twice (the last word landed 1.4 s past the audio).
    pub(super) fn alignment_origin(
        &self,
        sync_alignment: bool,
        rate: SampleRate,
        bytes_per_sample: u64,
    ) -> Duration {
        let bytes = if sync_alignment {
            self.server_audio_bytes
        } else {
            self.audio_bytes
        };
        Self::bytes_duration(bytes, rate, bytes_per_sample)
    }

    fn bytes_duration(bytes: u64, rate: SampleRate, bytes_per_sample: u64) -> Duration {
        let bytes_per_second = u128::from(rate.hz()) * u128::from(bytes_per_sample);
        if bytes_per_second == 0 {
            return Duration::ZERO;
        }
        let nanos = u128::from(bytes) * 1_000_000_000 / bytes_per_second;
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // tests build `Instant`s directly
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn chars(text: &str) -> Vec<String> {
        text.chars().map(String::from).collect()
    }

    fn collect(
        words: &mut Words,
        origin: Duration,
        text: &str,
        starts: &[f64],
    ) -> Vec<(String, Duration)> {
        let mut out = Vec::new();
        words.feed(origin, &chars(text), starts, |w, s| {
            out.push((w.to_string(), s));
        });
        out
    }

    #[test]
    fn words_start_at_their_first_character() {
        let mut w = Words::default();
        let got = collect(
            &mut w,
            ms(1000),
            "Hi you ",
            &[0.0, 60.0, 120.0, 130.0, 160.0, 190.0, 220.0],
        );
        assert_eq!(
            got,
            [("Hi".to_owned(), ms(1000)), ("you".to_owned(), ms(1130))]
        );
    }

    #[test]
    fn a_word_can_span_messages_and_leading_whitespace_is_skipped() {
        let mut w = Words::default();
        assert!(collect(&mut w, ms(0), " Hel", &[0.0, 10.0, 20.0, 30.0]).is_empty());
        let got = collect(&mut w, ms(100), "lo w", &[0.0, 10.0, 20.0, 30.0]);
        // "Hello" started at the 'H' of the first message.
        assert_eq!(got, [("Hello".to_owned(), ms(10))]);
        let mut rest = Vec::new();
        w.finish(|word, start| rest.push((word.to_string(), start)));
        assert_eq!(rest, [("w".to_owned(), ms(130))]);
        // Nothing left.
        w.finish(|_, _| panic!("no word expected"));
    }

    #[test]
    fn offsets_never_go_backwards_and_bad_times_are_clamped() {
        let mut w = Words::default();
        let got = collect(
            &mut w,
            ms(500),
            "a b c d e ",
            &[
                0.0,
                1.0,
                -5.0,
                1.0,
                f64::NAN,
                1.0,
                f64::INFINITY,
                1.0,
                1e300,
                1.0,
            ],
        );
        let starts: Vec<Duration> = got.iter().map(|(_, s)| *s).collect();
        assert_eq!(starts.len(), 5);
        assert!(starts.windows(2).all(|p| p[0] <= p[1]), "{starts:?}");
        assert_eq!(starts[0], ms(500));
        // A later message with a smaller origin cannot go back either.
        let got = collect(&mut w, ms(0), "z ", &[0.0, 1.0]);
        assert!(got[0].1 >= starts[4]);
    }

    #[test]
    fn missing_start_times_reuse_the_previous_one() {
        let mut w = Words::default();
        let got = collect(&mut w, ms(0), "ab cd ", &[10.0]);
        assert_eq!(got[0].1, ms(10));
        assert_eq!(got[1].1, ms(10));
    }

    #[test]
    fn multi_character_entries_and_non_ascii_are_handled() {
        let mut w = Words::default();
        let chars: Vec<String> = ["Mer", "ha", "ba", " ", "dü", "nya", " "]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let mut out = Vec::new();
        w.feed(
            ms(0),
            &chars,
            &[0.0, 30.0, 60.0, 90.0, 100.0, 130.0, 160.0],
            |wd, s| {
                out.push((wd.to_string(), s));
            },
        );
        assert_eq!(
            out,
            [("Merhaba".to_owned(), ms(0)), ("dünya".to_owned(), ms(100))]
        );
    }

    #[test]
    fn spoken_characters_ignore_whitespace() {
        assert_eq!(count_spoken(&chars("a b\tc\n")), 3);
        assert_eq!(count_spoken(&["ab".to_owned(), " ".to_owned()]), 2);
        assert_eq!(count_spoken::<String>(&[]), 0);
    }

    #[test]
    fn owed_follows_alignment_coverage_or_the_first_response() {
        let mut live = Live::new(ContextId(1), "x".to_owned(), Instant::now());
        assert!(!live.owed(true), "nothing sent");
        live.chars_sent = 10;
        assert!(live.owed(true) && live.owed(false));
        // Audio without alignment: the first response completes it.
        live.responded = true;
        assert!(!live.owed(true) && !live.owed(false));
        // Alignment seen: coverage decides.
        live.aligned_seen = true;
        live.chars_aligned = 4;
        assert!(live.owed(true));
        assert!(!live.owed(false), "coverage is ignored when switched off");
        // Normalized text longer than the input must not stand in for the plain count.
        live.plain_seen = true;
        live.chars_normalized = 12;
        assert!(
            live.owed(true),
            "the plain count decides once a plain alignment was seen"
        );
        live.chars_normalized = 0;
        live.chars_aligned = 10;
        assert!(!live.owed(true));
        // More text re-arms it.
        live.chars_sent = 15;
        assert!(live.owed(true));
        // A closed server context owes nothing to wait for.
        live.server_open = false;
        assert!(!live.awaiting_server(true));
    }

    #[test]
    fn the_normalized_count_decides_only_without_a_plain_alignment() {
        let mut live = Live::new(ContextId(1), "x".to_owned(), Instant::now());
        live.chars_sent = 8;
        live.aligned_seen = true;
        live.chars_normalized = 10;
        assert!(
            !live.owed(true),
            "normalized only: its count is all there is"
        );
        live.plain_seen = true;
        live.chars_aligned = 5;
        assert!(
            live.owed(true),
            "5 of 8 plain characters, however long the normalized text"
        );
        assert!(!live.covered(true));
        live.chars_aligned = 8;
        assert!(live.covered(true) && !live.owed(true));
    }

    #[test]
    fn a_short_alignment_degrades_only_after_a_response() {
        let mut live = Live::new(ContextId(1), "x".to_owned(), Instant::now());
        live.chars_sent = 8;
        live.aligned_seen = true;
        live.plain_seen = true;
        live.chars_aligned = 5;
        assert!(!live.degrade(true), "no response at all: a stall");
        live.responded = true;
        live.chars_sent = 40;
        assert!(!live.degrade(true), "most of the text unaligned: a stall");
        live.responded = false;
        live.chars_sent = 8;
        live.responded = true;
        assert!(!live.degrade(false), "coverage not in use");
        assert!(live.degrade(true));
        assert!(!live.owed(true), "falls back to the first response");
        assert!(!live.covered(true));
        // New text is owed again until the server responds to it.
        live.responded = false;
        assert!(live.owed(true));
        assert!(!live.degrade(true), "once is enough");
    }

    #[test]
    fn audio_duration_follows_the_format() {
        let mut live = Live::new(ContextId(1), "x".to_owned(), Instant::now());
        live.audio_bytes = 32_000;
        assert_eq!(
            live.alignment_origin(false, SampleRate::Hz16000, 2),
            ms(1000)
        );
        live.audio_bytes = 8_000;
        assert_eq!(
            live.alignment_origin(false, SampleRate::Hz8000, 1),
            ms(1000)
        );
    }

    #[test]
    fn the_sync_alignment_origin_is_the_audio_before_the_server_context() {
        let mut live = Live::new(ContextId(1), "x".to_owned(), Instant::now());
        live.audio_bytes = 32_000;
        assert_eq!(live.alignment_origin(true, SampleRate::Hz16000, 2), ms(0));
        live.reopen("y".to_owned(), Instant::now());
        live.audio_bytes = 48_000;
        assert_eq!(
            live.alignment_origin(true, SampleRate::Hz16000, 2),
            ms(1000)
        );
        assert_eq!(
            live.alignment_origin(false, SampleRate::Hz16000, 2),
            ms(1500)
        );
    }
}
