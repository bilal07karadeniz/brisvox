//! Interruption policy: which barge-ins are real.
//!
//! A `VadStart` while the bot is thinking or speaking is only a *candidate* barge-in. Coughs,
//! the bot's own echo and listener noises ("uh-huh") must not cut the bot off, so the
//! [`TurnMachine`](crate::turn::TurnMachine) asks an [`InterruptionPolicy`] whether the evidence
//! gathered so far justifies an interruption. The policy is pure and deterministic: it reads
//! nothing but the [`Observation`] it is given (durations measured by the machine from the
//! inputs' timestamps, never from a clock) and does no IO.
//!
//! # Guards
//!
//! A candidate is confirmed (the machine emits `Interrupt` and starts the user turn) when no
//! guard blocks it:
//!
//! | Guard | Blocks while | Evidence it waits for |
//! |---|---|---|
//! | Backchannel | the latest STT interim consists only of backchannel phrases and VAD has run for less than `backchannel_max_speech` (when set) | a substantive word, or time |
//! | Echo guard | the bot's audio is younger than `echo_guard` and the interims hold fewer than `echo_min_words` words | STT words, or the end of the window |
//! | Minimum speech | VAD has been active for less than `min_speech` (live: less than `min_speech` plus `vad_hangover` on the clock) | time |
//! | Minimum words | the interims hold fewer than `min_words` words | STT words |
//! | Transcript wait | backchannels are configured, no interim has arrived and VAD has been active for less than `backchannel_wait` (zero means [`DEFAULT_BACKCHANNEL_WAIT`]) | an interim, or time |
//!
//! `min_speech` and `min_words` combine by [`Confirm`] (default: both must hold). The other
//! guards always apply on top. A guard that is switched off (zero duration, zero words, empty
//! backchannel list) never blocks; with everything off ([`InterruptionPolicy::disabled`], the
//! default) every candidate is confirmed at once, which is exactly the plain always-interrupt behaviour.
//!
//! # When the user has stopped
//!
//! [`InterruptionPolicy::assess_at_stop`] judges a candidate at its VAD stop. The utterance is
//! over, so a short but substantive word ("stop", "wait") must not be lost to `min_speech`:
//! when the latest interim is speech with at least one word, the duration requirement is waived
//! (STT has heard a word, so the burst was not noise). `min_words`, the backchannel guard and
//! the echo guard still apply as configured: the word count is an explicit choice. A candidate
//! that is still blocked at its stop is dropped for good: the machine emits nothing for it
//! (only a statistic), so a very short utterance that STT never transcribed, a backchannel or
//! an echo burst never reaches the LLM. That is the cost of the guards.
//!
//! # The backchannel guard cannot stall a real utterance
//!
//! A backchannel verdict rests on the latest interim, which can be stale (STT lag or loss). A
//! real backchannel is short, so once VAD has run for `backchannel_max_speech` the guard stops
//! blocking and the remaining guards decide.
//!
//! Interim transcripts are cumulative per utterance, so the machine counts the words of the
//! longest interim seen, but classifies backchannels on the latest interim only (a partial
//! interim such as "ye" is not in any list and would otherwise confirm the candidate; STT
//! providers emit whole words in practice, which is why the lists hold whole words).
//!
//! # Defaults are guesses
//!
//! The backchannel phrase lists and the numbers of [`InterruptionPolicy::recommended`] are
//! sensible defaults chosen without live data. They are not tuned. Production deployments are
//! expected to adjust them (the lists are plain data on [`BackchannelSet`]).

use std::time::Duration;

use crate::segment::Language;

/// Longest token sequence (after normalisation) still tested against the backchannel phrases.
/// A longer interim is never a pure backchannel, and this bounds the matcher's cost: the text
/// is tokenised only up to one token past this limit, however long it is.
const MAX_BACKCHANNEL_TOKENS: usize = 16;

/// The wait for a first interim used when backchannels are configured but
/// [`InterruptionPolicy::backchannel_wait`] is zero. Without any wait the backchannel guard
/// could never act (the candidate would be confirmed on the VAD start, before any interim).
pub const DEFAULT_BACKCHANNEL_WAIT: Duration = Duration::from_millis(350);

/// The default [`InterruptionPolicy::backchannel_max_speech`] of
/// [`InterruptionPolicy::recommended`]. Not tuned.
pub const DEFAULT_BACKCHANNEL_MAX_SPEECH: Duration = Duration::from_millis(1500);

/// The default [`InterruptionPolicy::vad_hangover`] of [`InterruptionPolicy::recommended`]: the
/// `min_silence` of `brisvox-inference`'s VAD defaults. Not tuned.
pub const DEFAULT_VAD_HANGOVER: Duration = Duration::from_millis(100);

/// How `min_speech` and `min_words` combine when both are configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Confirm {
    /// Both guards must hold (the default; a configured guard is a requirement).
    #[default]
    All,
    /// Either guard holding is enough.
    Any,
}

/// Why a candidate barge-in was not (yet) confirmed. Also the reason recorded when it is
/// dropped ([`InterruptionStats`](crate::turn::InterruptionStats)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VetoReason {
    /// The interim transcript was only backchannel phrases.
    Backchannel,
    /// Within the echo window of the bot's audio and STT had not confirmed enough words.
    EchoGuard,
    /// VAD was active for less than `min_speech`.
    MinSpeech,
    /// The interims held fewer than `min_words` words.
    MinWords,
    /// No interim arrived within `backchannel_wait`, so a backchannel could not be ruled out.
    Unconfirmed,
}

impl VetoReason {
    /// Every reason.
    pub const ALL: [VetoReason; 5] = [
        VetoReason::Backchannel,
        VetoReason::EchoGuard,
        VetoReason::MinSpeech,
        VetoReason::MinWords,
        VetoReason::Unconfirmed,
    ];

    /// The slot of this reason in the statistics. Exhaustive.
    pub(crate) fn index(self) -> usize {
        match self {
            Self::Backchannel => 0,
            Self::EchoGuard => 1,
            Self::MinSpeech => 2,
            Self::MinWords => 3,
            Self::Unconfirmed => 4,
        }
    }
}

/// What the latest STT interim of a candidate looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextKind {
    /// No interim with a word has arrived yet.
    #[default]
    None,
    /// The latest interim contains something other than backchannel phrases.
    Speech,
    /// The latest interim consists only of backchannel phrases.
    Backchannel,
}

/// A set of backchannel phrases of one language: short listener sounds and acknowledgements that
/// must not interrupt the bot ("yeah", "uh-huh", "evet", "hı-hı").
///
/// Phrases are stored normalised (see [`BackchannelSet::new`]); matching ignores case,
/// punctuation and hyphens, so "Uh-huh.", "uh huh" and "UH HUH" are the same phrase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackchannelSet {
    language: Language,
    phrases: Vec<Vec<String>>,
}

/// English backchannels. A default, not tuned. "yes" and "no" are deliberately absent: they
/// answer a question and an answer must be heard.
const ENGLISH: &[&str] = &[
    "yeah", "yea", "yep", "yup", "uh-huh", "uhuh", "mm-hmm", "mmhmm", "mhm", "mm", "hmm", "okay",
    "ok", "right", "alright", "sure", "i see", "got it", "oh", "ah", "uh", "um", "cool",
];

/// Turkish backchannels (extended from the original 19 phrases). A default, not tuned; the
/// reasoning is in `docs/TURKISH.md`. Whole words only, because matching is on whole tokens of
/// the latest interim. Groups:
///
/// - **Listener sounds and acknowledgements**: "evet", "hı-hı", "he", "ha", "tamam", "aynen",
///   "anladım", "olur", "doğru" ("right"), "tabii ki", "elbette", "eyvallah".
/// - **Fillers a speaker produces while thinking** (STT writes them as words): "ee", "ıı", "ımm",
///   "şey", "yani", "ya".
/// - **Reactions**: "öyle mi", "gerçekten mi", "yok artık", "hadi ya", "vay be", "harika",
///   "güzel", "süper", "iyi".
///
/// Deliberately absent: "hayır" (no), "olmaz", "yok", "dur", "bekle", "bir dakika" and any
/// other word that stops or corrects the bot, and "teşekkürler"/"sağ ol" (thanks often end the
/// call, the bot should react).
///
/// Repetitions need no entry: "evet evet", "hı hı hı" and "tamam tamam" are sequences of listed
/// phrases and match as such.
const TURKISH: &[&str] = &[
    // The original defaults.
    "evet",
    "hı-hı",
    "hıhı",
    "hı",
    "he",
    "hm",
    "hmm",
    "mhm",
    "mm",
    "tamam",
    "tamamdır",
    "aynen",
    "anladım",
    "anlıyorum",
    "peki",
    "tabii",
    "tabi",
    "öyle mi",
    "ya",
    // Listener sounds as STT spells them.
    "ha",
    "haa",
    "hıı",
    "hım",
    "hımm",
    "ımm",
    "mmm",
    "aha",
    "aa",
    "ah",
    "oh",
    "ohh",
    "ay",
    "of",
    // Fillers while thinking.
    "ee",
    "eee",
    "ıı",
    "ııı",
    "şey",
    "yani",
    // Acknowledgements.
    "olur",
    "doğru",
    "tabii ki",
    "tabi ki",
    "elbette",
    "anlaşıldı",
    "anlaşılıyor",
    "eyvallah",
    "peki tamam",
    // Reactions.
    "gerçekten mi",
    "sahi mi",
    "ciddi mi",
    "yok artık",
    "hadi ya",
    "demek öyle",
    "vay",
    "vay be",
    "harika",
    "güzel",
    "süper",
    "iyi",
];

impl BackchannelSet {
    /// A set from `phrases`. Each phrase is normalised: lower-cased (the Turkish dotted capital
    /// "İ" becomes "i"), every non-alphanumeric character is a separator, so "uh-huh" is the two
    /// tokens "uh", "huh". Phrases that normalise to nothing are dropped. Note that the
    /// Turkish dotless "ı" is kept distinct from "i" ("hı" and "hi" differ) and that the
    /// capital "I" lower-cases to "i": an all-capitals "HI-HI" is not the Turkish "hı-hı".
    pub fn new<I, S>(language: Language, phrases: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let phrases = phrases
            .into_iter()
            .map(|p| tokens(p.as_ref()))
            .filter(|t| !t.is_empty())
            .collect();
        Self { language, phrases }
    }

    /// The English defaults (see the module docs: not tuned).
    pub fn english() -> Self {
        Self::new(Language::En, ENGLISH.iter().copied())
    }

    /// The Turkish defaults (see the module docs: not tuned; the list and the reasoning behind
    /// it are in `docs/TURKISH.md`).
    pub fn turkish() -> Self {
        Self::new(Language::Tr, TURKISH.iter().copied())
    }

    /// The language of this set.
    pub fn language(&self) -> Language {
        self.language
    }

    /// Number of phrases.
    pub fn len(&self) -> usize {
        self.phrases.len()
    }

    /// Whether the set has no phrases.
    pub fn is_empty(&self) -> bool {
        self.phrases.is_empty()
    }

    /// Whether `phrase` (normalised like the entries) is in the set.
    pub fn contains(&self, phrase: &str) -> bool {
        let wanted = tokens(phrase);
        !wanted.is_empty() && self.phrases.contains(&wanted)
    }

    /// Lengths of the phrases that `words` starts with.
    fn starts_at<'a>(&'a self, words: &'a [String]) -> impl Iterator<Item = usize> + 'a {
        self.phrases
            .iter()
            .filter(move |p| words.starts_with(p))
            .map(Vec::len)
    }
}

/// Lower-cased alphanumeric tokens of `text`. Everything else separates tokens; the combining
/// dot above (left behind when "İ" is lower-cased) is dropped without separating.
fn tokens(text: &str) -> Vec<String> {
    tokens_limited(text, usize::MAX)
}

/// As [`tokens`], but stops as soon as there are more than `max` tokens (the result then holds
/// `max + 1`), so the cost does not grow with the length of `text`.
fn tokens_limited(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for c in text.chars() {
        if out.len() > max {
            return out;
        }
        if c == '\u{307}' {
            continue;
        }
        if c.is_alphanumeric() {
            current.extend(c.to_lowercase().filter(|l| *l != '\u{307}'));
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() && out.len() <= max {
        out.push(current);
    }
    out
}

/// Number of words in `text`: whitespace-separated pieces holding at least one alphanumeric
/// character ("uh-huh" is one word, a lone "-" is none).
pub fn count_words(text: &str) -> u32 {
    let n = text
        .split_whitespace()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .count();
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// What the machine knows about a candidate barge-in when it asks the policy. All durations are
/// derived from input timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    /// How long VAD has been active since the candidate began (to the evaluation time, or to
    /// the VAD stop when the user stopped).
    pub speech: Duration,
    /// Words in the longest interim transcript seen for the candidate.
    pub words: u32,
    /// What the latest interim looks like.
    pub text: TextKind,
    /// How long the bot's audio has been playing (`None`: the bot has not produced audio yet).
    pub bot_audio_age: Option<Duration>,
}

/// The policy's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assessment {
    /// The barge-in is real: interrupt.
    Confirmed,
    /// Not (yet) real; the first guard that blocks it.
    Blocked(VetoReason),
}

/// The interruption policy. All fields are public data; build one with struct update syntax from
/// [`InterruptionPolicy::default`] (everything off) or [`InterruptionPolicy::recommended`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptionPolicy {
    /// VAD must stay active this long before a barge-in interrupts. Zero: off. Measured in
    /// *speech* time: a clock-driven recheck waits an extra [`vad_hangover`] for the VAD to
    /// report a stop that may already have happened (see there).
    ///
    /// [`vad_hangover`]: InterruptionPolicy::vad_hangover
    pub min_speech: Duration,
    /// STT interims must hold this many words before a barge-in interrupts. Zero: off.
    pub min_words: u32,
    /// How `min_speech` and `min_words` combine when both are on.
    pub confirm: Confirm,
    /// Echo guard: during the first `echo_guard` of the bot's audio, VAD alone cannot interrupt;
    /// STT must confirm `echo_min_words` words. Zero: off.
    pub echo_guard: Duration,
    /// Words STT must confirm inside the echo window (at least 1 is always required).
    pub echo_min_words: u32,
    /// Backchannel phrase sets (their union applies). Empty: backchannel handling off.
    pub backchannels: Vec<BackchannelSet>,
    /// How long a candidate without any interim waits before it is assumed not to be a
    /// backchannel (fail open: the user can always interrupt). Used only when backchannels are
    /// configured. Zero means [`DEFAULT_BACKCHANNEL_WAIT`] (350 ms): with no wait the guard
    /// could never act, because a candidate is judged on its VAD start before any interim.
    pub backchannel_wait: Duration,
    /// Once VAD has been active this long, a backchannel verdict stops blocking (the latest
    /// interim may be stale, and a real backchannel is short). Zero: no cap, a backchannel
    /// interim blocks until the VAD stop or a substantive interim.
    pub backchannel_max_speech: Duration,
    /// How long the VAD takes to report a stop after the speech ended (its `min_silence`). A
    /// VAD only reports a stop once the silence has lasted that long and then backdates it, so
    /// a burst of `d` of speech is still "active" for `d + vad_hangover` on the clock. While a
    /// candidate is judged live (a tick or an interim before the stop), the `min_speech`
    /// comparison therefore uses `elapsed - vad_hangover`: a 220 ms cough does not pass a
    /// 300 ms `min_speech` just because its stop has not been delivered yet (the recheck tick
    /// is armed at `min_speech + vad_hangover`). At the stop the backdated duration is exact
    /// and nothing is subtracted. Zero: the VAD reports stops without delay (the default).
    /// Set it to the VAD's hangover (`recommended()` uses 100 ms, the default of
    /// `brisvox-inference`'s Silero VAD); the simulator's energy VAD uses 300 ms.
    pub vad_hangover: Duration,
}

impl Default for InterruptionPolicy {
    /// Everything off: every candidate barge-in is confirmed at once (the plain always-interrupt behaviour).
    fn default() -> Self {
        Self {
            min_speech: Duration::ZERO,
            min_words: 0,
            confirm: Confirm::All,
            echo_guard: Duration::ZERO,
            echo_min_words: 1,
            backchannels: Vec::new(),
            backchannel_wait: Duration::ZERO,
            backchannel_max_speech: Duration::ZERO,
            vad_hangover: Duration::ZERO,
        }
    }
}

impl InterruptionPolicy {
    /// Everything off (same as [`Default`]).
    pub fn disabled() -> Self {
        Self::default()
    }

    /// A conversational starting point, **not tuned** (no live data): 300 ms minimum speech, a
    /// 400 ms echo window needing one STT word, English and Turkish backchannels, a 350 ms wait
    /// for a first interim, a 1.5 s cap on backchannel blocking and a 100 ms VAD hangover.
    pub fn recommended() -> Self {
        Self {
            min_speech: Duration::from_millis(300),
            min_words: 0,
            confirm: Confirm::All,
            echo_guard: Duration::from_millis(400),
            echo_min_words: 1,
            backchannels: vec![BackchannelSet::english(), BackchannelSet::turkish()],
            backchannel_wait: DEFAULT_BACKCHANNEL_WAIT,
            backchannel_max_speech: DEFAULT_BACKCHANNEL_MAX_SPEECH,
            vad_hangover: DEFAULT_VAD_HANGOVER,
        }
    }

    /// Whether every guard is off, so that each candidate is confirmed at once.
    pub fn is_disabled(&self) -> bool {
        self.min_speech.is_zero()
            && self.min_words == 0
            && self.echo_guard.is_zero()
            && !self.has_backchannels()
    }

    fn has_backchannels(&self) -> bool {
        self.backchannels.iter().any(|set| !set.is_empty())
    }

    /// Classifies an interim transcript: [`TextKind::Backchannel`] when it is made of backchannel
    /// phrases only, [`TextKind::Speech`] when it has words but not only those, and
    /// [`TextKind::None`] when it has no word at all. Always `Speech` for text with words when no
    /// backchannels are configured.
    pub fn classify(&self, text: &str) -> TextKind {
        if count_words(text) == 0 {
            return TextKind::None;
        }
        if self.is_only_backchannel(text) {
            TextKind::Backchannel
        } else {
            TextKind::Speech
        }
    }

    /// Whether `text` is non-empty and made only of backchannel phrases (any configured set).
    /// Interims longer than 16 tokens are never backchannels.
    pub fn is_only_backchannel(&self, text: &str) -> bool {
        if !self.has_backchannels() {
            return false;
        }
        let words = tokens_limited(text, MAX_BACKCHANNEL_TOKENS);
        let n = words.len();
        if n == 0 || n > MAX_BACKCHANNEL_TOKENS {
            return false;
        }
        // reach[i]: the first i tokens are a sequence of phrases.
        let mut reach = vec![false; n.saturating_add(1)];
        if let Some(first) = reach.first_mut() {
            *first = true;
        }
        for i in 0..n {
            if !reach.get(i).copied().unwrap_or(false) {
                continue;
            }
            let Some(rest) = words.get(i..) else { continue };
            for set in &self.backchannels {
                for len in set.starts_at(rest) {
                    if let Some(slot) = reach.get_mut(i.saturating_add(len)) {
                        *slot = true;
                    }
                }
            }
        }
        reach.get(n).copied().unwrap_or(false)
    }

    /// Whether the bot's audio is young enough for the echo guard to apply.
    fn in_echo_window(&self, o: &Observation) -> bool {
        !self.echo_guard.is_zero() && o.bot_audio_age.is_some_and(|age| age < self.echo_guard)
    }

    /// The wait for a first interim: `backchannel_wait`, or the default when that is zero.
    fn effective_wait(&self) -> Duration {
        if self.backchannel_wait.is_zero() {
            DEFAULT_BACKCHANNEL_WAIT
        } else {
            self.backchannel_wait
        }
    }

    /// Whether the transcript wait applies: backchannels are on, no interim yet, and VAD has
    /// been active for less than the wait.
    fn waiting_for_text(&self, o: &Observation) -> bool {
        self.has_backchannels() && o.text == TextKind::None && o.speech < self.effective_wait()
    }

    /// Whether the backchannel guard blocks: the latest interim is a backchannel and the cap
    /// (if any) is not reached yet.
    fn backchannel_blocks(&self, o: &Observation) -> bool {
        o.text == TextKind::Backchannel
            && (self.backchannel_max_speech.is_zero() || o.speech < self.backchannel_max_speech)
    }

    /// Decides a candidate that is still in progress (VAD active). Pure: the same observation
    /// always gives the same answer.
    pub fn assess(&self, o: &Observation) -> Assessment {
        self.assess_with(o, false, self.vad_hangover)
    }

    /// Decides a candidate at its VAD stop (the utterance is over). As [`assess`], except that
    /// an interim with at least one word of speech waives the minimum speech duration: a short
    /// substantive word ("stop") is a complete utterance, not noise. The minimum word count, the
    /// backchannel guard and the echo guard still apply.
    ///
    /// [`assess`]: InterruptionPolicy::assess
    pub fn assess_at_stop(&self, o: &Observation) -> Assessment {
        let substantive = o.words >= 1
            && (o.text == TextKind::Speech
                || (o.text == TextKind::Backchannel && !self.backchannel_blocks(o)));
        // The stop time is backdated: `o.speech` is the exact speech length, no hangover in it.
        self.assess_with(o, substantive, Duration::ZERO)
    }

    /// `hangover` is subtracted from the observed duration for the `min_speech` comparison only
    /// (see [`InterruptionPolicy::vad_hangover`]).
    fn assess_with(
        &self,
        o: &Observation,
        waive_min_speech: bool,
        hangover: Duration,
    ) -> Assessment {
        if self.backchannel_blocks(o) {
            return Assessment::Blocked(VetoReason::Backchannel);
        }
        if self.in_echo_window(o) && o.words < self.echo_min_words.max(1) {
            return Assessment::Blocked(VetoReason::EchoGuard);
        }
        let speech_on = !self.min_speech.is_zero();
        let words_on = self.min_words > 0;
        let speech_ok = waive_min_speech || o.speech.saturating_sub(hangover) >= self.min_speech;
        let words_ok = o.words >= self.min_words;
        let held = match (speech_on, words_on, self.confirm) {
            (false, false, _) => true,
            (true, false, _) => speech_ok,
            (false, true, _) => words_ok,
            (true, true, Confirm::All) => speech_ok && words_ok,
            (true, true, Confirm::Any) => speech_ok || words_ok,
        };
        if !held {
            return Assessment::Blocked(if speech_on && !speech_ok {
                VetoReason::MinSpeech
            } else {
                VetoReason::MinWords
            });
        }
        if self.waiting_for_text(o) {
            return Assessment::Blocked(VetoReason::Unconfirmed);
        }
        Assessment::Confirmed
    }

    /// The shortest time after `o` at which a time-driven guard may stop blocking (the machine
    /// arms its timer for it), or `None` when only new evidence (words, a VAD stop) can change
    /// the answer. A returned time is not a promise that the candidate is confirmed then, only
    /// that it must be assessed again.
    pub fn recheck_in(&self, o: &Observation) -> Option<Duration> {
        if self.backchannel_blocks(o) {
            // Only the cap (time) or new text can change the answer.
            return if self.backchannel_max_speech.is_zero() {
                None
            } else {
                Some(self.backchannel_max_speech.saturating_sub(o.speech))
            };
        }
        let mut soonest: Option<Duration> = None;
        let mut consider = |d: Duration| {
            if !d.is_zero() {
                soonest = Some(soonest.map_or(d, |s| s.min(d)));
            }
        };
        if !self.min_speech.is_zero() {
            // Live, the speech is only proven `vad_hangover` after it began to matter.
            let needed = self.min_speech.saturating_add(self.vad_hangover);
            if o.speech < needed {
                consider(needed.saturating_sub(o.speech));
            }
        }
        if self.in_echo_window(o)
            && let Some(age) = o.bot_audio_age
        {
            consider(self.echo_guard.saturating_sub(age));
        }
        if self.waiting_for_text(o) {
            consider(self.effective_wait().saturating_sub(o.speech));
        }
        soonest
    }
}
