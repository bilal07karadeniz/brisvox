//! The resampler does not allocate after construction. A counting global allocator in
//! this test binary only (nextest runs each test in its own process).
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
// test helpers
// The counting allocator is the only unsafe code: it forwards to the system allocator.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use brisvox_audio::StreamResampler;
use brisvox_audio::analysis::sine;
use brisvox_core::frame::SampleRate;

struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards every call unchanged to the system allocator; only counts allocations.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[test]
fn process_and_flush_do_not_allocate_after_construction() {
    for (from, to) in [
        (SampleRate::Hz8000, SampleRate::Hz16000),
        (SampleRate::Hz24000, SampleRate::Hz8000),
        (SampleRate::Hz48000, SampleRate::Hz16000),
        (SampleRate::Hz22050, SampleRate::Hz16000),
        (SampleRate::Hz16000, SampleRate::Hz16000),
    ] {
        let mut rs = StreamResampler::new(from, to).unwrap();
        let input = sine(440.0, from.hz(), from.hz() as usize, 0.5);
        let mut out = vec![0i16; rs.max_output_len(input.len()) + 1_024];
        // Warm-up (first calls may initialise lazily inside the FFT library).
        let n = rs
            .process(&input[..from.hz() as usize / 10], &mut out)
            .unwrap();
        assert!(n > 0 || rs.chunk_frames() > 0);
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        for frame in input[from.hz() as usize / 10..].chunks(from.hz() as usize / 50) {
            rs.process(frame, &mut out).unwrap();
        }
        let flushed = rs.flush(&mut out).unwrap();
        let after = ALLOCATIONS.load(Ordering::Relaxed);
        assert_eq!(
            after - before,
            0,
            "{from:?}->{to:?} allocated (flushed {flushed})"
        );
    }
}
