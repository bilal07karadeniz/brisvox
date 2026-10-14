//! Played-text truncation from the `PlayoutBook` on the all-production pipeline (`docs/PIPELINE.md`
//! section 5), judged against the WIRE: what the loopback client received, and
//! when. The oracle never reads the book or the context code under test. It takes the barge-in
//! instant from the `Clear` the client received, the heard position from the audio frames the
//! client got before it (a frame counts as played over the 20 ms after its arrival), and the heard
//! words from the TTS's own word offsets. The transport reports positions every 20 ms
//! (`position_interval`), so the position under test and the oracle's can differ by one frame:
//! the oracle accepts every word count between those at `position - 20 ms` and `position + 20 ms`,
//! and a separate test proves that this tolerance does not hide a deliberate one-word error.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

mod common;
#[path = "common/output_rig.rs"]
mod output_rig;

use std::sync::Arc;
use std::time::Duration;

use brisvox_agent::context::Message;
use brisvox_core::{TurnEpoch, TurnEvent};
use brisvox_engine::{
    BoxFuture, Cut, CutRequest, EngineConfig, InterruptionConfig, RoundKey, TruncationSource,
};
use brisvox_providers::Role;
use brisvox_sim::latency::LatencyProfile;
use brisvox_sim::mock_common::StreamFault;
use brisvox_sim::mock_llm::{LlmFault, LlmScript};
use brisvox_sim::mock_tts::MockTts;
use brisvox_sim::output::WireKind;
use brisvox_sim::scenario::Scenario;
use brisvox_sim::session::{SessionRun, SimSession};
use brisvox_sim::suite::Profile;
use brisvox_sim::suite::played_text as suite;
use common::MS;
use common::llm::{SlowTool, ToolProbe};
use output_rig::{HybridOutput, Policy, ToolFactory};

use brisvox_agent::tools::{
    EngineConfig as ToolEngineConfig, SessionId, ToolEngine, ToolKind, ToolRegistry, ToolSpec,
};
use brisvox_core::{SharedClock, SimClock};

/// One frame of the transport's output (and the position interval of the loopback session).
const FRAME: Duration = Duration::from_millis(20);

/// The scripted answer, streamed in three deltas.
const STORY: [&str; 3] = [
    "Here is a very long answer, ",
    "with many words, punctuation! ",
    "and it keeps going on and on.",
];

/// The reply to the caller's second utterance.
const REPLY: &str = "Sure.";

/// Deterministic generator (splitmix64).
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) % n
    }
}

fn sim_clock() -> SharedClock {
    Arc::new(SimClock::new())
}

/// The caller says something, waits `silence`, and barges in with "stop please".
fn scenario(silence: Duration) -> Scenario {
    Scenario::new()
        .silence(100 * MS)
        .speak("tell me everything", 800 * MS)
        .silence(silence)
        .speak("stop please", 600 * MS)
        .with_tail(500 * MS)
}

fn quick() -> HybridOutput {
    HybridOutput::new(Policy::Quick, InterruptionConfig::OFF)
}

/// Byte offset just past each whitespace token of `text`.
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

/// The wire oracle for turn 0 of one run.
struct Oracle {
    full: String,
    ends: Vec<usize>,
    /// Whether the caller barged in on turn 0.
    barge_in: bool,
    /// The heard position read off the wire: audio of epoch 0 the client had received before the
    /// `Clear`, or all of it without a barge-in.
    position: Duration,
    /// Word counts the oracle accepts, inclusive.
    lo: usize,
    hi: usize,
    /// Words the TTS timed in total.
    total_words: usize,
}

impl Oracle {
    fn new(run: &SessionRun, full: &str) -> Self {
        Self::with_margin(run, full, Duration::ZERO)
    }

    /// Like [`Self::new`] for an engine configured with `lead_margin = margin`: the audio the
    /// client holds is not yet heard, so the heard position is the wire position less the margin
    /// (an answer that played out without a barge-in is unaffected).
    fn with_margin(run: &SessionRun, full: &str, margin: Duration) -> Self {
        let zero = TurnEpoch::ZERO;
        let interrupted = run
            .output
            .iter()
            .any(|r| r.kind == "interrupt" && r.epoch.is_some_and(|e| e > 0));
        let clear = run
            .wire
            .iter()
            .find(|r| r.kind == WireKind::Clear && r.epoch.is_some_and(|e| e >= 1))
            .map(|r| r.at);
        let barge_at = if interrupted { clear } else { None };
        let mut position = Duration::ZERO;
        for record in run
            .wire
            .iter()
            .filter(|r| r.kind == WireKind::Audio && r.epoch == Some(0))
        {
            let duration = record.audio.unwrap_or_default();
            position += match barge_at {
                Some(at) => at.saturating_sub(record.at).min(duration),
                None => duration,
            };
        }
        if barge_at.is_some() {
            position = position.saturating_sub(margin);
        }
        let starts: Vec<Duration> = run
            .probes
            .words()
            .iter()
            .filter(|(e, _, _)| *e == zero)
            .map(|(_, _, start)| *start)
            .collect();
        let heard_at = |p: Duration| starts.iter().filter(|s| **s < p).count();
        let (lo, hi) = if barge_at.is_some() {
            (
                heard_at(position.saturating_sub(FRAME)),
                heard_at(position + FRAME),
            )
        } else {
            // Nothing cut it: the whole answer is expected, exactly.
            (starts.len(), starts.len())
        };
        Self {
            full: full.to_owned(),
            ends: token_ends(full),
            barge_in: barge_at.is_some(),
            position,
            lo,
            hi,
            total_words: starts.len(),
        }
    }

    /// The text of the first `k` whitespace tokens of the answer.
    fn prefix(&self, k: usize) -> &str {
        match k.checked_sub(1) {
            None => "",
            Some(last) => self.full.get(..self.ends[last]).unwrap_or(&self.full),
        }
    }

    /// Whether `text` is the answer cut after a word count the oracle accepts.
    fn accepts(&self, text: &str) -> bool {
        let k = text.split_whitespace().count();
        k <= self.ends.len() && (self.lo..=self.hi).contains(&k) && text == self.prefix(k)
    }

    /// The window is a single word count: the tolerance blurs nothing.
    fn narrow(&self) -> bool {
        self.lo == self.hi
    }
}

fn assistant_texts(run: &SessionRun) -> Vec<String> {
    run.probes
        .history()
        .into_iter()
        .filter(|m| m.role == Role::Assistant)
        .map(|m| m.content.to_string())
        .collect()
}

/// What the context kept of the answer: the first assistant message, or nothing when the answer
/// was removed (then the first assistant message is the reply to the second utterance).
fn kept(run: &SessionRun) -> String {
    match assistant_texts(run).first().map(String::as_str) {
        None | Some(REPLY) => String::new(),
        Some(text) => text.to_owned(),
    }
}

async fn story_run(factory: &HybridOutput, silence: u64) -> SessionRun {
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
        .unwrap()
}

/// The 40 seeded silences: the barge-in comes 0.4 s to 7 s after the caller's first utterance,
/// from the first word to after the answer has played out.
fn silences() -> Vec<u64> {
    let mut rng = Rng(0x7039_0007);
    (0..40).map(|_| 400 + rng.below(6_600)).collect()
}

/// The 40 seeded barge-ins against `factory`, whose `lead_margin` is `margin`: the context must
/// hold the answer cut at the wire position less the margin, and the cut must not delay the next
/// turn's LLM request (the settle wait is measured from the barge-in, not from the question).
async fn forty_seeded_barge_ins(factory: &HybridOutput, margin: Duration) {
    let full: String = STORY.concat();
    let (mut interrupted, mut partial, mut narrow, mut whole) = (0, 0, 0, 0);
    let mut barge_ins = Vec::new();
    for (case, silence) in silences().into_iter().enumerate() {
        let run = story_run(factory, silence).await;
        assert_eq!(run.outcome, Ok(()), "case {case}");
        let oracle = Oracle::with_margin(&run, &full, margin);
        let context = kept(&run);
        assert!(
            oracle.accepts(&context),
            "case {case} (silence {silence} ms, heard position {:?}, accepted words {}..={}): \
             the context holds {context:?}",
            oracle.position,
            oracle.lo,
            oracle.hi
        );
        // The truncation costs the next turn no time: its request leaves at the turn end (the
        // caller talked on for longer than `settle_wait` after the barge-in).
        for turn in &run.turns {
            if let Some(gap) = turn.between(TurnEvent::TurnEnd, TurnEvent::LlmRequest) {
                assert!(
                    gap <= FRAME,
                    "case {case}: the LLM request left {gap:?} after the turn end"
                );
            }
        }
        interrupted += usize::from(oracle.barge_in);
        barge_ins.push(oracle.barge_in);
        let words = context.split_whitespace().count();
        if oracle.barge_in && words > 0 && words < oracle.total_words {
            partial += 1;
            narrow += usize::from(oracle.narrow());
        }
        whole += usize::from(!oracle.barge_in);
    }
    assert!(interrupted >= 12, "only {interrupted} interrupted calls");
    assert!(partial >= 8, "only {partial} calls cut mid-answer");
    assert!(whole >= 3, "only {whole} answers heard whole");
    assert!(
        narrow * 10 >= partial * 6,
        "the tolerance blurred {} of {partial} cuts",
        partial - narrow
    );
    // The transport reports a position at every 20 ms frame here and flushes a final one on a
    // clear only if audio was released since its last report. A barge-in between two frames has
    // none to wait for: the settle wait then runs out (counted; it is measured from the barge-in,
    // so by the time the turn ends it costs no time) and the last report is used, which is the
    // final position anyway. It can only happen to a call that was interrupted.
    let mut timeouts = 0;
    for (call, barged_in) in factory.handles().iter().zip(&barge_ins) {
        // The turn driver told the book about the clear (`clear_hook`): the utterance is cut.
        let round = call
            .input
            .book
            .snapshot(RoundKey::new(TurnEpoch::ZERO, 0))
            .unwrap();
        assert_eq!(round.cleared, *barged_in);
        let llm = call.llm.snapshot();
        assert_eq!((llm.cut_errors, llm.validate_failures), (0, 0), "{llm:?}");
        let book = call.input.book.stats();
        assert_eq!(book.conflicting_registrations, 0, "{book:?}");
        // The clear hook told the book about every barge-in (a driver wired with `with_commands`
        // leaves this at zero and makes every mid-utterance cut wait out `settle_wait`).
        assert_eq!(book.clears_noted > 0, *barged_in, "{book:?}");
        timeouts += book.settle_timeouts;
    }
    assert!(
        timeouts <= interrupted as u64,
        "{timeouts} settle timeouts in {interrupted} interrupted calls"
    );
    // The final position of most barge-ins is flushed right after the clear: those settle
    // without a timeout. A `clear_hook` that told the book after the command (the flush then no
    // longer counts as post-clear), or never, would time out every interrupted call.
    assert!(
        timeouts < interrupted as u64,
        "every one of the {interrupted} interrupted calls ran out its settle wait"
    );
}

#[tokio::test(start_paused = true)]
async fn forty_seeded_barge_ins_leave_the_context_at_the_wire_arrival_oracle() {
    // The loopback client's playback is the wire arrival: the rig runs with `lead_margin` zero.
    forty_seeded_barge_ins(&quick(), Duration::ZERO).await;
}

#[tokio::test(start_paused = true)]
async fn forty_seeded_barge_ins_with_the_production_lead_margin_cut_160_ms_before_the_wire() {
    // The same 40 cases with the default 160 ms margin: the context is the wire position less
    // 160 ms, never more (a margin subtracted twice, or from a fully played answer, would fail).
    let mut factory = quick();
    factory.config.lead_margin = Duration::from_millis(160);
    assert_eq!(
        factory.config.lead_margin,
        EngineConfig::DEFAULT.lead_margin
    );
    forty_seeded_barge_ins(&factory, Duration::from_millis(160)).await;
}

/// A truncation source that makes one deliberate word of error on top of the production one.
#[derive(Clone, Copy)]
enum Fault {
    /// The last heard word is dropped.
    DropWord,
    /// The next word, not heard, is kept.
    AddWord,
}

struct OffByOne {
    inner: Arc<dyn TruncationSource>,
    fault: Fault,
}

impl TruncationSource for OffByOne {
    fn cut<'a>(&'a self, request: &'a CutRequest) -> BoxFuture<'a, Cut> {
        Box::pin(async move {
            let Cut::Heard(mut truncation) = self.inner.cut(request).await else {
                return self.inner.cut(request).await;
            };
            let text: &str = &truncation.text;
            let mutated = match self.fault {
                Fault::DropWord if text.trim().is_empty() => None,
                Fault::DropWord => Some(
                    text.trim_end()
                        .rsplit_once(char::is_whitespace)
                        .map_or(String::new(), |(head, _)| head.trim_end().to_owned()),
                ),
                Fault::AddWord => {
                    let ends = token_ends(&request.sent);
                    let kept = text.split_whitespace().count();
                    ends.get(kept)
                        .and_then(|end| request.sent.get(..*end))
                        .map(str::to_owned)
                }
            };
            if let Some(mutated) = mutated {
                truncation.text = Arc::from(mutated);
            }
            Cut::Heard(truncation)
        })
    }
}

fn faulty(fault: Fault) -> HybridOutput {
    quick().with_source_wrap(Arc::new(move |inner| Arc::new(OffByOne { inner, fault })))
}

#[tokio::test(start_paused = true)]
async fn the_tolerance_does_not_mask_a_deliberate_one_word_error() {
    let full: String = STORY.concat();
    for (name, fault) in [("dropped", Fault::DropWord), ("added", Fault::AddWord)] {
        let factory = faulty(fault);
        let (mut cuts, mut caught, mut masked) = (0, 0, 0);
        for (case, silence) in silences().into_iter().enumerate() {
            let run = story_run(&factory, silence).await;
            assert_eq!(run.outcome, Ok(()), "case {case}");
            let oracle = Oracle::new(&run, &full);
            // The fault needs something to drop (a heard word) or to add (an unheard one).
            let applicable = oracle.barge_in
                && match fault {
                    Fault::DropWord => oracle.hi >= 1,
                    Fault::AddWord => oracle.lo < oracle.total_words,
                };
            if !applicable {
                continue;
            }
            cuts += 1;
            let context = kept(&run);
            if oracle.accepts(&context) {
                // Only a window of more than one word count can let a one-word error through.
                assert!(
                    !oracle.narrow(),
                    "case {case} ({name}): {context:?} passed a single-count window"
                );
                masked += 1;
            } else {
                caught += 1;
            }
        }
        assert!(cuts >= 15, "{name}: only {cuts} cuts were faulted");
        assert!(
            caught * 100 >= cuts * 60,
            "{name}: the oracle caught only {caught} of {cuts} (masked {masked})"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_fully_played_answer_is_kept_whole_and_barge_ins_are_counted_not_estimated() {
    // The shared played-text checks that do not read the output tap's timing (the tap sits in
    // front of the pacer, so its audio records are not wire arrival; the oracle above is).
    suite::a_fully_played_answer_is_kept_whole(&quick(), Profile::Budget)
        .await
        .unwrap();
    suite::timed_barge_ins_are_counted_and_never_estimated(&quick(), Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_word_starting_in_the_unplayed_part_of_the_chunk_in_flight_is_not_heard() {
    // 70 ms per char in 40 ms chunks: "is" starts at 350 ms, inside the chunk [320, 360) ms. A
    // position that counted the whole in-flight chunk as played would hear "is" for a barge-in
    // between 320 and 350 ms into the audio.
    let full: String = STORY.concat();
    let factory = quick();
    let (mut in_flight, mut in_flight_shifted) = (0, 0);
    for (shift, silence) in [0u64, 15]
        .into_iter()
        .flat_map(|shift| (400..1_000u64).step_by(10).map(move |s| (shift, s)))
    {
        let clock = sim_clock();
        let mut session = SimSession::new(clock.clone());
        session.llm.push(LlmScript::text(STORY));
        session.llm.push(LlmScript::text([REPLY]));
        session.tts = MockTts::new(clock)
            .with_latency(
                LatencyProfile::ZERO,
                LatencyProfile::Fixed(Duration::from_millis(shift)),
                LatencyProfile::ZERO,
                0,
            )
            .with_audio(70, 40 * MS);
        let run = session
            .run_with(
                &factory,
                &scenario(Duration::from_millis(silence)),
                2_000 * MS,
            )
            .await
            .unwrap();
        let oracle = Oracle::new(&run, &full);
        if !oracle.barge_in {
            continue;
        }
        let context = kept(&run);
        assert!(
            oracle.accepts(&context),
            "silence {silence}: position {:?}, accepted {}..={}, context {context:?}",
            oracle.position,
            oracle.lo,
            oracle.hi
        );
        // The position moves in whole frames from the first frame on (the pacer releases the
        // first frame at once), so on the caller's 20 ms grid the position that puts the
        // barge-in at the start of the chunk in flight is exactly 320 ms; with the TTS shifted by
        // 15 ms the audio is off that grid like a real TTS's, and the position lands inside the
        // chunk (325 ms). Both leave "is" (350 ms) unheard: the whole chunk counted as played
        // (360 ms) would hear it. A position of 340 ms (or 345 ms) is NOT asserted to cut before
        // "is": the frame [340, 360) ms is counted when it is released, so the reported position
        // may run up to one frame ahead of the wire (pacer docs) and "Here is" is inside the
        // oracle's one-frame window (`accepts` above).
        if oracle.position >= 320 * MS && oracle.position <= 330 * MS {
            assert_eq!(
                context, "Here",
                "shift {shift} ms, silence {silence}: {:?}",
                oracle.position
            );
            in_flight += 1;
            in_flight_shifted += u32::from(shift > 0);
        }
    }
    assert!(
        in_flight >= 1,
        "no run put the barge-in inside the chunk with a word starting in its unplayed rest"
    );
    assert!(
        in_flight_shifted >= 1,
        "no run with the audio off the 20 ms grid put the barge-in inside the chunk"
    );
}

/// The documented estimate, computed independently: `played x 14` chars of `full`, cut back to a
/// whitespace boundary.
fn estimate(full: &str, played: Duration) -> String {
    let chars = (played.as_secs_f64() * 14.0).floor() as usize;
    let chars_of: Vec<(usize, char)> = full.char_indices().collect();
    let Some(&(limit, at_limit)) = chars_of.get(chars) else {
        return full.to_owned();
    };
    let head = &full[..limit];
    let cut = if at_limit.is_whitespace() {
        head
    } else {
        head.rfind(char::is_whitespace).map_or("", |i| &head[..i])
    };
    cut.trim_end().to_owned()
}

#[tokio::test(start_paused = true)]
async fn without_word_timestamps_the_cut_is_the_speaking_rate_estimate_of_the_wire_position() {
    let full: String = STORY.concat();
    let factory = quick();
    for silence in [900u64, 1_500, 2_600, 4_200] {
        let clock = sim_clock();
        let mut session = SimSession::new(clock.clone());
        session.llm.push(LlmScript::text(STORY));
        session.llm.push(LlmScript::text([REPLY]));
        session.tts = MockTts::new(clock).without_word_timings();
        let run = session
            .run_with(
                &factory,
                &scenario(Duration::from_millis(silence)),
                2_000 * MS,
            )
            .await
            .unwrap();
        assert_eq!(run.outcome, Ok(()), "silence {silence}");
        let oracle = Oracle::new(&run, &full);
        assert!(oracle.barge_in, "silence {silence}");
        // The cut is an estimate and is flagged as one in the metrics.
        let stats = run.probes.truncation_stats();
        assert!(
            stats.truncations >= 1 && stats.estimated >= 1,
            "silence {silence}: the estimate is not flagged: {stats:?}"
        );
        assert_eq!(stats.unmatched_words, 0, "silence {silence}: {stats:?}");
        // No word timestamps: the position alone decides, within one frame.
        let low = estimate(&full, oracle.position.saturating_sub(FRAME));
        let high = estimate(&full, oracle.position + FRAME);
        let context = kept(&run);
        assert!(
            full.starts_with(&context)
                && context.len() >= low.len()
                && context.len() <= high.len()
                && !context.is_empty(),
            "silence {silence} (position {:?}): {context:?} outside {low:?} to {high:?}",
            oracle.position
        );
    }
    // A whole answer without timestamps is not estimated away.
    suite::a_fully_played_answer_without_word_timestamps_is_not_estimated_away(
        &factory,
        Profile::Budget,
    )
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_barge_in_while_the_llm_is_still_streaming_keeps_only_the_heard_part_of_what_was_sent() {
    // Wire-oracle twin of the shared check of the same name. The LLM pauses 9 s before its third
    // delta, so at the barge-in the context message holds only the first two deltas and the TTS
    // is idle once they have played. The cut applies to the text sent so far, the heard words
    // are judged against the wire, and the aborted stream's tail never reaches the context.
    let delivered = format!("{}{}", STORY[0], STORY[1]);
    let factory = quick();
    let mut rng = Rng(0x7039_0004);
    let (mut partial, mut all_delivered) = (0, 0);
    for case in 0..16 {
        // From the first words to well after the delivered audio (about 3.5 s) has played.
        let silence = 400 + rng.below(4_400);
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
            .run_with(
                &factory,
                &scenario(Duration::from_millis(silence)),
                14_000 * MS,
            )
            .await
            .unwrap();
        assert_eq!(run.outcome, Ok(()), "case {case}");
        let oracle = Oracle::new(&run, &delivered);
        assert!(oracle.barge_in, "case {case}: no barge-in");
        let context = kept(&run);
        assert!(
            oracle.accepts(&context),
            "case {case} (silence {silence} ms, heard position {:?}, accepted words {}..={}): \
             the context holds {context:?}",
            oracle.position,
            oracle.lo,
            oracle.hi
        );
        assert!(
            assistant_texts(&run)
                .iter()
                .all(|m| !m.contains("keeps going")),
            "case {case}: the aborted stream's tail reached the context: {:?}",
            assistant_texts(&run)
        );
        let words = context.split_whitespace().count();
        if words > 0 && words < oracle.total_words {
            partial += 1;
        }
        all_delivered += usize::from(words == oracle.total_words);
        for call in factory.handles() {
            let llm = call.llm.snapshot();
            assert_eq!((llm.cut_errors, llm.validate_failures), (0, 0), "{llm:?}");
        }
    }
    assert!(
        partial >= 3,
        "only {partial} cuts inside the delivered text"
    );
    assert!(
        all_delivered >= 3,
        "only {all_delivered} barge-ins after the delivered text had played"
    );
}

#[tokio::test(start_paused = true)]
async fn after_a_tts_disconnect_the_context_holds_exactly_the_words_that_were_spoken() {
    let full: String = STORY.concat();
    let mut seen = 0;
    for after_events in [30usize, 60, 90, 120] {
        let clock = sim_clock();
        let session = SimSession::new(clock.clone());
        session.llm.push(LlmScript::text(STORY));
        session.llm.push(LlmScript::text([REPLY]));
        session
            .tts
            .push_session_faults(vec![StreamFault::Disconnect { after_events }]);
        let factory = quick();
        // The caller waits until long after the audio stopped.
        let run = session
            .run_with(&factory, &scenario(6_000 * MS), 2_000 * MS)
            .await
            .unwrap();
        assert_eq!(run.outcome, Ok(()), "after {after_events} events");
        let spoken: Duration = run
            .wire
            .iter()
            .filter(|r| r.kind == WireKind::Audio && r.epoch == Some(0))
            .filter_map(|r| r.audio)
            .sum();
        let starts: Vec<Duration> = run
            .probes
            .words()
            .iter()
            .filter(|(e, _, _)| *e == TurnEpoch::ZERO)
            .map(|(_, _, start)| *start)
            .collect();
        let heard = starts.iter().filter(|s| **s < spoken).count();
        let oracle = Oracle::new(&run, &full);
        assert!(
            heard < token_ends(&full).len(),
            "after {after_events} events the TTS had not died yet ({spoken:?})"
        );
        // Exact: the whole audio that existed was played, nothing was cut by a clear.
        assert_eq!(
            kept(&run),
            oracle.prefix(heard),
            "after {after_events} events: spoken {spoken:?}, {heard} words"
        );
        seen += usize::from(heard > 0);
        for call in factory.handles() {
            let llm = call.llm.snapshot();
            assert_eq!(llm.cut_errors, 0, "{llm:?}");
            assert_eq!(llm.validate_failures, 0, "{llm:?}");
        }
    }
    assert!(
        seen >= 2,
        "the disconnect came before any word in most runs"
    );
}

// ---- rounds: text, tool, text -------------------------------------------------------------------

const ROUND_ONE: &str = "Let me check.";
const ROUND_TWO: &str = "It is nine in the morning, and the sky over the city is clear today.";

/// A `lookup` tool that takes `delay` and may announce itself with a filler after `filler_after`.
fn lookup(delay: Duration, filler_after: Option<Duration>) -> ToolFactory {
    Arc::new(move |clock: &SharedClock| {
        let mut registry = ToolRegistry::new();
        registry
            .register(
                ToolSpec::new(
                    "lookup",
                    "Looks something up.",
                    serde_json::json!({"type": "object", "properties": {"q": {"type": "string"}}}),
                    ToolKind::ReadOnly,
                )
                .with_timeout(Duration::from_secs(30))
                .with_filler_after(filler_after),
                SlowTool {
                    probe: Arc::new(ToolProbe::default()),
                    clock: clock.clone(),
                    delay,
                    result: serde_json::json!({"time": "09:00"}),
                },
            )
            .unwrap();
        let registry = Arc::new(registry);
        let engine = Arc::new(ToolEngine::new(
            Arc::clone(&registry),
            SessionId::new(1),
            clock.clone(),
            ToolEngineConfig::default(),
        ));
        (registry, engine)
    })
}

fn tool_script() -> LlmScript {
    tool_script_for(ROUND_ONE)
}

fn tool_script_for(text: &'static str) -> LlmScript {
    LlmScript::text([text]).with_tool_call("call_1", "lookup", r#"{"q":"time"}"#)
}

/// A first round whose last word ("it") starts 120 ms before the end of its audio, inside the
/// 160 ms production lead margin: dropping it is the visible effect of a margin that is
/// subtracted when it should not be.
const ROUND_ONE_LATE_WORD: &str = "Let me check it";

fn kinds(run: &SessionRun) -> Vec<String> {
    let context = run.probes.context.lock().unwrap().clone();
    context.validate().unwrap();
    context
        .messages()
        .iter()
        .map(|m| match m {
            Message::System { .. } => "system".to_owned(),
            Message::User { .. } => "user".to_owned(),
            Message::Assistant {
                text, tool_calls, ..
            } if tool_calls.is_empty() => format!("assistant:{text}"),
            Message::Assistant { text, .. } => format!("assistant+calls:{text}"),
            Message::Tool { is_error, .. } => if *is_error { "tool!" } else { "tool" }.to_owned(),
        })
        .collect()
}

/// Barge-ins in the second round of a tool turn whose first round is `round_one`, on an engine
/// with `lead_margin = margin`.
async fn second_round_barge_ins(margin: Duration, round_one: &'static str) {
    let round_one_audio = 60 * MS * u32::try_from(round_one.len()).unwrap();
    let mut rng = Rng(0x7039_0102);
    let (mut cut_inside, mut checked, mut chained) = (0, 0, 0);
    for case in 0..14 {
        // The tool answers at once; the second round's audio starts about 1 s after the turn.
        let silence = 1_100 + rng.below(3_000);
        let mut factory = quick().with_tool_factory(lookup(Duration::ZERO, None));
        factory.config.lead_margin = margin;
        let session = SimSession::new(sim_clock());
        session.llm.push(tool_script_for(round_one));
        session.llm.push(LlmScript::text([ROUND_TWO]));
        session.llm.push(LlmScript::text([REPLY]));
        let run = session
            .run_with(
                &factory,
                &scenario(Duration::from_millis(silence)),
                2_000 * MS,
            )
            .await
            .unwrap();
        assert_eq!(run.outcome, Ok(()), "case {case}");
        let kinds = kinds(&run);
        let interrupted = run
            .output
            .iter()
            .any(|r| r.kind == "interrupt" && r.epoch.is_some_and(|e| e > 0));
        if !interrupted {
            continue;
        }

        // The oracle for round two: its words are offsets inside its own utterance, which starts
        // `round_one_audio` after the epoch's first sample on the wire.
        let clear = run
            .wire
            .iter()
            .find(|r| r.kind == WireKind::Clear && r.epoch.is_some_and(|e| e >= 1))
            .unwrap()
            .at;
        let position: Duration = run
            .wire
            .iter()
            .filter(|r| r.kind == WireKind::Audio && r.epoch == Some(0))
            .map(|r| clear.saturating_sub(r.at).min(r.audio.unwrap_or_default()))
            .sum();
        let starts: Vec<Duration> = run
            .probes
            .words()
            .iter()
            .filter(|(e, _, _)| *e == TurnEpoch::ZERO)
            .map(|(_, _, s)| *s)
            .collect();
        // Words of round one restart at zero: the second utterance's offsets follow the first
        // word that goes backwards.
        let split = starts
            .windows(2)
            .position(|w| w[1] < w[0])
            .map_or(starts.len(), |i| i + 1);
        let second = &starts[split..];
        let in_round_two = |p: Duration| {
            let local = p.saturating_sub(round_one_audio);
            second.iter().filter(|s| **s < local).count()
        };
        // The client holds `margin` of audio unplayed at the clear (the last round has nothing
        // behind it).
        let heard = position.saturating_sub(margin);
        let lo = in_round_two(heard.saturating_sub(FRAME));
        let hi = in_round_two(heard + FRAME);
        let second_ends = token_ends(ROUND_TWO);
        let prefix = |k: usize| match k.checked_sub(1) {
            None => "",
            Some(last) => &ROUND_TWO[..second_ends[last]],
        };

        // Round one was played in full whatever happens in round two. With a lead margin the
        // chain decides: the audio of round two behind it takes the margin off round one, so
        // round one is whole once round two holds more than the margin (a margin subtracted
        // from round one alone would drop its last word).
        let behind_round_one = position.saturating_sub(round_one_audio);
        let whole_one = format!("assistant+calls:{round_one}");
        assert_eq!(kinds[0], "user", "case {case}: {kinds:?}");
        assert_eq!(kinds[2], "tool", "case {case}: {kinds:?}");
        if behind_round_one >= margin + FRAME {
            assert_eq!(kinds[1], whole_one, "case {case}: {kinds:?}");
            chained += usize::from(!margin.is_zero());
        } else {
            assert!(
                whole_one.starts_with(kinds[1].as_str())
                    && kinds[1].starts_with("assistant+calls:"),
                "case {case}: {kinds:?}"
            );
        }
        let tail = &kinds[3];
        if lo == 0 && hi == 0 {
            // Nothing of round two was heard: its message is removed.
            assert_eq!(kinds[3], "user", "case {case}: {kinds:?}");
        } else {
            let text = tail.strip_prefix("assistant:").unwrap_or_else(|| {
                panic!("case {case}: expected the cut second round, got {kinds:?}")
            });
            let k = text.split_whitespace().count();
            assert!(
                (lo..=hi).contains(&k) && text == prefix(k),
                "case {case} (silence {silence} ms, heard {heard:?}, accepted {lo}..={hi}): \
                 {text:?}"
            );
            cut_inside += usize::from(k < second_ends.len());
        }
        checked += 1;
        for call in factory.handles() {
            let llm = call.llm.snapshot();
            assert_eq!((llm.cut_errors, llm.validate_failures), (0, 0), "{llm:?}");
        }
    }
    assert!(checked >= 6, "only {checked} barge-ins");
    assert!(cut_inside >= 3, "only {cut_inside} cuts inside round two");
    if !margin.is_zero() {
        assert!(
            chained >= 4,
            "only {chained} cases put the margin behind round two"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_barge_in_in_the_second_round_keeps_the_first_whole_and_cuts_the_second_to_the_wire() {
    second_round_barge_ins(Duration::ZERO, ROUND_ONE).await;
}

#[tokio::test(start_paused = true)]
async fn with_the_production_margin_the_first_round_is_chained_behind_the_second() {
    // 160 ms margin: round one's last word starts inside it, so only the audio of round two
    // sitting behind round one keeps that word heard.
    second_round_barge_ins(Duration::from_millis(160), ROUND_ONE_LATE_WORD).await;
}

#[tokio::test(start_paused = true)]
async fn a_barge_in_during_tool_execution_keeps_the_spoken_text_and_closes_the_call() {
    // The tool takes 5 s: the caller interrupts while it runs, after "Let me check." has played.
    let factory = quick().with_tool_factory(lookup(5_000 * MS, None));
    let session = SimSession::new(sim_clock());
    session.llm.push(tool_script());
    // The interrupted round two never runs: the next script answers the caller's second turn.
    session.llm.push(LlmScript::text([REPLY]));
    let run = session
        .run_with(&factory, &scenario(2_500 * MS), 2_000 * MS)
        .await
        .unwrap();
    assert_eq!(run.outcome, Ok(()));
    let kinds = kinds(&run);
    assert_eq!(
        kinds,
        [
            "user".to_owned(),
            format!("assistant+calls:{ROUND_ONE}"),
            "tool!".to_owned(),
            "user".to_owned(),
            format!("assistant:{REPLY}"),
        ],
        "the spoken text is kept, the open call is closed with an error result, and the second \
         round never ran"
    );
    for call in factory.handles() {
        let llm = call.llm.snapshot();
        assert_eq!(llm.cut_errors, 0, "{llm:?}");
        assert_eq!(llm.validate_failures, 0, "{llm:?}");
        // The tool round's message, and the reply at Stop.
        assert_eq!(llm.cut_requests, 2, "{llm:?}");
        assert_eq!(llm.cuts_heard + llm.cuts_whole, 2, "{llm:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn after_a_tool_wait_the_production_margin_does_not_take_the_last_word_back() {
    // The tool takes 5 s and the caller interrupts 2.5 s into the turn, long after the first
    // round ("Let me check it", whose last word starts inside the 160 ms margin) was played and
    // its audio drained from the client's buffer. A fixed margin subtracted from the last
    // release would drop "it".
    let mut factory = quick().with_tool_factory(lookup(5_000 * MS, None));
    factory.config.lead_margin = Duration::from_millis(160);
    let session = SimSession::new(sim_clock());
    session.llm.push(tool_script_for(ROUND_ONE_LATE_WORD));
    session.llm.push(LlmScript::text([REPLY]));
    let run = session
        .run_with(&factory, &scenario(2_500 * MS), 2_000 * MS)
        .await
        .unwrap();
    assert_eq!(run.outcome, Ok(()));
    let kinds = kinds(&run);
    assert_eq!(
        kinds[1],
        format!("assistant+calls:{ROUND_ONE_LATE_WORD}"),
        "{kinds:?}"
    );
    assert_eq!(kinds[2], "tool!", "{kinds:?}");
    for call in factory.handles() {
        let llm = call.llm.snapshot();
        assert_eq!((llm.cut_errors, llm.validate_failures), (0, 0), "{llm:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_barge_in_while_the_first_round_plays_cuts_its_text_and_keeps_the_tool_pair() {
    // The caller interrupts while "Let me check." plays and the tool still runs: the message
    // keeps the wire-oracle prefix of its text and its call, the call is closed with an error.
    let mut partial = 0;
    for silence in (300..1_000u64).step_by(40) {
        let factory = quick().with_tool_factory(lookup(5_000 * MS, None));
        let session = SimSession::new(sim_clock());
        session.llm.push(tool_script());
        session.llm.push(LlmScript::text([REPLY]));
        let run = session
            .run_with(
                &factory,
                &scenario(Duration::from_millis(silence)),
                2_000 * MS,
            )
            .await
            .unwrap();
        assert_eq!(run.outcome, Ok(()), "silence {silence}");
        let kinds = kinds(&run);
        assert_eq!(
            kinds.iter().filter(|k| k.starts_with("tool")).count(),
            1,
            "silence {silence}: the call has its result: {kinds:?}"
        );
        let oracle = Oracle::new(&run, ROUND_ONE);
        let text = kinds[1]
            .strip_prefix("assistant+calls:")
            .unwrap_or_else(|| panic!("silence {silence}: {kinds:?}"));
        assert!(
            oracle.accepts(text),
            "silence {silence}: position {:?}, accepted {}..={}, message {text:?}",
            oracle.position,
            oracle.lo,
            oracle.hi
        );
        partial += usize::from(text != ROUND_ONE);
    }
    assert!(partial >= 3, "only {partial} of the barge-ins cut the text");
}

#[tokio::test(start_paused = true)]
async fn a_barge_in_before_any_audio_empties_the_text_but_keeps_the_call_and_its_result() {
    // The TTS needs 1.2 s to the first byte; the caller interrupts at about 2 s, before it.
    let factory = quick().with_tool_factory(lookup(5_000 * MS, None));
    let clock = sim_clock();
    let mut session = SimSession::new(clock.clone());
    session.llm.push(tool_script());
    session.llm.push(LlmScript::text([REPLY]));
    session.tts = MockTts::new(clock).with_latency(
        LatencyProfile::ZERO,
        LatencyProfile::Fixed(1_200 * MS),
        LatencyProfile::ZERO,
        1,
    );
    let run = session
        .run_with(&factory, &scenario(900 * MS), 2_000 * MS)
        .await
        .unwrap();
    assert_eq!(run.outcome, Ok(()));
    let kinds = kinds(&run);
    assert_eq!(
        kinds,
        [
            "user".to_owned(),
            "assistant+calls:".to_owned(),
            "tool!".to_owned(),
            "user".to_owned(),
            format!("assistant:{REPLY}"),
        ],
        "nothing of the first round was heard: its text is empty, the call and its result stay"
    );
    for call in factory.handles() {
        let llm = call.llm.snapshot();
        assert_eq!((llm.cut_errors, llm.validate_failures), (0, 0), "{llm:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn the_filler_round_is_cut_like_any_round_and_kept_as_heard() {
    // A 4 s tool with a filler after 300 ms: the filler is its own round, put into the
    // context after the tool's result and cut to what the caller heard of it.
    let factory = quick().with_tool_factory(lookup(4_000 * MS, Some(300 * MS)));
    let session = SimSession::new(sim_clock());
    session.llm.push(tool_script());
    session.llm.push(LlmScript::text([REPLY]));
    let run = session
        .run_with(&factory, &scenario(2_800 * MS), 2_000 * MS)
        .await
        .unwrap();
    assert_eq!(run.outcome, Ok(()));
    let history = run.probes.history();
    let fillers: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, m)| m.content.to_lowercase().contains("moment"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(fillers.len(), 1, "{history:?}");
    let at = fillers[0];
    // Never between a call and its result: right after the tool result.
    assert_eq!(history[at].role, Role::Assistant, "{history:?}");
    assert_eq!(history[at - 1].role, Role::Tool, "{history:?}");
    for call in factory.handles() {
        let llm = call.llm.snapshot();
        assert_eq!((llm.fillers, llm.fillers_in_context), (1, 1), "{llm:?}");
        // Asked about: the tool round's message, the filler and, at Stop, the reply.
        assert_eq!(llm.cut_requests, 3, "{llm:?}");
        assert_eq!(llm.cut_errors, 0, "{llm:?}");
        // The filler was spoken: it has an utterance in the book and, now, a message, cut.
        let book = &call.input.book;
        let filler = (0..4u32)
            .filter_map(|r| book.snapshot(RoundKey::new(TurnEpoch::ZERO, r)))
            .find(|s| s.msg_index == Some(at) && s.utterance.is_some())
            .expect("the filler round has an utterance and its message");
        assert!(filler.total_audio > Duration::ZERO);
        assert!(filler.applied);
    }
}
