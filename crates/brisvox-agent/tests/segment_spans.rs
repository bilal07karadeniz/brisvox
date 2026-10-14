//! `Segmenter::push_spans` / `finish_spans`: the byte ranges of the chunks over the
//! accumulated stream.
//!
//! The properties the production output chain relies on:
//! - the chunks are exactly those `push` / `finish` emit, however the text is split into deltas;
//! - the ranges are increasing and disjoint, and each starts and ends on a non-whitespace char;
//! - the raw slices `prev_end..end` concatenate to the stream (trailing whitespace excepted),
//!   byte for byte, also when the segmenter dropped whitespace beyond its run cap;
//! - the text sent as those slices is always a prefix of the message text, so
//!   `PlayedText::push_sent` followed by `apply_truncation` never fails with `NotAPrefix`, at any
//!   playback position.

use std::time::Duration;

use brisvox_agent::context::{Context, Message};
use brisvox_agent::played::{PlayedConfig, PlayedText, apply_truncation};
use brisvox_agent::segment::{Language, Segmenter, SegmenterConfig, Span};
use proptest::prelude::*;

/// Runs `deltas` through `push_spans`, then `finish_spans`.
fn spans_of(config: SegmenterConfig, deltas: &[&str]) -> Vec<Span> {
    let mut seg = Segmenter::new(config);
    let mut out = Vec::new();
    for delta in deltas {
        seg.push_spans(delta, &mut out);
    }
    seg.finish_spans(&mut out);
    assert_eq!(seg.stream_len(), 0, "finish starts a new stream");
    out
}

/// Runs `deltas` through `push`, then `finish`.
fn chunks_of(config: SegmenterConfig, deltas: &[&str]) -> Vec<String> {
    let mut seg = Segmenter::new(config);
    let mut out = Vec::new();
    for delta in deltas {
        seg.push(delta, &mut out);
    }
    seg.finish(&mut out);
    out
}

/// The raw slices the output chain would send: `raw[prev_end..end]` for each span.
fn raw_slices<'a>(raw: &'a str, spans: &[Span]) -> Vec<&'a str> {
    let mut prev = 0;
    spans
        .iter()
        .map(|span| {
            let slice = &raw[prev..span.end];
            prev = span.end;
            slice
        })
        .collect()
}

/// Cuts `text` into deltas of the given char counts (cycled), never splitting a char.
fn split_text<'a>(text: &'a str, sizes: &[usize]) -> Vec<&'a str> {
    let mut deltas = Vec::new();
    let mut rest = text;
    let mut i = 0;
    while !rest.is_empty() {
        let n = sizes
            .get(i % sizes.len().max(1))
            .copied()
            .unwrap_or(1)
            .max(1);
        i += 1;
        let cut = rest
            .char_indices()
            .nth(n)
            .map_or(rest.len(), |(offset, _)| offset);
        let (head, tail) = rest.split_at(cut);
        deltas.push(head);
        rest = tail;
    }
    deltas
}

/// Everything the properties check, for one input and one split.
fn check(config: SegmenterConfig, text: &str, sizes: &[usize]) {
    let deltas = split_text(text, sizes);
    let spans = spans_of(config, &deltas);

    // The same chunks as `push` / `finish`.
    let texts: Vec<&str> = spans.iter().map(|s| s.text.as_str()).collect();
    let pushed = chunks_of(config, &deltas);
    assert_eq!(texts, pushed, "config {config:?}, text {text:?}");

    // Ranges: increasing, disjoint, on char boundaries, non-whitespace at both ends.
    let mut prev_end = 0;
    for span in &spans {
        assert!(span.start >= prev_end, "overlap in {spans:?}");
        assert!(span.end > span.start, "empty range in {spans:?}");
        assert!(
            text.is_char_boundary(span.start) && text.is_char_boundary(span.end),
            "range {}..{} is not on char boundaries of {text:?}",
            span.start,
            span.end
        );
        let slice = &text[span.start..span.end];
        assert!(
            !slice.starts_with(char::is_whitespace) && !slice.ends_with(char::is_whitespace),
            "range {slice:?} is not trimmed"
        );
        // The chunk is the range without the whitespace the segmenter dropped beyond its cap.
        let squeeze = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        assert_eq!(squeeze(slice), squeeze(&span.text), "chunk vs range");
        prev_end = span.end;
    }

    // The raw slices are the stream, trailing whitespace excepted.
    let slices = raw_slices(text, &spans);
    let joined: String = slices.concat();
    assert_eq!(joined, text.trim_end(), "config {config:?}");

    // The sent text is a prefix of the message at any playback position.
    let mut played = PlayedText::new();
    for slice in &slices {
        played.push_sent(slice);
    }
    for (i, word) in text.split_whitespace().enumerate() {
        played.push_word(
            word,
            Duration::from_millis(100 * u64::try_from(i).unwrap_or(0)),
        );
    }
    let mut context = Context::new();
    context.push(Message::user("hi"));
    context.push(Message::assistant(text));
    let words = text.split_whitespace().count();
    for position_ms in (0..=(words as u64 + 2) * 100).step_by(50) {
        let heard = played.heard(Duration::from_millis(position_ms), &PlayedConfig::default());
        let mut copy = context.clone();
        let outcome = apply_truncation(&mut copy, 1, &heard);
        assert!(
            outcome.is_ok(),
            "{outcome:?} at {position_ms} ms for {heard:?} of {text:?} (sent {:?})",
            played.sent()
        );
    }
}

fn config(language: Language, first_min_words: usize, max_chunk_chars: usize) -> SegmenterConfig {
    SegmenterConfig {
        language,
        first_min_words,
        max_chunk_chars,
    }
}

#[test]
fn the_spans_of_a_simple_answer_cover_the_raw_text() {
    let text = "  Merhaba, nasılsınız? Ben iyiyim.  ";
    let spans = spans_of(config(Language::Tr, 2, 250), &[text]);
    let chunks: Vec<(&str, usize, usize)> = spans
        .iter()
        .map(|s| (s.text.as_str(), s.start, s.end))
        .collect();
    assert_eq!(
        chunks,
        [
            ("Merhaba, nasılsınız?", 2, 2 + "Merhaba, nasılsınız?".len()),
            (
                "Ben iyiyim.",
                2 + "Merhaba, nasılsınız? ".len(),
                2 + "Merhaba, nasılsınız? Ben iyiyim.".len()
            ),
        ]
    );
    let slices = raw_slices(text, &spans);
    assert_eq!(slices, ["  Merhaba, nasılsınız?", " Ben iyiyim."]);
    assert_eq!(slices.concat(), text.trim_end());
}

#[test]
fn offsets_count_bytes_across_deltas_and_restart_after_finish() {
    let mut seg = Segmenter::new(config(Language::En, 3, 250));
    let mut out = Vec::new();
    seg.push_spans("Çok güzel bir gün. ", &mut out);
    assert!(out.is_empty(), "the period waits for its lookahead");
    assert_eq!(seg.stream_len(), "Çok güzel bir gün. ".len());
    seg.push_spans("Yarın da", &mut out);
    let first = out.clone();
    assert_eq!(first.len(), 1);
    assert_eq!(
        (first[0].start, first[0].end),
        (0, "Çok güzel bir gün.".len())
    );
    seg.finish_spans(&mut out);
    assert_eq!(out.len(), 2);
    assert_eq!(
        (out[1].start, out[1].end),
        (
            "Çok güzel bir gün. ".len(),
            "Çok güzel bir gün. Yarın da".len()
        )
    );
    // A new stream counts from zero.
    let mut next = Vec::new();
    seg.push_spans("One two three, four", &mut next);
    seg.finish_spans(&mut next);
    assert_eq!(next[0].start, 0);
}

#[test]
fn whitespace_dropped_by_the_run_cap_stays_inside_the_raw_slices() {
    // A run of 40 spaces is longer than the cap (max_chunk_chars + 1 = 9): the segmenter keeps
    // 9 of them, but the raw slice must hold all 40 for the sent text to equal the message.
    let text = format!("One two three.{}Four five six.", " ".repeat(40));
    let spans = spans_of(config(Language::En, 3, 8), &[&text]);
    let slices = raw_slices(&text, &spans);
    assert_eq!(slices.concat(), text);
    assert!(slices.iter().any(|s| s.len() > 40), "{slices:?}");
}

#[test]
fn trailing_whitespace_is_never_sent() {
    for tail in [" ", "\n", "   \n\t "] {
        let text = format!("It is done.{tail}");
        let spans = spans_of(config(Language::En, 3, 250), &[&text]);
        let slices = raw_slices(&text, &spans);
        assert_eq!(slices.concat(), "It is done.");
    }
}

#[test]
fn a_cut_at_max_chunk_chars_loses_no_byte() {
    // A long token without whitespace is cut by the hard cap; a long sentence is cut at
    // whitespace inside the limit; both in Turkish.
    let long_word = "çok".repeat(40);
    let sentence = "ev ".repeat(60);
    for text in [long_word, sentence.clone(), format!("{sentence}{sentence}")] {
        check(config(Language::Tr, 3, 20), &text, &[1, 3, 7]);
        check(config(Language::Tr, 0, 1), &text, &[2]);
    }
}

#[test]
fn whitespace_only_text_sends_nothing() {
    let spans = spans_of(config(Language::En, 3, 250), &["   ", "\n\n", " \t"]);
    assert!(spans.is_empty());
}

const TOKENS: &[&str] = &[
    "a",
    "Bu",
    "x",
    "Dr",
    "Mr",
    "I",
    "ı",
    "İ",
    "ş",
    "doç",
    "merhaba",
    "nasılsınız",
    "the",
    "answer",
    " ",
    "  ",
    "\n",
    "\t",
    "                    ",
    " \n ",
    ".",
    "!",
    "?",
    "…",
    "...",
    ",",
    ";",
    ":",
    " - ",
    "-",
    "\"",
    ")",
    "*",
    "3",
    "14",
    "1.000",
    "10:30",
    "e.g",
    "E.U",
    "https://x.co/a",
    "é\u{301}",
    "👨\u{200d}👩",
    "🇹🇷",
    "No",
    "Dr.",
    "3. sınıf",
];

fn text_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(TOKENS), 0..60).prop_map(|t| t.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn spans_cover_the_raw_text_for_any_split(
        text in text_strategy(),
        sizes in prop::collection::vec(1usize..9, 1..6),
        language in prop::sample::select(vec![Language::En, Language::Tr]),
        first_min_words in 0usize..4,
        max_chunk_chars in 1usize..60,
    ) {
        check(config(language, first_min_words, max_chunk_chars), &text, &sizes);
    }

    #[test]
    fn spans_do_not_depend_on_how_the_text_is_split(
        text in text_strategy(),
        a in prop::collection::vec(1usize..9, 1..6),
        b in prop::collection::vec(1usize..9, 1..6),
        max_chunk_chars in 4usize..80,
    ) {
        let cfg = config(Language::Tr, 2, max_chunk_chars);
        let whole = spans_of(cfg, &[&text]);
        let first = spans_of(cfg, &split_text(&text, &a));
        let second = spans_of(cfg, &split_text(&text, &b));
        prop_assert_eq!(&whole, &first);
        prop_assert_eq!(&first, &second);
    }

    #[test]
    fn realistic_answers_in_both_languages(
        sentences in prop::collection::vec(
            prop::sample::select(vec![
                "Tabii, hemen bakıyorum.",
                "Sure, I can help with that.",
                "Randevunuz 12 Mart saat 14:30'da.",
                "Your total is $12.50, thanks!",
                "Dr. Yılmaz sizi bekliyor.",
                "Ne zaman uygun olursunuz?",
                "Is Tuesday at 3 p.m. okay?",
                "Bir saniye lütfen…",
            ]),
            1..8,
        ),
        sep in prop::sample::select(vec![" ", "  ", "\n", " \n"]),
        sizes in prop::collection::vec(1usize..12, 1..5),
        language in prop::sample::select(vec![Language::En, Language::Tr]),
    ) {
        let text = sentences.join(sep);
        check(config(language, 3, 250), &text, &sizes);
        check(config(language, 2, 30), &text, &sizes);
    }
}
