//! The first-chunk policy measured: how long the first TTS chunk of a response waits
//! (`SegmenterStageConfig::FIRST_MIN_WORDS_EN` / `_TR`) against the chaos suite's first-audio SLO.
//!
//! The chaos suite's answer is the common shape of a voice LLM's reply: a one-word opener and a
//! comma, then a long sentence (`Sure, here is a fairly long answer to your question.`, streamed
//! word by word, 30 ms apart, after a log-normal first token with a 350 ms median). The first
//! chunk may end at the earliest clause or sentence boundary once it holds the minimum number of
//! words. With one word it is `Sure,`; with two or three, the next boundary is the end of the
//! sentence, nine tokens (270 ms) later.
//!
//! The test plays the suite's unaffected reference calls (the 100 seeds the chaos round checks,
//! alone, on the production pipeline with the suite's end of turn) under each word count, prints
//! the first-audio distribution, and asserts that the production default keeps every turn within
//! the SLO, so the 100-call chaos test runs on the default.
#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::indexing_slicing)]

use std::time::Duration;

use brisvox_engine::SegmenterStageConfig;
use brisvox_engine::sim::EngineFactory;
use brisvox_sim::suite::chaos::{SLO, unaffected_first_audio};

/// The seeds of the chaos round's reference calls.
const SEEDS: std::ops::Range<u64> = 2_000..2_100;

fn summary(mut samples: Vec<Duration>) -> (Duration, Duration, Duration) {
    samples.sort_unstable();
    let pick = |q: usize| samples[(q * samples.len()).div_ceil(100).saturating_sub(1)];
    (pick(50), pick(95), *samples.last().unwrap())
}

#[tokio::test(start_paused = true)]
async fn the_default_first_chunk_keeps_every_reference_call_within_the_slo() {
    let ms = |d: Duration| d.as_secs_f64() * 1_000.0;
    // `None`: the production default (the language's, English here). Two words behave as three
    // on this answer (the opener has one word), so three stands for both.
    let mut measured = Vec::new();
    for words in [None, Some(3)] {
        let mut factory = EngineFactory::new()
            .with_smart_turn(0.9)
            .without_handle_log();
        if let Some(words) = words {
            factory = factory.with_first_chunk_words(words);
        }
        let latencies = unaffected_first_audio(&factory, SEEDS).await.unwrap();
        let over = latencies.iter().filter(|l| **l > SLO).count();
        let (p50, p95, max) = summary(latencies);
        let words = words.unwrap_or(SegmenterStageConfig::FIRST_MIN_WORDS_EN);
        println!(
            "first chunk after {words} word(s): first audio p50 {:.1} ms, p95 {:.1} ms, max \
             {:.1} ms; {over} of {} turns over the {} ms SLO",
            ms(p50),
            ms(p95),
            ms(max),
            2 * SEEDS.count(),
            SLO.as_millis()
        );
        measured.push((words, p50, max));
    }
    let (default_words, default_p50, default_max) = measured[0];
    assert!(
        default_max <= SLO,
        "the default first chunk ({default_words} words) answers a reference call in {:.1} ms, \
         over the SLO: {measured:?}",
        ms(default_max)
    );
    // The default is the word count that keeps the opener (`Sure,`) a chunk of its own: it
    // starts the audio well before a first chunk that waits for the whole sentence.
    let (_, three_p50, _) = measured[1];
    assert!(
        default_p50 + Duration::from_millis(100) < three_p50,
        "{measured:?}"
    );
}
