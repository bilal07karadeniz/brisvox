//! The filler policy of the tool layer (TOOLS.md item 9): when a tool round has not
//! produced its result after a delay, what the agent says meanwhile.
//!
//! The policy only *decides*. It names the delay (to be measured on the injected clock by the
//! caller that races the tool round against it) and the phrase, with the key of a pre-rendered
//! audio clip when the clip cache has one. The caller (the engine's `LlmRunner`) speaks the text
//! as a round of its own, so it takes part in the played output like any other speech.
//!
//! # Delay
//! Two modes:
//! - [`FillerPolicy::new`] (a fixed delay, [`DEFAULT_FILLER_DELAY`] = 1.2 s unless changed with
//!   [`FillerPolicy::with_delay`]): every tool that has a filler enabled uses the same delay. A
//!   tool opts out with `ToolSpec::with_filler_after(None)`; its own `Some(delay)` is not read.
//! - [`FillerPolicy::per_tool`]: each tool's `ToolSpec::filler_after` is the delay (the behaviour
//!   before the global policy existed).
//!
//! A round with several calls fills after the smallest delay of its calls: the user waits for the
//! slowest tool, but the first tool that would have announced itself wins.
//!
//! # Phrases
//! Phrases rotate round-robin (a counter, no randomness, so simulations are deterministic and a
//! caller never hears the same phrase twice in a row when there are two or more). A tool can have
//! phrases of its own ([`FillerPolicy::with_tool_phrases`]: "Checking the calendar."); other tools
//! use the generic list. Only a filler that is actually spoken advances the rotation
//! ([`FillerPolicy::choose`] is called when the delay has passed, not when a round starts).
//!
//! # Pre-rendered clips
//! A [`FillerPhrase`] may carry a [`ClipKey`] naming a pre-rendered audio clip of its text. The
//! key reaches the caller only when the policy's [`FillerClips`] cache reports the clip as
//! available; otherwise the choice carries the text alone and the TTS renders it. The cache is a
//! trait so the engine can back it with whatever holds the audio.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::assemble::Assembled;

use super::ToolRegistry;

/// The delay of a [`FillerPolicy::new`] policy: 1.2 seconds without a tool result.
pub const DEFAULT_FILLER_DELAY: Duration = Duration::from_millis(1200);

/// The generic filler phrase (English): the policy's phrase until it is given others.
pub const DEFAULT_FILLER_TEXT: &str = "One moment, please.";

/// The key of a pre-rendered audio clip, for example `"en/one-moment"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClipKey(Arc<str>);

impl ClipKey {
    /// A key from its text.
    pub fn new(key: impl Into<Arc<str>>) -> Self {
        Self(key.into())
    }

    /// The key as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ClipKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The cache of pre-rendered filler audio: says which clips exist.
pub trait FillerClips: Send + Sync {
    /// Whether the clip under `key` is ready to play.
    fn contains(&self, key: &ClipKey) -> bool;
}

/// A [`FillerClips`] over a fixed set of keys.
#[derive(Debug, Clone, Default)]
pub struct StaticClips(BTreeSet<ClipKey>);

impl StaticClips {
    /// A cache holding exactly `keys`.
    pub fn new(keys: impl IntoIterator<Item = ClipKey>) -> Self {
        Self(keys.into_iter().collect())
    }
}

impl FillerClips for StaticClips {
    fn contains(&self, key: &ClipKey) -> bool {
        self.0.contains(key)
    }
}

/// One filler phrase: the text to speak and, optionally, the key of its pre-rendered clip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillerPhrase {
    /// What is said. Also what TTS renders when the clip is not available.
    pub text: Arc<str>,
    /// The pre-rendered clip of `text`, if one was made.
    pub clip: Option<ClipKey>,
}

impl FillerPhrase {
    /// A phrase without a clip.
    pub fn new(text: impl Into<Arc<str>>) -> Self {
        Self {
            text: text.into(),
            clip: None,
        }
    }

    /// The same phrase with the key of its pre-rendered clip.
    #[must_use]
    pub fn with_clip(mut self, clip: ClipKey) -> Self {
        self.clip = Some(clip);
        self
    }
}

/// Where a round's filler delay comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delay {
    /// One delay for every tool that has a filler.
    Fixed(Duration),
    /// Each tool's `ToolSpec::filler_after`.
    PerTool,
}

/// The filler settings of a session, for people who build a policy from configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillerConfig {
    /// Whether fillers are spoken at all. Default `true`.
    pub enabled: bool,
    /// The delay of every tool that has a filler. `None` uses each tool's own
    /// `ToolSpec::filler_after`. Default `Some(`[`DEFAULT_FILLER_DELAY`]`)`.
    pub delay: Option<Duration>,
}

impl FillerConfig {
    /// On, with the fixed delay [`DEFAULT_FILLER_DELAY`] (1.2 s).
    pub const DEFAULT: Self = Self {
        enabled: true,
        delay: Some(DEFAULT_FILLER_DELAY),
    };

    /// On, with each tool's own `ToolSpec::filler_after` as its delay (how fillers worked before
    /// the global policy existed).
    pub const PER_TOOL: Self = Self {
        enabled: true,
        delay: None,
    };
}

impl Default for FillerConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// When a tool round needs a filler: the delay, and the tool that set it (the filler's phrase may
/// depend on the tool).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillerPlan {
    /// How long after the round started, on the injected clock, the filler is due.
    pub after: Duration,
    /// The tool whose delay this is (the first of the round's calls with the smallest delay).
    pub tool: Arc<str>,
}

/// What to say when a [`FillerPlan`]'s delay has passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillerChoice {
    /// What is said (rendered by TTS unless the clip plays).
    pub text: Arc<str>,
    /// The pre-rendered clip of `text`, only when the policy's cache has it ready.
    pub clip: Option<ClipKey>,
}

/// The filler policy. A clone shares the clip cache but starts its own phrase rotation, so a
/// policy built once from configuration and cloned into each session keeps every session's
/// phrases independent of the others (and deterministic in a simulation of many sessions).
pub struct FillerPolicy {
    enabled: bool,
    delay: Delay,
    generic: Vec<FillerPhrase>,
    by_tool: BTreeMap<Arc<str>, Vec<FillerPhrase>>,
    clips: Option<Arc<dyn FillerClips>>,
    /// The rotation of this session's phrases: never shared between clones.
    next: AtomicUsize,
}

impl Clone for FillerPolicy {
    fn clone(&self) -> Self {
        Self {
            enabled: self.enabled,
            delay: self.delay,
            generic: self.generic.clone(),
            by_tool: self.by_tool.clone(),
            clips: self.clips.clone(),
            next: AtomicUsize::new(0),
        }
    }
}

impl fmt::Debug for FillerPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FillerPolicy")
            .field("enabled", &self.enabled)
            .field("delay", &self.delay)
            .field("generic", &self.generic.len())
            .field("tools_with_phrases", &self.by_tool.len())
            .field("has_clip_cache", &self.clips.is_some())
            .finish()
    }
}

impl Default for FillerPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl FillerPolicy {
    /// A policy with the fixed delay [`DEFAULT_FILLER_DELAY`] and the phrase
    /// [`DEFAULT_FILLER_TEXT`].
    pub fn new() -> Self {
        Self {
            enabled: true,
            delay: Delay::Fixed(DEFAULT_FILLER_DELAY),
            generic: vec![FillerPhrase::new(DEFAULT_FILLER_TEXT)],
            by_tool: BTreeMap::new(),
            clips: None,
            next: AtomicUsize::new(0),
        }
    }

    /// A policy that uses each tool's `ToolSpec::filler_after` as its delay, with the phrase
    /// [`DEFAULT_FILLER_TEXT`] (how fillers worked before the global policy existed).
    pub fn per_tool() -> Self {
        Self {
            delay: Delay::PerTool,
            ..Self::new()
        }
    }

    /// A policy from `config`.
    pub fn from_config(config: FillerConfig) -> Self {
        let policy = match config.delay {
            Some(delay) => Self::new().with_delay(delay),
            None => Self::per_tool(),
        };
        policy.with_enabled(config.enabled)
    }

    /// Turns fillers on or off for the whole session.
    #[must_use]
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// One delay for every tool that has a filler (replaces the per-tool delays).
    #[must_use]
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = Delay::Fixed(delay);
        self
    }

    /// The generic phrases, spoken in turn. An empty list switches fillers off.
    #[must_use]
    pub fn with_phrases(mut self, phrases: impl IntoIterator<Item = FillerPhrase>) -> Self {
        self.generic = phrases.into_iter().collect();
        self
    }

    /// Phrases for one tool, spoken in turn when that tool sets the round's delay. An empty list
    /// removes them (the generic phrases are used).
    #[must_use]
    pub fn with_tool_phrases(
        mut self,
        tool: impl Into<Arc<str>>,
        phrases: impl IntoIterator<Item = FillerPhrase>,
    ) -> Self {
        let tool = tool.into();
        let phrases: Vec<FillerPhrase> = phrases.into_iter().collect();
        if phrases.is_empty() {
            self.by_tool.remove(&tool);
        } else {
            self.by_tool.insert(tool, phrases);
        }
        self
    }

    /// The cache of pre-rendered clips. Without one, a choice never carries a clip key.
    #[must_use]
    pub fn with_clips(mut self, clips: Arc<dyn FillerClips>) -> Self {
        self.clips = Some(clips);
        self
    }

    /// Replaces the generic phrase with one text (no clip), keeping everything else. The engine's
    /// `with_phrases(filler, fallback)` goes through this.
    #[must_use]
    pub fn with_text(self, text: impl Into<Arc<str>>) -> Self {
        self.with_phrases([FillerPhrase::new(text)])
    }

    /// Whether the policy can say anything: enabled, with at least one phrase.
    pub fn is_active(&self) -> bool {
        self.enabled && !self.generic.is_empty()
    }

    /// When the round made of `calls` needs a filler: after the smallest delay of its calls whose
    /// tool has a filler (`ToolSpec::filler_after` is `Some`). `None` when fillers are off, when no
    /// call asks for one, or when the calls are invalid or unknown.
    pub fn plan(&self, registry: &ToolRegistry, calls: &[Assembled]) -> Option<FillerPlan> {
        if !self.is_active() {
            return None;
        }
        let mut best: Option<FillerPlan> = None;
        for call in calls {
            let Assembled::Complete(call) = call else {
                continue;
            };
            let Some(own) = registry.spec(&call.name).and_then(|spec| spec.filler_after) else {
                continue;
            };
            let after = match self.delay {
                Delay::Fixed(delay) => delay,
                Delay::PerTool => own,
            };
            // Strictly smaller: the first call wins a tie.
            if best.as_ref().is_none_or(|b| after < b.after) {
                best = Some(FillerPlan {
                    after,
                    tool: Arc::clone(&call.name),
                });
            }
        }
        best
    }

    /// The next phrase for `plan`, with its clip key when the cache has the clip. Call it when
    /// the filler is actually spoken: it advances the rotation. `None` when the policy has no
    /// phrase to say.
    pub fn choose(&self, plan: &FillerPlan) -> Option<FillerChoice> {
        if !self.enabled {
            return None;
        }
        let pool = match self.by_tool.get(&plan.tool) {
            Some(phrases) if !phrases.is_empty() => phrases,
            _ => &self.generic,
        };
        if pool.is_empty() {
            return None;
        }
        let turn = self.next.fetch_add(1, Ordering::Relaxed);
        let phrase = pool.get(turn % pool.len())?;
        let clip = match (&phrase.clip, &self.clips) {
            (Some(key), Some(cache)) if cache.contains(key) => Some(key.clone()),
            _ => None,
        };
        Some(FillerChoice {
            text: Arc::clone(&phrase.text),
            clip,
        })
    }
}
