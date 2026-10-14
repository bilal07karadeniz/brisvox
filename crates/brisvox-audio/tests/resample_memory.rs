//! Memory retained by one `StreamResampler` instance. The sinc backend keeps its
//! filter table per instance (rubato does not share it), so sessions cost this much each. A
//! counting global allocator in this test binary only; one test, so no other thread allocates
//! while it measures.
#![allow(clippy::unwrap_used)]
// test helpers
// The counting allocator is the only unsafe code: it forwards to the system allocator.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use brisvox_audio::StreamResampler;
use brisvox_core::frame::SampleRate;

struct Counting;

/// Bytes currently allocated (allocations minus frees, as requested sizes).
static LIVE: AtomicIsize = AtomicIsize::new(0);

// SAFETY: forwards every call unchanged to the system allocator; only counts bytes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(
            new_size as isize - layout.size() as isize,
            Ordering::Relaxed,
        );
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Bytes an instance keeps alive: the live-byte delta across its construction.
fn retained(from: SampleRate, to: SampleRate) -> isize {
    let before = LIVE.load(Ordering::Relaxed);
    let rs = StreamResampler::new(from, to).unwrap();
    let after = LIVE.load(Ordering::Relaxed);
    drop(rs);
    after - before
}

#[test]
fn an_instance_retains_its_filter_tables_and_the_fft_stays_small() {
    // Sinc: 128 oversampled filter phases of `sinc_len` f32 taps (rubato's table), plus
    // buffers. 16 kHz: 192 taps = 96 KiB; 48 kHz: 512 taps = 256 KiB.
    let table = |taps: usize| 128 * taps * 4;
    let cases = [
        (SampleRate::Hz16000, SampleRate::Hz8000, table(192)),
        (SampleRate::Hz22050, SampleRate::Hz8000, table(224)),
        (SampleRate::Hz24000, SampleRate::Hz8000, table(224)),
        (SampleRate::Hz44100, SampleRate::Hz8000, table(384)),
        (SampleRate::Hz48000, SampleRate::Hz8000, table(512)),
    ];
    for (from, to, table_bytes) in cases {
        let bytes = retained(from, to);
        println!("{from:?}->{to:?}: retains {bytes} bytes (table {table_bytes})");
        let table_bytes = isize::try_from(table_bytes).unwrap();
        assert!(
            bytes >= table_bytes,
            "{from:?}->{to:?}: {bytes} < {table_bytes}"
        );
        assert!(
            bytes < table_bytes + 64 * 1024,
            "{from:?}->{to:?}: {bytes} bytes, table {table_bytes}"
        );
    }
    for (from, to) in [
        (SampleRate::Hz8000, SampleRate::Hz16000),
        (SampleRate::Hz48000, SampleRate::Hz16000),
        (SampleRate::Hz44100, SampleRate::Hz24000),
    ] {
        let bytes = retained(from, to);
        println!("{from:?}->{to:?} (fft): retains {bytes} bytes");
        assert!(
            (0..64 * 1024).contains(&bytes),
            "{from:?}->{to:?}: {bytes} bytes"
        );
    }
}
