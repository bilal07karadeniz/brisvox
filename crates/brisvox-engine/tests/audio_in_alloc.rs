//! A steady-state audio frame allocates nothing in `AudioIn` and its VAD tee/worker.
//!
//! The counting allocator counts only the allocations of the thread that reads the counter: the
//! test runs on a current-thread runtime, so the stage, the VAD worker (inline offload) and the
//! sink all run on it, and other test threads cannot disturb the count.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
#![allow(unsafe_code)] // the counting allocator forwards to the system allocator

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use brisvox_core::frame::{Frame, SampleRate};
use brisvox_core::{
    Clock, Direction, EpochPolicy, Links, Pipeline, Processor, ProcessorContext, SimClock,
};
use brisvox_engine::{AudioIn, EnergyModel, EngineConfig, Offload, VadSetup, VadWorkerConfig};
use common::{MS, sine_frame, square_frame};
use tokio_util::sync::CancellationToken;

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

/// Drops every frame and counts the audio ones: holding no frame lets buffers be reclaimed.
struct Sink(Arc<AtomicUsize>);

impl Processor for Sink {
    fn name(&self) -> &'static str {
        "sink"
    }

    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        frame: Frame,
        _direction: Direction,
        _ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        if matches!(frame, Frame::Data(_)) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

/// Lets every ready task run to completion.
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

/// Feeds `warmup + measured` frames at `rate`, one at a time, and returns the allocations made
/// while the measured ones went through AudioIn, the tee, the VAD worker and the sink.
async fn steady_state_allocations(rate: SampleRate, speech_every: usize) -> usize {
    let (warmup, measured) = (150usize, 400usize);
    let clock = SimClock::new();
    let origin = clock.now();
    let config = EngineConfig::DEFAULT;
    let vad = VadSetup::new(
        Offload::Inline(SimClock::shared()),
        VadWorkerConfig::default(),
        || Ok(EnergyModel::from_pcm_rms(500.0)),
    );
    let seen = Arc::new(AtomicUsize::new(0));
    let audio_in = AudioIn::new(&config).with_vad(vad);
    let vad_stats = audio_in.vad_stats();
    let running = Pipeline::new("alloc")
        .then_with_queue(audio_in, config.queues.input)
        .then(Sink(Arc::clone(&seen)))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    // Built outside the measured region. Speech alternates with silence so the worker also
    // emits events (start/stop frames are not boxed, so they must not allocate either).
    let frames: Vec<Frame> = (0..(warmup + measured) as u64)
        .map(|i| {
            let speaking = (i as usize / speech_every) % 2 == 1;
            if rate == SampleRate::Hz16000 {
                square_frame(origin, i, rate, if speaking { 3_000 } else { 0 }).into()
            } else {
                sine_frame(origin, i, rate, if speaking { 8_000.0 } else { 0.0 }).into()
            }
        })
        .collect();
    let input = running.input().clone();
    let mut frames = frames.into_iter();
    for frame in frames.by_ref().take(warmup) {
        input.try_send(frame).unwrap();
        settle().await;
    }
    let before = allocations();
    for frame in frames {
        input.try_send(frame).unwrap();
        settle().await;
    }
    let made = allocations() - before;
    assert_eq!(
        seen.load(Ordering::Relaxed),
        warmup + measured,
        "all frames went through"
    );
    // The measured path included a live VAD worker: every frame went through it, its jobs ran,
    // and it found the speech/silence alternation (otherwise "0 allocations" would be vacuous
    // for the tee and the worker).
    let vad = vad_stats.snapshot();
    assert_eq!(vad.frames, (warmup + measured) as u64, "{rate:?}: {vad:?}");
    assert!(vad.jobs_ok > 0, "{rate:?}: {vad:?}");
    assert!(vad.starts > 0 && vad.stops > 0, "{rate:?}: {vad:?}");
    assert_eq!(vad.tee_dropped, 0, "{rate:?}: {vad:?}");
    assert!(!vad.failed_open, "{rate:?}: {vad:?}");
    let _ = clock.now();
    drop(running);
    made
}

#[tokio::test(start_paused = true)]
async fn a_steady_state_frame_allocates_nothing() {
    // The counter sees this thread's allocations (a test that cannot fail proves nothing).
    let before = allocations();
    let probe = std::hint::black_box(Box::new(7u64));
    assert!(
        allocations() > before,
        "the counting allocator is not counting"
    );
    drop(probe);
    for (rate, label) in [
        (SampleRate::Hz16000, "pass-through"),
        (SampleRate::Hz8000, "8 kHz"),
        (SampleRate::Hz22050, "22.05 kHz"),
        (SampleRate::Hz24000, "24 kHz"),
        (SampleRate::Hz44100, "44.1 kHz"),
        (SampleRate::Hz48000, "48 kHz"),
    ] {
        let made = steady_state_allocations(rate, 40).await;
        assert_eq!(
            made, 0,
            "{label}: {made} allocations over 400 steady-state frames"
        );
    }
    let _ = MS;
}
