//! The chaos suite on the all-production pipeline: 100 concurrent calls on
//! `EngineFactory` (every stage of `build_session_pipeline` over a loopback transport, paused
//! clock) with random STT/LLM/TTS disconnects and stalls, mid-turn barge-ins, slow transport
//! consumers (a slow reader of the loopback client) and hang-ups (the call future dropped), judged
//! by `brisvox_sim::suite::chaos` under the Budget profile: outcome Ok, no stale audio, context
//! == played (the exact heard prefix after a TTS disconnect), unaffected calls identical to the
//! same call alone and within the first-audio SLO, recovery on the next turn, no task left
//! behind, the heap back within 4 KiB of its baseline.
//!
//! Limitation: paused time cannot show blocking or CPU-heavy work on the runtime; that is
//! `realtime_load.rs` (real clock, real pool).
// The counting allocator below is the only unsafe code: a test-binary global allocator that
// forwards to the system allocator and counts live bytes.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use brisvox_engine::sim::EngineFactory;
use brisvox_sim::suite::Profile;
use brisvox_sim::suite::chaos as suite;

/// Counts live heap bytes of this test binary (nextest runs each test in its own process).
struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);

fn bytes(layout: Layout) -> isize {
    isize::try_from(layout.size()).unwrap_or(isize::MAX)
}

// SAFETY: forwards every call unchanged to the system allocator; only adds atomic counting.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: same contract as the caller's.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE.fetch_add(bytes(layout), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(bytes(layout), Ordering::Relaxed);
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: same contract as the caller's.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            LIVE.fetch_add(bytes(layout), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: same contract as the caller's.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            LIVE.fetch_add(
                isize::try_from(new_size).unwrap_or(isize::MAX) - bytes(layout),
                Ordering::Relaxed,
            );
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[tokio::test(start_paused = true)]
async fn a_hundred_chaotic_calls_stay_isolated_and_leave_nothing_behind() {
    // One accommodation, because the shared SLO (1.5 s from the end of speech to the first audio
    // of every unaffected call) is set for the reference, which speaks from the LLM's first token
    // after a 280 ms end of turn: the adaptive policy with a fixed-answer model (the reference's
    // end of turn) instead of the fixed 450 ms silence. The first TTS chunk is the production
    // default (one word, `tests/first_chunk_policy.rs` measures it on these calls).
    // The factory keeps no handles, so nothing of a finished call stays on the heap.
    let factory = EngineFactory::new()
        .with_smart_turn(0.9)
        .without_handle_log();
    suite::a_hundred_chaotic_calls_stay_isolated_and_leave_nothing_behind(
        &factory,
        Profile::Budget,
        Some(&|| LIVE.load(Ordering::Relaxed)),
    )
    .await
    .unwrap();
}
