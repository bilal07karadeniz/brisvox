//! Played-text truncation: the caller barges in at random points of a long answer, and
//! the context must hold exactly the text that had played.
//!
//! The oracle is independent of the truncation code and of the playout log: the heard position is
//! computed from the audio frames the output tap received (a chunk in flight at the barge-in
//! counts only the part that had played), and the expected text is the answer's own characters up
//! to the end of the last word that started before that position. Exact compares char for char,
//! punctuation and spacing included; Budget accepts a word prefix within
//! [`BUDGET_WORD_SLACK`](super::BUDGET_WORD_SLACK) word of the oracle and a playout log within
//! [`BUDGET_PLAYED_SLACK`] of the frames. A reply that played out in
//! full is expected whole in both.

use std::time::Duration;

use brisvox_core::TurnEpoch;
use brisvox_providers::Role;

use super::{BUDGET_PLAYED_SLACK, Profile, ensure, ensure_eq, sim_clock, some};
use crate::error::Result;
use crate::factory::SessionFactory;
use crate::latency::LatencyProfile;
use crate::mock_llm::{LlmFault, LlmScript};
use crate::mock_tts::MockTts;
use crate::scenario::Scenario;
use crate::session::{SessionRun, SimSession};

const MS: Duration = Duration::from_millis(1);

/// The scripted reply to the caller's second utterance.
const REPLY: &str = "Sure.";

/// The scripted answer, streamed in three deltas. Its words carry punctuation and spacing that
/// the truncation must keep.
const STORY: [&str; 3] = [
    "Here is a very long answer, ",
    "with many words, punctuation! ",
    "and it keeps going on and on.",
];

/// The speaking rate of the estimate fallback, as documented (`DEFAULT_CHARS_PER_SECOND`).
const ESTIMATE_CHARS_PER_SECOND: f64 = 14.0;

/// Deterministic generator (splitmix64): the same interruption points on every run.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// Byte offset just past each whitespace token of `text` (the token's own punctuation included).
fn token_ends(text: &str) -> Vec<usize> {
    let mut ends = Vec::new();
    let mut in_token = false;
    for (offset, c) in text.char_indices() {
        if c.is_whitespace() {
            if in_token {
                ends.push(offset);
            }
            in_token = false;
        } else {
            in_token = true;
        }
    }
    if in_token {
        ends.push(text.len());
    }
    ends
}

/// What the oracle derives from the output records for turn 0.
struct Heard {
    /// The barge-in instant (since the run's origin), if the caller interrupted turn 0.
    barge_in: Option<Duration>,
    /// Audio of turn 0 that had played at the barge-in, or all of it without one.
    played: Duration,
    /// The expected context text: the answer up to the last word that started before `played`.
    expected: Option<String>,
    /// Words that start inside the unplayed remainder of the chunk in flight at the barge-in:
    /// a position that counted the whole chunk would wrongly hear them.
    words_in_flight: usize,
}

/// The oracle. `played` comes from the audio frames the output tap received, not from the
/// playout log under test: each chunk started to play when the tap got it.
fn expected_heard(run: &SessionRun, full: &str, profile: Profile) -> Result<Heard> {
    let zero = TurnEpoch::ZERO;
    // The barge-in that ends turn 0 (the first interrupt to a later turn), if any.
    let barge_in = run
        .output
        .iter()
        .find(|r| r.kind == "interrupt" && r.epoch.is_some_and(|e| e > 0))
        .map(|r| r.at);
    let mut played = Duration::ZERO;
    let mut offset = Duration::ZERO;
    let mut in_flight_end = None;
    for record in run
        .output
        .iter()
        .filter(|r| r.kind == "audio" && r.epoch == Some(0))
    {
        let duration = record.audio.unwrap_or_default();
        let part = match barge_in {
            Some(b) => b.saturating_sub(record.at).min(duration),
            None => duration,
        };
        played += part;
        if part > Duration::ZERO && part < duration {
            in_flight_end = Some(offset + duration);
        }
        offset += duration;
    }
    let words = run.probes.words();
    let heard = words
        .iter()
        .filter(|(e, _, start)| *e == zero && *start < played)
        .count();
    let words_in_flight = in_flight_end.map_or(0, |end| {
        words
            .iter()
            .filter(|(e, _, start)| *e == zero && *start > played && *start < end)
            .count()
    });
    let ends = token_ends(full);
    let expected = match heard.checked_sub(1) {
        None => None,
        Some(last) => {
            let end = some(ends.get(last).copied(), "token end")?;
            Some(some(full.get(..end), "answer prefix")?.to_owned())
        }
    };
    // The code under test reads the playout log: it must agree with the output records.
    if let Some(b) = barge_in {
        let logged = run.probes.played_at(zero, run.origin + b);
        match profile {
            Profile::Exact => ensure_eq!(
                logged,
                played,
                "the playout log disagrees with the frames the output received"
            ),
            Profile::Budget => ensure!(
                logged.abs_diff(played) <= BUDGET_PLAYED_SLACK,
                "the playout log ({logged:?}) disagrees with the frames the output received \
                 ({played:?}) by more than {BUDGET_PLAYED_SLACK:?}"
            ),
        }
    }
    Ok(Heard {
        barge_in,
        played,
        expected,
        words_in_flight,
    })
}

/// The documented estimate, computed independently: `played x 14` chars of `full`, cut back to a
/// whitespace boundary.
fn expected_estimate(full: &str, played: Duration) -> String {
    let chars = (played.as_secs_f64() * ESTIMATE_CHARS_PER_SECOND).floor() as usize;
    let chars_of: Vec<(usize, char)> = full.char_indices().collect();
    let Some(&(limit, at_limit)) = chars_of.get(chars) else {
        return full.to_owned();
    };
    let head = full.get(..limit).unwrap_or_default();
    let cut = if at_limit.is_whitespace() {
        head
    } else {
        head.rfind(char::is_whitespace)
            .map_or("", |i| head.get(..i).unwrap_or_default())
    };
    cut.trim_end().to_owned()
}

/// The assistant messages of the conversation, in order.
fn assistant_texts(run: &SessionRun) -> Vec<String> {
    run.probes
        .history()
        .into_iter()
        .filter(|m| m.role == Role::Assistant)
        .map(|m| m.content.to_string())
        .collect()
}

fn first_assistant(run: &SessionRun) -> String {
    assistant_texts(run).first().cloned().unwrap_or_default()
}

fn scenario(silence: Duration) -> Scenario {
    Scenario::new()
        .silence(100 * MS)
        .speak("tell me everything", 800 * MS)
        .silence(silence)
        .speak("stop please", 600 * MS)
        .with_tail(500 * MS)
}

/// Whether `text` is cut at a word boundary of `full` (a whole-word prefix).
fn is_word_prefix(text: &str, full: &str) -> bool {
    let words: Vec<&str> = text.split_whitespace().collect();
    let all: Vec<&str> = full.split_whitespace().collect();
    all.starts_with(&words)
}

/// Barge-ins at 40 random points of a long answer leave the context at exactly the heard text.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn random_barge_ins_leave_the_context_at_exactly_the_heard_text<F: SessionFactory>(
    factory: &F,
    profile: Profile,
) -> Result<()> {
    let full: String = STORY.concat();
    let mut rng = Rng(0x7031_0031);
    let mut interrupted = 0usize;
    let mut partial = 0usize;
    let mut mid_chunk = 0usize;
    for case in 0..40 {
        // The bot starts answering at 1.2 s; the barge-in comes 0.4-7 s after the caller stops
        // speaking at 0.9 s, so it lands anywhere from the first word to after the answer ends.
        let silence = Duration::from_millis(400 + rng.below(6_600));
        let session = SimSession::new(sim_clock());
        session.llm.push(LlmScript::text(STORY));
        session.llm.push(LlmScript::text([REPLY]));
        let run = session
            .run_with(factory, &scenario(silence), 2_000 * MS)
            .await?;
        ensure_eq!(run.outcome, Ok(()), "case {case} outcome");
        run.check_no_stale_audio_after_interrupt()?;

        let heard = expected_heard(&run, &full, profile)?;
        let total = run
            .probes
            .words()
            .iter()
            .filter(|w| w.0 == TurnEpoch::ZERO)
            .count();
        let heard_words = heard
            .expected
            .as_ref()
            .map_or(0, |text| text.split_whitespace().count());
        if heard.barge_in.is_some() {
            interrupted += 1;
            // The barge-in fell inside a 40 ms chunk: only part of that chunk had played. Only
            // counted for an interrupted call (a whole answer's length need not be a multiple
            // of the chunk length either).
            if !heard.played.as_millis().is_multiple_of(40) {
                mid_chunk += 1;
            }
        }
        if heard_words > 0 && heard_words < total {
            partial += 1;
        }

        let assistant = assistant_texts(&run);
        match (profile, &heard.expected) {
            (Profile::Exact, None) => {
                // Nothing of the answer was heard: no message of it is left (the "Sure." reply
                // to the second utterance may be).
                ensure!(
                    assistant.iter().all(|m| !full.starts_with(m.as_str())),
                    "case {case}: {assistant:?}"
                );
            }
            (Profile::Exact, Some(text)) => {
                let content = assistant.first().cloned().unwrap_or_default();
                // Char-exact: the same characters as the answer, up to the end of the last word
                // that started before the caller stopped hearing it.
                ensure_eq!(
                    content,
                    *text,
                    "case {case} (played {:?}) context",
                    heard.played
                );
            }
            (Profile::Budget, expected) => {
                // The first message is a piece of the answer, or the reply to the second
                // utterance (nothing of the answer kept), or absent. Anything else is a
                // garbled or wrong context and fails.
                let content = match assistant.first().map(String::as_str) {
                    None | Some("" | REPLY) => String::new(),
                    Some(message) => {
                        ensure!(
                            full.starts_with(message) && is_word_prefix(message, &full),
                            "case {case}: the first assistant message {message:?} is not a                              prefix of the answer"
                        );
                        message.to_owned()
                    }
                };
                // Nothing heard means nothing kept: no word of slack for an unheard word.
                ensure!(
                    expected.is_some() || content.is_empty(),
                    "case {case} (played {:?}): nothing was heard but the context holds                      {content:?}",
                    heard.played
                );
                profile.heard_text(
                    &format!("case {case} context (played {:?})", heard.played),
                    &content,
                    expected.as_deref().unwrap_or(""),
                    &full,
                )?;
            }
        }
    }
    // The random points cover both cuts (some answers interrupted mid-way) and whole answers,
    // and barge-ins inside a chunk.
    ensure!(interrupted >= 12, "only {interrupted} interrupted calls");
    ensure!(partial >= 8, "only {partial} calls cut mid-answer");
    ensure!(mid_chunk >= 8, "only {mid_chunk} barge-ins inside a chunk");
    Ok(())
}

/// A run with `silence` between the caller's two utterances (default mocks, the story).
async fn story_run<F: SessionFactory>(factory: &F, silence: u64) -> Result<SessionRun> {
    let session = SimSession::new(sim_clock());
    session.llm.push(LlmScript::text(STORY));
    session.llm.push(LlmScript::text([REPLY]));
    session
        .run_with(
            factory,
            &scenario(Duration::from_millis(silence)),
            2_000 * MS,
        )
        .await
}

/// When the first audio of turn 0 reached the output, and when the caller's barge-in did.
fn first_audio_and_barge_in(run: &SessionRun) -> Result<(Duration, Duration)> {
    let first = some(
        run.output
            .iter()
            .find(|r| r.kind == "audio" && r.epoch == Some(0))
            .map(|r| r.at),
        "turn-0 audio",
    )?;
    let barge_in = some(
        run.output
            .iter()
            .find(|r| r.kind == "interrupt" && r.epoch.is_some_and(|e| e > 0))
            .map(|r| r.at),
        "barge-in",
    )?;
    Ok((first, barge_in))
}

/// Like [`story_run`], with the TTS speaking 70 ms per char in 40 ms chunks: "is" starts at
/// 350 ms, inside the chunk [320, 360) ms. `shift_ms` delays the TTS's first byte, which moves
/// the audio off the 20 ms grid of the caller's frames (a real TTS is never on it).
async fn slow_story_run<F: SessionFactory>(
    factory: &F,
    silence: u64,
    shift_ms: u64,
) -> Result<SessionRun> {
    let clock = sim_clock();
    let mut session = SimSession::new(clock.clone());
    session.llm.push(LlmScript::text(STORY));
    session.llm.push(LlmScript::text([REPLY]));
    session.tts = MockTts::new(clock)
        .with_latency(
            LatencyProfile::ZERO,
            LatencyProfile::Fixed(Duration::from_millis(shift_ms)),
            LatencyProfile::ZERO,
            0,
        )
        .with_audio(70, 40 * MS);
    session
        .run_with(
            factory,
            &scenario(Duration::from_millis(silence)),
            2_000 * MS,
        )
        .await
}

/// A word starting in the unplayed part of the chunk in flight at the barge-in is not heard.
///
/// Exact: "Here is ...": "is" starts at 350 ms (5 chars x 70 ms), inside the chunk [320, 360) ms.
/// The caller barges in 340 ms into the audio (the VAD works in 20 ms steps): a position that
/// counted the whole in-flight chunk (360 ms) would hear "is"; the playback clock (340 ms) does
/// not. The barge-in instant follows the silence in 20 ms steps, so the test searches the
/// silence that lands on 340 ms (a bounded search, so a changed VAD fails instead of hanging).
/// Budget cannot hit an instant that exactly; it searches a silence whose barge-in lands inside a
/// chunk with a word starting in its unplayed rest, and requires that the context is a word
/// prefix not longer than the oracle's (no slack: one word too many is the bug).
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn a_word_starting_in_the_unplayed_part_of_the_chunk_in_flight_is_not_heard<
    F: SessionFactory,
>(
    factory: &F,
    profile: Profile,
) -> Result<()> {
    let full: String = STORY.concat();
    if !profile.is_exact() {
        // Search a barge-in that lands inside a chunk with a word starting in its unplayed rest
        // (the oracle counts it from the output frames, bounded search). Then the context must
        // not hold that word: a pipeline that counted the whole in-flight chunk as played would
        // hear exactly one word too many, so there is no word of slack here.
        //
        // A pipeline that releases its first output frame at once (the production pacer)
        // puts the output frames on the caller's 20 ms grid, so a barge-in always lands
        // on a frame edge and no frame is in flight. A real TTS never sits on that grid: the
        // search therefore tries a 15 ms shift of the TTS's first byte first (the barge-in then
        // lands 5 ms before the word, inside its frame), and the unshifted audio only as a
        // fallback for a pipeline whose frames are off the grid by themselves.
        let mut found = None;
        'search: for shift_ms in [15u64, 0] {
            for silence in 400..1_000u64 {
                let candidate = slow_story_run(factory, silence, shift_ms).await?;
                if candidate.outcome != Ok(()) {
                    return Err(super::assertion(format!(
                        "silence {silence} ms: pipeline outcome {:?}",
                        candidate.outcome
                    )));
                }
                let heard = expected_heard(&candidate, &full, profile)?;
                if heard.barge_in.is_some() && heard.words_in_flight >= 1 {
                    found = Some((candidate, heard));
                    break 'search;
                }
            }
        }
        let (run, heard) = some(
            found,
            "a silence that puts the barge-in inside a chunk with a word starting in flight",
        )?;
        let content = first_assistant(&run);
        let content = if content == REPLY {
            String::new()
        } else {
            content
        };
        ensure!(
            is_word_prefix(&content, &full),
            "{content:?} is not a word prefix of the answer"
        );
        let oracle_words = heard
            .expected
            .as_deref()
            .map_or(0, |t| t.split_whitespace().count());
        ensure!(
            content.split_whitespace().count() <= oracle_words,
            "context {content:?} holds a word that was still in flight (oracle: {:?})",
            heard.expected
        );
        return Ok(());
    }
    let mut run = None;
    for silence in 400..1_000u64 {
        let candidate = slow_story_run(factory, silence, 0).await?;
        let (first, barge_in) = first_audio_and_barge_in(&candidate)?;
        if barge_in.checked_sub(first) == Some(340 * MS) {
            run = Some(candidate);
            break;
        }
    }
    let run = some(
        run,
        "a silence that puts the barge-in 340 ms into the audio",
    )?;
    let heard = expected_heard(&run, &full, profile)?;
    ensure_eq!(heard.played, 340 * MS, "played at the barge-in");
    ensure_eq!(heard.words_in_flight, 1, "the word \"is\" starts in flight");
    ensure_eq!(heard.expected.as_deref(), Some("Here"), "oracle text");
    let context = assistant_texts(&run);
    ensure_eq!(
        context.first().map(String::as_str),
        Some("Here"),
        "context text"
    );
    Ok(())
}

/// The caller stays silent long after the answer has played out: no barge-in, the answer is
/// kept whole.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn a_fully_played_answer_is_kept_whole<F: SessionFactory>(
    factory: &F,
    _profile: Profile,
) -> Result<()> {
    let full: String = STORY.concat();
    let run = story_run(factory, 9_000).await?;
    ensure!(
        run.output.iter().all(|r| r.kind != "interrupt"),
        "unexpected interrupt"
    );
    ensure_eq!(first_assistant(&run), full, "the context text");
    ensure_eq!(run.probes.truncation_stats().truncations, 0, "truncations");
    Ok(())
}

/// A fully played answer without word timestamps is not estimated away.
///
/// The mock speaks 60 ms per char (about 16.7 chars/s), faster than the 14 chars/s estimate:
/// applying the estimate to the whole playout would cut the answer at about 73 of 87 chars.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn a_fully_played_answer_without_word_timestamps_is_not_estimated_away<
    F: SessionFactory,
>(
    factory: &F,
    _profile: Profile,
) -> Result<()> {
    let full: String = STORY.concat();
    let clock = sim_clock();
    let mut session = SimSession::new(clock.clone());
    session.llm.push(LlmScript::text(STORY));
    session.llm.push(LlmScript::text([REPLY]));
    session.tts = MockTts::new(clock).without_word_timings();
    let run = session
        .run_with(factory, &scenario(9_000 * MS), 2_000 * MS)
        .await?;
    ensure!(
        run.output.iter().all(|r| r.kind != "interrupt"),
        "unexpected interrupt"
    );
    ensure_eq!(first_assistant(&run), full, "the context text");
    let stats = run.probes.truncation_stats();
    ensure_eq!((stats.truncations, stats.estimated), (0, 0), "{stats:?}");
    Ok(())
}

/// A barge-in is counted, and a provider without timestamps is flagged as estimated.
///
/// Exact: the estimate is exactly `played x 14` chars, cut back to a word boundary. Budget: the
/// context is a word prefix between the estimates for the played position minus and plus
/// [`BUDGET_PLAYED_SLACK`].
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn a_barge_in_is_counted_and_a_provider_without_timestamps_is_flagged_as_estimated<
    F: SessionFactory,
>(
    factory: &F,
    profile: Profile,
) -> Result<()> {
    let full: String = STORY.concat();
    for silence in [900u64, 1_500, 2_600, 4_200] {
        let clock = sim_clock();
        let mut session = SimSession::new(clock.clone());
        session.llm.push(LlmScript::text(STORY));
        session.llm.push(LlmScript::text([REPLY]));
        session.tts = MockTts::new(clock).without_word_timings();
        let run = session
            .run_with(
                factory,
                &scenario(Duration::from_millis(silence)),
                2_000 * MS,
            )
            .await?;
        ensure_eq!(run.outcome, Ok(()), "silence {silence} outcome");
        let heard = expected_heard(&run, &full, profile)?;
        ensure!(heard.barge_in.is_some(), "silence {silence}: no barge-in");
        let stats = run.probes.truncation_stats();
        ensure!(stats.truncations >= 1, "silence {silence}: {stats:?}");
        ensure!(
            stats.estimated >= 1,
            "silence {silence}: estimate not flagged in metrics: {stats:?}"
        );
        ensure_eq!(
            stats.unmatched_words,
            0,
            "silence {silence}: {stats:?} unmatched words"
        );
        let content = first_assistant(&run);
        match profile {
            Profile::Exact => ensure_eq!(
                content,
                expected_estimate(&full, heard.played),
                "silence {silence} (played {:?}) context",
                heard.played
            ),
            Profile::Budget => {
                let low =
                    expected_estimate(&full, heard.played.saturating_sub(BUDGET_PLAYED_SLACK));
                let high = expected_estimate(&full, heard.played + BUDGET_PLAYED_SLACK);
                ensure!(
                    is_word_prefix(&content, &full)
                        && content.len() >= low.len()
                        && content.len() <= high.len(),
                    "silence {silence} (played {:?}): context {content:?} outside {low:?} to \
                     {high:?}",
                    heard.played
                );
            }
        }
        ensure!(
            content.split_whitespace().count() >= 2,
            "silence {silence}: estimate too short: {content:?}"
        );
        ensure!(content.len() < full.len(), "silence {silence}: nothing cut");
    }
    Ok(())
}

/// Timed barge-ins are counted and never estimated.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn timed_barge_ins_are_counted_and_never_estimated<F: SessionFactory>(
    factory: &F,
    _profile: Profile,
) -> Result<()> {
    let run = story_run(factory, 1_500).await?;
    let stats = run.probes.truncation_stats();
    ensure!(stats.truncations >= 1, "{stats:?}");
    ensure_eq!(stats.estimated, 0, "{stats:?} estimated truncations");
    Ok(())
}

/// A barge-in while the LLM is still streaming keeps only the heard part of what was sent.
///
/// The LLM pauses 9 s before its third delta, so at the barge-in the context message holds only
/// the first two deltas (the sent text is shorter than the final answer) and the TTS is idle once
/// they have played. The cut must apply to the text sent so far, and the aborted stream must not
/// append its tail later.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn a_barge_in_while_the_llm_is_still_streaming_keeps_only_the_heard_part_of_what_was_sent<
    F: SessionFactory,
>(
    factory: &F,
    profile: Profile,
) -> Result<()> {
    let full: String = STORY.concat();
    let delivered = format!("{}{}", STORY[0], STORY[1]);
    let mut rng = Rng(0x7031_0004);
    let mut partial = 0usize;
    let mut all_delivered = 0usize;
    for case in 0..16 {
        // Barge-ins from the first words to well after the delivered audio (3.5 s) has played.
        let silence = Duration::from_millis(400 + rng.below(4_400));
        let session = SimSession::new(sim_clock());
        session
            .llm
            .push(LlmScript::text(STORY).with_fault(LlmFault::Stall {
                before_event: 2,
                duration: 9_000 * MS,
            }));
        session.llm.push(LlmScript::text([REPLY]));
        // A long linger: the stalled delta would arrive during it if the stream was not aborted.
        let run = session
            .run_with(factory, &scenario(silence), 14_000 * MS)
            .await?;
        ensure_eq!(run.outcome, Ok(()), "case {case} outcome");
        run.check_no_stale_audio_after_interrupt()?;

        let heard = expected_heard(&run, &full, profile)?;
        ensure!(heard.barge_in.is_some(), "case {case}: no barge-in");
        let content = match (profile, first_assistant(&run)) {
            // A production pipeline that drops the whole response of a barge-in before any
            // audio leaves the reply to the second utterance as the first assistant message
            // (the same normalisation as the random barge-ins above).
            (Profile::Budget, message) if message == REPLY => String::new(),
            (_, message) => message,
        };
        // Never more than what had been sent when the caller barged in.
        ensure!(
            delivered.starts_with(&content),
            "case {case}: {content:?} is not a prefix of the delivered text"
        );
        ensure!(!content.contains("keeps going"), "case {case}: {content:?}");
        match &heard.expected {
            Some(text) => {
                profile.heard_text(
                    &format!("case {case} (played {:?}) context", heard.played),
                    &content,
                    text,
                    &delivered,
                )?;
                // Exact: the whole delivered text had played. Budget: the whole SPOKEN text had
                // (a production segmenter holds back the last sentence of a stalled stream until
                // the next character or the end of the response, so less than the delivered text
                // may have been spoken): every word the TTS spoke for the turn had started
                // playing at the barge-in.
                let everything_played = if profile.is_exact() {
                    text.len() >= delivered.trim_end().len()
                } else {
                    let spoken = run
                        .probes
                        .words()
                        .iter()
                        .filter(|(epoch, _, _)| *epoch == TurnEpoch::ZERO)
                        .count();
                    let heard_words = run.played_text(TurnEpoch::ZERO).split_whitespace().count();
                    spoken > 0 && heard_words >= spoken
                };
                if everything_played {
                    all_delivered += 1;
                } else {
                    partial += 1;
                }
            }
            None => {
                // Nothing heard means nothing kept, in both profiles: the Budget word slack
                // covers a word at the cut, never a word of an answer the caller did not hear.
                ensure!(
                    content.is_empty(),
                    "case {case} (nothing heard): the context holds {content:?}"
                );
            }
        }
        // No message anywhere holds the tail of the answer.
        ensure!(
            assistant_texts(&run)
                .iter()
                .all(|m| !m.contains("keeps going")),
            "case {case}: the aborted stream's tail reached the context"
        );
    }
    ensure!(
        partial >= 3,
        "only {partial} cuts inside the delivered text"
    );
    ensure!(
        all_delivered >= 3,
        "only {all_delivered} barge-ins after the delivered text had played"
    );
    Ok(())
}

/// Runs every played-text check in order.
///
/// # Errors
/// The first failing check's [`crate::Error::Assertion`], prefixed with its name.
pub async fn all<F: SessionFactory>(factory: &F, profile: Profile) -> Result<()> {
    macro_rules! each {
        ($($name:ident),+ $(,)?) => {$(
            $name(factory, profile).await.map_err(|e| {
                super::assertion(format!("{}: {e}", stringify!($name)))
            })?;
        )+};
    }
    each!(
        random_barge_ins_leave_the_context_at_exactly_the_heard_text,
        a_word_starting_in_the_unplayed_part_of_the_chunk_in_flight_is_not_heard,
        a_fully_played_answer_is_kept_whole,
        a_fully_played_answer_without_word_timestamps_is_not_estimated_away,
        a_barge_in_is_counted_and_a_provider_without_timestamps_is_flagged_as_estimated,
        timed_barge_ins_are_counted_and_never_estimated,
        a_barge_in_while_the_llm_is_still_streaming_keeps_only_the_heard_part_of_what_was_sent,
    );
    Ok(())
}
