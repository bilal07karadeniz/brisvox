//! Pre-rendered filler clips (`docs/TOOLS.md` "Tool UX"): the audio the
//! [`TtsStage`](crate::TtsStage) plays instead of rendering a filler phrase through the TTS
//! provider.
//!
//! The tool layer's [`FillerPolicy`](brisvox_agent::tools::FillerPolicy) names a clip by its
//! [`ClipKey`] when its cache reports the clip as available. A [`FillerClipStore`] is that cache
//! and also holds the audio: give the same store to the policy
//! ([`FillerPolicy::with_clips`](brisvox_agent::tools::FillerPolicy::with_clips)) and to the TTS
//! stage ([`TtsStage::with_clips`](crate::TtsStage::with_clips)), or let
//! [`SessionBuilder::with_filler_clips`](crate::session::SessionBuilder::with_filler_clips) do
//! both.
//!
//! A clip is PCM16 mono at the TTS provider's rate (the stage's
//! [`TtsStageConfig::tts_rate`](crate::TtsStageConfig::tts_rate); a clip at another rate is not
//! played, the text is rendered instead) with the start of each of its words, so a barge-in in
//! the middle of a clip is cut to the words that were heard like any other speech. The audio is
//! a [`Bytes`]: playing it shares the buffer, nothing is copied per call or per frame (the output
//! framer copies each piece once, as it does for provider audio).
//!
//! The store is built once (at startup) and read-only afterwards: no lock on the hot path.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use brisvox_agent::tools::{ClipKey, FillerClips};
use brisvox_core::frame::SampleRate;
use bytes::Bytes;

/// Bytes per PCM16 sample.
const BYTES_PER_SAMPLE: usize = 2;

/// A word of a clip: its text and where it starts, from the clip's first sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipWord {
    /// The word as written in the phrase (punctuation included, as a TTS provider reports it).
    pub word: Arc<str>,
    /// Offset of the word's first sample from the clip's first sample.
    pub start: Duration,
}

impl ClipWord {
    /// A word starting `start` into the clip.
    pub fn new(word: impl Into<Arc<str>>, start: Duration) -> Self {
        Self {
            word: word.into(),
            start,
        }
    }
}

/// Why a clip was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipError {
    /// The clip has no audio.
    Empty,
    /// The audio is not a whole number of PCM16 samples (an odd byte count).
    OddLength(usize),
    /// The clip has no words (nothing could be cut to what was heard).
    NoWords,
    /// Word `index` starts before the word before it.
    WordsOutOfOrder(usize),
    /// Word `index` starts at or after the end of the audio.
    WordPastEnd(usize),
}

impl fmt::Display for ClipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the clip has no audio"),
            Self::OddLength(bytes) => write!(f, "{bytes} bytes are not whole PCM16 samples"),
            Self::NoWords => f.write_str("the clip has no words"),
            Self::WordsOutOfOrder(index) => {
                write!(f, "word {index} starts before the word before it")
            }
            Self::WordPastEnd(index) => write!(f, "word {index} starts after the audio ends"),
        }
    }
}

impl std::error::Error for ClipError {}

/// One pre-rendered filler: PCM16 mono audio at `rate` and its word timings.
#[derive(Clone, PartialEq, Eq)]
pub struct FillerClip {
    rate: SampleRate,
    pcm: Bytes,
    words: Arc<[ClipWord]>,
    duration: Duration,
}

impl fmt::Debug for FillerClip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FillerClip")
            .field("rate", &self.rate)
            .field("bytes", &self.pcm.len())
            .field("duration", &self.duration)
            .field("words", &self.words.len())
            .finish()
    }
}

impl FillerClip {
    /// A clip of `pcm` (little-endian PCM16 mono at `rate`) whose words start at the given
    /// offsets (in order, each before the end of the audio).
    ///
    /// # Errors
    /// [`ClipError`] when the audio is empty or not whole samples, or the words are missing, out
    /// of order or past the end.
    pub fn new(rate: SampleRate, pcm: Bytes, words: Vec<ClipWord>) -> Result<Self, ClipError> {
        let duration = pcm_duration(rate, &pcm)?;
        if words.is_empty() {
            return Err(ClipError::NoWords);
        }
        let mut previous = Duration::ZERO;
        for (index, word) in words.iter().enumerate() {
            if word.start < previous {
                return Err(ClipError::WordsOutOfOrder(index));
            }
            if word.start >= duration {
                return Err(ClipError::WordPastEnd(index));
            }
            previous = word.start;
        }
        Ok(Self {
            rate,
            pcm,
            words: words.into(),
            duration,
        })
    }

    /// A clip of `pcm` speaking `text`, for audio rendered without word timings: the words of
    /// `text` (split at whitespace) are spread over the audio in proportion to their length.
    /// The estimate is coarse; a clip with the TTS provider's own timings ([`Self::new`]) is cut
    /// more precisely at a barge-in.
    ///
    /// # Errors
    /// As [`Self::new`]; [`ClipError::NoWords`] when `text` is blank.
    pub fn with_spread_words(rate: SampleRate, pcm: Bytes, text: &str) -> Result<Self, ClipError> {
        let duration = pcm_duration(rate, &pcm)?;
        let weights: Vec<(&str, u128)> = text
            .split_whitespace()
            .map(|word| (word, u128::try_from(word.chars().count()).unwrap_or(1) + 1))
            .collect();
        let total: u128 = weights.iter().map(|(_, weight)| weight).sum();
        let nanos = duration.as_nanos();
        let mut before = 0u128;
        let mut words = Vec::with_capacity(weights.len());
        for (word, weight) in weights {
            let start = nanos.saturating_mul(before).checked_div(total).unwrap_or(0);
            words.push(ClipWord::new(
                word,
                Duration::from_nanos(u64::try_from(start).unwrap_or(u64::MAX)),
            ));
            before = before.saturating_add(weight);
        }
        Self::new(rate, pcm, words)
    }

    /// The rate of the audio.
    pub const fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The audio: little-endian PCM16 mono at [`Self::rate`].
    pub const fn pcm(&self) -> &Bytes {
        &self.pcm
    }

    /// The words and their start offsets, in order.
    pub fn words(&self) -> &[ClipWord] {
        &self.words
    }

    /// How long the clip plays.
    pub const fn duration(&self) -> Duration {
        self.duration
    }
}

/// The duration of `pcm` at `rate`, after the checks every clip passes.
fn pcm_duration(rate: SampleRate, pcm: &Bytes) -> Result<Duration, ClipError> {
    if pcm.is_empty() {
        return Err(ClipError::Empty);
    }
    if !pcm.len().is_multiple_of(BYTES_PER_SAMPLE) {
        return Err(ClipError::OddLength(pcm.len()));
    }
    let samples = u128::try_from(pcm.len() / BYTES_PER_SAMPLE).unwrap_or(u128::MAX);
    let nanos = samples
        .saturating_mul(1_000_000_000)
        .checked_div(u128::from(rate.hz()))
        .unwrap_or(0);
    Ok(Duration::from_nanos(
        u64::try_from(nanos).unwrap_or(u64::MAX),
    ))
}

/// The filler clips of an agent, by key. Built once, then shared read-only (see the
/// [module docs](self)). It is the [`FillerClips`] cache of the filler policy: a key is
/// available when the store holds its clip.
#[derive(Debug, Clone, Default)]
pub struct FillerClipStore {
    clips: HashMap<ClipKey, Arc<FillerClip>>,
}

impl FillerClipStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// The store with `clip` under `key` (replacing a clip already there).
    #[must_use]
    pub fn with_clip(mut self, key: ClipKey, clip: FillerClip) -> Self {
        self.insert(key, clip);
        self
    }

    /// Puts `clip` under `key` (replacing a clip already there).
    pub fn insert(&mut self, key: ClipKey, clip: FillerClip) {
        self.clips.insert(key, Arc::new(clip));
    }

    /// The clip under `key`.
    pub fn get(&self, key: &ClipKey) -> Option<Arc<FillerClip>> {
        self.clips.get(key).cloned()
    }

    /// Number of clips.
    pub fn len(&self) -> usize {
        self.clips.len()
    }

    /// Whether the store holds no clip.
    pub fn is_empty(&self) -> bool {
        self.clips.is_empty()
    }
}

impl FillerClips for FillerClipStore {
    fn contains(&self, key: &ClipKey) -> bool {
        self.clips.contains_key(key)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn silence(ms: usize, rate: SampleRate) -> Bytes {
        let samples = usize::try_from(rate.hz()).unwrap() * ms / 1000;
        Bytes::from(vec![0u8; samples * BYTES_PER_SAMPLE])
    }

    #[test]
    fn a_clip_knows_its_duration_and_checks_its_words() {
        let rate = SampleRate::Hz24000;
        let clip = FillerClip::new(
            rate,
            silence(800, rate),
            vec![
                ClipWord::new("One", Duration::ZERO),
                ClipWord::new("moment.", Duration::from_millis(300)),
            ],
        )
        .unwrap();
        assert_eq!(clip.duration(), Duration::from_millis(800));
        assert_eq!(clip.words().len(), 2);

        let late = vec![ClipWord::new("One", Duration::from_millis(800))];
        assert_eq!(
            FillerClip::new(rate, silence(800, rate), late),
            Err(ClipError::WordPastEnd(0))
        );
        let backwards = vec![
            ClipWord::new("One", Duration::from_millis(200)),
            ClipWord::new("moment.", Duration::from_millis(100)),
        ];
        assert_eq!(
            FillerClip::new(rate, silence(800, rate), backwards),
            Err(ClipError::WordsOutOfOrder(1))
        );
        assert_eq!(
            FillerClip::new(rate, Bytes::new(), Vec::new()),
            Err(ClipError::Empty)
        );
        assert_eq!(
            FillerClip::new(rate, Bytes::from_static(&[0, 0, 0]), Vec::new()),
            Err(ClipError::OddLength(3))
        );
        assert_eq!(
            FillerClip::new(rate, silence(10, rate), Vec::new()),
            Err(ClipError::NoWords)
        );
    }

    #[test]
    fn spread_words_cover_the_clip_in_proportion_to_their_length() {
        let rate = SampleRate::Hz16000;
        let clip = FillerClip::with_spread_words(rate, silence(1000, rate), "One moment, please.")
            .unwrap();
        let words: Vec<(&str, u128)> = clip
            .words()
            .iter()
            .map(|w| (&*w.word, w.start.as_millis()))
            .collect();
        // Weights 4, 8, 8 of 20.
        assert_eq!(words, vec![("One", 0), ("moment,", 200), ("please.", 600)]);
        assert_eq!(
            FillerClip::with_spread_words(rate, silence(100, rate), "  "),
            Err(ClipError::NoWords)
        );
    }

    #[test]
    fn the_store_is_the_policys_cache() {
        let rate = SampleRate::Hz24000;
        let key = ClipKey::new("en/one-moment");
        let store = FillerClipStore::new().with_clip(
            key.clone(),
            FillerClip::with_spread_words(rate, silence(500, rate), "One moment.").unwrap(),
        );
        assert!(store.contains(&key));
        assert!(!store.contains(&ClipKey::new("en/other")));
        assert_eq!(
            store.get(&key).unwrap().duration(),
            Duration::from_millis(500)
        );
        assert_eq!(store.len(), 1);
        assert!(!store.is_empty());
    }
}
