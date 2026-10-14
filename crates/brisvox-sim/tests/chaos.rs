//! Chaos scenarios: 100 concurrent simulated calls with random provider disconnects and
//! stalls, slow transport consumers, mid-turn barge-ins and hang-ups, in paused time.
//!
//! The body lives in `brisvox_sim::suite::chaos` so the production pipeline runs it too. This file supplies what only a test binary can: the counting global allocator the
//! suite reads to prove the heap returns to its baseline, and the reference factory with the
//! exact profile.
//!
//! Limitation: paused time cannot show blocking or CPU-heavy work on the runtime; real-time load
//! is covered by `brisvox-bench sim-load`.
// The counting allocator below is the only unsafe code: a test-binary global allocator that
// forwards to the system allocator and counts live bytes.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use brisvox_sim::factory::ReferenceFactory;
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
    suite::a_hundred_chaotic_calls_stay_isolated_and_leave_nothing_behind(
        &ReferenceFactory,
        Profile::Exact,
        Some(&|| LIVE.load(Ordering::Relaxed)),
    )
    .await
    .unwrap();
}
