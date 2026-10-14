//! Global allocator micro-bench (evaluation of the allocator choice, from a perf audit).
//!
//! The LLM path allocates one `Arc<str>` per streamed token on the LLM task and the TTS/segmenter
//! side drops it on another worker thread: a cross-thread free, the pattern general-purpose
//! allocators handle worst. This bench reproduces just that: `PAIRS` producer threads allocate
//! token-sized `Arc<str>` and pass them over a bounded channel to a consumer thread that drops
//! them, `TOKENS` per pair, and reports the median nanoseconds per token over `REPS` runs. A
//! same-thread alloc+free loop is the baseline.
//!
//! Run it once per allocator and compare:
//!
//! ```text
//! cargo run --release -p brisvox-server --example alloc_bench --no-default-features   # system
//! cargo run --release -p brisvox-server --example alloc_bench                          # mimalloc
//! ```
//!
//! Results: mimalloc is the default allocator of the server binary (`docs/ARCHITECTURE.md`).
// This is a wall-clock benchmark, not engine logic: it measures real elapsed time on purpose.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::Instant;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Token texts of realistic sizes (a BPE token is 1-12 bytes; markdown/numbers a bit more).
const TOKENS_TEXT: [&str; 8] = [
    "The",
    " quick",
    " brown",
    "fox,",
    " jumps",
    " over",
    " 1,234.56",
    " lazy-dog.\n",
];

/// Tokens per producer/consumer pair per run.
const TOKENS: usize = 1_000_000;
/// Repetitions per configuration; the median is reported.
const REPS: usize = 7;
/// Bounded channel between producer and consumer (the real queues are bounded too).
const CHANNEL: usize = 256;

fn allocator_name() -> &'static str {
    if cfg!(feature = "mimalloc") {
        "mimalloc"
    } else {
        "system"
    }
}

/// `pairs` producer->consumer thread pairs; returns the wall-clock ns per token of one pair.
fn cross_thread(pairs: usize) -> f64 {
    let start = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..pairs {
            let (tx, rx) = sync_channel::<Arc<str>>(CHANNEL);
            scope.spawn(move || {
                for i in 0..TOKENS {
                    let text = TOKENS_TEXT
                        .get(i % TOKENS_TEXT.len())
                        .copied()
                        .unwrap_or_default();
                    if tx.send(Arc::from(text)).is_err() {
                        break;
                    }
                }
            });
            scope.spawn(move || {
                while let Ok(token) = rx.recv() {
                    drop(std::hint::black_box(token));
                }
            });
        }
    });
    start.elapsed().as_nanos() as f64 / TOKENS as f64
}

/// Allocate and free on the same thread (no channel): the allocator's best case.
fn same_thread() -> f64 {
    let start = Instant::now();
    for i in 0..TOKENS {
        let text = TOKENS_TEXT
            .get(i % TOKENS_TEXT.len())
            .copied()
            .unwrap_or_default();
        drop(std::hint::black_box(Arc::<str>::from(text)));
    }
    start.elapsed().as_nanos() as f64 / TOKENS as f64
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(f64::NAN)
}

fn main() {
    println!("allocator: {}", allocator_name());
    let base = median((0..REPS).map(|_| same_thread()).collect());
    println!("same-thread alloc+free        : {base:8.1} ns/token");
    for pairs in [1, 4, 16] {
        let ns = median((0..REPS).map(|_| cross_thread(pairs)).collect());
        println!("cross-thread, {pairs:2} producer pairs: {ns:8.1} ns/token");
    }
}
