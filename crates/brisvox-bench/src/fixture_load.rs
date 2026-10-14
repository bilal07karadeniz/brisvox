//! Whole simulated calls driven by real speech fixtures.
//!
//! [`crate::sim_load`] drives its calls with a synthetic scripted caller. This module plays the
//! committed FLEURS recordings instead (`fixtures/`, see `fixtures/NOTICE`): English
//! (`en_two_utterances`), Turkish (`tr_two_utterances`) and one call that switches language
//! (`en_tr_mixed`). Every fixture runs as N concurrent calls through the full reference
//! pipeline (energy VAD, mock STT scripted from the fixture transcripts, turn manager, mock LLM,
//! mock TTS, pacer, transport), in real time on the multi-thread runtime, against mock providers
//! with the same latency model as the load simulation. Replies are in the language of the
//! utterance they answer.
//!
//! **Silence is padded.** The recordings leave 1.0-1.5 s between utterances, less than any bot
//! needs to answer (about 1.2 s of voice-to-voice plus the audio itself), so played as recorded
//! the caller would talk over every answer and the run would measure barge-ins, not latency.
//! [`pad_gaps`] widens every gap between two utterances to at least
//! [`FixtureLoadConfig::gap`] with digital silence (the speech samples are untouched), the same
//! way [`crate::sim_load`] keeps 3.5 s of silence after each utterance.
//!
//! What it measures: server-side voice-to-voice (`FirstAudioOut - UserSpeechEnd`) per answered
//! turn, and the internal overhead of [`crate::sim_load`], per fixture and per utterance
//! language. What it does not: the mock STT returns the fixture transcript, so recognition
//! quality (Turkish word error rate) and real provider latency are not measured here; those need
//! the live provider tasks.

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::clock::{deadline_after, wait_until};
use brisvox_core::{SystemClock, TurnEvent};
use brisvox_providers::{Message, Role};
use brisvox_sim::fixture::{Fixture, fixtures_dir};
use brisvox_sim::session::SimSession;

use crate::BenchError;
use crate::sim_load::{Collected, LLM_FIRST_TOKEN_MEDIAN_MS, TTS_FIRST_BYTE_MEDIAN_MS, configure};
use crate::stats::{Summary, ms};

const MS: Duration = Duration::from_millis(1);

/// The committed real-speech fixtures: English, Turkish, and English followed by Turkish.
pub const FIXTURES: [&str; 3] = ["en_two_utterances", "tr_two_utterances", "en_tr_mixed"];

/// What the mock LLM answers an English utterance.
const ANSWER_EN: &str = "Okay, got it.";
/// What the mock LLM answers a Turkish utterance (13 characters, like the English answer, so
/// the mock TTS audio has the same length).
const ANSWER_TR: &str = "Peki, anladım";

/// What to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureLoadConfig {
    /// Fixture names (files in `fixtures/`), run one after the other.
    pub fixtures: Vec<String>,
    /// Concurrent calls per fixture.
    pub sessions: usize,
    /// Base seed of the mock latency samplers (call `i` uses `seed + i`).
    pub seed: u64,
    /// Call starts are spread evenly over this window.
    pub ramp: Duration,
    /// Minimum silence between two utterances (see [`pad_gaps`]); must cover the bot's answer.
    pub gap: Duration,
    /// How long a call stays open after the fixture audio ended (the last answer needs time).
    pub linger: Duration,
    /// tokio worker threads (`None`: one per core).
    pub worker_threads: Option<usize>,
}

impl FixtureLoadConfig {
    /// All of [`FIXTURES`], 10 concurrent calls each, 4 s minimum silence between utterances,
    /// 500 ms ramp, 2.5 s linger.
    pub fn new() -> Self {
        Self {
            fixtures: FIXTURES.iter().map(|n| (*n).to_owned()).collect(),
            sessions: 10,
            seed: 1,
            ramp: 500 * MS,
            gap: 4_000 * MS,
            linger: 2_500 * MS,
            worker_threads: None,
        }
    }
}

impl Default for FixtureLoadConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Results for one fixture.
#[derive(Debug, Clone, PartialEq)]
pub struct FixtureReport {
    /// The fixture name.
    pub name: String,
    /// Languages of its utterances, in order (`en`, `tr`).
    pub languages: Vec<String>,
    /// Concurrent calls.
    pub sessions: usize,
    /// Turns scripted across all calls (one per utterance).
    pub turns_expected: usize,
    /// Turns that produced bot audio.
    pub turns_answered: usize,
    /// Barge-ins (should be 0: the bot finishes before the next utterance).
    pub interrupts: usize,
    /// Calls that failed or missed the deadline.
    pub failed_sessions: usize,
    /// Finished calls whose conversation is not exactly "utterance n's transcript from the
    /// caller, then `answer_for` its language from the bot" for every utterance n in order.
    /// It proves that the mock STT delivered the fixture transcripts to the LLM and that each
    /// utterance got the reply of its own language (should be 0).
    pub conversation_mismatches: usize,
    /// Voice-to-voice per answered turn (`FirstAudioOut - UserSpeechEnd`).
    pub response: Summary,
    /// Voice-to-voice split by the language of the utterance.
    pub response_by_language: Vec<(String, Summary)>,
    /// Internal overhead per answered turn (see [`crate::sim_load`]).
    pub overhead: Summary,
    /// Wall time of the fixture's run.
    pub wall: Duration,
}

impl FixtureReport {
    /// The harness's own correctness checks; empty when the run is valid.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let name = &self.name;
        if self.failed_sessions > 0 {
            problems.push(format!("{name}: {} calls failed", self.failed_sessions));
        }
        if self.conversation_mismatches > 0 {
            problems.push(format!(
                "{name}: {} calls whose transcripts or replies do not match the fixture",
                self.conversation_mismatches
            ));
        }
        if self.turns_answered < self.turns_expected {
            problems.push(format!(
                "{name}: {}/{} turns answered",
                self.turns_answered, self.turns_expected
            ));
        }
        if self.interrupts > 0 {
            problems.push(format!("{name}: {} barge-ins", self.interrupts));
        }
        if self.overhead.count < self.turns_answered {
            problems.push(format!(
                "{name}: overhead measured for {} of {} answered turns (missing marks)",
                self.overhead.count, self.turns_answered
            ));
        }
        problems
    }
}

/// Runs every fixture of `config` on a fresh multi-thread runtime.
///
/// # Errors
/// [`BenchError::Config`] for no calls, no fixtures or a fixture that cannot be loaded,
/// [`BenchError::Io`] if the runtime cannot be created.
pub fn run(config: &FixtureLoadConfig) -> Result<Vec<FixtureReport>, BenchError> {
    if config.sessions == 0 || config.fixtures.is_empty() {
        return Err(BenchError::Config(
            "need at least one call and one fixture".into(),
        ));
    }
    let mut loaded = Vec::with_capacity(config.fixtures.len());
    for name in &config.fixtures {
        let fixture = Fixture::load(&fixtures_dir(), name)
            .map_err(|e| BenchError::Config(format!("fixture {name}: {e}")))?;
        loaded.push(Arc::new(pad_gaps(&fixture, config.gap)));
    }
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_time().thread_name("bench-worker");
    if let Some(workers) = config.worker_threads {
        builder.worker_threads(workers.max(1));
    }
    let runtime = builder.build()?;
    runtime.block_on(async {
        let mut reports = Vec::with_capacity(loaded.len());
        for fixture in loaded {
            reports.push(drive(config, fixture).await);
        }
        Ok(reports)
    })
}

/// A copy of `fixture` in which every silence between two utterances is at least `min_gap` long.
///
/// Shorter gaps get digital silence (zero samples) inserted right before the next utterance; the
/// recorded samples are copied unchanged, and the segment boundaries are shifted to match, so
/// the ground truth stays exact. Gaps that are already long enough, and the audio before the first
/// and after the last utterance, are kept as recorded.
pub fn pad_gaps(fixture: &Fixture, min_gap: Duration) -> Fixture {
    let rate = u64::from(fixture.sample_rate.hz());
    let min_gap_samples =
        u64::try_from(min_gap.as_micros().saturating_mul(u128::from(rate)) / 1_000_000)
            .unwrap_or(u64::MAX);
    let total = u64::try_from(fixture.pcm.len() / 2).unwrap_or(u64::MAX);
    // The bytes of the original samples `from..to`.
    let bytes = |from: u64, to: u64| {
        let (a, b) = (
            usize::try_from(from.saturating_mul(2)).unwrap_or(usize::MAX),
            usize::try_from(to.saturating_mul(2)).unwrap_or(usize::MAX),
        );
        fixture.pcm.get(a..b).unwrap_or_default()
    };
    let mut pcm: Vec<u8> = Vec::with_capacity(fixture.pcm.len());
    let mut segments = Vec::with_capacity(fixture.segments.len());
    let mut copied = 0u64; // original samples copied so far
    let mut written = 0u64; // samples in `pcm`
    let mut previous_end: Option<u64> = None;
    for segment in &fixture.segments {
        // The original audio up to this utterance, then the extra silence, then the utterance.
        pcm.extend_from_slice(bytes(copied, segment.start));
        written += segment.start.saturating_sub(copied);
        if let Some(end) = previous_end {
            let gap = segment.start.saturating_sub(end);
            let extra = min_gap_samples.saturating_sub(gap);
            let extra_bytes = usize::try_from(extra.saturating_mul(2)).unwrap_or(0);
            pcm.resize(pcm.len().saturating_add(extra_bytes), 0);
            written += extra;
        }
        let start = written;
        pcm.extend_from_slice(bytes(segment.start, segment.end));
        written += segment.end.saturating_sub(segment.start);
        copied = segment.end;
        previous_end = Some(segment.end);
        segments.push(brisvox_sim::fixture::FixtureSegment {
            start,
            end: written,
            ..segment.clone()
        });
    }
    pcm.extend_from_slice(bytes(copied, total));
    Fixture {
        name: fixture.name.clone(),
        sample_rate: fixture.sample_rate,
        pcm: pcm.into(),
        segments,
    }
}

/// A copy of `fixture` in which every silence between two utterances is exactly `gap` long (the
/// barge-in calls of the V2V benchmark: the next utterance starts while the bot still speaks).
///
/// A longer recorded gap keeps its first `gap` of audio (the room tone after the utterance) and
/// loses the rest; a shorter one gets digital silence appended, as [`pad_gaps`] does. The speech
/// samples are copied unchanged and the segment boundaries shifted to match, so the ground truth
/// stays exact. The audio before the first and after the last utterance is kept as recorded.
pub fn set_gaps(fixture: &Fixture, gap: Duration) -> Fixture {
    let rate = u64::from(fixture.sample_rate.hz());
    let gap_samples = u64::try_from(gap.as_micros().saturating_mul(u128::from(rate)) / 1_000_000)
        .unwrap_or(u64::MAX);
    let total = u64::try_from(fixture.pcm.len() / 2).unwrap_or(u64::MAX);
    let bytes = |from: u64, to: u64| {
        let (a, b) = (
            usize::try_from(from.saturating_mul(2)).unwrap_or(usize::MAX),
            usize::try_from(to.saturating_mul(2)).unwrap_or(usize::MAX),
        );
        fixture.pcm.get(a..b).unwrap_or_default()
    };
    let mut pcm: Vec<u8> = Vec::with_capacity(fixture.pcm.len());
    let mut segments = Vec::with_capacity(fixture.segments.len());
    let mut written = 0u64; // samples in `pcm`
    let mut previous_end: Option<u64> = None;
    for segment in &fixture.segments {
        match previous_end {
            None => {
                pcm.extend_from_slice(bytes(0, segment.start));
                written += segment.start;
            }
            Some(end) => {
                let keep = segment.start.saturating_sub(end).min(gap_samples);
                pcm.extend_from_slice(bytes(end, end.saturating_add(keep)));
                let extra = gap_samples.saturating_sub(keep);
                let extra_bytes = usize::try_from(extra.saturating_mul(2)).unwrap_or(0);
                pcm.resize(pcm.len().saturating_add(extra_bytes), 0);
                written += keep + extra;
            }
        }
        let start = written;
        pcm.extend_from_slice(bytes(segment.start, segment.end));
        written += segment.end.saturating_sub(segment.start);
        previous_end = Some(segment.end);
        segments.push(brisvox_sim::fixture::FixtureSegment {
            start,
            end: written,
            ..segment.clone()
        });
    }
    pcm.extend_from_slice(bytes(previous_end.unwrap_or(0), total));
    Fixture {
        name: fixture.name.clone(),
        sample_rate: fixture.sample_rate,
        pcm: pcm.into(),
        segments,
    }
}

/// The lowest median voice-to-voice a run can plausibly report: 60 percent of the mock LLM
/// first-token and TTS first-byte medians (the STT final and the endpoint wait come on top).
/// A sanity floor derived from the configured latency model, not a measurement.
pub fn response_floor() -> Duration {
    Duration::from_millis((LLM_FIRST_TOKEN_MEDIAN_MS + TTS_FIRST_BYTE_MEDIAN_MS) * 6 / 10)
}

/// The conversation a finished call must have: for every segment of `fixture` in order, the
/// caller's transcript and the bot's reply for the segment's language.
fn expected_conversation(fixture: &Fixture) -> Vec<(Role, String)> {
    fixture
        .segments
        .iter()
        .flat_map(|s| {
            [
                (Role::User, s.text.clone()),
                (Role::Assistant, answer_for(&s.lang).to_owned()),
            ]
        })
        .collect()
}

/// Whether `history` (the call's conversation as the LLM saw it) is exactly `expected`.
fn conversation_matches(history: &[Message], expected: &[(Role, String)]) -> bool {
    history.len() == expected.len()
        && history
            .iter()
            .zip(expected)
            .all(|(m, (role, text))| m.role == *role && m.content.trim() == text.trim())
}

/// The mock LLM's answer to an utterance in language `lang`.
fn answer_for(lang: &str) -> &'static str {
    if lang == "tr" { ANSWER_TR } else { ANSWER_EN }
}

/// Plays `fixture` as `config.sessions` concurrent calls and collects their turns.
async fn drive(config: &FixtureLoadConfig, fixture: Arc<Fixture>) -> FixtureReport {
    let clock = SystemClock::shared();
    let start = clock.now();
    let call = fixture
        .duration()
        .saturating_add(config.linger)
        .saturating_add(config.ramp);
    let deadline = deadline_after(start, call.saturating_mul(3) + Duration::from_secs(30));
    let calls = u32::try_from(config.sessions).unwrap_or(u32::MAX);
    let step = config.ramp.checked_div(calls).unwrap_or_default();
    let answers: Vec<&str> = fixture
        .segments
        .iter()
        .map(|s| answer_for(&s.lang))
        .collect();
    let mut handles = Vec::with_capacity(config.sessions);
    for i in 0..calls {
        let session = SimSession::new(clock.clone());
        configure(&session, config.seed.wrapping_add(u64::from(i)), &answers);
        let fixture = Arc::clone(&fixture);
        let clock = clock.clone();
        let linger = config.linger;
        let begin = deadline_after(start, step.saturating_mul(i));
        handles.push(tokio::spawn(async move {
            wait_until(&*clock, begin).await;
            session.run_fixture(&fixture, linger).await
        }));
    }
    let expected = expected_conversation(&fixture);
    let mut mismatches = 0usize;
    let mut collected = Collected::default();
    let mut by_language: Vec<(String, Vec<Duration>)> = Vec::new();
    for mut handle in handles {
        tokio::select! {
            // A finished call is collected even if the deadline has passed meanwhile.
            biased;
            joined = &mut handle => match joined {
                Ok(Ok(run)) => {
                    collected.add_run(&run);
                    if !conversation_matches(&run.probes.history(), &expected) {
                        mismatches = mismatches.saturating_add(1);
                    }
                    // Turn n answers utterance n; only attributable when every utterance
                    // produced a turn.
                    if run.turns.len() == fixture.segments.len() {
                        for (turn, segment) in run.turns.iter().zip(&fixture.segments) {
                            let Some(response) =
                                turn.between(TurnEvent::UserSpeechEnd, TurnEvent::FirstAudioOut)
                            else {
                                continue;
                            };
                            match by_language.iter_mut().find(|(l, _)| *l == segment.lang) {
                                Some((_, samples)) => samples.push(response),
                                None => by_language.push((segment.lang.clone(), vec![response])),
                            }
                        }
                    }
                }
                Ok(Err(_)) | Err(_) => collected.failed = collected.failed.saturating_add(1),
            },
            () = clock.sleep_until(deadline) => {
                handle.abort();
                collected.failed = collected.failed.saturating_add(1);
            }
        }
    }
    let wall = clock.now().saturating_duration_since(start);
    by_language.sort_by(|a, b| a.0.cmp(&b.0));
    FixtureReport {
        name: fixture.name.clone(),
        languages: fixture.segments.iter().map(|s| s.lang.clone()).collect(),
        sessions: config.sessions,
        turns_expected: config.sessions.saturating_mul(fixture.segments.len()),
        turns_answered: collected.answered,
        interrupts: collected.interrupts,
        failed_sessions: collected.failed,
        conversation_mismatches: mismatches,
        response: Summary::of(collected.response),
        response_by_language: by_language
            .into_iter()
            .map(|(lang, samples)| (lang, Summary::of(samples)))
            .collect(),
        overhead: Summary::of(collected.overhead),
        wall,
    }
}

/// A markdown report (one row per fixture, then one per fixture and language), with
/// `environment` lines on top.
pub fn markdown(reports: &[FixtureReport], environment: &[String]) -> String {
    let mut out = String::from("# Real-speech fixture calls\n\n");
    for line in environment {
        out.push_str(&format!("- {line}\n"));
    }
    out.push_str(&format!(
        "- replies: English utterances get \"{ANSWER_EN}\", Turkish ones \"{ANSWER_TR}\"; mock \
         latency medians STT final 150 / LLM first token 350 / TTS first byte 150 ms \
         (log-normal, sigma 0.3); the mock STT returns the fixture transcript, so recognition \
         quality is not measured\n"
    ));
    out.push_str(
        "\nResponse = server-side voice-to-voice (`FirstAudioOut - UserSpeechEnd`); overhead = \
         internal overhead as defined in `docs/LATENCY.md`. Times in ms; percentiles below 100 \
         samples are coarse (p99 = max).\n\n\
         | fixture | languages | calls | answered | failed | barge-ins | response p50 | response \
         p90 | response p99 | overhead p50 | overhead p99 | wall s |\n\
         |---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    for r in reports {
        out.push_str(&format!(
            "| {} | {} | {} | {}/{} | {} | {} | {} | {} | {} | {} | {} | {:.1} |\n",
            r.name,
            r.languages.join(", "),
            r.sessions,
            r.turns_answered,
            r.turns_expected,
            r.failed_sessions,
            r.interrupts,
            ms(r.response.p50),
            ms(r.response.p90),
            ms(r.response.p99),
            ms(r.overhead.p50),
            ms(r.overhead.p99),
            r.wall.as_secs_f64(),
        ));
    }
    out.push_str(
        "\n| fixture | utterance language | turns | response p50 | response p90 | response p99 \
         | response max |\n|---|---|---:|---:|---:|---:|---:|\n",
    );
    for r in reports {
        for (lang, s) in &r.response_by_language {
            out.push_str(&format!(
                "| {} | {lang} | {} | {} | {} | {} | {} |\n",
                r.name,
                s.count,
                ms(s.p50),
                ms(s.p90),
                ms(s.p99),
                ms(s.max),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_runs() {
        let mut none = FixtureLoadConfig::new();
        none.sessions = 0;
        assert!(matches!(run(&none), Err(BenchError::Config(_))));
        let mut no_fixtures = FixtureLoadConfig::new();
        no_fixtures.fixtures.clear();
        assert!(matches!(run(&no_fixtures), Err(BenchError::Config(_))));
        let mut missing = FixtureLoadConfig::new();
        missing.fixtures = vec!["no_such_fixture".to_owned()];
        assert!(matches!(run(&missing), Err(BenchError::Config(_))));
    }

    #[test]
    fn the_committed_fixtures_include_turkish_speech() {
        for name in FIXTURES {
            let fixture = Fixture::load(&fixtures_dir(), name).unwrap();
            assert!(!fixture.segments.is_empty(), "{name}");
        }
        let tr = Fixture::load(&fixtures_dir(), "tr_two_utterances").unwrap();
        assert!(tr.segments.iter().all(|s| s.lang == "tr"));
        let mixed = Fixture::load(&fixtures_dir(), "en_tr_mixed").unwrap();
        let langs: Vec<&str> = mixed.segments.iter().map(|s| s.lang.as_str()).collect();
        assert_eq!(langs, ["en", "tr"]);
    }

    #[test]
    fn padding_widens_short_gaps_and_keeps_the_speech_samples() {
        let fixture = Fixture::load(&fixtures_dir(), "en_tr_mixed").unwrap();
        let rate = u64::from(fixture.sample_rate.hz());
        let original_gap = fixture.segments[1].start - fixture.segments[0].end;
        assert!(original_gap < rate * 2, "the recording leaves a short gap");
        let padded = pad_gaps(&fixture, 4_000 * MS);
        assert_eq!(padded.segments.len(), 2);
        // The first utterance does not move; the second starts exactly 4 s after the first ends.
        assert_eq!(padded.segments[0].start, fixture.segments[0].start);
        assert_eq!(padded.segments[1].start - padded.segments[0].end, 4 * rate);
        // Texts and languages are preserved, and so is the duration of each utterance.
        for (a, b) in fixture.segments.iter().zip(&padded.segments) {
            assert_eq!((&a.text, &a.lang), (&b.text, &b.lang));
            assert_eq!(a.end - a.start, b.end - b.start);
        }
        // The speech samples are byte-identical, and the padding is digital silence.
        let samples = |f: &Fixture, i: usize| {
            let seg = &f.segments[i];
            f.pcm[(seg.start * 2) as usize..(seg.end * 2) as usize].to_vec()
        };
        for i in 0..2 {
            assert_eq!(samples(&fixture, i), samples(&padded, i), "utterance {i}");
        }
        assert_eq!(
            padded.pcm.len(),
            fixture.pcm.len() + ((4 * rate - original_gap) * 2) as usize
        );
        // Long enough already, or no padding asked: untouched.
        let gap =
            fixture.time_of(fixture.segments[1].start) - fixture.time_of(fixture.segments[0].end);
        let same = pad_gaps(&fixture, gap);
        assert_eq!(same.pcm, fixture.pcm);
        assert_eq!(same.segments[1].start, fixture.segments[1].start);
        assert_eq!(pad_gaps(&fixture, Duration::ZERO).pcm, fixture.pcm);
    }

    #[test]
    fn set_gaps_makes_every_gap_exact_and_keeps_the_speech_samples() {
        let fixture = Fixture::load(&fixtures_dir(), "en_two_utterances").unwrap();
        let rate = u64::from(fixture.sample_rate.hz());
        let samples = |f: &Fixture, i: usize| {
            let seg = &f.segments[i];
            f.pcm[(seg.start * 2) as usize..(seg.end * 2) as usize].to_vec()
        };
        let original_gap = fixture.segments[1].start - fixture.segments[0].end;
        for gap_ms in [500u64, 1_600, 6_000] {
            let set = set_gaps(&fixture, Duration::from_millis(gap_ms));
            let gap = gap_ms * rate / 1_000;
            assert_eq!(set.segments[0].start, fixture.segments[0].start);
            assert_eq!(set.segments[1].start - set.segments[0].end, gap, "{gap_ms}");
            for i in 0..2 {
                assert_eq!(samples(&fixture, i), samples(&set, i), "{gap_ms} {i}");
                assert_eq!(set.segments[i].text, fixture.segments[i].text);
            }
            // The recording's tail after the last utterance is kept.
            let tail = fixture.pcm.len() as u64 / 2 - fixture.segments[1].end;
            assert_eq!(set.pcm.len() as u64 / 2 - set.segments[1].end, tail);
            assert_eq!(
                set.pcm.len() as i64 - fixture.pcm.len() as i64,
                2 * (gap as i64 - original_gap as i64)
            );
        }
    }

    #[test]
    fn answers_follow_the_utterance_language() {
        assert_eq!(answer_for("tr"), ANSWER_TR);
        assert_eq!(answer_for("en"), ANSWER_EN);
        // The literal replies the docs and the report name.
        assert_eq!(ANSWER_EN, "Okay, got it.");
        assert_eq!(ANSWER_TR, "Peki, anladım");
        assert_eq!(ANSWER_TR.chars().count(), ANSWER_EN.chars().count());
    }

    #[test]
    fn the_expected_conversation_pairs_each_transcript_with_its_language_reply() {
        let mixed = Fixture::load(&fixtures_dir(), "en_tr_mixed").unwrap();
        let expected = expected_conversation(&mixed);
        assert_eq!(expected.len(), 4);
        assert_eq!(expected[0], (Role::User, mixed.segments[0].text.clone()));
        assert_eq!(expected[1], (Role::Assistant, ANSWER_EN.to_owned()));
        assert_eq!(expected[2], (Role::User, mixed.segments[1].text.clone()));
        assert_eq!(expected[3], (Role::Assistant, ANSWER_TR.to_owned()));
        // The Turkish transcript really is Turkish text (not the English one twice).
        assert_ne!(expected[0].1, expected[2].1);
        let history: Vec<Message> = expected
            .iter()
            .map(|(role, text)| Message::new(*role, text.as_str()))
            .collect();
        assert!(conversation_matches(&history, &expected));
        // Every way the harness could go wrong is a mismatch: the wrong reply language, the
        // wrong order, a missing turn, a transcript from another utterance.
        let mut wrong_language = history.clone();
        wrong_language[3] = Message::new(Role::Assistant, ANSWER_EN);
        assert!(!conversation_matches(&wrong_language, &expected));
        let mut swapped = history.clone();
        swapped.swap(0, 2);
        assert!(!conversation_matches(&swapped, &expected));
        assert!(!conversation_matches(&history[..3], &expected));
        let mut other_transcript = history.clone();
        other_transcript[2] = Message::new(Role::User, mixed.segments[0].text.as_str());
        assert!(!conversation_matches(&other_transcript, &expected));
    }

    #[test]
    fn the_response_floor_comes_from_the_configured_latency_medians() {
        assert_eq!(response_floor(), Duration::from_millis(300));
    }

    #[test]
    fn problems_flag_every_correctness_signal() {
        let healthy = FixtureReport {
            name: "x".into(),
            languages: vec!["tr".into()],
            sessions: 2,
            turns_expected: 4,
            turns_answered: 4,
            interrupts: 0,
            failed_sessions: 0,
            conversation_mismatches: 0,
            response: Summary::of(vec![MS; 4]),
            response_by_language: Vec::new(),
            overhead: Summary::of(vec![MS; 4]),
            wall: Duration::from_secs(1),
        };
        assert!(healthy.problems().is_empty());
        let broken = [
            FixtureReport {
                failed_sessions: 1,
                ..healthy.clone()
            },
            FixtureReport {
                turns_answered: 3,
                overhead: Summary::of(vec![MS; 3]),
                ..healthy.clone()
            },
            FixtureReport {
                interrupts: 1,
                ..healthy.clone()
            },
            FixtureReport {
                conversation_mismatches: 1,
                ..healthy.clone()
            },
            FixtureReport {
                overhead: Summary::of(vec![MS; 3]),
                ..healthy.clone()
            },
        ];
        for report in broken {
            assert_eq!(report.problems().len(), 1, "{report:?}");
        }
    }
}
