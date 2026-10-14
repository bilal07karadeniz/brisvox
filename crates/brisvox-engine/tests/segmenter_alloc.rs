//! `Segmenter::push_spans` allocates per emitted chunk, never per character or per delta
//! (CONTRIBUTING.md hot-path rule). This is the machine-independent guard behind the 50 us
//! rule: wall-clock bounds in a debug test run are loose by an order of magnitude (see the cost
//! test in `segmenter_stage.rs`), but a per-character allocation, or a rebuilt offsets vector per
//! push, shows up here as an exact count.
//!
//! The counting allocator counts only the allocations of the thread that reads the counter; the
//! tests run on that thread, so nothing else is counted.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods
)]
#![allow(unsafe_code)] // the counting allocator forwards to the system allocator

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use brisvox_agent::Language;
use brisvox_agent::segment::Segmenter;
use brisvox_engine::SegmenterStageConfig;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

struct Counting;

fn count() {
    // Const-initialised, destructor-free thread local: reading it never allocates.
    let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
}

// SAFETY: forwards every call unchanged to the system allocator; only counts allocations.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocations() -> usize {
    ALLOCATIONS.with(Cell::get)
}

/// What one run over `text` in deltas of `delta_chars` cost.
struct Run {
    chars: usize,
    pushes: usize,
    spans: usize,
    allocations: usize,
}

fn run(language: Language, text: &str, delta_chars: usize) -> Run {
    let chars: Vec<char> = text.chars().collect();
    let deltas: Vec<String> = chars
        .chunks(delta_chars)
        .map(|c| c.iter().collect())
        .collect();
    let mut segmenter = Segmenter::new(SegmenterStageConfig::new(language).segmenter);
    // The scratch vector of the stage is reused across calls: give it room up front.
    let mut spans = Vec::with_capacity(64);
    let mut emitted = 0;
    let before = allocations();
    for delta in &deltas {
        segmenter.push_spans(delta, &mut spans);
        emitted += spans.len();
        spans.clear();
    }
    let allocations = allocations() - before;
    Run {
        chars: chars.len(),
        pushes: deltas.len(),
        spans: emitted,
        allocations,
    }
}

#[test]
fn a_push_allocates_per_chunk_not_per_character_or_per_delta() {
    let sentence = "Randevunuz yarın saat 14:30'da, Dr. Yılmaz sizi bekliyor olacak; lütfen kimliğinizi unutmayın. ";
    let shapes: [(&str, String); 4] = [
        ("sentences", sentence.repeat(120)),
        (
            "no punctuation",
            "randevunuz yarın saat on dört otuzda doktor yılmaz sizi bekliyor olacak lütfen "
                .repeat(150),
        ),
        ("whitespace flood", format!("a{}b", " \n".repeat(6000))),
        ("one long token", "x".repeat(12_000)),
    ];
    for (shape, text) in &shapes {
        for language in [Language::En, Language::Tr] {
            for delta_chars in [4, 12] {
                let r = run(language, text, delta_chars);
                println!(
                    "{shape} {language:?} /{delta_chars}: {} chars, {} pushes, {} spans, {} allocations",
                    r.chars, r.pushes, r.spans, r.allocations
                );
                // Per chunk: the chunk text and a little growth of the pending buffer. Text with
                // sentence ends adds allocations per push while a candidate period waits for its
                // lookahead character (the abbreviation check re-reads the token before it):
                // measured 1.6 per push at 4-char deltas and 2.3 at 12-char deltas, bounded here
                // by 3. A per-character allocation would be `chars`: 4 to 12 per push.
                let per_push = if *shape == "sentences" { 3 } else { 0 };
                let budget = 4 * r.spans + per_push * r.pushes + 64;
                assert!(
                    r.allocations <= budget,
                    "{shape} {language:?} /{delta_chars}: {} allocations for {} chars, {}                      pushes and {} spans (budget {budget})",
                    r.allocations,
                    r.chars,
                    r.pushes,
                    r.spans
                );
            }
        }
    }
}
