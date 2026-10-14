//! Played-text truncation: what the caller actually heard of an assistant message.
//!
//! When the caller barges in, the LLM context must hold only the words that were played, not the
//! whole response that was synthesized. A [`PlayedText`] tracks one assistant utterance: the text
//! sent to TTS (in order, possibly in several segments), the word timestamps the TTS reported (on
//! the utterance's own timeline, see `brisvox_core::frame::WordTimestamp`) and the playback
//! position, which the transport reports as the duration of the utterance's audio actually
//! played. [`PlayedText::heard`] turns that into a [`Truncation`]; [`apply_truncation`] writes it
//! into a [`Context`].
//!
//! # Mapping words to text
//! - Words are taken in start order (ties in arrival order). Timestamps may arrive late or out of
//!   order: the mapping is computed when `heard` is called, from whatever has arrived so far.
//! - A word counts as heard when playback has passed its start by at least
//!   [`PlayedConfig::min_word_played`] (and by more than zero): by default a word counts as soon
//!   as its first sample played. A word cut mid-way therefore counts with the default config and
//!   does not count when a minimum is set and not yet reached.
//! - Each word is located in the sent text, searching forward from where the previous word ended
//!   (so repeated words map to successive occurrences). The core of the word (without leading or
//!   trailing non-alphanumerics; a word with no alphanumerics, e.g. an emoji, is matched whole)
//!   must start and end on word boundaries in the text, so `ha` does not match inside `haha`.
//! - A word that is only punctuation (`.`, `!`, `…`) is placed at the next non-space position if
//!   the text has it there, and is otherwise zero-width: it never moves the cursor past text.
//! - The heard text is the sent text up to the end of the last heard word, including the
//!   punctuation attached to that word (`dünya!` keeps the `!`; `"Merhaba!"` keeps the closing
//!   quote). Opening brackets and quotes belong to the next word and are not kept. Everything
//!   before the last heard word is kept verbatim (commas, spacing, line breaks).
//! - A timed word that skips text no timestamp covers (the TTS lost a chunk) is a gap: the heard
//!   text stops there, since the caller may not have heard the lost text ([`Truncation::gaps`]).
//! - A heard word that cannot be located (rewritten by the provider, e.g. "bin" for "1000") is
//!   counted in [`Truncation::unmatched_words`]. Its text is known only when it is isolated: one
//!   such word between two timed words (or at the end) whose untimed text is exactly one
//!   whitespace token. Then that token is its text. Any other unlocatable word (a run of several
//!   rewrites, a rewrite that expands to several words such as "3.5" spoken as "üç nokta beş", or
//!   one whose text is not a single token) has unknown text, so the heard text stops before it
//!   and never extends past text that is not provably heard.
//! - Offsets are byte offsets on char boundaries of the sent text, so Turkish letters and emoji
//!   are safe.
//!
//! # Fallback without timestamps
//! When no word timestamp arrived for the utterance, the heard text is estimated from the played
//! duration at [`PlayedConfig::chars_per_second`] (default 14) and cut back to a whitespace
//! boundary. Such a [`Truncation`] has `estimated == true`; record it with
//! [`TruncationStats::record`] so the estimate shows up in metrics.
//!
//! # Applying to the context
//! [`apply_truncation`] works on the assistant message at a given index (the utterance's own
//! message, not necessarily the last one: the LLM may append more messages while the TTS is still
//! ahead of playback):
//! - heard text equal to the message text: nothing changes;
//! - heard text a shorter prefix: the message text is replaced. Its opaque `provider_data` is
//!   dropped when the message has no tool calls (it may embed the complete text, and the adapter
//!   re-derives it), and kept when it has tool calls (it may be a signature that must be echoed
//!   with those calls);
//! - nothing heard and no tool calls: the message is removed (an empty assistant message is
//!   invalid, see [`Context::validate`]);
//! - nothing heard but tool calls: the text is emptied and the calls are kept. The model did issue
//!   them and SideEffect tools may already have run (`docs/TOOLS.md`), so the call history must
//!   stay exactly as issued; only the spoken text is cut;
//! - the heard text is not a prefix of the message text, or the index is not an assistant
//!   message: an error, and the context is untouched.

use std::sync::Arc;
use std::time::Duration;

use crate::context::{Context, Message};

/// Default speaking rate for the fallback estimate, in characters per second.
pub const DEFAULT_CHARS_PER_SECOND: f64 = 14.0;

/// How the heard text is decided.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayedConfig {
    /// How much of a word must have played before the word counts as heard. Zero (the default)
    /// counts a word as soon as its first sample played; any word whose start is not yet passed
    /// by the playback position never counts.
    pub min_word_played: Duration,
    /// Speaking rate for the fallback estimate used when no word timestamps arrived.
    pub chars_per_second: f64,
}

impl Default for PlayedConfig {
    fn default() -> Self {
        Self {
            min_word_played: Duration::ZERO,
            chars_per_second: DEFAULT_CHARS_PER_SECOND,
        }
    }
}

/// What the caller heard of one assistant utterance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truncation {
    /// The heard prefix of the sent text (empty when nothing was heard).
    pub text: Arc<str>,
    /// `true` when the text is a char-rate estimate (no word timestamps arrived).
    pub estimated: bool,
    /// Words counted as heard (located or not; for an estimate, the whole words of the prefix).
    pub words_heard: usize,
    /// Heard words whose text could not be located as such (rewritten by the provider), whether
    /// or not their text was then resolved (never for an estimate).
    pub unmatched_words: usize,
    /// Places where the timed words skipped text that no word covered (the TTS lost part of the
    /// response), or where an unlocatable word's text is unknown. The heard text stops at the
    /// first such place: text the caller may not have heard is never kept.
    pub gaps: usize,
    /// The playback position the decision was made at.
    pub played: Duration,
}

/// Counters for truncations, for metrics. Flags estimates separately so a provider without
/// timestamps shows up as a rising `estimated` count.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TruncationStats {
    /// Truncations recorded.
    pub truncations: u64,
    /// Of those, char-rate estimates.
    pub estimated: u64,
    /// Heard words that could not be located in the sent text.
    pub unmatched_words: u64,
    /// Gaps in the timed text (see [`Truncation::gaps`]).
    pub gaps: u64,
}

impl TruncationStats {
    /// Counts one truncation.
    pub fn record(&mut self, truncation: &Truncation) {
        self.truncations = self.truncations.saturating_add(1);
        if truncation.estimated {
            self.estimated = self.estimated.saturating_add(1);
        }
        let unmatched = u64::try_from(truncation.unmatched_words).unwrap_or(u64::MAX);
        self.unmatched_words = self.unmatched_words.saturating_add(unmatched);
        let gaps = u64::try_from(truncation.gaps).unwrap_or(u64::MAX);
        self.gaps = self.gaps.saturating_add(gaps);
    }
}

/// A word the TTS reported for the utterance.
#[derive(Debug, Clone)]
struct TimedWord {
    start: Duration,
    word: Arc<str>,
    /// Arrival order, the tie-break for equal starts.
    seq: usize,
}

/// The sent text and timestamps of one assistant utterance (see the [module docs](self)).
#[derive(Debug, Default, Clone)]
pub struct PlayedText {
    sent: String,
    segments: usize,
    words: Vec<TimedWord>,
    next_seq: usize,
}

impl PlayedText {
    /// An empty tracker (nothing sent yet).
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one segment of the text sent to TTS. Segments are concatenated verbatim, in the
    /// order they were sent: the sent text must be the response's text, unchanged.
    pub fn push_sent(&mut self, segment: &str) {
        self.sent.push_str(segment);
        self.segments = self.segments.saturating_add(1);
    }

    /// Records a word the TTS timed. `start` is the offset from the utterance's first audio
    /// sample. May be called in any order and at any time before `heard`.
    pub fn push_word(&mut self, word: impl Into<Arc<str>>, start: Duration) {
        self.words.push(TimedWord {
            start,
            word: word.into(),
            seq: self.next_seq,
        });
        self.next_seq = self.next_seq.saturating_add(1);
    }

    /// The text sent so far, concatenated.
    pub fn sent(&self) -> &str {
        &self.sent
    }

    /// Number of text segments sent.
    pub const fn segments(&self) -> usize {
        self.segments
    }

    /// Whether any word timestamp has arrived (if not, `heard` estimates).
    pub fn has_timestamps(&self) -> bool {
        !self.words.is_empty()
    }

    /// What was heard when playback was at `played`: the sent text up to the last heard word, or
    /// a char-rate estimate when no timestamps arrived (see the [module docs](self)).
    pub fn heard(&self, played: Duration, config: &PlayedConfig) -> Truncation {
        if self.words.is_empty() {
            return self.estimate(played, config);
        }
        let mut order: Vec<&TimedWord> = self.words.iter().collect();
        order.sort_by_key(|w| (w.start, w.seq));
        let sent = self.sent.as_str();
        let placements = place(sent, &order);

        // `cut` is the heard prefix; it stops growing at the first blocking place. Words after a
        // blocker are still counted as heard (and unmatched) so the stats see them.
        let mut cut = 0usize;
        let mut blocked = false;
        let mut words_heard = 0usize;
        let mut unmatched_words = 0usize;
        let mut gaps = 0usize;
        for (_, placement) in order
            .iter()
            .zip(placements.iter())
            .take_while(|(w, _)| is_heard(w.start, played, config.min_word_played))
        {
            words_heard = words_heard.saturating_add(1);
            match *placement {
                Placement::Span {
                    end,
                    gap,
                    rewritten,
                } => {
                    if rewritten {
                        unmatched_words = unmatched_words.saturating_add(1);
                    }
                    if gap {
                        if !blocked {
                            gaps = gaps.saturating_add(1);
                        }
                        blocked = true;
                    } else if !blocked {
                        cut = cut.max(end);
                    }
                }
                Placement::Unplaced { gap_before } => {
                    unmatched_words = unmatched_words.saturating_add(1);
                    if gap_before && !blocked {
                        gaps = gaps.saturating_add(1);
                    }
                    blocked = true;
                }
            }
        }
        Truncation {
            text: Arc::from(sent.get(..cut).unwrap_or_default()),
            estimated: false,
            words_heard,
            unmatched_words,
            gaps,
            played,
        }
    }

    /// The fallback: `played` times the speaking rate, in chars, cut back to a whitespace.
    fn estimate(&self, played: Duration, config: &PlayedConfig) -> Truncation {
        let rate = if config.chars_per_second.is_finite() && config.chars_per_second > 0.0 {
            config.chars_per_second
        } else {
            0.0
        };
        // `as` saturates for negative, NaN and huge values, which is what we want here.
        let chars = (played.as_secs_f64() * rate).floor() as usize;
        let sent = self.sent.as_str();
        let limit = sent
            .char_indices()
            .nth(chars)
            .map_or(sent.len(), |(i, _)| i);
        let cut = if limit >= sent.len() {
            sent.len()
        } else if sent
            .get(limit..)
            .and_then(|tail| tail.chars().next())
            .is_some_and(char::is_whitespace)
        {
            limit
        } else {
            // Mid-word: back up to the last whitespace before the limit (or to nothing).
            sent.get(..limit)
                .and_then(|head| head.rfind(char::is_whitespace))
                .unwrap_or(0)
        };
        let text = sent.get(..cut).unwrap_or_default().trim_end();
        Truncation {
            text: Arc::from(text),
            estimated: true,
            words_heard: text.split_whitespace().count(),
            unmatched_words: 0,
            gaps: 0,
            played,
        }
    }
}

/// Where one timed word sits in the sent text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    /// In the text: the cut may move to `end` (attached punctuation included), unless `gap` is
    /// set, which blocks the heard text here. `rewritten` marks a word whose text was not found
    /// as such but resolved to the single token it stands for.
    Span {
        end: usize,
        gap: bool,
        rewritten: bool,
    },
    /// Its text is unknown. `gap_before` is set when untimed text precedes it.
    Unplaced { gap_before: bool },
}

/// Stores `value` at `index` (a no-op for an index out of range, which cannot happen for the
/// indices [`place`] uses).
fn set(placements: &mut [Placement], index: usize, value: Placement) {
    if let Some(slot) = placements.get_mut(index) {
        *slot = value;
    }
}

/// Places every timed word (in `order`, start order) in `sent`. See the module docs for the rules.
fn place(sent: &str, order: &[&TimedWord]) -> Vec<Placement> {
    let mut placements = vec![Placement::Unplaced { gap_before: false }; order.len()];
    // Where the previous located word ended (its punctuation included).
    let mut cursor = 0usize;
    // Indices of the words not located since the last located word: a run of rewrites.
    let mut run: Vec<usize> = Vec::new();
    for (i, timed) in order.iter().enumerate() {
        if is_punctuation(&timed.word) {
            let at = skip_whitespace(sent, cursor);
            if sent
                .get(at..)
                .is_some_and(|tail| tail.starts_with(&*timed.word))
            {
                run.clear();
                let end = at.saturating_add(timed.word.len());
                cursor = extend_over_punctuation(sent, end);
                set(
                    &mut placements,
                    i,
                    Placement::Span {
                        end: cursor,
                        gap: false,
                        rewritten: false,
                    },
                );
            } else {
                set(
                    &mut placements,
                    i,
                    Placement::Span {
                        end: cursor,
                        gap: false,
                        rewritten: false,
                    },
                );
            }
            continue;
        }
        let Some((start, end)) = locate(sent, cursor, &timed.word) else {
            run.push(i);
            continue;
        };
        let region_has_text = sent
            .get(cursor..start)
            .is_some_and(|region| region.chars().any(char::is_alphanumeric));
        if run.is_empty() {
            set(
                &mut placements,
                i,
                Placement::Span {
                    end: extend_over_punctuation(sent, end),
                    gap: region_has_text,
                    rewritten: false,
                },
            );
        } else {
            match (run.as_slice(), single_token(sent, cursor, start)) {
                ([only], Some((_, token_end))) => {
                    // One rewritten word for exactly one token: the token is its text.
                    set(
                        &mut placements,
                        *only,
                        Placement::Span {
                            end: extend_over_punctuation(sent, token_end),
                            gap: false,
                            rewritten: true,
                        },
                    );
                    set(
                        &mut placements,
                        i,
                        Placement::Span {
                            end: extend_over_punctuation(sent, end),
                            gap: false,
                            rewritten: false,
                        },
                    );
                }
                _ => {
                    for &k in &run {
                        set(
                            &mut placements,
                            k,
                            Placement::Unplaced {
                                gap_before: region_has_text,
                            },
                        );
                    }
                    set(
                        &mut placements,
                        i,
                        Placement::Span {
                            end: extend_over_punctuation(sent, end),
                            gap: true,
                            rewritten: false,
                        },
                    );
                }
            }
            run.clear();
        }
        cursor = extend_over_punctuation(sent, end);
    }
    // A run at the end of the text: resolved only as the single token that is left.
    if !run.is_empty() {
        let tail_has_text = sent
            .get(cursor..)
            .is_some_and(|tail| tail.chars().any(char::is_alphanumeric));
        match (run.as_slice(), single_token(sent, cursor, sent.len())) {
            ([only], Some((_, token_end))) => {
                set(
                    &mut placements,
                    *only,
                    Placement::Span {
                        end: extend_over_punctuation(sent, token_end),
                        gap: false,
                        rewritten: true,
                    },
                );
            }
            _ => {
                for &k in &run {
                    set(
                        &mut placements,
                        k,
                        Placement::Unplaced {
                            gap_before: tail_has_text,
                        },
                    );
                }
            }
        }
    }
    placements
}

/// Whether `playback` at `played` has passed `start` by at least `min` (and by more than zero).
fn is_heard(start: Duration, played: Duration, min: Duration) -> bool {
    let elapsed = played.saturating_sub(start);
    !elapsed.is_zero() && elapsed >= min
}

/// Locates `word` in `sent` at or after byte offset `from`, as a whole word. Returns the byte
/// span of its core (the word without leading and trailing non-alphanumerics).
fn locate(sent: &str, from: usize, word: &str) -> Option<(usize, usize)> {
    let core = word.trim_matches(|c: char| !c.is_alphanumeric());
    let needle = if core.is_empty() { word } else { core };
    let first = needle.chars().next()?;
    let last = needle.chars().next_back()?;
    let mut at = from;
    while let Some(rest) = sent.get(at..) {
        let found = rest.find(needle)?;
        let start = at.saturating_add(found);
        let end = start.saturating_add(needle.len());
        let before_ok = !first.is_alphanumeric()
            || sent
                .get(..start)
                .and_then(|head| head.chars().next_back())
                .is_none_or(|c| !c.is_alphanumeric());
        let after_ok = !last.is_alphanumeric()
            || sent
                .get(end..)
                .and_then(|tail| tail.chars().next())
                .is_none_or(|c| !c.is_alphanumeric());
        if before_ok && after_ok {
            return Some((start, end));
        }
        let step = sent
            .get(start..)
            .and_then(|tail| tail.chars().next())
            .map_or(1, char::len_utf8);
        at = start.saturating_add(step);
    }
    None
}

/// Whether a timed word is only punctuation (no letters, digits or spaces; emoji are not).
fn is_punctuation(word: &str) -> bool {
    !word.is_empty()
        && word.chars().all(|c| {
            !c.is_alphanumeric()
                && !c.is_whitespace()
                && (c.is_ascii_punctuation() || is_typographic_punctuation(c))
        })
}

/// Typographic punctuation outside ASCII (dashes, ellipsis, curly and guillemet quotes).
const fn is_typographic_punctuation(c: char) -> bool {
    matches!(
        c,
        '…' | '—' | '–' | '“' | '”' | '‘' | '’' | '«' | '»' | '‹' | '›' | '¡' | '¿'
    )
}

/// Punctuation that closes the word before it: sentence and clause marks and closing brackets
/// and quotes. Opening marks (`(`, `“`, `¡`) and dashes are not included.
const fn is_closing_punctuation(c: char) -> bool {
    matches!(
        c,
        '.' | ',' | '!' | '?' | ';' | ':' | '…' | ')' | ']' | '}' | '”' | '»' | '›'
    )
}

/// The index of the first non-whitespace char at or after `from` (the text length if none).
fn skip_whitespace(sent: &str, from: usize) -> usize {
    let tail = sent.get(from..).unwrap_or_default();
    from.saturating_add(tail.len().saturating_sub(tail.trim_start().len()))
}

/// The single whitespace-delimited token of `sent[from..to]`, as its byte span, if the range
/// holds exactly one token (whitespace around it is allowed).
fn single_token(sent: &str, from: usize, to: usize) -> Option<(usize, usize)> {
    let region = sent.get(from..to)?;
    let mut token: Option<(usize, usize)> = None;
    let mut open: Option<usize> = None;
    for (offset, c) in region.char_indices() {
        let at = from.saturating_add(offset);
        if c.is_whitespace() {
            if let Some(start) = open.take() {
                if token.is_some() {
                    return None;
                }
                token = Some((start, at));
            }
        } else if open.is_none() {
            open = Some(at);
        }
    }
    if let Some(start) = open {
        if token.is_some() {
            return None;
        }
        token = Some((start, to));
    }
    token
}

/// Moves `end` past the punctuation that closes the word before it (see
/// [`is_closing_punctuation`]). An ASCII quote closes only when a space, the end of the text or
/// closing punctuation follows it; before a letter it opens a quotation or is an apostrophe.
fn extend_over_punctuation(sent: &str, end: usize) -> usize {
    let mut end = end;
    while let Some(c) = sent.get(end..).and_then(|tail| tail.chars().next()) {
        let attaches = match c {
            '"' | '\'' => ascii_quote_closes(sent, end.saturating_add(c.len_utf8())),
            other => is_closing_punctuation(other),
        };
        if !attaches {
            break;
        }
        end = end.saturating_add(c.len_utf8());
    }
    end
}

/// Whether an ASCII quote whose next byte is at `after` closes a quotation.
fn ascii_quote_closes(sent: &str, after: usize) -> bool {
    match sent.get(after..).and_then(|tail| tail.chars().next()) {
        None => true,
        Some(next) => next.is_whitespace() || is_closing_punctuation(next),
    }
}

/// Why a truncation could not be applied to a context. The context is left unchanged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TruncateError {
    /// The message at the index is not an assistant message (or there is none there).
    #[error("the message to truncate is not an assistant message")]
    NoAssistant,
    /// The heard text is not a prefix of the assistant message text (the sent text and the
    /// stored message disagree).
    #[error("the heard text is not a prefix of the assistant message")]
    NotAPrefix,
}

/// What [`apply_truncation`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncationOutcome {
    /// The message already held exactly the heard text.
    Unchanged,
    /// The message text was replaced by the heard prefix.
    Shortened,
    /// Nothing was heard and the message had no tool calls: the message was removed.
    Removed,
}

/// Index of the last assistant message of `context`, if there is one.
pub fn last_assistant_index(context: &Context) -> Option<usize> {
    context
        .messages()
        .iter()
        .rposition(|m| matches!(m, Message::Assistant { .. }))
}

/// Replaces the text of the assistant message at `index` of `context` with what was heard (see
/// the [module docs](self) for the rules). `index` is the utterance's own message: a later
/// assistant message does not change which one is cut.
///
/// # Errors
/// [`TruncateError::NoAssistant`] when the message at `index` is not an assistant message, and
/// [`TruncateError::NotAPrefix`] when the heard text does not start the message text. The
/// context is unchanged in both cases.
pub fn apply_truncation(
    context: &mut Context,
    index: usize,
    truncation: &Truncation,
) -> Result<TruncationOutcome, TruncateError> {
    let messages = context.messages_mut();
    let remove = match messages.get_mut(index) {
        Some(Message::Assistant {
            text,
            tool_calls,
            provider_data,
        }) => {
            if truncation.text.is_empty() && tool_calls.is_empty() {
                // An empty assistant message is invalid whatever it held before.
                true
            } else if **text == *truncation.text {
                return Ok(TruncationOutcome::Unchanged);
            } else if !text.starts_with(&*truncation.text) {
                return Err(TruncateError::NotAPrefix);
            } else {
                *text = Arc::clone(&truncation.text);
                if tool_calls.is_empty() {
                    *provider_data = None;
                }
                false
            }
        }
        _ => return Err(TruncateError::NoAssistant),
    };
    if remove {
        messages.remove(index);
        Ok(TruncationOutcome::Removed)
    } else {
        Ok(TruncationOutcome::Shortened)
    }
}

#[cfg(test)]
mod tests {
    use brisvox_core::TurnEpoch;

    use super::*;
    use crate::context::{ToolCall, ToolCallId};

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    /// Words at the given starts (ms), in order.
    fn tracker(segments: &[&str], words: &[(&str, u64)]) -> PlayedText {
        let mut played = PlayedText::new();
        for segment in segments {
            played.push_sent(segment);
        }
        for (word, start) in words {
            played.push_word(*word, ms(*start));
        }
        played
    }

    fn heard_text(played: &PlayedText, at_ms: u64) -> String {
        played
            .heard(ms(at_ms), &PlayedConfig::default())
            .text
            .to_string()
    }

    #[test]
    fn punctuation_and_spacing_are_kept_up_to_the_last_heard_word() {
        let p = tracker(
            &["Merhaba, dünya! Nasılsın?  İyiyim.\nTeşekkürler"],
            &[
                ("Merhaba", 0),
                ("dünya", 300),
                ("Nasılsın", 600),
                ("İyiyim", 900),
                ("Teşekkürler", 1200),
            ],
        );
        assert_eq!(heard_text(&p, 0), "");
        assert_eq!(heard_text(&p, 1), "Merhaba,");
        assert_eq!(heard_text(&p, 301), "Merhaba, dünya!");
        assert_eq!(heard_text(&p, 601), "Merhaba, dünya! Nasılsın?");
        assert_eq!(heard_text(&p, 901), "Merhaba, dünya! Nasılsın?  İyiyim.");
        assert_eq!(
            heard_text(&p, 1300),
            "Merhaba, dünya! Nasılsın?  İyiyim.\nTeşekkürler"
        );
    }

    #[test]
    fn a_word_counts_once_started_by_default_and_after_a_minimum_otherwise() {
        let p = tracker(
            &["Merhaba, dünya! Nasılsın?"],
            &[("Merhaba", 0), ("dünya", 300)],
        );
        // 20 ms into "dünya": started, so counted by default.
        assert_eq!(heard_text(&p, 320), "Merhaba, dünya!");
        // Exactly at its start: nothing of it played.
        assert_eq!(heard_text(&p, 300), "Merhaba,");
        let strict = PlayedConfig {
            min_word_played: ms(100),
            ..PlayedConfig::default()
        };
        assert_eq!(p.heard(ms(320), &strict).text.as_ref(), "Merhaba,");
        assert_eq!(p.heard(ms(410), &strict).text.as_ref(), "Merhaba, dünya!");
    }

    #[test]
    fn words_split_across_tts_segments_map_on_the_joined_text() {
        let p = tracker(
            &["Merhaba, dün", "ya! Nasıl", "sın?"],
            &[("Merhaba", 0), ("dünya", 300), ("Nasılsın", 600)],
        );
        assert_eq!(p.segments(), 3);
        assert_eq!(p.sent(), "Merhaba, dünya! Nasılsın?");
        assert_eq!(heard_text(&p, 301), "Merhaba, dünya!");
        assert_eq!(heard_text(&p, 700), "Merhaba, dünya! Nasılsın?");
    }

    #[test]
    fn repeated_words_map_to_successive_occurrences() {
        let p = tracker(&["ha ha ha!"], &[("ha", 0), ("ha", 200), ("ha", 400)]);
        assert_eq!(heard_text(&p, 1), "ha");
        assert_eq!(heard_text(&p, 201), "ha ha");
        assert_eq!(heard_text(&p, 401), "ha ha ha!");
    }

    #[test]
    fn a_word_does_not_match_inside_a_longer_word() {
        let p = tracker(&["haha ha"], &[("haha", 0), ("ha", 100)]);
        assert_eq!(heard_text(&p, 101), "haha ha");
        // A timed "ha" skips the "ha" inside "haha" (no whole-word match there) and maps to the
        // standalone word; the untimed "haha" before it is a gap, so nothing is kept.
        let p = tracker(&["haha ha"], &[("ha", 0)]);
        let t = p.heard(ms(1), &PlayedConfig::default());
        assert_eq!(t.text.as_ref(), "");
        assert_eq!(t.gaps, 1);
        // "hah" occurs only inside "haha": unlocatable, and its text is not one token (the
        // tail is "haha ha"), so nothing is kept.
        let p = tracker(&["haha ha"], &[("hah", 0)]);
        let t = p.heard(ms(1), &PlayedConfig::default());
        assert_eq!(t.text.as_ref(), "");
        assert_eq!(t.unmatched_words, 1);
        assert_eq!(t.gaps, 1);
    }

    #[test]
    fn turkish_letters_map_on_char_boundaries() {
        let p = tracker(
            &["Işık, ığdır, İstanbul. Çok güzel."],
            &[
                ("Işık", 0),
                ("ığdır", 250),
                ("İstanbul", 500),
                ("Çok", 800),
                ("güzel", 1000),
            ],
        );
        assert_eq!(heard_text(&p, 251), "Işık, ığdır,");
        assert_eq!(heard_text(&p, 501), "Işık, ığdır, İstanbul.");
        assert_eq!(heard_text(&p, 1001), "Işık, ığdır, İstanbul. Çok güzel.");
    }

    #[test]
    fn emoji_words_are_matched_whole_and_keep_their_punctuation() {
        let p = tracker(
            &["Harika 🙂 gidiyoruz 🚀!"],
            &[("Harika", 0), ("🙂", 300), ("gidiyoruz", 600), ("🚀", 900)],
        );
        assert_eq!(heard_text(&p, 301), "Harika 🙂");
        assert_eq!(heard_text(&p, 601), "Harika 🙂 gidiyoruz");
        assert_eq!(heard_text(&p, 901), "Harika 🙂 gidiyoruz 🚀!");
    }

    #[test]
    fn a_word_the_text_does_not_contain_is_counted_and_resolved_when_isolated() {
        // The provider wrote "bin" for "1000": one word for one token, so the token is its text.
        let p = tracker(&["1000 kişi geldi"], &[("bin", 0), ("kişi", 300)]);
        let t = p.heard(ms(301), &PlayedConfig::default());
        assert_eq!(t.text.as_ref(), "1000 kişi");
        assert_eq!(t.words_heard, 2);
        assert_eq!(t.unmatched_words, 1);
        assert_eq!(t.gaps, 0);
        assert!(!t.estimated);
        // Only the rewritten word has played: it covers the token "1000".
        let t = p.heard(ms(1), &PlayedConfig::default());
        assert_eq!(t.text.as_ref(), "1000");
        assert_eq!(t.unmatched_words, 1);
        assert_eq!(t.gaps, 0);
    }

    #[test]
    fn an_expanded_rewrite_stops_the_heard_text_instead_of_taking_the_next_token() {
        // "3.5" is spoken as three words and "kg" as "kilogram": the timed words cannot say which
        // text each covers, so nothing past "Oran" is provably heard, and "kg" is not kept.
        let p = tracker(
            &["Oran 3.5 kg ağır"],
            &[
                ("Oran", 0),
                ("üç", 100),
                ("nokta", 200),
                ("beş", 300),
                ("kilogram", 400),
                ("ağır", 500),
            ],
        );
        let t = p.heard(ms(250), &PlayedConfig::default());
        assert_eq!(t.text.as_ref(), "Oran");
        assert_eq!(t.unmatched_words, 2);
        assert_eq!(t.gaps, 1);
        assert_eq!(heard_text(&p, 501), "Oran");
    }

    #[test]
    fn a_rewrite_followed_by_untimed_text_is_a_gap_and_is_not_extended() {
        // The TTS timed "Merhaba" and then "bin" (for "1000") after text it lost: the lost text
        // is not one token, so the heard text stops after "Merhaba,".
        let p = tracker(
            &["Merhaba, uzun uzun bir cumle 1000 kisi"],
            &[("Merhaba", 0), ("bin", 300)],
        );
        let t = p.heard(ms(301), &PlayedConfig::default());
        assert_eq!(t.text.as_ref(), "Merhaba,");
        assert_eq!(t.unmatched_words, 1);
        assert_eq!(t.gaps, 1);
    }

    #[test]
    fn two_rewrites_in_a_row_are_not_resolved() {
        let p = tracker(
            &["Bir 1000 2024 son"],
            &[("Bir", 0), ("bin", 100), ("yüz", 200)],
        );
        let t = p.heard(ms(250), &PlayedConfig::default());
        assert_eq!(t.text.as_ref(), "Bir");
        assert_eq!(t.unmatched_words, 2);
    }

    #[test]
    fn punctuation_timed_as_its_own_word_is_not_a_gap() {
        // The TTS timed each punctuation mark as a word: the marks must not move the cut.
        let p = tracker(
            &["A. B. C."],
            &[
                ("A", 0),
                (".", 100),
                ("B", 200),
                (".", 300),
                ("C", 400),
                (".", 500),
            ],
        );
        assert_eq!(heard_text(&p, 250), "A. B.");
        let t = p.heard(ms(250), &PlayedConfig::default());
        assert_eq!(t.gaps, 0);
        assert_eq!(t.unmatched_words, 0);
        assert_eq!(heard_text(&p, 1), "A.");
        assert_eq!(heard_text(&p, 1000), "A. B. C.");
    }

    #[test]
    fn a_spaced_punctuation_word_is_placed_at_the_next_non_space_position() {
        let p = tracker(
            &["Hi , there ."],
            &[("Hi", 0), (",", 100), ("there", 200), (".", 300)],
        );
        assert_eq!(heard_text(&p, 150), "Hi ,");
        assert_eq!(heard_text(&p, 350), "Hi , there .");
    }

    #[test]
    fn late_and_out_of_order_timestamps_are_used_when_they_arrive() {
        let mut p = PlayedText::new();
        p.push_sent("Bir iki üç dört.");
        // The last word arrives first, the first one last.
        p.push_word("dört", ms(900));
        // Until the words before it arrive, "dört" sits behind an untimed gap: nothing is kept
        // beyond what is known to be heard.
        assert_eq!(heard_text(&p, 1000), "");
        p.push_word("üç", ms(600));
        p.push_word("iki", ms(300));
        assert_eq!(heard_text(&p, 350), "");
        p.push_word("Bir", ms(0));
        assert_eq!(heard_text(&p, 350), "Bir iki");
        assert_eq!(heard_text(&p, 650), "Bir iki üç");
        assert_eq!(heard_text(&p, 1000), "Bir iki üç dört.");
        assert!(p.has_timestamps());
    }

    #[test]
    fn a_gap_in_the_timed_text_stops_the_heard_text() {
        // The TTS lost "here is a fairly long answer to": the words after it are timed, but the
        // caller never heard that text, so the context keeps only what precedes the gap.
        let p = tracker(
            &["Sure, here is a fairly long answer to your question."],
            &[("Sure", 0), ("your", 300), ("question", 600)],
        );
        let t = p.heard(ms(700), &PlayedConfig::default());
        assert_eq!(t.text.as_ref(), "Sure,");
        assert_eq!(t.words_heard, 3);
        assert_eq!(t.gaps, 1);
        // Gapless timelines are unaffected.
        let p = tracker(
            &["Sure, here is your question."],
            &[("Sure", 0), ("here", 300), ("is", 600), ("your", 900)],
        );
        let t = p.heard(ms(1000), &PlayedConfig::default());
        assert_eq!(t.text.as_ref(), "Sure, here is your");
        assert_eq!(t.gaps, 0);
    }

    #[test]
    fn an_opening_bracket_or_quote_after_a_heard_word_is_not_kept() {
        let p = tracker(&["Merhaba,(dünya)"], &[("Merhaba", 0), ("dünya", 300)]);
        assert_eq!(heard_text(&p, 1), "Merhaba,");
        assert_eq!(heard_text(&p, 301), "Merhaba,(dünya)");
        let p = tracker(&["Merhaba,\"dünya\""], &[("Merhaba", 0), ("dünya", 300)]);
        assert_eq!(heard_text(&p, 1), "Merhaba,");
        assert_eq!(heard_text(&p, 301), "Merhaba,\"dünya\"");
        let p = tracker(&["Merhaba “dünya”"], &[("Merhaba", 0), ("dünya", 300)]);
        assert_eq!(heard_text(&p, 1), "Merhaba");
        assert_eq!(heard_text(&p, 301), "Merhaba “dünya”");
    }

    #[test]
    fn a_closing_quote_after_a_heard_word_is_kept() {
        let p = tracker(&["\"Merhaba!\" dedi"], &[("Merhaba", 0), ("dedi", 300)]);
        assert_eq!(heard_text(&p, 1), "\"Merhaba!\"");
        assert_eq!(heard_text(&p, 301), "\"Merhaba!\" dedi");
        let p = tracker(&["ÇOK İYİ” dedi"], &[("ÇOK", 0), ("İYİ", 300)]);
        assert_eq!(heard_text(&p, 1), "ÇOK");
        assert_eq!(heard_text(&p, 301), "ÇOK İYİ”");
    }

    #[test]
    fn without_timestamps_the_heard_text_is_estimated_at_word_boundaries() {
        let mut p = PlayedText::new();
        p.push_sent("Merhaba, dünya! Nasılsın?");
        assert!(!p.has_timestamps());
        let config = PlayedConfig::default(); // 14 chars/s
        // 14 chars: the cut would split "dünya!", so back up to the space after "Merhaba,".
        let t = p.heard(Duration::from_secs(1), &config);
        assert!(t.estimated);
        assert_eq!(t.text.as_ref(), "Merhaba,");
        assert_eq!(t.words_heard, 1);
        // 15 chars: the cut lands on the space after "dünya!".
        let fast = PlayedConfig {
            chars_per_second: 15.0,
            ..config
        };
        let t = p.heard(Duration::from_secs(1), &fast);
        assert_eq!(t.text.as_ref(), "Merhaba, dünya!");
        assert_eq!(t.words_heard, 2);
        // Nothing played, and everything played.
        assert_eq!(p.heard(Duration::ZERO, &config).text.as_ref(), "");
        assert_eq!(
            p.heard(Duration::from_secs(60), &config).text.as_ref(),
            "Merhaba, dünya! Nasılsın?"
        );
    }

    #[test]
    fn the_estimate_counts_chars_not_bytes_and_flags_itself() {
        let mut p = PlayedText::new();
        p.push_sent("ğüşiöç ğüşiöç 🙂🙂 abc");
        let config = PlayedConfig {
            chars_per_second: 6.0,
            ..PlayedConfig::default()
        };
        let t = p.heard(Duration::from_secs(1), &config);
        assert!(t.estimated);
        assert_eq!(t.text.as_ref(), "ğüşiöç");
        // A budget of 10 chars ends inside the second word: back up to the first word.
        let config = PlayedConfig {
            chars_per_second: 10.0,
            ..PlayedConfig::default()
        };
        let t = p.heard(Duration::from_secs(1), &config);
        assert_eq!(t.text.as_ref(), "ğüşiöç");
        // 14 chars: the second word is cut mid-way (char 14 is the emoji), so back up to its
        // preceding space: the first two words only.
        let config = PlayedConfig {
            chars_per_second: 14.0,
            ..PlayedConfig::default()
        };
        let t = p.heard(Duration::from_secs(1), &config);
        assert_eq!(t.text.as_ref(), "ğüşiöç ğüşiöç");
        assert_eq!(t.unmatched_words, 0);
    }

    #[test]
    fn stats_count_estimates_and_unmatched_words() {
        let timed = tracker(&["1000 kişi"], &[("bin", 0), ("kişi", 300)]);
        let mut untimed = PlayedText::new();
        untimed.push_sent("Merhaba dünya");
        let mut stats = TruncationStats::default();
        stats.record(&timed.heard(ms(301), &PlayedConfig::default()));
        stats.record(&untimed.heard(Duration::from_secs(1), &PlayedConfig::default()));
        assert_eq!(
            stats,
            TruncationStats {
                truncations: 2,
                estimated: 1,
                unmatched_words: 1,
                gaps: 0,
            }
        );
    }

    fn call(id: ToolCallId) -> ToolCall {
        ToolCall {
            id,
            provider_id: None,
            name: Arc::from("lookup"),
            arguments: serde_json::json!({"q": "x"}),
            provider_data: None,
        }
    }

    fn truncation(text: &str) -> Truncation {
        Truncation {
            text: Arc::from(text),
            estimated: false,
            words_heard: 0,
            unmatched_words: 0,
            gaps: 0,
            played: Duration::ZERO,
        }
    }

    #[test]
    fn applying_replaces_the_assistant_text_and_keeps_the_context_valid() {
        let mut ctx = Context::with_system("sys");
        ctx.push(Message::user("merhaba"));
        ctx.push(Message::assistant("Merhaba, dünya! Nasılsın?"));
        let outcome = apply_truncation(&mut ctx, 2, &truncation("Merhaba, dünya!")).ok();
        assert_eq!(outcome, Some(TruncationOutcome::Shortened));
        assert_eq!(
            ctx.messages().last().map(Message::text),
            Some("Merhaba, dünya!")
        );
        assert!(ctx.validate().is_ok());

        let same = apply_truncation(&mut ctx, 2, &truncation("Merhaba, dünya!")).ok();
        assert_eq!(same, Some(TruncationOutcome::Unchanged));
    }

    #[test]
    fn the_cut_targets_the_utterances_own_message_not_the_last_assistant_message() {
        // Assistant A is still being played when the LLM appends assistant B after a tool round.
        let mut ctx = Context::new();
        ctx.push(Message::user("anlat"));
        ctx.push(Message::assistant("Birinci cümle, ikinci cümle."));
        ctx.push(Message::assistant("Yeni cevap."));
        assert_eq!(last_assistant_index(&ctx), Some(2));
        let outcome = apply_truncation(&mut ctx, 1, &truncation("Birinci cümle,")).ok();
        assert_eq!(outcome, Some(TruncationOutcome::Shortened));
        assert_eq!(
            ctx.messages().get(1).map(Message::text),
            Some("Birinci cümle,")
        );
        assert_eq!(
            ctx.messages().get(2).map(Message::text),
            Some("Yeni cevap.")
        );
        // The message at the index is not the assistant one: nothing changes.
        assert_eq!(
            apply_truncation(&mut ctx, 0, &truncation("")),
            Err(TruncateError::NoAssistant)
        );
        assert_eq!(ctx.len(), 3);
    }

    #[test]
    fn nothing_heard_removes_a_text_only_assistant_message() {
        let mut ctx = Context::new();
        ctx.push(Message::user("merhaba"));
        ctx.push(Message::assistant("Merhaba!"));
        let outcome = apply_truncation(&mut ctx, 1, &truncation("")).ok();
        assert_eq!(outcome, Some(TruncationOutcome::Removed));
        assert_eq!(ctx.len(), 1);
        assert!(ctx.validate().is_ok());
    }

    #[test]
    fn an_empty_assistant_message_is_removed_even_when_nothing_changes() {
        // A response that never produced text (a stalled provider) leaves an empty message: the
        // heard text is empty too, but the message must not stay in the context.
        let mut ctx = Context::new();
        ctx.push(Message::user("merhaba"));
        ctx.push(Message::assistant(""));
        let outcome = apply_truncation(&mut ctx, 1, &truncation("")).ok();
        assert_eq!(outcome, Some(TruncationOutcome::Removed));
        assert_eq!(ctx.len(), 1);
        assert!(ctx.validate().is_ok());
    }

    #[test]
    fn nothing_heard_keeps_the_tool_calls_of_an_assistant_message() {
        let epoch = TurnEpoch::new(3);
        let id = ToolCallId::new(epoch, 0);
        let mut ctx = Context::new();
        ctx.push(Message::user("hava nasıl"));
        ctx.push(Message::assistant_with_calls(
            "Bakıyorum.",
            vec![call(id.clone())],
        ));
        // The tool ran before the caller barged in: its result still follows the call.
        ctx.push(Message::tool_result(id, "{\"temp\":21}"));
        let outcome = apply_truncation(&mut ctx, 1, &truncation("")).ok();
        assert_eq!(outcome, Some(TruncationOutcome::Shortened));
        let Some(Message::Assistant {
            text, tool_calls, ..
        }) = ctx.messages().get(1)
        else {
            panic!("assistant message expected");
        };
        assert!(text.is_empty());
        assert_eq!(tool_calls.len(), 1);
        assert!(ctx.validate().is_ok());
    }

    #[test]
    fn provider_data_is_dropped_for_text_and_kept_for_tool_calls() {
        let signature = serde_json::json!({"signature": "sig"});
        let mut ctx = Context::new();
        ctx.push(Message::Assistant {
            text: Arc::from("Bir iki üç."),
            tool_calls: Vec::new(),
            provider_data: Some(signature.clone()),
        });
        apply_truncation(&mut ctx, 0, &truncation("Bir")).ok();
        let Some(Message::Assistant { provider_data, .. }) = ctx.messages().first() else {
            panic!("assistant message expected");
        };
        assert_eq!(provider_data, &None);

        let id = ToolCallId::new(TurnEpoch::new(1), 0);
        let mut ctx = Context::new();
        ctx.push(Message::Assistant {
            text: Arc::from("Bakıyorum, bir iki."),
            tool_calls: vec![call(id.clone())],
            provider_data: Some(signature.clone()),
        });
        ctx.push(Message::tool_result(id, "{}"));
        apply_truncation(&mut ctx, 0, &truncation("Bakıyorum,")).ok();
        let Some(Message::Assistant {
            text,
            provider_data,
            ..
        }) = ctx.messages().first()
        else {
            panic!("assistant message expected");
        };
        assert_eq!(text.as_ref(), "Bakıyorum,");
        assert_eq!(provider_data, &Some(signature));
        assert!(ctx.validate().is_ok());
    }

    #[test]
    fn a_heard_text_that_is_not_a_prefix_leaves_the_context_alone() {
        let mut ctx = Context::new();
        ctx.push(Message::assistant("Merhaba"));
        let before = ctx.clone();
        assert_eq!(
            apply_truncation(&mut ctx, 0, &truncation("Selam")),
            Err(TruncateError::NotAPrefix)
        );
        assert_eq!(ctx, before);
        let mut empty = Context::new();
        empty.push(Message::user("merhaba"));
        assert_eq!(
            apply_truncation(&mut empty, 0, &truncation("")),
            Err(TruncateError::NoAssistant)
        );
        assert_eq!(
            apply_truncation(&mut empty, 5, &truncation("")),
            Err(TruncateError::NoAssistant)
        );
    }

    /// Deterministic generator (splitmix64) for the randomized test.
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: usize) -> usize {
            usize::try_from(self.next_u64() % u64::try_from(n).unwrap_or(1)).unwrap_or(0)
        }

        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items.get(self.below(items.len())).copied().unwrap_or("")
        }
    }

    /// How the TTS timed one token of a generated reply.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Timing {
        /// Timed with its own text.
        Timed,
        /// Timed, but the provider wrote a different word for it.
        Rewritten,
        /// No timestamp: the TTS lost it.
        Untimed,
    }

    /// One generated token: the core word, the punctuation attached to it, the separator after
    /// it, its timing, and whether its attached punctuation is timed as a word of its own.
    struct Token {
        core: &'static str,
        attached: &'static str,
        sep: &'static str,
        timing: Timing,
        punct_timed: bool,
        start: Duration,
    }

    /// Cores that are timed as themselves (no two of them is a rewrite or a lost token).
    const CORES: &[&str] = &[
        "merhaba",
        "dünya",
        "Işık",
        "ığdır",
        "İstanbul",
        "çok",
        "güzel",
        "ha",
        "ha",
        "şöyle",
        "bir",
        "iki",
        "kişi",
        "🙂",
        "🚀",
        "gözlük",
        "Ğ",
        "a",
    ];
    /// Cores of rewritten tokens: never timed as themselves, so they cannot be mislocated.
    const REWRITTEN_CORES: &[&str] = &["1000", "2024", "45"];
    /// What the provider said instead: words that do not occur in the text.
    const REPLACEMENTS: &[&str] = &["bin", "yüz", "xxx"];
    /// The core of an untimed (lost) token: never timed, so it cannot be mislocated either.
    const LOST_CORE: &str = "kayıp";
    /// Punctuation that closes a word (the only kind that attaches to a word).
    const ATTACHED: &[&str] = &["", "", "", ",", "!", "?", "…", ".", ")", "”"];
    const SEPARATORS: &[&str] = &[" ", " ", " ", "  ", "\n"];

    impl Token {
        /// The token's own text: core, attached punctuation (the separator is not part of it).
        fn text(&self) -> String {
            format!("{}{}", self.core, self.attached)
        }
    }

    /// Generates a reply of 1 to 12 tokens with random timings (see [`Timing`]).
    fn random_reply(rng: &mut Rng) -> Vec<Token> {
        let count = 1 + rng.below(12);
        let mut at = Duration::from_millis(u64::try_from(rng.below(200)).unwrap_or(0));
        (0..count)
            .map(|i| {
                let roll = rng.below(100);
                let timing = match roll {
                    0..=74 => Timing::Timed,
                    75..=86 => Timing::Rewritten,
                    _ => Timing::Untimed,
                };
                let core = match timing {
                    Timing::Timed => rng.pick(CORES),
                    Timing::Rewritten => rng.pick(REWRITTEN_CORES),
                    Timing::Untimed => LOST_CORE,
                };
                let attached = rng.pick(ATTACHED);
                let sep = if i + 1 == count {
                    ""
                } else {
                    rng.pick(SEPARATORS)
                };
                let start = at;
                at += Duration::from_millis(40 + u64::try_from(rng.below(660)).unwrap_or(0));
                Token {
                    core,
                    attached,
                    sep,
                    timing,
                    punct_timed: timing == Timing::Timed
                        && !attached.is_empty()
                        && rng.below(3) == 0,
                    start,
                }
            })
            .collect()
    }

    /// The word events a provider would send for `tokens`, in no particular order.
    fn events(tokens: &[Token], rng: &mut Rng) -> Vec<(Arc<str>, Duration)> {
        let mut out: Vec<(Arc<str>, Duration)> = Vec::new();
        for token in tokens {
            match token.timing {
                Timing::Timed => {
                    out.push((Arc::from(token.core), token.start));
                    if token.punct_timed {
                        out.push((Arc::from(token.attached), token.start + ms(1)));
                    }
                }
                Timing::Rewritten => out.push((Arc::from(rng.pick(REPLACEMENTS)), token.start)),
                Timing::Untimed => {}
            }
        }
        for i in (1..out.len()).rev() {
            out.swap(i, rng.below(i + 1));
        }
        out
    }

    /// Whether the generated timings are the ones the mapping resolves exactly: no rewritten
    /// token next to another rewrite or a lost token (those leave the text unknown, and the
    /// heard text then only stops earlier).
    fn resolves_exactly(tokens: &[Token]) -> bool {
        tokens.iter().enumerate().all(|(i, token)| {
            token.timing != Timing::Rewritten
                || [i.checked_sub(1), Some(i + 1)]
                    .into_iter()
                    .flatten()
                    .filter_map(|j| tokens.get(j))
                    .all(|n| n.timing == Timing::Timed)
        })
    }

    #[test]
    fn random_timings_leave_exactly_the_heard_words_or_stop_before_unknown_text() {
        let mut rng = Rng(0x0000_05EE_D031);
        let mut exact_cases = 0usize;
        let mut rewrites_resolved = 0usize;
        let mut punct_marks = 0usize;
        for case in 0..400 {
            let tokens = random_reply(&mut rng);
            let text: String = tokens
                .iter()
                .map(|t| format!("{}{}", t.text(), t.sep))
                .collect();
            // Where each token's own text ends in `text`.
            let mut ends = Vec::with_capacity(tokens.len());
            let mut at = 0usize;
            for token in &tokens {
                at += token.text().len();
                ends.push(at);
                at += token.sep.len();
            }
            // Playback position: somewhere before, inside, or after the reply.
            let last = tokens.last().map_or(Duration::ZERO, |t| t.start);
            let played = ms(u64::try_from(
                rng.below(usize::try_from(last.as_millis() + 400).unwrap_or(1)),
            )
            .unwrap_or(0));

            // Oracle: the text of the leading tokens that are timed and started before `played`;
            // a lost token ends the run (the design stops at a gap).
            let mut expected_end = 0usize;
            for (i, token) in tokens.iter().enumerate() {
                if token.timing == Timing::Untimed || token.start >= played {
                    break;
                }
                expected_end = ends.get(i).copied().unwrap_or(0);
            }
            let expected = text.get(..expected_end).unwrap_or_default();
            // Every rewritten token that started playing counts as unmatched, past a gap too.
            let heard_rewrites = tokens
                .iter()
                .filter(|t| t.timing == Timing::Rewritten && t.start < played)
                .count();

            // Segments cut at random char boundaries, timed words pushed in shuffled order.
            let mut cuts: Vec<usize> = (1..text.len())
                .filter(|i| text.is_char_boundary(*i))
                .filter(|_| rng.below(4) == 0)
                .collect();
            cuts.insert(0, 0);
            cuts.push(text.len());
            let mut tracker = PlayedText::new();
            for pair in cuts.windows(2) {
                let (a, b) = (
                    pair.first().copied().unwrap_or(0),
                    pair.get(1).copied().unwrap_or(0),
                );
                tracker.push_sent(text.get(a..b).unwrap_or_default());
            }
            let timed = events(&tokens, &mut rng);
            if timed.is_empty() {
                // No timestamp at all: the estimate path, covered by its own tests.
                continue;
            }
            punct_marks += timed.iter().filter(|(w, _)| is_punctuation(w)).count();
            for (word, start) in timed {
                tracker.push_word(word, start);
            }

            let truncation = tracker.heard(played, &PlayedConfig::default());
            let got = truncation.text.as_ref();
            assert!(!truncation.estimated, "case {case}");
            assert!(
                expected.starts_with(got),
                "case {case}: heard {got:?} is not a prefix of {expected:?} (text {text:?}, played {played:?})"
            );
            if resolves_exactly(&tokens) {
                exact_cases += 1;
                assert_eq!(got, expected, "case {case}: {text:?} at {played:?}");
                assert_eq!(truncation.unmatched_words, heard_rewrites, "case {case}");
                rewrites_resolved += heard_rewrites;
            }

            let mut ctx = Context::new();
            ctx.push(Message::user("konuş"));
            ctx.push(Message::assistant(text.as_str()));
            let outcome = apply_truncation(&mut ctx, 1, &truncation);
            assert!(outcome.is_ok(), "case {case}: {outcome:?}");
            assert!(ctx.validate().is_ok(), "case {case}");
            let stored = ctx.messages().last().map(|m| m.text().to_owned());
            if got.is_empty() {
                assert_eq!(ctx.len(), 1, "case {case}");
            } else {
                assert_eq!(stored.as_deref(), Some(got), "case {case}");
            }
        }
        // The generator covers the cases it is meant to: exact resolutions, rewrites resolved
        // as single tokens, and punctuation timed as words of its own.
        assert!(exact_cases >= 100, "only {exact_cases} exact cases");
        assert!(
            rewrites_resolved >= 20,
            "only {rewrites_resolved} rewrites resolved"
        );
        assert!(punct_marks >= 20, "only {punct_marks} punctuation words");
    }
}
