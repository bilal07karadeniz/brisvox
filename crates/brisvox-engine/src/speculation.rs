//! Speculative generation: the protocol between the [`TurnDriver`](crate::TurnDriver) and
//! the [`LlmRunner`](crate::LlmRunner), the transcript normalisation that decides a hit, and the
//! metrics. The design is in `docs/PIPELINE.md` section 12 ("Speculative generation").
//!
//! ```text
//! TurnDriver --candidate (non-final Transcript)--> LlmRunner: start a buffered LLM run
//!            --withdrawal (non-final, empty)-----> LlmRunner: user resumed, discard it
//!            --Transcript(final) + UserTurnEnded-> LlmRunner: same text? commit : discard + run
//! ```
//!
//! # The protocol (frames only, no side channel)
//! The driver never forwards STT interims, so a **non-final** [`TranscriptFrame`] that travels
//! downstream *from the driver* is free to carry the speculation signal, in the same ordered
//! lane as the final transcript and `UserTurnEnded` it competes with (a side channel would race
//! with them):
//! - [`candidate_frame`]: non-empty text, tagged with the user turn's epoch. "The turn may end
//!   here; this is what the caller said so far." A later candidate of the same epoch replaces the
//!   earlier one.
//! - [`withdrawal_frame`]: empty text. "The caller went on speaking; discard the speculation."
//!
//! The runner treats `UserTurnStarted`, `Interrupt`, `Stop` and `Cancel` as withdrawals too, and
//! the final `Transcript` + `UserTurnEnded` as the commit point. A dedicated `ControlKind` would
//! be cleaner (a follow-up that needs a change in `brisvox-core`); the `Transcript` form keeps
//! this task additive.
//!
//! # What a hit means
//! The runner commits a speculation when, at `UserTurnEnded`, (1) the normalised final text
//! ([`normalize`]) equals the normalised candidate, and (2) the context is exactly the context
//! the speculative request was built from (a late tool note that arrived meanwhile would make the
//! two differ), and (3) the speculation is younger than [`SpeculationConfig::max_age`]. Then the
//! buffered output is released at once. Otherwise it is discarded and the turn runs as without
//! speculation; the output is identical either way, only the latency differs.
//!
//! [`SpeculationConfig::max_age`]: crate::SpeculationConfig::max_age

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use brisvox_core::TurnEpoch;
use brisvox_core::frame::{DataFrame, Frame, TranscriptFrame};
use tokio::time::Instant;

/// Symbols that change what was said wherever they stand: every currency sign (the Unicode
/// currency block and the stand-alone ones such as `$`, `£`, `¥`, `¢`, the Turkish lira `₺`),
/// percent and per mille, at, plus, equals, comparison, slash, ampersand, hash, and the degree,
/// arithmetic and approximation signs (`20°` is not `20`, `3 × 4` is not `3 4`).
fn is_meaning_symbol(ch: char) -> bool {
    matches!(
        ch,
        '$' | '%'
            | '@'
            | '+'
            | '='
            | '<'
            | '>'
            | '/'
            | '&'
            | '#'
            | '*'
            | '~'
            | '^'
            | '\u{a2}' // cent
            | '\u{a3}' // pound
            | '\u{a4}' // generic currency
            | '\u{a5}' // yen
            | '\u{b0}' // degree
            | '\u{b1}' // plus-minus
            | '\u{d7}' // multiplication
            | '\u{f7}' // division
            | '\u{2030}' // per mille
            | '\u{2032}' // prime (feet, minutes)
            | '\u{2033}' // double prime (inches, seconds)
            | '\u{2103}' // degree Celsius
            | '\u{2109}' // degree Fahrenheit
            | '\u{20a0}'
            ..='\u{20cf}' // currency symbols block (euro, rupee, lira, won, ...)
            | '\u{58f}' // Armenian dram
            | '\u{60b}' // Afghani
            | '\u{9f2}'
            | '\u{9f3}' // Bengali rupee signs
            | '\u{e3f}' // Thai baht
            | '\u{fdfc}' // rial
            | '\u{fe69}'
            | '\u{ff04}' // small and full-width dollar
            | '\u{ffe0}'
            | '\u{ffe1}'
            | '\u{ffe5}'
            | '\u{ffe6}' // full-width cent, pound, yen, won
    )
}

/// Whether `ch` is a combining mark (the combining blocks; the general category Mn needs a
/// Unicode table that std does not carry). A mark belongs to the letter before it.
fn is_combining_mark(ch: char) -> bool {
    matches!(
        ch,
        '\u{300}'..='\u{36f}'
            | '\u{483}'..='\u{489}'
            | '\u{591}'..='\u{5bd}'
            | '\u{64b}'..='\u{65f}'
            | '\u{1ab0}'..='\u{1aff}'
            | '\u{1dc0}'..='\u{1dff}'
            | '\u{20d0}'..='\u{20ff}'
            | '\u{fe20}'..='\u{fe2f}'
    )
}

/// A letter, a digit or a combining mark (which belongs to the letter before it).
fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || is_combining_mark(ch)
}

/// The precomposed (NFC) letter for the lower-case `base` followed by the combining `mark`, for
/// the Latin letters of the supported languages (Turkish, English and the common European
/// accents). `None` when there is no such letter in the table, so exotic combinations are kept
/// as they are (they may miss, they never produce a wrong hit).
fn compose(base: char, mark: char) -> Option<char> {
    const TABLE: &[(char, char, char)] = &[
        // dot above (the Turkish capital I with dot lower-cases to i + U+0307)
        ('i', '\u{307}', 'i'),
        // grave
        ('a', '\u{300}', 'à'),
        ('e', '\u{300}', 'è'),
        ('i', '\u{300}', 'ì'),
        ('o', '\u{300}', 'ò'),
        ('u', '\u{300}', 'ù'),
        // acute
        ('a', '\u{301}', 'á'),
        ('e', '\u{301}', 'é'),
        ('i', '\u{301}', 'í'),
        ('o', '\u{301}', 'ó'),
        ('u', '\u{301}', 'ú'),
        ('y', '\u{301}', 'ý'),
        // circumflex
        ('a', '\u{302}', 'â'),
        ('e', '\u{302}', 'ê'),
        ('i', '\u{302}', 'î'),
        ('o', '\u{302}', 'ô'),
        ('u', '\u{302}', 'û'),
        // tilde
        ('a', '\u{303}', 'ã'),
        ('n', '\u{303}', 'ñ'),
        ('o', '\u{303}', 'õ'),
        // diaeresis
        ('a', '\u{308}', 'ä'),
        ('e', '\u{308}', 'ë'),
        ('i', '\u{308}', 'ï'),
        ('o', '\u{308}', 'ö'),
        ('u', '\u{308}', 'ü'),
        ('y', '\u{308}', 'ÿ'),
        // ring
        ('a', '\u{30a}', 'å'),
        // breve (the Turkish soft g)
        ('g', '\u{306}', 'ğ'),
        // cedilla (the Turkish c and s)
        ('c', '\u{327}', 'ç'),
        ('s', '\u{327}', 'ş'),
    ];
    TABLE
        .iter()
        .find(|(b, m, _)| *b == base && *m == mark)
        .map(|(_, _, c)| *c)
}

/// Whether the symbol at `i` is part of what was said: a minus or a range dash next to a digit
/// (`-50`, `5-10`), a decimal, thousands or time separator between digits or inside a token
/// without spaces (`3.5`, `10:30`, `john.doe`). Elsewhere a dash or punctuation is only
/// punctuation (`well-known`, `four.`).
fn is_meaning_joint(chars: &[char], i: usize) -> bool {
    let ch = chars.get(i).copied().unwrap_or(' ');
    let before = i.checked_sub(1).and_then(|j| chars.get(j)).copied();
    let after = chars.get(i + 1).copied();
    match ch {
        '-' | '\u{2212}' => {
            before.is_some_and(|c| c.is_ascii_digit()) || after.is_some_and(|c| c.is_ascii_digit())
        }
        '.' | ',' | ':' => before.is_some_and(is_word_char) && after.is_some_and(is_word_char),
        _ => false,
    }
}

/// The text two transcripts are compared by: lower-cased, with every run of characters that are
/// not letters or digits collapsed to one space and the ends trimmed. Apostrophes are dropped
/// without a gap (`don't` and `dont` are the same word), so punctuation, capitalisation and
/// spacing differences between an interim and a final do not count as a change of what was said.
/// Digits versus spelled-out numbers do (a different transcript), and so do the symbols that
/// change the meaning: sign, currency, percent, at, plus and the like, and the joints inside a
/// number or an address (`-50` is not `50`, `5%` is not `5`, `3.5` is not `35`,
/// `john.doe@gmail.com` is not `john doe gmail com`); the currency signs (`₺50` is not `50`),
/// the degree and the arithmetic signs are among them. The Turkish `İ` equals `i`, and a
/// combining mark belongs to its letter, so the composed (NFC) and decomposed (NFD) spelling of
/// a word are equal (for the Latin letters of the compose table; other combinations are kept as
/// they are and may miss, never produce a wrong hit). A hit releases a reply that was generated
/// from the candidate's spelling, so a difference that could change a tool argument must not
/// compare equal.
pub fn normalize(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut gap = false;
    for (i, &ch) in chars.iter().enumerate() {
        if ch.is_alphanumeric() {
            if gap && !out.is_empty() {
                out.push(' ');
            }
            gap = false;
            if ch == '\u{130}' {
                // The Turkish capital I with dot is `i`; `char::to_lowercase` would yield `i`
                // plus a combining dot, which would not equal the plain `i` of an interim.
                out.push('i');
            } else {
                out.extend(ch.to_lowercase());
            }
        } else if is_combining_mark(ch) {
            // A mark is transparent: it composes with the letter before it (so the NFC and NFD
            // spellings of a word are equal), and it never opens a gap.
            if !gap {
                let base = out.chars().last().filter(|c| c.is_alphanumeric());
                if let Some(base) = base {
                    match compose(base, ch) {
                        Some(composed) => {
                            out.pop();
                            out.push(composed);
                        }
                        None => out.push(ch),
                    }
                }
            }
        } else if is_meaning_symbol(ch) || is_meaning_joint(&chars, i) {
            if gap && !out.is_empty() {
                out.push(' ');
            }
            gap = false;
            out.push(ch);
        } else if ch != '\'' && ch != '\u{2019}' {
            gap = true;
        }
    }
    out
}

/// The frame with which the driver offers `text` as a speculation candidate of user turn `epoch`.
pub fn candidate_frame(epoch: TurnEpoch, text: &str, at: Instant) -> Frame {
    DataFrame::Transcript(TranscriptFrame {
        epoch,
        text: Arc::from(text),
        is_final: false,
        received_at: at,
    })
    .into()
}

/// The frame with which the driver withdraws the candidate of user turn `epoch`.
pub fn withdrawal_frame(epoch: TurnEpoch, at: Instant) -> Frame {
    candidate_frame(epoch, "", at)
}

/// What a non-final transcript from the driver means to the runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Hint {
    /// Speculate on this text.
    Candidate(Arc<str>),
    /// Drop the speculation.
    Withdraw,
}

impl Hint {
    /// The hint carried by a non-final transcript (`None` for a final one).
    pub(crate) fn of(transcript: &TranscriptFrame) -> Option<Self> {
        if transcript.is_final {
            None
        } else if transcript.text.trim().is_empty() {
            Some(Self::Withdraw)
        } else {
            Some(Self::Candidate(Arc::clone(&transcript.text)))
        }
    }
}

/// Why a speculation did not become the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Discard {
    /// The turn ended with other text.
    Changed,
    /// The context moved on (a late tool note).
    ContextChanged,
    /// Older than `max_age`.
    Expired,
    /// Replaced by a newer candidate.
    Superseded,
    /// The caller resumed.
    Withdrawn,
    /// The turn went away (barge-in, new turn, stop, no transcript).
    Abandoned,
}

macro_rules! spec_counters {
    ($($(#[$doc:meta])* $name:ident),* $(,)?) => {
        #[derive(Debug, Default)]
        pub(crate) struct SpecCounters {
            $(pub(crate) $name: AtomicU64,)*
        }

        /// A copy of the [`SpeculationStats`] counters.
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct SpeculationStatsSnapshot {
            $($(#[$doc])* pub $name: u64,)*
        }

        impl SpecCounters {
            fn snapshot(&self) -> SpeculationStatsSnapshot {
                SpeculationStatsSnapshot {
                    $($name: self.$name.load(Ordering::Relaxed),)*
                }
            }
        }
    };
}

spec_counters! {
    /// Speculative LLM runs started.
    started,
    /// Candidates identical (normalised) to the speculation already running (kept as it is).
    kept,
    /// Speculations committed: the turn ended with the same text.
    hits,
    /// Speculations discarded because the turn ended with other text.
    misses,
    /// Speculations discarded because the context changed meanwhile (a late tool note).
    context_changes,
    /// Speculations discarded because they were older than `max_age`.
    expired,
    /// Speculations replaced by a newer candidate before the turn ended.
    superseded,
    /// Speculations discarded because the caller went on speaking.
    withdrawn,
    /// Speculations discarded because the turn went away (barge-in, new turn, stop).
    abandoned,
    /// Candidates ignored because the runner was still answering the previous turn.
    skipped_busy,
    /// Candidates ignored because the runner drives a flow.
    skipped_flow,
    /// Candidates ignored because the context could not be settled (a round task would not stop).
    skipped_unsafe,
    /// Speculative runs whose stream failed (request error, stream error, timeout, cut off).
    failures,
    /// Speculative tool phases: the ReadOnly calls ran, the SideEffect calls were held.
    tool_phases,
    /// SideEffect calls held by a speculative tool phase (they run only at a commit).
    held_side_effects,
    /// Output tokens of discarded speculations (reported by the provider, else estimated from
    /// the bytes streamed: 4 bytes per token).
    wasted_output_tokens,
    /// Input tokens of discarded speculations (only what the provider reported).
    wasted_input_tokens,
    /// Total milliseconds the committed speculations were ahead of the turn end (request to
    /// commit).
    head_start_ms,
}

pub(crate) fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn add(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

impl SpecCounters {
    /// Counts one discarded speculation under `why`.
    pub(crate) fn discarded(&self, why: Discard) {
        bump(match why {
            Discard::Changed => &self.misses,
            Discard::ContextChanged => &self.context_changes,
            Discard::Expired => &self.expired,
            Discard::Superseded => &self.superseded,
            Discard::Withdrawn => &self.withdrawn,
            Discard::Abandoned => &self.abandoned,
        });
    }
}

/// Metrics of speculative generation, shared and readable while the session runs
/// ([`LlmRunner::speculation_stats`](crate::LlmRunner::speculation_stats)).
#[derive(Debug, Clone, Default)]
pub struct SpeculationStats {
    counters: Arc<SpecCounters>,
}

impl SpeculationStats {
    /// A copy of the counters.
    pub fn snapshot(&self) -> SpeculationStatsSnapshot {
        self.counters.snapshot()
    }

    pub(crate) fn counters(&self) -> &Arc<SpecCounters> {
        &self.counters
    }
}

impl SpeculationStatsSnapshot {
    /// Speculations that ended in a discard of any kind.
    pub fn discarded(&self) -> u64 {
        self.misses
            .saturating_add(self.context_changes)
            .saturating_add(self.expired)
            .saturating_add(self.superseded)
            .saturating_add(self.withdrawn)
            .saturating_add(self.abandoned)
    }

    /// The speculation hit rate: committed runs over all runs that were decided (committed or
    /// discarded). `None` before the first decision.
    pub fn hit_rate(&self) -> Option<f64> {
        let decided = self.hits.saturating_add(self.discarded());
        if decided == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)] // counters far below 2^52
        Some(self.hits as f64 / decided as f64)
    }

    /// Output tokens spent on discarded speculations.
    pub fn wasted_tokens(&self) -> u64 {
        self.wasted_output_tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalisation_ignores_case_punctuation_and_spacing() {
        let same = [
            ("Book a table, for four.", "book a table for four"),
            ("  BOOK   a\ttable --  for FOUR!? ", "book a table for four"),
            ("don't", "dont"),
            ("don\u{2019}t", "dont"),
            ("well-known", "well known"),
            ("Merhaba, nasılsın?", "merhaba nasılsın"),
            ("", ""),
            ("...", ""),
        ];
        for (raw, expected) in same {
            assert_eq!(normalize(raw), expected, "{raw:?}");
        }
        // A different word, an extra word or other digits are a different transcript.
        assert_ne!(
            normalize("book a table"),
            normalize("book a table for four")
        );
        assert_ne!(normalize("table for 4"), normalize("table for four"));
        assert_ne!(normalize("table for 4"), normalize("table for 5"));
    }

    #[test]
    fn normalisation_keeps_the_symbols_that_change_the_meaning() {
        let differ = [
            ("transfer -50 to savings", "transfer 50 to savings"),
            ("pay 5% tax", "pay 5 tax"),
            ("pay $5", "pay 5"),
            ("john.doe@gmail.com", "john doe gmail com"),
            ("version 3.5", "version 35"),
            ("meet at 10:30", "meet at 1030"),
            ("table for 5-10", "table for 510"),
            ("a+b", "a b"),
            ("ödeme ₺50", "ödeme 50"),
            ("pay ₹50", "pay 50"),
            ("pay ₩50", "pay 50"),
            ("pay 50¢", "pay 50"),
            ("set the heater to 20°", "set the heater to 20"),
            ("set it to 20°c", "set it to 20c"),
            ("3 × 4", "3 4"),
            ("plus ±2", "plus 2"),
            ("5*", "5"),
            ("~50", "50"),
            ("₺50", "₺500"),
        ];
        for (a, b) in differ {
            assert_ne!(normalize(a), normalize(b), "{a:?} vs {b:?}");
        }
        // Punctuation next to spaces, and a trailing full stop after a number, stay noise.
        assert_eq!(normalize("table for 4."), normalize("Table for 4"));
        assert_eq!(
            normalize("It costs 5. Then, go"),
            normalize("it costs 5 then go")
        );
        assert_eq!(
            normalize("transfer -50, now"),
            normalize("Transfer -50 now.")
        );
    }

    #[test]
    fn the_turkish_capital_i_with_dot_equals_the_plain_i() {
        let same = [
            ("İstanbul'a gidiyorum", "istanbul'a gidiyorum"),
            ("İyi günler", "iyi günler"),
            ("İKİ", "iki"),
            // the decomposed spelling: I + combining dot above
            ("I\u{307}stanbul", "istanbul"),
        ];
        for (a, b) in same {
            assert_eq!(normalize(a), normalize(b), "{a:?} vs {b:?}");
        }
        assert_eq!(normalize("İyi günler"), "iyi günler");
        // The dotless i is another letter: it stays distinct.
        assert_ne!(normalize("ısı"), normalize("isi"));
    }

    #[test]
    fn composed_and_decomposed_spellings_are_equal() {
        let same = [
            ("cafe\u{301}", "café"),
            ("e\u{301}te\u{301}", "été"),
            ("c\u{327}ay", "çay"),
            ("s\u{327}u", "şu"),
            ("g\u{306}ul", "ğul"),
            ("go\u{308}z", "göz"),
            ("gu\u{308}n", "gün"),
            ("cafe\u{301}.com", "café.com"),
            ("Cafe\u{301}!", "CAFÉ"),
        ];
        for (a, b) in same {
            assert_eq!(normalize(a), normalize(b), "{a:?} vs {b:?}");
        }
        // A mark does not open a gap inside a word.
        assert_eq!(normalize("e\u{301}te"), "éte");
        // Different letters stay different (no accent folding).
        assert_ne!(normalize("şu"), normalize("su"));
        assert_ne!(normalize("cafe"), normalize("café"));
        assert_ne!(normalize("cafe\u{301}"), normalize("cafe"));
        // A stray mark at the start or after a space is dropped.
        assert_eq!(normalize("\u{301}abc"), "abc");
        assert_eq!(normalize("a \u{301}b"), "a b");
    }

    #[test]
    #[allow(clippy::disallowed_methods)] // a frame needs some instant; no clock is involved
    fn the_hint_is_read_from_a_non_final_transcript() {
        let at = Instant::now();
        let epoch = TurnEpoch::new(3);
        let frame = |f: Frame| match f {
            Frame::Data(DataFrame::Transcript(t)) => t,
            other => panic!("not a transcript: {other:?}"),
        };
        let candidate = frame(candidate_frame(epoch, "hello there", at));
        assert!(!candidate.is_final);
        assert_eq!(candidate.epoch, epoch);
        assert_eq!(
            Hint::of(&candidate),
            Some(Hint::Candidate(Arc::from("hello there")))
        );
        let withdrawal = frame(withdrawal_frame(epoch, at));
        assert_eq!(Hint::of(&withdrawal), Some(Hint::Withdraw));
        let whitespace = frame(candidate_frame(epoch, "  \n", at));
        assert_eq!(Hint::of(&whitespace), Some(Hint::Withdraw));
        let mut final_ = candidate;
        final_.is_final = true;
        assert_eq!(Hint::of(&final_), None);
    }

    #[test]
    fn the_hit_rate_counts_decided_speculations() {
        let stats = SpeculationStats::default();
        assert_eq!(stats.snapshot().hit_rate(), None);
        let counters = stats.counters();
        bump(&counters.hits);
        bump(&counters.hits);
        bump(&counters.hits);
        counters.discarded(Discard::Changed);
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.discarded(), 1);
        assert_eq!(snapshot.hit_rate(), Some(0.75));
        for why in [
            Discard::ContextChanged,
            Discard::Expired,
            Discard::Superseded,
            Discard::Withdrawn,
            Discard::Abandoned,
        ] {
            counters.discarded(why);
        }
        assert_eq!(stats.snapshot().discarded(), 6);
        assert_eq!(stats.snapshot().hit_rate(), Some(3.0 / 9.0));
    }
}
