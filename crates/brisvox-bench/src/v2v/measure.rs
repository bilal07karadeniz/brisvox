//! The measurement itself: voice-to-voice latency per turn from ground-truth end-of-speech
//! times and the arrival times of the bot's audio (pure functions, no I/O, no clock).
//!
//! Definition (docs/LATENCY.md): V2V is the arrival, on the client side of the transport, of the
//! first non-silent bot audio after the ground-truth end of the user's speech (EOS), minus that
//! EOS. All times here are offsets from the moment the client started to stream the fixture.
//!
//! A turn is only a clean V2V sample if the bot's reply to it can be told apart from other bot
//! audio, so every turn gets an [`Outcome`]:
//! - [`Outcome::Answered`]: the first non-silent audio after EOS arrived before the user started
//!   the next utterance. Only these enter the percentiles.
//! - [`Outcome::TalkedOver`]: it arrived after the user had started the next utterance (the bot
//!   was too slow for the gap between utterances). The latency is reported but not aggregated: it
//!   would measure the fixture's gap, not the bot.
//! - [`Outcome::BotSpeaking`]: the bot was already sending audio when the user stopped (the tail
//!   of an earlier reply, or a premature one), so "the first audio after EOS" is not a reply to
//!   this turn.
//! - [`Outcome::NoResponse`]: no bot audio before the end of the call.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Serde helpers: durations as whole microseconds.
pub(crate) mod micros {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    /// Serializes a duration as microseconds (saturating).
    pub(crate) fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
    }

    /// Deserializes microseconds.
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_micros(u64::deserialize(d)?))
    }

    /// The `Option<Duration>` variant.
    pub(crate) mod opt {
        use std::time::Duration;

        use serde::{Deserialize, Deserializer, Serializer};

        /// Serializes an optional duration as microseconds or null.
        pub(crate) fn serialize<S: Serializer>(
            d: &Option<Duration>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            match d {
                Some(d) => s.serialize_some(&u64::try_from(d.as_micros()).unwrap_or(u64::MAX)),
                None => s.serialize_none(),
            }
        }

        /// Deserializes microseconds or null.
        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<Option<Duration>, D::Error> {
            Ok(Option::<u64>::deserialize(d)?.map(Duration::from_micros))
        }
    }
}

/// `d` cut to whole microseconds, the resolution of the reports (so a report survives a JSON round
/// trip unchanged).
pub fn whole_micros(d: Duration) -> Duration {
    Duration::from_micros(u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

/// Default for [`MeasureConfig::overlap_guard`].
pub const DEFAULT_OVERLAP_GUARD: Duration = Duration::from_millis(100);

/// Default peak amplitude (of 32768) below which a bot audio frame counts as silence: about
/// -44 dBFS, above codec and dither noise, far below any synthesized speech.
pub const DEFAULT_SILENCE_PEAK: u16 = 200;

/// One user turn of a fixture, as ground truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    /// Position in the fixture, from 0.
    pub index: usize,
    /// Language tag of the utterance (`en`, `tr`).
    pub lang: String,
    /// First speech sample, as an offset from the start of the stream.
    #[serde(rename = "speech_start_us", with = "micros")]
    pub speech_start: Duration,
    /// Ground-truth end of speech, as an offset from the start of the stream.
    #[serde(rename = "eos_us", with = "micros")]
    pub eos: Duration,
}

/// What became of a turn (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// A clean V2V sample.
    Answered,
    /// The first audio arrived after the user began the next utterance.
    TalkedOver,
    /// The bot was already speaking when the user stopped.
    BotSpeaking,
    /// No bot audio arrived.
    NoResponse,
}

impl Outcome {
    /// The name used in reports.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::TalkedOver => "talked_over",
            Self::BotSpeaking => "bot_speaking",
            Self::NoResponse => "no_response",
        }
    }
}

/// The result for one turn of one call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnResult {
    /// Position in the fixture, from 0.
    pub index: usize,
    /// Language tag of the utterance.
    pub lang: String,
    /// Ground-truth end of speech, as an offset from the start of the stream.
    #[serde(rename = "eos_us", with = "micros")]
    pub eos: Duration,
    /// What became of the turn.
    pub outcome: Outcome,
    /// First non-silent bot audio after EOS minus EOS: set for [`Outcome::Answered`] and
    /// [`Outcome::TalkedOver`], absent otherwise.
    #[serde(rename = "v2v_us", with = "micros::opt")]
    pub v2v: Option<Duration>,
}

/// Settings of [`measure`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasureConfig {
    /// Bot audio received within this long before EOS means the bot was already speaking
    /// ([`Outcome::BotSpeaking`]). The bot sends one frame per 20 ms, so a continuous reply is
    /// always seen within a few frames.
    pub overlap_guard: Duration,
}

impl Default for MeasureConfig {
    fn default() -> Self {
        Self {
            overlap_guard: DEFAULT_OVERLAP_GUARD,
        }
    }
}

/// Peak absolute sample value of PCM16 LE audio (0 for none). A trailing odd byte is ignored.
pub fn peak_abs(pcm: &[u8]) -> u16 {
    pcm.as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair).unsigned_abs())
        .max()
        .unwrap_or(0)
}

/// Whether a PCM16 LE frame is audible: its peak reaches `silence_peak` (at least 1, so digital
/// silence never counts).
pub fn is_audible(pcm: &[u8], silence_peak: u16) -> bool {
    peak_abs(pcm) >= silence_peak.max(1)
}

/// What one turn has seen of the bot's audio so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Slot {
    /// Audible audio arrived in `(eos - guard, eos]`.
    overlap: bool,
    /// The earliest audible arrival after EOS.
    first_after: Option<Duration>,
}

/// The online form of [`measure`]: arrivals are fed one at a time and each turn keeps two slots
/// (whether audio overlapped its EOS, the earliest arrival after it), so the memory of a call is
/// fixed by its turns, however much audio the bot sends. Each arrival costs one pass over the
/// turns (a fixture has a handful).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracker {
    turns: Vec<Turn>,
    config: MeasureConfig,
    slots: Vec<Slot>,
}

impl Tracker {
    /// A tracker for `turns`, nothing seen yet.
    pub fn new(turns: Vec<Turn>, config: MeasureConfig) -> Self {
        let slots = vec![Slot::default(); turns.len()];
        Self {
            turns,
            config,
            slots,
        }
    }

    /// Records the arrival of one non-silent bot audio frame (offset from the start of the
    /// stream). Arrivals may come in any order.
    pub fn observe(&mut self, at: Duration) {
        for (turn, slot) in self.turns.iter().zip(self.slots.iter_mut()) {
            if at > turn.eos {
                slot.first_after = Some(slot.first_after.map_or(at, |first| first.min(at)));
            } else if at > turn.eos.saturating_sub(self.config.overlap_guard) {
                slot.overlap = true;
            }
        }
    }

    /// Decides every turn from what was observed.
    pub fn results(&self) -> Vec<TurnResult> {
        self.turns
            .iter()
            .zip(&self.slots)
            .enumerate()
            .map(|(position, (turn, slot))| {
                let next_start = self
                    .turns
                    .get(position.saturating_add(1))
                    .map(|next| next.speech_start);
                let result = |outcome, v2v| TurnResult {
                    index: turn.index,
                    lang: turn.lang.clone(),
                    eos: turn.eos,
                    outcome,
                    v2v,
                };
                if slot.overlap {
                    return result(Outcome::BotSpeaking, None);
                }
                let Some(first) = slot.first_after else {
                    return result(Outcome::NoResponse, None);
                };
                let latency = whole_micros(first.saturating_sub(turn.eos));
                if next_start.is_some_and(|start| first >= start) {
                    result(Outcome::TalkedOver, Some(latency))
                } else {
                    result(Outcome::Answered, Some(latency))
                }
            })
            .collect()
    }
}

/// Decides every turn. `audible` are the arrival times (offsets from the start of the stream) of
/// the bot's non-silent audio frames, in any order. The same as feeding them to a [`Tracker`].
pub fn measure(turns: &[Turn], audible: &[Duration], config: MeasureConfig) -> Vec<TurnResult> {
    let mut tracker = Tracker::new(turns.to_vec(), config);
    for &at in audible {
        tracker.observe(at);
    }
    tracker.results()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn turn(index: usize, start: u64, eos: u64) -> Turn {
        Turn {
            index,
            lang: "en".to_owned(),
            speech_start: Duration::from_millis(start),
            eos: Duration::from_millis(eos),
        }
    }

    fn at(list: &[u64]) -> Vec<Duration> {
        list.iter().map(|&t| Duration::from_millis(t)).collect()
    }

    #[test]
    fn peak_and_audibility() {
        let pcm: Vec<u8> = [0i16, -300, 120, i16::MIN]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        assert_eq!(peak_abs(&pcm), 32_768);
        assert_eq!(peak_abs(&[]), 0);
        assert_eq!(peak_abs(&[0x10]), 0, "a lone byte is not a sample");
        let quiet: Vec<u8> = [0i16, 150, -199]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        assert!(!is_audible(&quiet, 200));
        assert!(is_audible(&quiet, 199));
        assert!(
            !is_audible(&[0, 0], 0),
            "a zero threshold still rejects digital silence"
        );
    }

    #[test]
    fn latency_is_first_audible_arrival_after_eos() {
        let turns = [turn(0, 500, 2_000), turn(1, 8_000, 9_000)];
        // Turn 0 is answered 700 ms after EOS (later frames of the same reply do not matter),
        // turn 1 at 1_234 ms.
        let audible = at(&[2_700, 2_720, 2_740, 10_234, 10_254]);
        let r = measure(&turns, &audible, MeasureConfig::default());
        assert_eq!(r[0].outcome, Outcome::Answered);
        assert_eq!(r[0].v2v, Some(700 * MS));
        assert_eq!(r[1].outcome, Outcome::Answered);
        assert_eq!(r[1].v2v, Some(1_234 * MS));
        assert_eq!((r[1].index, r[1].eos), (1, 9_000 * MS));
    }

    #[test]
    fn audio_before_eos_is_not_a_reply_and_arrival_order_does_not_matter() {
        let turns = [turn(0, 0, 1_000)];
        // Audio long before EOS (outside the guard) is ignored; the unsorted list still yields
        // the earliest arrival after EOS.
        let audible = at(&[1_900, 300, 1_500]);
        let r = measure(&turns, &audible, MeasureConfig::default());
        assert_eq!(r[0].outcome, Outcome::Answered);
        assert_eq!(r[0].v2v, Some(500 * MS));
    }

    #[test]
    fn a_reply_that_arrives_during_the_next_utterance_is_talked_over() {
        let turns = [turn(0, 0, 1_000), turn(1, 3_000, 4_000)];
        // Turn 0's reply arrives at 3_500 (the user is speaking again): reported, not aggregated.
        // Turn 1 is answered at 4_600.
        let r = measure(&turns, &at(&[3_500, 4_600]), MeasureConfig::default());
        assert_eq!(r[0].outcome, Outcome::TalkedOver);
        assert_eq!(r[0].v2v, Some(2_500 * MS));
        assert_eq!(r[1].outcome, Outcome::Answered);
        assert_eq!(r[1].v2v, Some(600 * MS));
        // Exactly at the next speech start still counts as talked over.
        let r = measure(&turns, &at(&[3_000]), MeasureConfig::default());
        assert_eq!(r[0].outcome, Outcome::TalkedOver);
    }

    #[test]
    fn audio_flowing_at_eos_marks_the_turn_bot_speaking() {
        let turns = [turn(0, 0, 1_000), turn(1, 2_000, 3_000)];
        // The bot is still sending at 2_950 and 3_000 when the user stops: turn 1 is not a clean
        // sample.
        let audible = at(&[2_950, 3_000, 3_020]);
        let r = measure(&turns, &audible, MeasureConfig::default());
        assert_eq!(
            r[0].outcome,
            Outcome::TalkedOver,
            "2_950 follows turn 0's EOS"
        );
        assert_eq!(r[1].outcome, Outcome::BotSpeaking);
        assert_eq!(r[1].v2v, None);
        // The guard is half-open: audio exactly `guard` before EOS is outside it.
        let r = measure(&turns[1..], &at(&[2_900, 3_500]), MeasureConfig::default());
        assert_eq!(r[0].outcome, Outcome::Answered);
        assert_eq!(r[0].v2v, Some(500 * MS));
        // A zero guard only looks at EOS itself.
        let none = MeasureConfig {
            overlap_guard: Duration::ZERO,
        };
        let r = measure(&turns[1..], &at(&[2_990, 3_400]), none);
        assert_eq!(r[0].outcome, Outcome::Answered);
    }

    #[test]
    fn no_audio_after_the_last_eos_is_no_response() {
        let turns = [turn(0, 0, 1_000), turn(1, 2_000, 3_000)];
        let r = measure(&turns, &at(&[1_400]), MeasureConfig::default());
        assert_eq!(r[0].outcome, Outcome::Answered);
        assert_eq!(r[1].outcome, Outcome::NoResponse);
        assert_eq!(r[1].v2v, None);
        assert!(measure(&[], &at(&[1]), MeasureConfig::default()).is_empty());
    }

    #[test]
    fn the_tracker_keeps_two_slots_per_turn_whatever_the_bot_sends() {
        let turns = [turn(0, 0, 1_000), turn(1, 3_000, 4_000)];
        let mut tracker = Tracker::new(turns.to_vec(), MeasureConfig::default());
        // A flood of arrivals (every 100 µs for 10 s) leaves the state at one slot per turn.
        for step in 0..100_000u64 {
            tracker.observe(Duration::from_micros(step * 100));
        }
        assert_eq!(tracker.slots.len(), 2);
        let r = tracker.results();
        assert_eq!(r[0].outcome, Outcome::BotSpeaking);
        assert_eq!(r[1].outcome, Outcome::BotSpeaking);
        // Out-of-order arrivals still give the earliest one after EOS.
        let mut tracker = Tracker::new(turns.to_vec(), MeasureConfig::default());
        for at in [2_000, 1_300, 1_700] {
            tracker.observe(at * MS);
        }
        assert_eq!(tracker.results()[0].v2v, Some(300 * MS));
        assert_eq!(tracker.results()[1].outcome, Outcome::NoResponse);
    }

    #[test]
    fn results_round_trip_as_microseconds() {
        let r = measure(
            &[turn(0, 0, 1_000)],
            &at(&[1_250]),
            MeasureConfig::default(),
        );
        let json = serde_json::to_string(&r[0]).unwrap();
        assert!(json.contains("\"eos_us\":1000000"), "{json}");
        assert!(json.contains("\"v2v_us\":250000"), "{json}");
        assert!(json.contains("\"outcome\":\"answered\""), "{json}");
        let back: TurnResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r[0]);
        let none = measure(&[turn(0, 0, 1_000)], &[], MeasureConfig::default());
        assert!(
            serde_json::to_string(&none[0])
                .unwrap()
                .contains("\"v2v_us\":null")
        );
    }
}
