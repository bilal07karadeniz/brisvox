//! The segmenter stage: raw slices of the response for TTS, flush and reset rules, the
//! stale-epoch gate, per-language first-chunk rules, and the cost of `Segmenter::push` against the
//! 50 us rule.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    clippy::cast_precision_loss
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use brisvox_agent::context::{Context, Message};
use brisvox_agent::played::{PlayedConfig, PlayedText, apply_truncation};
use brisvox_agent::segment::Segmenter;
use brisvox_agent::{Language, SegmenterConfig};
use brisvox_core::frame::{ControlFrame, ControlKind, DataFrame, Frame, SystemFrame, TextFrame};
use brisvox_core::{
    Direction, EpochPolicy, Links, Pipeline, Processor, ProcessorContext, RunningPipeline,
    SimClock, TimelineRecorder, TurnEpoch, TurnEvent,
};
use brisvox_engine::{SegmenterStage, SegmenterStageConfig, SegmenterStats};
use common::{Recorded, Recorder};
use tokio_util::sync::CancellationToken;

fn text(epoch: u32, text: &str) -> Frame {
    Frame::Data(DataFrame::LlmText(TextFrame {
        epoch: TurnEpoch::new(epoch),
        text: Arc::from(text),
    }))
}

fn control(epoch: u32, kind: ControlKind) -> Frame {
    Frame::Control(ControlFrame {
        epoch: TurnEpoch::new(epoch),
        kind,
    })
}

fn interrupt(epoch: u32) -> Frame {
    Frame::System(SystemFrame::Interrupt {
        new_epoch: TurnEpoch::new(epoch),
    })
}

struct Rig {
    running: RunningPipeline,
    rec: Recorded,
    stats: SegmenterStats,
}

impl Rig {
    async fn new(config: SegmenterStageConfig) -> Self {
        let stage = SegmenterStage::new(config);
        let stats = stage.stats();
        let rec = Recorded::default();
        let running = Pipeline::new("segmenter")
            .then(stage)
            .then(Recorder(rec.clone()))
            .start(Links::default(), &CancellationToken::new())
            .await
            .unwrap();
        Self {
            running,
            rec,
            stats,
        }
    }

    async fn feed(&self, frame: Frame) {
        self.running.input().send(frame).await.unwrap();
    }

    async fn settle(&self) {
        for _ in 0..30 {
            tokio::task::yield_now().await;
        }
    }

    /// The text chunks and control markers the stage emitted, in order: `"text:<slice>"` or the
    /// control frame's name.
    fn out(&self) -> Vec<String> {
        self.rec
            .all()
            .into_iter()
            .filter_map(|(frame, _)| match frame {
                Frame::Data(DataFrame::LlmText(t)) => Some(format!("text:{}", t.text)),
                Frame::Control(c) => Some(c.kind.name().to_owned()),
                Frame::System(SystemFrame::Interrupt { .. }) => Some("interrupt".to_owned()),
                _ => None,
            })
            .collect()
    }

    fn texts(&self) -> Vec<String> {
        self.rec
            .all()
            .into_iter()
            .filter_map(|(frame, _)| match frame {
                Frame::Data(DataFrame::LlmText(t)) => Some(t.text.to_string()),
                _ => None,
            })
            .collect()
    }
}

/// English with a first chunk of at least `words` words (the default is one; the tests
/// of the rule itself ask for more).
fn english_first_words(words: usize) -> SegmenterStageConfig {
    SegmenterStageConfig {
        segmenter: SegmenterConfig {
            first_min_words: words,
            ..SegmenterConfig::new(Language::En)
        },
    }
}

#[tokio::test(start_paused = true)]
async fn chunks_are_raw_slices_that_concatenate_to_the_response() {
    let rig = Rig::new(english_first_words(3)).await;
    let deltas = [
        "Hello th",
        "ere. How are",
        " you   doing today? Fi",
        "ne.\n\nThanks!  ",
    ];
    for delta in deltas {
        rig.feed(text(1, delta)).await;
    }
    rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
    rig.settle().await;
    let texts = rig.texts();
    assert_eq!(texts.concat(), deltas.concat().trim_end());
    // "Hello there." has two words: the first chunk (three words) runs on to the next sentence
    // end, with the caller's own spacing inside it. Later chunks carry the whitespace before
    // them.
    assert_eq!(texts[0], "Hello there. How are you   doing today?");
    assert!(texts[1].starts_with(' '), "{texts:?}");
    assert!(
        texts
            .iter()
            .skip(1)
            .all(|t| t.starts_with(char::is_whitespace)),
        "{texts:?}"
    );
    let stats = rig.stats.snapshot();
    assert_eq!(stats.chunks, texts.len() as u64);
    assert_eq!(stats.deltas, 4);
}

#[tokio::test(start_paused = true)]
async fn the_first_chunk_of_a_response_is_marked_on_the_timeline_once() {
    // One mark per response, not one lock per chunk.
    let timeline = TimelineRecorder::new();
    let clock = SimClock::shared();
    let stage =
        SegmenterStage::new(english_first_words(1)).with_timeline(timeline.clone(), clock.clone());
    let rec = Recorded::default();
    let running = Pipeline::new("segmenter")
        .then(stage)
        .then(Recorder(rec.clone()))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    let settle = || async {
        for _ in 0..30 {
            tokio::task::yield_now().await;
        }
    };
    let input = running.input();
    input
        .send(text(1, "Hello there. How are you? Fine."))
        .await
        .unwrap();
    input
        .send(control(1, ControlKind::LlmResponseEnded))
        .await
        .unwrap();
    settle().await;
    let chunks = rec
        .all()
        .into_iter()
        .filter(|(frame, _)| matches!(frame, Frame::Data(DataFrame::LlmText(_))))
        .count();
    assert!(chunks >= 3, "several chunks went out: {chunks}");
    let stats = timeline.stats();
    assert_eq!(
        (stats.recorded, stats.repeated),
        (1, 0),
        "one mark for {chunks} chunks"
    );
    assert!(
        timeline
            .current()
            .is_some_and(|turn| turn.at(TurnEvent::FirstChunk).is_some())
    );
    // The next response (a newer epoch) is marked again.
    input.send(text(2, "Another one. And more.")).await.unwrap();
    settle().await;
    assert_eq!(timeline.stats().recorded, 2);
    assert!(timeline.is_marked(TurnEpoch::new(2), TurnEvent::FirstChunk));
}

#[tokio::test(start_paused = true)]
async fn flush_and_response_ended_finish_the_stream_and_follow_the_tail() {
    let rig = Rig::new(SegmenterStageConfig::new(Language::En)).await;
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "Let me check that for you")).await;
    rig.feed(control(1, ControlKind::Flush)).await;
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "It is nine o'clock")).await;
    rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
    rig.settle().await;
    assert_eq!(
        rig.out(),
        [
            "llm_response_started",
            "text:Let me check that for you",
            "flush",
            "llm_response_started",
            "text:It is nine o'clock",
            "llm_response_ended",
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_new_round_without_a_flush_does_not_leave_its_predecessors_tail_behind() {
    let rig = Rig::new(SegmenterStageConfig::new(Language::En)).await;
    rig.feed(text(1, "First round text without an ending"))
        .await;
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "Second round")).await;
    rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
    rig.settle().await;
    assert_eq!(
        rig.out(),
        [
            "text:First round text without an ending",
            "llm_response_started",
            "text:Second round",
            "llm_response_ended",
        ],
        "each round's text is its own stream (offsets restart for every round)"
    );
}

#[tokio::test(start_paused = true)]
async fn an_interrupt_drops_pending_text_and_restarts_the_first_chunk_rule() {
    let rig = Rig::new(english_first_words(3)).await;
    // "Hello there." waits for its lookahead character: nothing is out yet.
    rig.feed(text(1, "Hello there, friend. Pending tail")).await;
    rig.settle().await;
    assert_eq!(rig.texts(), ["Hello there, friend."]);
    rig.feed(interrupt(2)).await;
    rig.feed(text(2, "Okay then. And more text here.")).await;
    rig.feed(control(2, ControlKind::LlmResponseEnded)).await;
    rig.settle().await;
    // "Pending tail" never comes out. The new response is a new stream: "Okay then." has only
    // two words, so the first chunk (three words) runs to the end of the second sentence, and
    // it starts at offset zero (no whitespace before it).
    assert_eq!(
        rig.texts(),
        ["Hello there, friend.", "Okay then. And more text here."]
    );
    assert_eq!(rig.stats.snapshot().resets, 1);
}

#[tokio::test(start_paused = true)]
async fn text_of_an_ended_epoch_is_dropped_before_it_reaches_the_stage() {
    let rig = Rig::new(SegmenterStageConfig::new(Language::En)).await;
    rig.feed(interrupt(2)).await;
    // This text would be emitted at once if it reached the stage (three words and a sentence end
    // followed by more text), so its absence is the gate's doing and not the stage's own reset.
    rig.feed(text(1, "Old answer here, friend. And more")).await;
    rig.settle().await;
    assert!(rig.texts().is_empty(), "{:?}", rig.texts());
    rig.feed(text(2, "The fresh answer starts here.")).await;
    rig.feed(control(2, ControlKind::LlmResponseEnded)).await;
    rig.settle().await;
    assert_eq!(rig.texts(), ["The fresh answer starts here."]);
    let stats = rig.stats.snapshot();
    assert_eq!(stats.deltas, 1, "the stale delta never reached the stage");
    assert_eq!(
        stats.foreign_epoch_resets, 0,
        "the stage's own foreign-epoch reset must not be what dropped it"
    );
}

/// Control: without the gate the same stale text is emitted, so the test above can fail.
#[tokio::test(start_paused = true)]
async fn the_stale_text_of_the_gate_test_is_emitted_when_it_is_not_stale() {
    let rig = Rig::new(SegmenterStageConfig::new(Language::En)).await;
    rig.feed(text(1, "Old answer here, friend. And more")).await;
    rig.settle().await;
    assert_eq!(rig.texts(), ["Old answer here,", " friend."]);
}

#[tokio::test(start_paused = true)]
async fn text_of_a_newer_epoch_starts_a_new_stream() {
    let rig = Rig::new(SegmenterStageConfig::new(Language::En)).await;
    rig.feed(text(1, "Leftover without any ending")).await;
    // The next turn's text arrives with no Flush/Ended/Interrupt in between (a dropped turn).
    rig.feed(text(2, "Fresh start. Next")).await;
    rig.feed(control(2, ControlKind::LlmResponseEnded)).await;
    rig.settle().await;
    let texts = rig.texts();
    assert!(!texts.iter().any(|t| t.contains("Leftover")), "{texts:?}");
    assert_eq!(texts.concat(), "Fresh start. Next");
    assert_eq!(rig.stats.snapshot().foreign_epoch_resets, 1);
}

#[tokio::test(start_paused = true)]
async fn the_first_chunk_waits_for_the_languages_word_count() {
    // One word in both languages (English measured in `first_chunk_policy.rs`, Turkish
    // assumed to follow it): the first chunk ends at the earliest clause or sentence boundary.
    for language in [Language::En, Language::Tr] {
        assert_eq!(
            SegmenterStageConfig::new(language)
                .segmenter
                .first_min_words,
            1,
            "{language:?}"
        );
    }
    let tr = Rig::new(SegmenterStageConfig::new(Language::Tr)).await;
    tr.feed(text(1, "Peki, bugün hava çok güzel olacak.\n"))
        .await;
    tr.settle().await;
    assert_eq!(
        tr.texts()[0],
        "Peki,",
        "a one-word Turkish opener is a chunk"
    );
    let en = Rig::new(SegmenterStageConfig::new(Language::En)).await;
    en.feed(text(1, "Sure, here is a fairly long answer. Ok"))
        .await;
    en.settle().await;
    assert_eq!(
        en.texts()[0],
        "Sure,",
        "a one-word English opener is a chunk"
    );
    // A custom config is honoured: with three words the clause after two is too early and the
    // sentence end is the first cut.
    let three = Rig::new(english_first_words(3)).await;
    three
        .feed(text(1, "Hello friend, how are you today? Ok"))
        .await;
    three.settle().await;
    assert_eq!(three.texts()[0], "Hello friend, how are you today?");
}

/// A tail that answers every text chunk with an upstream `TtsEnded` (the transport stage's job).
struct Echo;

impl Processor for Echo {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        frame: Frame,
        direction: Direction,
        ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        if direction == Direction::Downstream
            && matches!(frame, Frame::Data(DataFrame::LlmText(_)))
            && let Some(up) = ctx.sender(Direction::Upstream)
        {
            let _ = up.try_send(control(1, ControlKind::TtsEnded));
        }
        Ok(())
    }
}

/// A head that forwards downstream frames and records what comes back upstream.
struct Head(Recorded);

impl Processor for Head {
    fn name(&self) -> &'static str {
        "head"
    }

    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        frame: Frame,
        direction: Direction,
        ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        if direction == Direction::Upstream {
            self.0.record(frame, direction);
            return Ok(());
        }
        ctx.forward(frame, direction).await
    }
}

#[tokio::test(start_paused = true)]
async fn upstream_frames_pass_through_to_the_previous_stage() {
    let head = Recorded::default();
    let stage = SegmenterStage::new(SegmenterStageConfig::default());
    let running = Pipeline::new("up")
        .then(Head(head.clone()))
        .then(stage)
        .then(Echo)
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    running
        .input()
        .send(text(1, "Still running fine now."))
        .await
        .unwrap();
    running
        .input()
        .send(control(1, ControlKind::Flush))
        .await
        .unwrap();
    for _ in 0..30 {
        tokio::task::yield_now().await;
    }
    // The TtsEnded the tail sent came back upstream through the segmenter.
    let ups: Vec<_> = head
        .all()
        .into_iter()
        .filter(|(_, d)| *d == Direction::Upstream)
        .collect();
    assert!(!ups.is_empty(), "{:?}", head.all());
}

#[tokio::test(start_paused = true)]
async fn a_whitespace_flood_does_not_grow_the_retained_text() {
    let config = SegmenterStageConfig::new(Language::En);
    let cap = config.segmenter.max_chunk_chars * brisvox_engine::RAW_CAP_FACTOR;
    // Whitespace only, and whitespace behind a word (the segmenter holds the word and shortens
    // the run, the stage's copy would keep every byte).
    for prefix in ["", "Hmm"] {
        let rig = Rig::new(config).await;
        if !prefix.is_empty() {
            rig.feed(text(1, prefix)).await;
        }
        for i in 0..20_000 {
            rig.feed(text(
                1,
                if i % 7 == 0 {
                    "
 	  
  
	 "
                } else {
                    "          "
                },
            ))
            .await;
            if i % 200 == 0 {
                rig.settle().await;
            }
        }
        rig.settle().await;
        let stats = rig.stats.snapshot();
        assert!(
            stats.raw_high_water <= u64::try_from(cap + 16).unwrap(),
            "{prefix:?}: retained {} bytes over cap {cap}",
            stats.raw_high_water
        );
        assert!(stats.overflow_finishes > 0, "{prefix:?}: {stats:?}");
        assert!(
            rig.texts()
                .iter()
                .all(|t| t.trim().is_empty() || t.trim() == "Hmm")
        );
        // The stream is usable afterwards.
        rig.feed(text(1, " And then real words follow here.")).await;
        rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
        rig.settle().await;
        assert!(
            rig.texts()
                .concat()
                .ends_with("And then real words follow here."),
            "{:?}",
            rig.texts()
        );
    }
}

#[tokio::test(start_paused = true)]
async fn the_overflow_cut_loses_nothing_the_chunks_still_concatenate_to_the_message() {
    let rig = Rig::new(SegmenterStageConfig::new(Language::En)).await;
    let message = format!(
        "Hello there my friend{}and more words after the gap.",
        " ".repeat(40_000)
    );
    for piece in message.as_bytes().chunks(1000) {
        rig.feed(text(1, std::str::from_utf8(piece).unwrap())).await;
    }
    rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
    rig.settle().await;
    assert!(rig.stats.snapshot().overflow_finishes > 0);
    let texts = rig.texts();
    // Byte for byte: the whitespace run is a chunk of its own, not dropped.
    assert_eq!(texts.concat(), message);
    assert!(
        texts.iter().any(|t| t.trim().is_empty() && t.len() > 1000),
        "the run goes out as a whitespace chunk"
    );
    assert!(texts[0].starts_with("Hello there my friend"));
    // A barge-in during "and more": the sent text is a prefix of the message, so the context
    // keeps exactly the words that were played.
    let mut played = PlayedText::new();
    for slice in &texts {
        played.push_sent(slice);
    }
    for (i, word) in message.split_whitespace().enumerate() {
        played.push_word(word, Duration::from_millis(100 * i as u64));
    }
    let mut context = Context::new();
    context.push(Message::user("hi"));
    context.push(Message::assistant(message.as_str()));
    // 4 words are "Hello there my friend", the 6th is "more": heard up to 550 ms.
    let heard = played.heard(Duration::from_millis(550), &PlayedConfig::default());
    let outcome = apply_truncation(&mut context, 1, &heard);
    assert!(outcome.is_ok(), "{outcome:?}");
    let kept = context.messages().last().unwrap().text().to_owned();
    assert!(kept.starts_with("Hello there my friend"), "{kept:?}");
    assert!(
        !kept.contains("after the gap"),
        "unplayed text is gone: {kept:?}"
    );
    assert!(kept.len() < message.len());
}

/// Deterministic generator (splitmix64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n.max(1)).unwrap()).unwrap()
    }
}

#[tokio::test(start_paused = true)]
async fn what_the_stage_sends_is_always_a_prefix_of_the_message_at_any_playback_position() {
    const WORDS: &[&str] = &[
        "merhaba",
        "nasılsınız",
        "bugün",
        "hava",
        "çok",
        "güzel",
        "the",
        "answer",
        "is",
        "3.14",
        "Dr.",
        "Smith",
        "sure,",
        "okay.",
        "really?",
        "yes!",
        "—",
        "(fine)",
        "\"quoted\"",
        "ok…",
    ];
    let mut rng = Rng(7);
    for case in 0..60 {
        let language = if case % 2 == 0 {
            Language::En
        } else {
            Language::Tr
        };
        let mut message = String::new();
        for _ in 0..(5 + rng.below(40)) {
            message.push_str(WORDS[rng.below(WORDS.len())]);
            message.push_str(["", " ", " ", "  ", "\n", " \n "][rng.below(6)]);
        }
        let rig = Rig::new(SegmenterStageConfig::new(language)).await;
        // Random deltas of 1 to 9 chars.
        let chars: Vec<char> = message.chars().collect();
        let mut at = 0;
        while at < chars.len() {
            let n = 1 + rng.below(9);
            let delta: String = chars.iter().skip(at).take(n).collect();
            rig.feed(text(1, &delta)).await;
            at += n;
        }
        rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
        rig.settle().await;
        let slices = rig.texts();
        assert_eq!(slices.concat(), message.trim_end(), "case {case}");
        let mut played = PlayedText::new();
        for slice in &slices {
            played.push_sent(slice);
        }
        for (i, word) in message.split_whitespace().enumerate() {
            played.push_word(word, Duration::from_millis(100 * i as u64));
        }
        let mut context = Context::new();
        context.push(Message::user("hi"));
        context.push(Message::assistant(message.as_str()));
        for position in (0..=(message.split_whitespace().count() as u64 + 2) * 100).step_by(70) {
            let heard = played.heard(Duration::from_millis(position), &PlayedConfig::default());
            let mut copy = context.clone();
            let outcome = apply_truncation(&mut copy, 1, &heard);
            assert!(outcome.is_ok(), "case {case} at {position} ms: {outcome:?}");
        }
    }
}

/// `Segmenter::push` against the 50 us rule of CONTRIBUTING.md: a long answer arriving in LLM-sized
/// deltas (4 to 12 chars), and the inputs that make the scan longest (a long run without
/// punctuation, a whitespace flood, a single token without whitespace). The median push must be
/// inside the budget in release; the p99 and the worst push (the scan of the longest pending
/// text) are bounded too, so a quadratic `drain` (rescanning the buffer per char) cannot hide
/// behind a small median. The worst push is the best of three runs, so one scheduling hiccup
/// does not fail the test; the median and the p99 are the best of three as well.
///
/// The gate (`scripts/verify.sh`) runs this in debug, where the bounds are loose multiples of the
/// rule; the machine-independent guard against a per-character or per-push allocation blow-up is
/// the allocation count in `tests/segmenter_alloc.rs`. The release bounds are enforced by
/// `cargo test --release -p brisvox-engine --test segmenter_stage`.
#[test]
fn a_push_costs_less_than_the_worker_budget() {
    let sentence = "Randevunuz yarın saat 14:30'da, Dr. Yılmaz sizi bekliyor olacak; lütfen kimliğinizi unutmayın. ";
    let shapes: [(&str, String); 4] = [
        ("sentences", sentence.repeat(120)),
        (
            "no punctuation",
            "randevunuz yarın saat on dört otuzda doktor yılmaz sizi bekliyor olacak lütfen "
                .repeat(150),
        ),
        (
            "whitespace flood",
            format!(
                "a{}b",
                " 
"
                .repeat(6000)
            ),
        ),
        ("one long token", "x".repeat(12_000)),
    ];
    // Debug builds are several times slower than release; the bounds are the rule (median) and
    // generous multiples of it (tail) in release, and loose guards against a quadratic blow-up
    // in debug.
    let (median_budget, p99_budget, worst_budget) = if cfg!(debug_assertions) {
        (
            Duration::from_micros(500),
            Duration::from_millis(5),
            Duration::from_millis(50),
        )
    } else {
        (
            Duration::from_micros(50),
            Duration::from_micros(500),
            Duration::from_millis(2),
        )
    };
    for (shape, text) in &shapes {
        let chars: Vec<char> = text.chars().collect();
        for language in [Language::En, Language::Tr] {
            let mut rng = Rng(99);
            let mut best_worst = Duration::MAX;
            let (mut median, mut p99) = (Duration::MAX, Duration::MAX);
            for _ in 0..3 {
                let mut segmenter = Segmenter::new(SegmenterStageConfig::new(language).segmenter);
                let mut spans = Vec::new();
                let mut costs = Vec::new();
                let mut at = 0;
                while at < chars.len() {
                    let n = 4 + rng.below(9);
                    let delta: String = chars.iter().skip(at).take(n).collect();
                    let start = std::time::Instant::now();
                    segmenter.push_spans(&delta, &mut spans);
                    costs.push(start.elapsed());
                    spans.clear();
                    at += n;
                }
                costs.sort_unstable();
                median = median.min(costs[costs.len() / 2]);
                p99 = p99.min(costs[costs.len() * 99 / 100]);
                let worst = *costs.last().unwrap();
                best_worst = best_worst.min(worst);
            }
            println!(
                "{shape} {language:?}: median {median:?}, p99 {p99:?}, worst (best of 3) {best_worst:?}"
            );
            assert!(
                median < median_budget,
                "{shape} {language:?}: median {median:?} over {median_budget:?}"
            );
            assert!(
                p99 < p99_budget,
                "{shape} {language:?}: p99 {p99:?} over {p99_budget:?}"
            );
            assert!(
                best_worst < worst_budget,
                "{shape} {language:?}: worst {best_worst:?} over {worst_budget:?}"
            );
        }
    }
}
