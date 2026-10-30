//! Language switching mid-call: which language the caller speaks, and when the agent follows.
//!
//! A call can be configured with several languages ([`LanguageSettings`]: for each one the TTS
//! voice, optionally a TTS model, and the language the segmenter and the speech normalizer
//! use). The [`LanguageTracker`] of a call holds the language the agent speaks now and decides
//! when it changes.
//!
//! # What is observed
//! The STT stage collects the final segments of one utterance into an [`Utterance`] (text, and
//! the language the provider reported for each segment, if it did) and hands it to
//! [`LanguageTracker::observe`] right before it emits the `UtteranceFinal` of that utterance, so
//! the decision is taken before the turn driver and the LLM see the turn. The language of the
//! utterance is, in this order:
//! 1. what the provider reported (the language with the most words; a reported language that is
//!    not configured is ignored: the utterance counts as no evidence);
//! 2. otherwise a text-based detection on the final text ([`whatlang`], restricted to the
//!    configured languages, accepted only when the detector calls it reliable).
//!
//! # When it switches
//! - An utterance with fewer than [`LanguageSettings::min_words`] words is no evidence at all
//!   (a backchannel, a name, a single foreign word): it neither starts nor breaks a streak.
//! - The agent switches to language `L` after [`LanguageSettings::switch_after`] consecutive
//!   utterances in `L` (utterances in the current language reset the streak), or at once on one
//!   utterance whose provider-reported confidence is at least
//!   [`LanguageSettings::switch_confidence`]. Only configured languages are ever chosen.
//!
//! # Where it takes effect
//! Never mid-response. The new language is read at turn boundaries: the LLM runner snapshots it
//! when a turn starts (the request gets a short `Reply in <language>.` line appended to the
//! leading system message of the provider request, the stored context is not touched), and the
//! TTS stage snapshots it when the first frame of a new response epoch arrives (voice, TTS
//! model and the speech normalizer's language; a session that speaks another voice is replaced
//! before the first text of the response). A response in progress keeps the language it
//! started with.
//!
//! # Cost
//! One observation per utterance (not per frame): a handful of integer operations when the
//! provider reports the language, plus one trigram detection over at most
//! [`DETECT_CHARS`] characters when it does not (about 10 microseconds for a sentence, see
//! [`DETECT_CHARS`]).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use brisvox_agent::Language;
use brisvox_core::TurnEpoch;
use brisvox_providers::SpokenLanguage;
use whatlang::{Detector, Lang};

use crate::error::{Error, Result};
use crate::events::{EngineEvent, EngineEvents};

/// Most characters of an utterance the text detector looks at (its tail). Bounds the detection
/// cost: in a release build, with two languages allowed, a 130-character sentence takes about
/// 10 microseconds (a 20-character one 3), so the longest input is well under the engine's 50
/// microsecond rule for work on a tokio worker.
pub const DETECT_CHARS: usize = 300;

/// Most bytes of final text an [`Utterance`] keeps (the rest still counts as words).
const MAX_UTTERANCE_BYTES: usize = 4096;

/// Most languages one call is configured with.
pub const MAX_LANGUAGES: usize = 8;

/// Default for [`LanguageSettings::switch_after`].
pub const DEFAULT_SWITCH_AFTER: u32 = 2;

/// Default for [`LanguageSettings::min_words`].
pub const DEFAULT_MIN_WORDS: usize = 2;

/// Default for [`LanguageSettings::switch_confidence`].
pub const DEFAULT_SWITCH_CONFIDENCE: f32 = 0.9;

/// One language the agent can speak.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageProfile {
    /// Lowercase primary language code (`"en"`, `"tr"`), as the STT providers report it.
    pub code: Arc<str>,
    /// The language of the segmenter and the speech normalizer while this language is spoken.
    pub language: Language,
    /// TTS provider voice id for this language.
    pub voice: Arc<str>,
    /// TTS model for this language, when it differs from the provider's configured one.
    pub tts_model: Option<Arc<str>>,
    /// Language name for the LLM line (`"Turkish"`).
    name: Arc<str>,
    /// The per-turn line for the LLM (`"Reply in Turkish."`).
    instruction: Arc<str>,
}

impl LanguageProfile {
    /// A profile for the engine language `language` with its TTS `voice`; `code` is the
    /// language code the STT providers report for it.
    pub fn new(code: &str, language: Language, voice: impl Into<Arc<str>>) -> Self {
        let name: Arc<str> = Arc::from(match language {
            Language::En => "English",
            Language::Tr => "Turkish",
        });
        Self {
            code: Arc::from(code.to_ascii_lowercase().as_str()),
            language,
            voice: voice.into(),
            tts_model: None,
            instruction: Arc::from(format!("Reply in {name}.").as_str()),
            name,
        }
    }

    /// The same with a TTS model for this language.
    #[must_use]
    pub fn with_tts_model(mut self, model: impl Into<Arc<str>>) -> Self {
        self.tts_model = Some(model.into());
        self
    }

    /// The language name used in the LLM line.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The line appended to the LLM request while this language is spoken.
    pub fn instruction(&self) -> &Arc<str> {
        &self.instruction
    }
}

/// The languages of a call and the rules of switching between them.
#[derive(Debug, Clone, PartialEq)]
pub struct LanguageSettings {
    profiles: Vec<LanguageProfile>,
    default: usize,
    /// Consecutive utterances in another language before the agent follows. At least 1.
    pub switch_after: u32,
    /// Provider-reported confidence (`0.0..=1.0`) at which one utterance is enough.
    pub switch_confidence: f32,
    /// Utterances with fewer words are no evidence.
    pub min_words: usize,
}

impl LanguageSettings {
    /// Settings over `profiles`, starting in the language with code `default`, with the default
    /// rules ([`DEFAULT_SWITCH_AFTER`], [`DEFAULT_SWITCH_CONFIDENCE`], [`DEFAULT_MIN_WORDS`]).
    ///
    /// # Errors
    /// [`Error::Config`] when `profiles` is empty or longer than [`MAX_LANGUAGES`], two profiles
    /// share a code, or `default` is not one of them.
    pub fn new(profiles: Vec<LanguageProfile>, default: &str) -> Result<Self> {
        if profiles.is_empty() || profiles.len() > MAX_LANGUAGES {
            return Err(Error::Config("languages: between 1 and 8 languages"));
        }
        for (i, a) in profiles.iter().enumerate() {
            if profiles.iter().skip(i + 1).any(|b| a.code == b.code) {
                return Err(Error::Config("languages: a language is listed twice"));
            }
        }
        let default = default.to_ascii_lowercase();
        let Some(default) = profiles.iter().position(|p| *p.code == *default) else {
            return Err(Error::Config(
                "languages: the default is not a listed language",
            ));
        };
        Ok(Self {
            profiles,
            default,
            switch_after: DEFAULT_SWITCH_AFTER,
            switch_confidence: DEFAULT_SWITCH_CONFIDENCE,
            min_words: DEFAULT_MIN_WORDS,
        })
    }

    /// The configured languages.
    pub fn profiles(&self) -> &[LanguageProfile] {
        &self.profiles
    }

    /// Index of the language the call starts in.
    pub const fn default_index(&self) -> usize {
        self.default
    }

    /// The profile of the language the call starts in.
    pub fn default_profile(&self) -> Option<&LanguageProfile> {
        self.profiles.get(self.default)
    }

    /// Index of the profile with `code` (case-insensitive primary code).
    pub fn index_of(&self, code: &str) -> Option<usize> {
        self.profiles
            .iter()
            .position(|p| p.code.eq_ignore_ascii_case(code))
    }
}

/// The final text of one utterance and what the provider reported about its language, collected
/// by the STT stage and handed to [`LanguageTracker::observe`].
#[derive(Debug, Default)]
pub struct Utterance {
    text: String,
    words: usize,
    reported: Vec<Reported>,
}

#[derive(Debug)]
struct Reported {
    code: Arc<str>,
    confidence: Option<f32>,
    words: usize,
}

impl Utterance {
    /// An empty utterance.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a final segment with the language the provider reported for it (if any).
    pub fn push(&mut self, text: &str, language: Option<&SpokenLanguage>) {
        let words = text.split_whitespace().count();
        if words == 0 {
            return;
        }
        self.words = self.words.saturating_add(words);
        if self.text.len() < MAX_UTTERANCE_BYTES {
            if !self.text.is_empty() {
                self.text.push(' ');
            }
            self.text.push_str(text.trim());
        }
        if let Some(language) = language {
            match self.reported.iter_mut().find(|r| r.code == language.code) {
                Some(known) => {
                    known.words = known.words.saturating_add(words);
                    known.confidence = match (known.confidence, language.confidence) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (a, b) => a.or(b),
                    };
                }
                None => self.reported.push(Reported {
                    code: Arc::clone(&language.code),
                    confidence: language.confidence,
                    words,
                }),
            }
        }
    }

    /// Words collected so far.
    pub const fn words(&self) -> usize {
        self.words
    }

    /// Forgets everything (the tracker's buffer is reused for the next utterance).
    pub fn clear(&mut self) {
        self.text.clear();
        self.words = 0;
        self.reported.clear();
    }
}

/// Where the language of an utterance came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageSource {
    /// Reported by the STT provider.
    Provider,
    /// Detected from the final text.
    Text,
}

/// A change of the spoken language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageSwitch {
    /// The language code before.
    pub from: Arc<str>,
    /// The language code now.
    pub to: Arc<str>,
    /// What the decision rested on.
    pub source: LanguageSource,
}

/// A point-in-time copy of the [`LanguageTracker`] counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LanguageStatsSnapshot {
    /// Utterances observed.
    pub observed: u64,
    /// Utterances too short to count.
    pub too_short: u64,
    /// Utterances whose language could not be told (nothing reported, the text detector was not
    /// sure) or was reported as a language that is not configured.
    pub undecided: u64,
    /// Utterances decided from the provider's report.
    pub from_provider: u64,
    /// Utterances decided from the text.
    pub from_text: u64,
    /// Switches made.
    pub switches: u64,
}

#[derive(Debug, Default)]
struct Counters {
    observed: AtomicU64,
    too_short: AtomicU64,
    undecided: AtomicU64,
    from_provider: AtomicU64,
    from_text: AtomicU64,
    switches: AtomicU64,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// The language the call speaks now. Shared by the STT stage (observes), the LLM runner and the
/// TTS stage (read at turn boundaries).
pub struct LanguageTracker {
    settings: Arc<LanguageSettings>,
    current: AtomicUsize,
    /// Bumped by every switch: a speculative LLM run started before it is stale.
    generation: AtomicU64,
    streak: Mutex<Option<(usize, u32)>>,
    /// The language chosen for the response of an epoch ([`Self::language_for`]).
    locked: Mutex<Option<(TurnEpoch, usize)>>,
    detector: Detector,
    /// `detector`'s languages: (whatlang language, profile index).
    detectable: Vec<(Lang, usize)>,
    counters: Counters,
    events: Option<EngineEvents>,
}

impl std::fmt::Debug for LanguageTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LanguageTracker")
            .field("current", &self.current_code())
            .finish_non_exhaustive()
    }
}

/// The detector's language for an engine language.
const fn lang_of(language: Language) -> Lang {
    match language {
        Language::En => Lang::Eng,
        Language::Tr => Lang::Tur,
    }
}

impl LanguageTracker {
    /// A tracker starting in the default language of `settings`.
    pub fn new(settings: Arc<LanguageSettings>) -> Self {
        let mut detectable: Vec<(Lang, usize)> = Vec::new();
        for (index, profile) in settings.profiles.iter().enumerate() {
            let lang = lang_of(profile.language);
            if !detectable.iter().any(|(l, _)| *l == lang) {
                detectable.push((lang, index));
            }
        }
        let detector = Detector::with_allowlist(detectable.iter().map(|(l, _)| *l).collect());
        Self {
            current: AtomicUsize::new(settings.default),
            generation: AtomicU64::new(0),
            streak: Mutex::new(None),
            locked: Mutex::new(None),
            detector,
            detectable,
            counters: Counters::default(),
            events: None,
            settings,
        }
    }

    /// Publishes an [`EngineEvent::LanguageSwitched`] to `events` at every switch.
    #[must_use]
    pub fn with_events(mut self, events: EngineEvents) -> Self {
        self.events = Some(events);
        self
    }

    /// The settings.
    pub fn settings(&self) -> &Arc<LanguageSettings> {
        &self.settings
    }

    /// Index of the language spoken now.
    pub fn current(&self) -> usize {
        self.current
            .load(Ordering::Acquire)
            .min(self.settings.profiles.len().saturating_sub(1))
    }

    /// The profile of the language spoken now.
    pub fn current_profile(&self) -> Option<&LanguageProfile> {
        self.settings.profiles.get(self.current())
    }

    /// The code of the language spoken now (empty if the settings were empty, which
    /// [`LanguageSettings::new`] refuses).
    pub fn current_code(&self) -> &str {
        self.current_profile().map_or("", |p| &*p.code)
    }

    /// The profile at `index`.
    pub fn profile(&self, index: usize) -> Option<&LanguageProfile> {
        self.settings.profiles.get(index)
    }

    /// The number of switches so far: a value that differs from an earlier read means the
    /// language changed in between.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// The language of the response to the turn of `epoch`, as an index into the settings: the
    /// language spoken when the first stage asked for this epoch, and the same answer for every
    /// later ask, whatever the tracker decides meanwhile. The LLM runner asks when the turn
    /// starts, so the request, the segmenter and the TTS voice of one response always agree and
    /// a switch decided while a response plays only shows in the next one. A stale epoch gets
    /// the current language.
    pub fn language_for(&self, epoch: TurnEpoch) -> usize {
        let mut locked = self.locked.lock().unwrap_or_else(PoisonError::into_inner);
        match *locked {
            Some((at, index)) if at == epoch => index,
            Some((at, _)) if at > epoch => self.current(),
            _ => {
                let index = self.current();
                *locked = Some((epoch, index));
                index
            }
        }
    }

    /// Whether `index` is the language the call started in. Phrases written once for the agent
    /// (the tool filler, clips) are in that language, so a response in another one skips them.
    pub fn is_default(&self, index: usize) -> bool {
        index == self.settings.default_index()
    }

    /// The line for the LLM request of the language `index`, `None` when only one language is
    /// configured (nothing to pin).
    pub fn instruction_of(&self, index: usize) -> Option<Arc<str>> {
        if self.settings.profiles.len() < 2 {
            return None;
        }
        self.profile(index).map(|p| Arc::clone(p.instruction()))
    }

    /// The language spoken now and its line for a request built now, from one read of the
    /// current language (for a speculative request, which is built before its turn's language
    /// is locked: the caller keeps the index and compares it with
    /// [`Self::language_for`] when the turn ends). The line is `None` with one language.
    pub fn instruction_now(&self) -> (usize, Option<Arc<str>>) {
        let index = self.current();
        (index, self.instruction_of(index))
    }

    /// A copy of the counters.
    pub fn stats(&self) -> LanguageStatsSnapshot {
        let c = &self.counters;
        LanguageStatsSnapshot {
            observed: c.observed.load(Ordering::Relaxed),
            too_short: c.too_short.load(Ordering::Relaxed),
            undecided: c.undecided.load(Ordering::Relaxed),
            from_provider: c.from_provider.load(Ordering::Relaxed),
            from_text: c.from_text.load(Ordering::Relaxed),
            switches: c.switches.load(Ordering::Relaxed),
        }
    }

    fn streak(&self) -> MutexGuard<'_, Option<(usize, u32)>> {
        self.streak.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes one finished utterance into account (see the [module docs](self) for the rule).
    /// Returns the switch when this utterance caused one.
    pub fn observe(&self, utterance: &Utterance) -> Option<LanguageSwitch> {
        bump(&self.counters.observed);
        if self.settings.profiles.len() < 2 {
            return None;
        }
        if utterance.words < self.settings.min_words {
            bump(&self.counters.too_short);
            return None;
        }
        let Some((index, confidence, source)) = self.decide(utterance) else {
            bump(&self.counters.undecided);
            return None;
        };
        bump(match source {
            LanguageSource::Provider => &self.counters.from_provider,
            LanguageSource::Text => &self.counters.from_text,
        });
        let current = self.current();
        let target = {
            let mut streak = self.streak();
            if index == current {
                *streak = None;
                return None;
            }
            let count = match *streak {
                Some((candidate, count)) if candidate == index => count.saturating_add(1),
                _ => 1,
            };
            let confident = confidence.is_some_and(|c| c >= self.settings.switch_confidence);
            if count >= self.settings.switch_after.max(1) || confident {
                *streak = None;
                index
            } else {
                *streak = Some((index, count));
                return None;
            }
        };
        self.switch_to(current, target, source)
    }

    fn switch_to(&self, from: usize, to: usize, source: LanguageSource) -> Option<LanguageSwitch> {
        let (from_profile, to_profile) = (self.profile(from)?, self.profile(to)?);
        self.current.store(to, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        bump(&self.counters.switches);
        let switch = LanguageSwitch {
            from: Arc::clone(&from_profile.code),
            to: Arc::clone(&to_profile.code),
            source,
        };
        tracing::info!(
            target: "brisvox::engine",
            from = &*switch.from,
            to = &*switch.to,
            source = ?source,
            "language switch"
        );
        if let Some(events) = &self.events {
            events.publish(EngineEvent::LanguageSwitched {
                from: Arc::clone(&switch.from),
                to: Arc::clone(&switch.to),
            });
        }
        Some(switch)
    }

    /// The configured language of the utterance and how sure the provider was, if it can be
    /// told.
    fn decide(&self, utterance: &Utterance) -> Option<(usize, Option<f32>, LanguageSource)> {
        if !utterance.reported.is_empty() {
            // Words per configured language; the largest wins (ties: the one reported first).
            let mut best: Option<(usize, usize, Option<f32>)> = None;
            let mut seen: Vec<(usize, usize, Option<f32>)> = Vec::new();
            for reported in &utterance.reported {
                let Some(index) = self.settings.index_of(&reported.code) else {
                    continue;
                };
                match seen.iter_mut().find(|(i, _, _)| *i == index) {
                    Some(entry) => entry.1 = entry.1.saturating_add(reported.words),
                    None => seen.push((index, reported.words, reported.confidence)),
                }
            }
            for entry in seen {
                if best.is_none_or(|b| entry.1 > b.1) {
                    best = Some(entry);
                }
            }
            return best
                .map(|(index, _, confidence)| (index, confidence, LanguageSource::Provider));
        }
        let index = self.detect_text(&utterance.text)?;
        Some((index, None, LanguageSource::Text))
    }

    /// The configured language the tail of `text` is written in, when the detector is sure.
    fn detect_text(&self, text: &str) -> Option<usize> {
        let tail = tail_chars(text, DETECT_CHARS);
        let info = self.detector.detect(tail)?;
        if !info.is_reliable() {
            return None;
        }
        self.detectable
            .iter()
            .find(|(lang, _)| *lang == info.lang())
            .map(|(_, index)| *index)
    }
}

/// The last `max` characters of `text` (on a character boundary).
fn tail_chars(text: &str, max: usize) -> &str {
    let count = text.chars().count();
    if count <= max {
        return text;
    }
    let skip = count - max;
    text.char_indices()
        .nth(skip)
        .and_then(|(at, _)| text.get(at..))
        .unwrap_or(text)
}

/// Appends `instruction` to the leading system message of `messages` (a provider request), or
/// puts it in a system message of its own at the front when the request has none. The stored
/// conversation is never touched: `messages` is the request's copy.
pub fn pin_language(messages: &mut Vec<brisvox_providers::Message>, instruction: &str) {
    use brisvox_providers::{Message, Role};
    match messages.first_mut() {
        Some(first) if first.role == Role::System => {
            let mut text = String::with_capacity(first.content.len() + instruction.len() + 2);
            text.push_str(&first.content);
            text.push_str("\n\n");
            text.push_str(instruction);
            first.content = Arc::from(text.as_str());
        }
        _ => messages.insert(0, Message::new(Role::System, instruction)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Arc<LanguageSettings> {
        let profiles = vec![
            LanguageProfile::new("en", Language::En, "en-voice"),
            LanguageProfile::new("tr", Language::Tr, "tr-voice").with_tts_model("tr-model"),
        ];
        Arc::new(LanguageSettings::new(profiles, "en").unwrap())
    }

    fn tracker() -> LanguageTracker {
        LanguageTracker::new(settings())
    }

    fn said(text: &str, language: Option<&str>) -> Utterance {
        let mut u = Utterance::new();
        let spoken = language.map(SpokenLanguage::new);
        u.push(text, spoken.as_ref());
        u
    }

    #[test]
    fn settings_are_checked() {
        let p = |c: &str| LanguageProfile::new(c, Language::En, "v");
        assert!(LanguageSettings::new(vec![], "en").is_err());
        assert!(LanguageSettings::new(vec![p("en"), p("en")], "en").is_err());
        assert!(LanguageSettings::new(vec![p("en")], "tr").is_err());
        assert!(LanguageSettings::new(vec![p("en"), p("tr")], "TR").is_ok());
    }

    #[test]
    fn switches_after_n_consecutive_utterances_not_before() {
        let t = tracker();
        assert_eq!(t.current_code(), "en");
        assert!(
            t.observe(&said("bana yardım eder misin", Some("tr")))
                .is_none()
        );
        assert_eq!(t.current_code(), "en", "one utterance is not enough");
        let switch = t
            .observe(&said("siparişim nerede acaba", Some("tr")))
            .unwrap();
        assert_eq!((&*switch.from, &*switch.to), ("en", "tr"));
        assert_eq!(switch.source, LanguageSource::Provider);
        assert_eq!(t.current_code(), "tr");
        assert_eq!(t.generation(), 1);
        assert_eq!(t.stats().switches, 1);
    }

    #[test]
    fn an_utterance_in_the_current_language_breaks_the_streak() {
        let t = tracker();
        assert!(
            t.observe(&said("bana yardım eder misin", Some("tr")))
                .is_none()
        );
        assert!(t.observe(&said("where is my order", Some("en"))).is_none());
        assert!(
            t.observe(&said("siparişim nerede acaba", Some("tr")))
                .is_none()
        );
        assert_eq!(t.current_code(), "en");
    }

    #[test]
    fn short_utterances_are_no_evidence() {
        let t = tracker();
        assert!(
            t.observe(&said("bana yardım eder misin", Some("tr")))
                .is_none()
        );
        // A one-word backchannel in English neither switches nor breaks the streak.
        assert!(t.observe(&said("okay", Some("en"))).is_none());
        assert_eq!(t.stats().too_short, 1);
        assert!(
            t.observe(&said("siparişim nerede acaba", Some("tr")))
                .is_some()
        );
    }

    #[test]
    fn unconfigured_languages_never_win() {
        let t = tracker();
        for _ in 0..5 {
            assert!(
                t.observe(&said("wo ist meine bestellung", Some("de")))
                    .is_none()
            );
        }
        assert_eq!(t.current_code(), "en");
        assert_eq!(t.stats().undecided, 5);
    }

    #[test]
    fn a_confident_report_switches_at_once() {
        let t = tracker();
        let mut u = Utterance::new();
        u.push(
            "siparişim nerede acaba",
            Some(&SpokenLanguage::new("tr").with_confidence(0.97)),
        );
        assert!(t.observe(&u).is_some());
        assert_eq!(t.current_code(), "tr");
        // A doubtful report does not.
        let mut u = Utterance::new();
        u.push(
            "where is my order",
            Some(&SpokenLanguage::new("en").with_confidence(0.6)),
        );
        assert!(t.observe(&u).is_none());
        assert_eq!(t.current_code(), "tr");
    }

    #[test]
    fn the_language_with_the_most_words_wins_a_mixed_utterance() {
        let t = tracker();
        let mut u = Utterance::new();
        u.push("bana şu siparişi göster", Some(&SpokenLanguage::new("tr")));
        u.push("please", Some(&SpokenLanguage::new("en")));
        assert!(t.observe(&u).is_none());
        let mut u = Utterance::new();
        u.push("bana şu siparişi göster", Some(&SpokenLanguage::new("tr")));
        u.push("please", Some(&SpokenLanguage::new("en")));
        assert!(t.observe(&u).is_some());
        assert_eq!(t.current_code(), "tr");
    }

    #[test]
    fn text_detection_is_the_fallback() {
        let t = tracker();
        let tr = "Merhaba, siparişimin nerede olduğunu öğrenmek istiyorum lütfen bana yardım eder misiniz";
        assert!(t.observe(&said(tr, None)).is_none());
        let switch = t.observe(&said(tr, None)).unwrap();
        assert_eq!(switch.source, LanguageSource::Text);
        assert_eq!(t.current_code(), "tr");
        let en =
            "Hello, I would like to find out where my order is, could you please help me with that";
        assert!(t.observe(&said(en, None)).is_none());
        assert!(t.observe(&said(en, None)).is_some());
        assert_eq!(t.current_code(), "en");
        assert_eq!(t.stats().from_text, 4);
    }

    #[test]
    fn a_single_language_never_switches() {
        let profiles = vec![LanguageProfile::new("en", Language::En, "v")];
        let t = LanguageTracker::new(Arc::new(LanguageSettings::new(profiles, "en").unwrap()));
        assert!(t.instruction_of(0).is_none());
        assert_eq!(t.instruction_now(), (0, None));
        assert!(
            t.observe(&said("bana yardım eder misin", Some("tr")))
                .is_none()
        );
    }

    #[test]
    fn an_epoch_keeps_the_language_it_started_with() {
        let t = tracker();
        let e = TurnEpoch::new;
        assert_eq!(t.language_for(e(1)), 0);
        let _ = t.observe(&said("bana yardım eder misin", Some("tr")));
        let _ = t.observe(&said("siparişim nerede acaba", Some("tr")));
        assert_eq!(t.current_code(), "tr");
        assert_eq!(
            t.language_for(e(1)),
            0,
            "the response in progress keeps English"
        );
        assert_eq!(t.language_for(e(2)), 1, "the next one is Turkish");
        assert_eq!(t.language_for(e(2)), 1);
        assert_eq!(
            t.language_for(e(1)),
            1,
            "a stale epoch gets the current language"
        );
        assert_eq!(t.instruction_of(1).as_deref(), Some("Reply in Turkish."));
    }

    #[test]
    fn pins_the_language_in_the_leading_system_message() {
        use brisvox_providers::{Message, Role};
        let mut messages = vec![
            Message::new(Role::System, "Be brief."),
            Message::new(Role::User, "hi"),
        ];
        pin_language(&mut messages, "Reply in Turkish.");
        assert_eq!(&*messages[0].content, "Be brief.\n\nReply in Turkish.");
        assert_eq!(messages.len(), 2);
        let mut bare = vec![Message::new(Role::User, "hi")];
        pin_language(&mut bare, "Reply in Turkish.");
        assert_eq!(bare[0].role, Role::System);
        assert_eq!(bare.len(), 2);
    }

    /// The detection runs on the STT task once per utterance, so it must stay far below the
    /// engine's 50 us rule. This is a debug build (no optimisation, dependencies included); the
    /// release figure is several times lower.
    #[test]
    #[allow(clippy::disallowed_methods)]
    fn text_detection_cost_is_bounded() {
        let t = tracker();
        let text = "Merhaba, siparişimin nerede olduğunu öğrenmek istiyorum lütfen bana yardım eder misiniz,                     dün akşam verdim ama hâlâ bir haber alamadım";
        let rounds = 2000u32;
        let started = std::time::Instant::now();
        let mut hits = 0u32;
        for _ in 0..rounds {
            if t.detect_text(std::hint::black_box(text)).is_some() {
                hits += 1;
            }
        }
        let per = started.elapsed() / rounds;
        eprintln!(
            "detect_text: {per:?} per {}-char sentence (debug build)",
            text.chars().count()
        );
        assert_eq!(hits, rounds);
    }

    #[test]
    fn tail_keeps_character_boundaries() {
        assert_eq!(tail_chars("çok güzel", 5), "güzel");
        assert_eq!(tail_chars("ab", 5), "ab");
    }

    #[tokio::test]
    async fn events_are_published_at_a_switch() {
        let events = EngineEvents::new(8);
        let mut stream = events.subscribe();
        let t = LanguageTracker::new(settings()).with_events(events);
        let _ = t.observe(&said("bana yardım eder misin", Some("tr")));
        let _ = t.observe(&said("siparişim nerede acaba", Some("tr")));
        let got = stream.recv().await;
        assert!(
            matches!(got, Some(EngineEvent::LanguageSwitched { .. })),
            "{got:?}"
        );
    }
}
