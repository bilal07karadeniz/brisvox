//! A steady-state caller frame allocates nothing on the whole user side: `AudioIn` (with its VAD
//! tee and worker) -> `SttStage` (into the provider's channel) -> `TurnDriver` (with and without
//! the Smart Turn ring) (CONTRIBUTING.md hot-path rule).
//!
//! The VAD worker runs on the inline offload here; on the inference pool (production) each batch
//! job also costs a fixed number of allocations on the submitting thread, pinned by the last
//! test of this file (about one batch per 20 ms frame).
//!
//! The counting allocator counts only the allocations of the thread that reads the counter: the
//! test runs on a current-thread runtime, so every stage, the VAD worker (inline offload), the
//! STT stage's session task and the null provider's drain task run on it, and other test threads
//! cannot disturb the count. Per-turn work (a VAD start or stop, the finalize it causes, the
//! turn decision) may allocate and is left out of the measured windows: they lie in steady
//! speech and steady silence, well past every transition.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
#![allow(unsafe_code)] // the counting allocator forwards to the system allocator

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use brisvox_core::frame::{Frame, SampleRate};
use brisvox_core::{
    Direction, EpochPolicy, Links, Pipeline, Processor, ProcessorContext, SimClock,
    TimelineRecorder,
};
use brisvox_engine::sim::FixedTurnModel;
use brisvox_engine::{
    AudioIn, EnergyModel, EngineConfig, Offload, SttStage, SttStageConfig, TurnDriver,
    TurnDriverConfig, VadSetup, VadWorkerConfig,
};
use brisvox_providers::{
    Result as ProviderResult, SttConfig, SttEvent, SttInput, SttProvider, SttSession,
};
use common::square_frame;
use tokio::sync::mpsc;
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

/// A provider that takes every input and never answers (no allocation per input of its own).
struct NullStt {
    audio: Arc<AtomicUsize>,
}

impl SttProvider for NullStt {
    fn name(&self) -> &'static str {
        "null"
    }

    async fn connect(&self, _config: SttConfig) -> ProviderResult<SttSession> {
        let (input, mut inputs) = mpsc::channel::<SttInput>(64);
        let (events_tx, events) = mpsc::channel::<SttEvent>(8);
        let audio = Arc::clone(&self.audio);
        tokio::spawn(async move {
            let _events = events_tx;
            while let Some(item) = inputs.recv().await {
                if matches!(item, SttInput::Audio(_)) {
                    audio.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        Ok(SttSession { input, events })
    }
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
    for _ in 0..30 {
        tokio::task::yield_now().await;
    }
}

/// What one run measured: allocations in steady speech and in steady silence, per region of
/// `MEASURED` frames.
struct Measured {
    speech: usize,
    silence: usize,
}

const MEASURED: u64 = 200;

async fn user_side_allocations(smart_turn: bool) -> Measured {
    let clock = SimClock::shared();
    let origin = clock.now();
    let config = EngineConfig::DEFAULT;
    let vad = VadSetup::new(
        Offload::Inline(clock.clone()),
        VadWorkerConfig::default(),
        || Ok(EnergyModel::from_pcm_rms(500.0)),
    );
    let audio_in = AudioIn::new(&config).with_vad(vad);
    let vad_stats = audio_in.vad_stats();
    let provider_audio = Arc::new(AtomicUsize::new(0));
    let stt = SttStage::new(
        Arc::new(NullStt {
            audio: Arc::clone(&provider_audio),
        }),
        SttStageConfig::from_engine(&config),
        clock.clone(),
    );
    let stt_stats = stt.stats();
    let mut driver = TurnDriver::new(
        TurnDriverConfig::from_engine(&config),
        clock.clone(),
        TimelineRecorder::new(),
    );
    if smart_turn {
        driver = driver.with_smart_turn(
            Arc::new(FixedTurnModel(0.9)),
            Offload::Inline(clock.clone()),
        );
    }
    let seen = Arc::new(AtomicUsize::new(0));
    let running = Pipeline::new("user-alloc")
        .then_with_queue(audio_in, config.queues.input)
        .then_with_queue(stt, config.queues.user)
        .then_with_queue(driver, config.queues.user)
        .then(Sink(Arc::clone(&seen)))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    let input = running.input().clone();
    input
        .try_send(Frame::from(brisvox_core::frame::SystemFrame::Start))
        .unwrap();
    settle().await;

    // Silence, then a long stretch of speech, then a long silence. Measured: the last MEASURED
    // frames of each stretch (the speech start, the stop, the finalize and the turn decision all
    // happen before them).
    let (lead, speech, silence) = (60u64, 150 + MEASURED, 150 + MEASURED);
    let total = lead + speech + silence;
    // The frames' buffers are shared, as the transports make them (each a split of a reused
    // `BytesMut`): a `Bytes` made from a `Vec` would allocate its shared header at its first
    // clone, which is the STT stage's (one allocation per frame, the buffer's, not the engine's;
    // see docs/LATENCY.md). `keep` holds the clone that did the promotion here, outside the
    // measured region.
    let mut keep = Vec::new();
    let frames: Vec<Frame> = (0..total)
        .map(|i| {
            let speaking = (lead..lead + speech).contains(&i);
            let frame = square_frame(
                origin,
                i,
                SampleRate::Hz16000,
                if speaking { 3_000 } else { 0 },
            );
            keep.push(frame.data().clone());
            frame.into()
        })
        .collect();
    let mut made = Vec::new();
    let mut before = 0usize;
    for (i, frame) in (0u64..).zip(frames) {
        if i == lead + speech - MEASURED || i == total - MEASURED {
            before = allocations();
        }
        input.try_send(frame).unwrap();
        settle().await;
        if i == lead + speech - 1 || i == total - 1 {
            made.push(allocations() - before);
        }
    }
    // Every frame went through the stages and reached the provider's channel (the driver keeps
    // the caller's audio: nothing reaches the sink); the VAD found the speech (otherwise "0
    // allocations" would be vacuous).
    assert_eq!(
        seen.load(Ordering::Relaxed),
        0,
        "the driver passes no caller audio on"
    );
    let vad = vad_stats.snapshot();
    assert!(vad.starts >= 1 && vad.stops >= 1, "{vad:?}");
    let stt = stt_stats.snapshot();
    assert_eq!(stt.audio_in, total, "{stt:?}");
    assert_eq!(stt.audio_dropped_queue, 0, "{stt:?}");
    assert_eq!(provider_audio.load(Ordering::Relaxed) as u64, total);
    drop(input);
    drop(keep);
    drop(running);
    Measured {
        speech: made[0],
        silence: made[1],
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_steady_state_caller_frame_allocates_nothing_from_audio_in_to_the_turn_driver() {
    // The counter sees this thread's allocations (a test that cannot fail proves nothing).
    let before = allocations();
    let probe = std::hint::black_box(Box::new(7u64));
    assert!(
        allocations() > before,
        "the counting allocator is not counting"
    );
    drop(probe);
    for smart_turn in [false, true] {
        let measured = user_side_allocations(smart_turn).await;
        assert_eq!(
            (measured.speech, measured.silence),
            (0, 0),
            "smart turn {smart_turn}: allocations over {MEASURED} frames of speech, of silence"
        );
    }
}

/// Heap allocations one job submitted to the inference pool costs on the submitting (tokio)
/// thread: the reply channel, the abandonment flag and the boxed job (the timeout's sleep and the
/// wait allocate nothing). In production the VAD worker runs on the pool and forms about one batch
/// per 20 ms frame, so the AudioIn -> VAD path costs this much per frame on top of the zero the
/// inline measurement above shows (docs/TESTING.md). Pinned so a regression in the submission
/// path fails here.
const POOL_SUBMIT_ALLOCATIONS: usize = 3;

#[tokio::test(flavor = "current_thread")]
async fn a_pool_job_costs_a_fixed_number_of_allocations_on_the_submitting_thread() {
    use brisvox_core::{SharedClock, SystemClock};
    use brisvox_inference::{InferencePool, PoolConfig};

    let clock: SharedClock = SystemClock::shared();
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 1,
            queue_capacity: 4,
            name: "alloc-test".to_owned(),
        },
        clock,
    )
    .unwrap();
    let offload = Offload::Pool(pool.handle());
    let timeout = std::time::Duration::from_secs(5);
    // Warm the runtime's and the pool's lazily built parts outside the measured jobs.
    for _ in 0..8 {
        offload.run(timeout, || 1u32).await.unwrap();
    }
    const JOBS: usize = 200;
    let before = allocations();
    for i in 0..JOBS {
        let done = offload.run(timeout, move || i).await.unwrap();
        assert_eq!(done.value, i);
    }
    let made = allocations() - before;
    assert_eq!(
        made,
        JOBS * POOL_SUBMIT_ALLOCATIONS,
        "allocations over {JOBS} pool jobs on the submitting thread"
    );
    drop(offload);
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
}
