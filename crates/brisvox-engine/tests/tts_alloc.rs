//! The TTS slice path allocates per provider chunk, never per frame (the hot-path
//! rule of CONTRIBUTING.md): converting a chunk to the call's output rate, cutting it into exact 20 ms frames and
//! handing the frames to the next stage's inbox costs the same number of allocations whether the
//! chunk holds one frame or a hundred.
//!
//! The counting allocator counts only the allocations of the thread that reads the counter; the
//! tests run on that thread (the stage test uses a current-thread runtime), so nothing else is
//! counted.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods
)]
#![allow(unsafe_code)] // the counting allocator forwards to the system allocator

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use brisvox_core::frame::{
    AudioFrame, ControlFrame, ControlKind, DataFrame, Frame, SampleRate, TextFrame,
};
use brisvox_core::{
    Direction, EpochPolicy, FrameSender, Inbox, Links, Pipeline, Processor, ProcessorContext,
    QueueConfig, SimClock, TimelineRecorder, TurnEpoch,
};
use brisvox_engine::{CallRatesCell, EngineConfig, OutputFramer, TtsStage, TtsStageConfig};
use brisvox_providers::{
    ContextId, Result as ProviderResult, TtsConfig, TtsEvent, TtsInput, TtsProvider, TtsSession,
};
use brisvox_transport::CallRates;
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::time::Instant;
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

/// A provider chunk of `ms` milliseconds of 24 kHz audio.
fn chunk(ms: usize) -> Vec<u8> {
    (0..ms * 24 * 2).map(|i| (i % 251) as u8).collect()
}

/// The slice path under test: the framer, the frames built from its output and the next stage's
/// inbox.
struct Path {
    framer: OutputFramer,
    inbox: Inbox,
    sender: FrameSender,
    scratch: Vec<Bytes>,
    /// The frames of the previous chunk, still alive while the next one is converted.
    held: Vec<Bytes>,
    seq: u64,
    now: Instant,
}

impl Path {
    fn new(from: SampleRate, to: SampleRate, frame_samples: usize) -> Self {
        let inbox = Inbox::new(QueueConfig::DEFAULT);
        let sender = inbox.sender(Direction::Downstream);
        Self {
            framer: OutputFramer::new(from, to, frame_samples).unwrap(),
            inbox,
            sender,
            scratch: Vec::with_capacity(512),
            held: Vec::with_capacity(512),
            seq: 0,
            now: Instant::now(),
        }
    }

    /// Pushes `pcm` through the framer and hands every frame to the inbox (dropping what the
    /// inbox returns), counting the allocations of that whole path. The frames of the previous
    /// call are still alive while this one converts (they sit in the pipeline's queues and the
    /// transport's buffers), so the framer cannot reuse their block. Returns (allocations,
    /// frames).
    fn run(&mut self, pcm: &[u8]) -> (usize, usize) {
        self.scratch.clear();
        let before = allocations();
        let scratch = &mut self.scratch;
        self.framer
            .push(pcm, &mut |frame| scratch.push(frame))
            .unwrap();
        let frames = self.scratch.len();
        self.held.clear();
        for bytes in self.scratch.drain(..) {
            self.seq += 1;
            self.held.push(bytes.clone());
            let audio = AudioFrame::new(
                bytes,
                SampleRate::Hz16000,
                self.now,
                self.seq,
                TurnEpoch::new(1),
            )
            .unwrap();
            self.sender.try_send(Frame::from(audio)).unwrap();
            // The next stage takes it at once.
            drop(self.inbox.try_recv().unwrap());
        }
        (allocations() - before, frames)
    }
}

#[test]
fn the_slice_path_allocates_per_chunk_and_never_per_frame() {
    for (from, to, frame_samples) in [
        (SampleRate::Hz24000, SampleRate::Hz16000, 320),
        (SampleRate::Hz24000, SampleRate::Hz8000, 160),
        (SampleRate::Hz16000, SampleRate::Hz16000, 320),
    ] {
        let mut path = Path::new(from, to, frame_samples);
        // Warm up with the biggest chunk: the scratch buffers reach their final size.
        for _ in 0..3 {
            path.run(&chunk(1_000));
        }
        let mut per_chunk = Vec::new();
        for ms in [20usize, 100, 400, 1_000] {
            let pcm = chunk(ms);
            let mut worst = 0;
            let mut frames = 0;
            for _ in 0..10 {
                let (allocated, produced) = path.run(&pcm);
                worst = worst.max(allocated);
                frames = frames.max(produced);
            }
            println!("{from:?}->{to:?}: {ms} ms chunk, up to {frames} frames, {worst} allocations");
            per_chunk.push((ms, frames, worst));
        }
        // The cost of a chunk is the same for a 20 ms chunk and a 1 s chunk (50 frames): a
        // handful per chunk (the block, the shared counter of the split), none per frame.
        let (_, small_frames, small) = per_chunk[0];
        let (_, big_frames, big) = per_chunk[3];
        assert!(big_frames >= 20 * small_frames.max(1), "{per_chunk:?}");
        assert!(small <= 4 && big <= 4, "{from:?}->{to:?}: {per_chunk:?}");
        assert!(
            big <= small + 1,
            "{from:?}->{to:?}: allocations grow with the frame count: {per_chunk:?}"
        );
    }
}

#[test]
fn frames_whose_predecessors_are_gone_reuse_the_block_and_allocate_nothing() {
    let mut path = Path::new(SampleRate::Hz24000, SampleRate::Hz16000, 320);
    for _ in 0..3 {
        path.run(&chunk(1_000));
    }
    let pcm = chunk(400);
    for _ in 0..5 {
        // Nothing of the earlier chunks is alive any more at the start of the push.
        path.held.clear();
        path.scratch.clear();
        let before = allocations();
        let scratch = &mut path.scratch;
        path.framer
            .push(&pcm, &mut |frame| scratch.push(frame))
            .unwrap();
        let allocated = allocations() - before;
        assert!(path.scratch.len() >= 19);
        path.scratch.clear();
        assert_eq!(allocated, 0, "the block is reused once its frames are gone");
    }
}

// ---------------------------------------------------------------------------------------------
// The real stage

/// A provider that hands the test the sending end of the event channel.
struct EventsTts {
    events: Mutex<Option<mpsc::Sender<TtsEvent>>>,
    /// Keeps the session's other channels open.
    keep: Mutex<Vec<(mpsc::Receiver<TtsInput>, mpsc::Receiver<ContextId>)>>,
}

impl TtsProvider for EventsTts {
    fn name(&self) -> &'static str {
        "events"
    }

    async fn connect(&self, _config: TtsConfig) -> ProviderResult<TtsSession> {
        let (input, input_rx) = mpsc::channel(16);
        let (cancel, cancel_rx) = mpsc::channel(4);
        let (events_tx, events) = mpsc::channel(64);
        *self.events.lock().unwrap_or_else(PoisonError::into_inner) = Some(events_tx);
        self.keep
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((input_rx, cancel_rx));
        Ok(TtsSession {
            input,
            cancel,
            events,
        })
    }
}

/// The end of the pipeline: counts the audio frames and drops everything (no allocation).
struct CountSink(Arc<AtomicUsize>);

impl Processor for CountSink {
    fn name(&self) -> &'static str {
        "count-sink"
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
        if matches!(frame, Frame::Data(DataFrame::Audio(_))) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

/// `chunks` provider events of `ms` milliseconds each through a REAL `TtsStage` (reader task,
/// ledger lookups, framer, frame construction, the pipeline queue, a downstream stage); returns
/// (allocations from the first event sent to the last frame received, frames received).
async fn stage_run(
    events: &mpsc::Sender<TtsEvent>,
    received: &AtomicUsize,
    fed: &AtomicUsize,
    ms: usize,
    chunks: usize,
) -> (usize, usize) {
    let pcm = Bytes::from(chunk(ms));
    let target = fed.fetch_add(chunks * (ms / 20), Ordering::Relaxed) + chunks * (ms / 20) - 1;
    let before = allocations();
    for _ in 0..chunks {
        events
            .send(TtsEvent::Audio {
                context: ContextId(1),
                audio: pcm.clone(),
            })
            .await
            .unwrap();
    }
    let mut spins = 0usize;
    while received.load(Ordering::Relaxed) < target {
        spins += 1;
        assert!(spins < 1_000_000, "frames never arrived");
        tokio::task::yield_now().await;
    }
    (allocations() - before, chunks * (ms / 20))
}

/// The test above models the path by hand; this one drives the real stage, so a regression in
/// `Reader::audio` / `send_frames` (a `Vec` or an `Arc` per frame, a clone per frame, a
/// per-frame ledger allocation) shows up. Twenty provider events of 20 ms (one frame each) and
/// twenty of 1 s (fifty frames each) must cost the same number of allocations, give or take the
/// channel's block allocations: 980 frames more, no allocation more.
#[test]
fn the_real_stage_allocates_per_chunk_and_never_per_frame() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let clock = SimClock::shared();
        let provider = Arc::new(EventsTts {
            events: Mutex::new(None),
            keep: Mutex::new(Vec::new()),
        });
        let config = TtsStageConfig::from_engine(&EngineConfig::DEFAULT);
        let stage = TtsStage::new(
            Arc::clone(&provider),
            config,
            clock.clone(),
            TimelineRecorder::new(),
        )
        .with_rates(CallRatesCell::new(CallRates::symmetric(
            SampleRate::Hz16000,
        )));
        let received = Arc::new(AtomicUsize::new(0));
        // Frames the events fed so far (the resampler's ramp-in makes the output one short).
        let fed = AtomicUsize::new(0);
        let running = Pipeline::new("alloc")
            .then(stage)
            .then(CountSink(Arc::clone(&received)))
            .start(Links::default(), &CancellationToken::new())
            .await
            .unwrap();
        for _ in 0..30 {
            tokio::task::yield_now().await;
        }
        // Open context 1 (the stage numbers contexts from 1) and speak on it.
        let epoch = TurnEpoch::new(1);
        for frame in [
            Frame::Control(ControlFrame {
                epoch,
                kind: ControlKind::LlmResponseStarted,
            }),
            Frame::Data(DataFrame::LlmText(TextFrame {
                epoch,
                text: Arc::from("Hello there."),
            })),
        ] {
            running.input().send(frame).await.unwrap();
        }
        for _ in 0..30 {
            tokio::task::yield_now().await;
        }
        let events = provider
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap();
        // Warm up: the resampler, the queues and the scratch buffers reach their final size.
        for _ in 0..3 {
            stage_run(&events, &received, &fed, 1_000, 20).await;
        }
        let mut rows = Vec::new();
        for ms in [20usize, 1_000] {
            let mut worst = 0;
            let mut frames = 0;
            for _ in 0..5 {
                let (allocated, produced) = stage_run(&events, &received, &fed, ms, 20).await;
                worst = worst.max(allocated);
                frames = produced;
            }
            println!("real stage: 20 x {ms} ms chunks, {frames} frames, {worst} allocations");
            rows.push((ms, frames, worst));
        }
        let (small_frames, small) = (rows[0].1, rows[0].2);
        let (big_frames, big) = (rows[1].1, rows[1].2);
        assert_eq!((small_frames, big_frames), (20, 1_000));
        assert!(
            big <= small + 8,
            "allocations grow with the frame count: {rows:?}"
        );
    });
}
