//! Output pacer timing: absolute 20 ms grid, interrupt flush (on a timed wait, on a due
//! frame and on a push blocked downstream), due deadlines, per-session phase (opt-in),
//! the first frame at once (the default), underruns, turn ends and frame durations. Paused
//! tokio time with a `CountingClock` over `SimClock`.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)] // test helpers (see clippy.toml)

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use brisvox_audio::pacer::{DEFAULT_MAX_LAG, FRAME, OutputPacer, PacerMetrics, phase_offset};
use brisvox_core::clock::deadline_after;
use brisvox_core::frame::{
    AudioFrame, ControlFrame, ControlKind, DataFrame, Frame, SampleRate, SystemFrame,
};
use brisvox_core::queue::OverflowPolicies;
use brisvox_core::{
    Clock, Direction, EpochPolicy, FrameSender, Inbox, Processor, ProcessorContext, QueueConfig,
    Result, SharedClock, SimClock, TurnEpoch, run_processor,
};
use bytes::Bytes;
use tokio::task::JoinHandle;
use tokio::time::{Instant, Sleep};
use tokio_util::sync::CancellationToken;

/// A clock that counts every `sleep_until` call and the ones on an already-due deadline (those
/// must never happen: a due frame is released without touching the timer). It can also run a
/// hook on the next `now()` call, to interleave an interrupt at an exact point of the pacer's
/// work. With `late` set, every timer it creates fires `late` after its deadline, like a coarse
/// OS timer tick.
#[derive(Default)]
struct CountingClock {
    inner: SimClock,
    late: Duration,
    sleeps: AtomicUsize,
    due_sleeps: AtomicUsize,
    on_next_now: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl std::fmt::Debug for CountingClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CountingClock").finish_non_exhaustive()
    }
}

impl Clock for CountingClock {
    fn now(&self) -> Instant {
        let hook = self.on_next_now.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        self.inner.now()
    }

    fn wall(&self) -> SystemTime {
        self.inner.wall()
    }

    fn sleep_until(&self, deadline: Instant) -> Sleep {
        self.sleeps.fetch_add(1, Ordering::Relaxed);
        if deadline <= self.inner.now() {
            self.due_sleeps.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.sleep_until(deadline_after(deadline, self.late))
    }
}

/// One record per audio frame that reaches the sink: (time, epoch, sequence number).
type AudioLog = Arc<Mutex<Vec<(Instant, TurnEpoch, u64)>>>;
/// One record per `TtsEnded` that reaches the sink: (time, epoch).
type EndLog = Arc<Mutex<Vec<(Instant, TurnEpoch)>>>;

/// Records every audio frame and turn end it receives. It keeps everything (`KeepAll`), so a
/// stale frame that the pacer let through shows up in the log. With `stall = (seq, pause)` it
/// sleeps for `pause` after recording frame `seq`: a slow consumer.
struct Sink {
    clock: SharedClock,
    audio: AudioLog,
    ends: EndLog,
    stall: Option<(u64, Duration)>,
}

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
        _dir: Direction,
        _ctx: &mut ProcessorContext,
    ) -> Result<()> {
        if let Frame::Control(ControlFrame {
            epoch,
            kind: ControlKind::TtsEnded,
        }) = &frame
        {
            self.ends.lock().unwrap().push((self.clock.now(), *epoch));
        }
        if let Frame::Data(DataFrame::Audio(audio)) = &frame {
            let seq = audio.seq();
            self.audio
                .lock()
                .unwrap()
                .push((self.clock.now(), audio.epoch(), seq));
            if let Some((stall_seq, pause)) = self.stall
                && seq == stall_seq
            {
                self.clock.sleep(pause).await;
            }
        }
        Ok(())
    }
}

/// A running pacer -> sink chain, wired by hand so the sink's inbox size is under test control.
struct Rig {
    input: FrameSender,
    audio: AudioLog,
    ends: EndLog,
    metrics: PacerMetrics,
    tasks: Vec<JoinHandle<Result<()>>>,
}

/// Starts `pacer` feeding a sink whose inbox holds `sink_slots` frames. A one-slot sink makes
/// the pacer block on a busy consumer, so a stall shows up in the pacer's own timing.
fn start(
    clock: &SharedClock,
    pacer: OutputPacer,
    stall: Option<(u64, Duration)>,
    sink_slots: usize,
) -> Rig {
    let metrics = pacer.metrics();
    let audio = AudioLog::default();
    let ends = EndLog::default();
    let sink_inbox = Inbox::new(QueueConfig {
        system_capacity: 4,
        data_capacity: sink_slots,
        policies: OverflowPolicies::DEFAULT,
    });
    let sink_ctx = ProcessorContext::new(None, None);
    let sink = Sink {
        clock: clock.clone(),
        audio: audio.clone(),
        ends: ends.clone(),
        stall,
    };
    let pacer_inbox = Inbox::new(QueueConfig::DEFAULT);
    let input = pacer_inbox.sender(Direction::Downstream);
    let pacer_ctx = ProcessorContext::new(Some(sink_inbox.sender(Direction::Downstream)), None);
    let cancel = CancellationToken::new();
    let tasks = vec![
        tokio::spawn(run_processor(pacer, pacer_inbox, pacer_ctx, cancel.clone())),
        tokio::spawn(run_processor(sink, sink_inbox, sink_ctx, cancel)),
    ];
    Rig {
        input,
        audio,
        ends,
        metrics,
        tasks,
    }
}

impl Rig {
    async fn send(&self, frame: AudioFrame) {
        self.input.send(frame.into()).await.unwrap();
    }

    async fn send_interrupt(&self, new_epoch: u32) {
        self.input
            .send(
                SystemFrame::Interrupt {
                    new_epoch: TurnEpoch::new(new_epoch),
                }
                .into(),
            )
            .await
            .unwrap();
    }

    async fn send_tts_ended(&self, epoch: u32) {
        self.input
            .send(
                ControlFrame {
                    epoch: TurnEpoch::new(epoch),
                    kind: ControlKind::TtsEnded,
                }
                .into(),
            )
            .await
            .unwrap();
    }

    /// Lets the pipeline drain (a `Stop` follows every queued frame) and returns the audio log.
    async fn stop(self) -> Vec<(Instant, TurnEpoch, u64)> {
        self.input.send(SystemFrame::Stop.into()).await.unwrap();
        for task in self.tasks {
            task.await.unwrap().unwrap();
        }
        self.audio.lock().unwrap().clone()
    }
}

/// A PCM16 16 kHz frame of `ms` milliseconds (16 samples per ms), stamped by `clock`.
fn frame(clock: &dyn Clock, seq: u64, epoch: u32, ms: u64) -> AudioFrame {
    let bytes = usize::try_from(ms * 16 * 2).unwrap();
    AudioFrame::new(
        Bytes::from(vec![0u8; bytes]),
        SampleRate::Hz16000,
        clock.now(),
        seq,
        TurnEpoch::new(epoch),
    )
    .unwrap()
}

/// The `k`-th grid point after `first`, for 20 ms frames.
fn grid(first: Instant, k: u64) -> Instant {
    first + FRAME * u32::try_from(k).unwrap()
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// A pacer with the optional per-session phase on (the original behaviour: the first frame of the
/// session waits for `phase_offset(seed)`). Most tests below pin the grid through that phase.
fn phased(clock: &SharedClock, seed: u64) -> OutputPacer {
    OutputPacer::new(clock.clone(), seed).with_first_frame_phase(true)
}

/// A pacer with the phase `on` or off (the production default), for the tests that run in both
/// modes. The offset of the session's first frame is `pacer.phase()` when on and zero when off.
fn with_phase(clock: &SharedClock, seed: u64, on: bool) -> OutputPacer {
    OutputPacer::new(clock.clone(), seed).with_first_frame_phase(on)
}

/// Where the session's first frame leaves relative to its arrival in a mode: the seeded phase
/// when on, at once when off.
fn first_offset(pacer: &OutputPacer, on: bool) -> Duration {
    if on { pacer.phase() } else { Duration::ZERO }
}

#[tokio::test(start_paused = true)]
async fn frames_stay_on_an_absolute_grid_over_10_000_frames_with_a_slow_consumer() {
    for phase_on in [false, true] {
        grid_over_10_000_frames(phase_on).await;
    }
}

async fn grid_over_10_000_frames(phase_on: bool) {
    let clock = Arc::new(CountingClock::default());
    let shared: SharedClock = clock.clone();
    let start_at = shared.now();
    let pacer = with_phase(&shared, 42, phase_on);
    let phase = first_offset(&pacer, phase_on);
    // The consumer stalls for 40 ms (less than max_lag) on frame 3000.
    let rig = start(&shared, pacer, Some((3_000, ms(40))), 1);
    for seq in 0..10_000 {
        rig.send(frame(&*shared, seq, 0, 20)).await;
    }
    let metrics = rig.metrics.clone();
    let log = rig.stop().await;

    assert_eq!(log.len(), 10_000);
    let first = log[0].0;
    assert_eq!(
        first - start_at,
        phase,
        "session phase on the first frame (phase on: {phase_on})"
    );
    for (k, (at, _, seq)) in log.iter().enumerate() {
        let k = u64::try_from(k).unwrap();
        assert_eq!(*seq, k);
        let due = grid(first, k);
        assert!(*at >= due, "frame {k} released early");
        assert!(
            *at - due <= DEFAULT_MAX_LAG,
            "frame {k} lagged beyond max_lag"
        );
        // Once the stall is absorbed, every frame is back exactly on its grid point.
        if k >= 3_002 {
            assert_eq!(*at, due, "frame {k} drifted off the grid");
        }
    }
    assert_eq!(
        clock.due_sleeps.load(Ordering::Relaxed),
        0,
        "a due frame touched the timer"
    );
    // One pinned Sleep for the pacer (re-armed with reset for every later wait) plus one for the
    // sink's stall: no per-frame timer allocation.
    assert_eq!(
        clock.sleeps.load(Ordering::Relaxed),
        2,
        "the pacer created more than one timer"
    );
    let stats = metrics.snapshot();
    assert_eq!(stats.frames_played, 10_000);
    assert_eq!(stats.underruns, 0, "a 40 ms stall is within max_lag");
}

#[tokio::test(start_paused = true)]
async fn an_interrupt_flushes_at_once_and_at_most_one_old_frame_reaches_the_sink() {
    let clock = Arc::new(CountingClock::default());
    let shared: SharedClock = clock.clone();
    let pacer = phased(&shared, 1);
    let phase = pacer.phase();
    let start_at = shared.now();
    let rig = start(&shared, pacer, None, 1);
    for seq in 0..10 {
        rig.send(frame(&*shared, seq, 0, 20)).await;
    }
    // Frames 0..=2 play at phase + 0, 20 and 40 ms; frame 3 is due at phase + 60 ms. The
    // interrupt lands at phase + 50 ms, mid-interval, whatever the phase is.
    shared.sleep_until(start_at + phase + ms(50)).await;
    let interrupt_at = shared.now();
    rig.send_interrupt(1).await;
    for seq in 100..104 {
        rig.send(frame(&*shared, seq, 1, 20)).await;
    }
    let log = rig.stop().await;

    let old_after = log
        .iter()
        .filter(|(at, epoch, _)| *epoch == TurnEpoch::ZERO && *at > interrupt_at)
        .count();
    assert!(
        old_after <= 1,
        "{old_after} old-epoch frames after the interrupt"
    );
    let new: Vec<_> = log
        .iter()
        .filter(|(_, epoch, _)| *epoch == TurnEpoch::new(1))
        .collect();
    assert_eq!(new.len(), 4, "every frame of the new turn plays");
    assert_eq!(
        new[0].0, interrupt_at,
        "the next turn must start at the interrupt, not on the old grid"
    );
    let played_old: Vec<u64> = log
        .iter()
        .filter(|(_, e, _)| *e == TurnEpoch::ZERO)
        .map(|(_, _, seq)| *seq)
        .collect();
    assert_eq!(
        played_old,
        vec![0, 1, 2],
        "exactly frames 0..=2 played, frame 3 was flushed"
    );
    assert_eq!(clock.due_sleeps.load(Ordering::Relaxed), 0);
}

#[tokio::test(start_paused = true)]
async fn a_frame_already_dispatched_when_the_interrupt_lands_is_the_only_one_left() {
    let clock = Arc::new(CountingClock::default());
    let shared: SharedClock = clock.clone();
    let pacer = phased(&shared, 3);
    let phase = pacer.phase();
    let start_at = shared.now();
    // The sink stalls for 30 ms on frame 2 (recorded at phase + 40 ms, busy until phase + 70 ms).
    // Frame 3 is due at phase + 60 ms and is dispatched into the sink's one-frame inbox, so it
    // is already handed downstream when the interrupt lands.
    let rig = start(&shared, pacer, Some((2, ms(30))), 1);
    for seq in 0..6 {
        rig.send(frame(&*shared, seq, 0, 20)).await;
    }
    // Interrupt at phase + 65 ms: frame 3 sits in the sink's inbox, frame 4 (due at phase + 80
    // ms) waits on the timer and is abandoned.
    shared.sleep_until(start_at + phase + ms(65)).await;
    let interrupt_at = shared.now();
    rig.send_interrupt(1).await;
    rig.send(frame(&*shared, 100, 1, 20)).await;
    let log = rig.stop().await;

    let old_after: Vec<u64> = log
        .iter()
        .filter(|(at, epoch, _)| *epoch == TurnEpoch::ZERO && *at > interrupt_at)
        .map(|(_, _, seq)| *seq)
        .collect();
    assert_eq!(
        old_after,
        vec![3],
        "exactly the dispatched frame 3 may play after the interrupt"
    );
    // The whole sequence is exact: 0..=2 on the grid, the dispatched 3, then the new turn.
    let order: Vec<(TurnEpoch, u64)> = log.iter().map(|(_, e, s)| (*e, *s)).collect();
    assert_eq!(
        order,
        vec![
            (TurnEpoch::ZERO, 0),
            (TurnEpoch::ZERO, 1),
            (TurnEpoch::ZERO, 2),
            (TurnEpoch::ZERO, 3),
            (TurnEpoch::new(1), 100),
        ]
    );
    assert!(
        log.iter().all(|(_, _, seq)| *seq != 4 && *seq != 5),
        "frames doomed by the interrupt were released"
    );
    assert_eq!(clock.due_sleeps.load(Ordering::Relaxed), 0);
}

#[tokio::test(start_paused = true)]
async fn a_frame_blocked_downstream_and_doomed_by_an_interrupt_is_neither_played_nor_counted() {
    let clock = Arc::new(CountingClock::default());
    let shared: SharedClock = clock.clone();
    let pacer = phased(&shared, 3);
    let phase = pacer.phase();
    let start_at = shared.now();
    // Sink stalls 100 ms on frame 2 (busy until phase + 140 ms). Frame 3 fills its one slot at
    // phase + 60 ms; frame 4 is due at phase + 80 ms and blocks inside `forward`.
    let rig = start(&shared, pacer, Some((2, ms(100))), 1);
    let metrics = rig.metrics.clone();
    for seq in 0..6 {
        rig.send(frame(&*shared, seq, 0, 20)).await;
    }
    // Interrupt at phase + 85 ms, while frame 4 is blocked in the push. The push is abandoned
    // and frame 4 is dropped by the framework.
    shared.sleep_until(start_at + phase + ms(85)).await;
    rig.send_interrupt(1).await;
    rig.send(frame(&*shared, 100, 1, 20)).await;
    let log = rig.stop().await;

    let order: Vec<(TurnEpoch, u64)> = log.iter().map(|(_, e, s)| (*e, *s)).collect();
    assert_eq!(
        order,
        vec![
            (TurnEpoch::ZERO, 0),
            (TurnEpoch::ZERO, 1),
            (TurnEpoch::ZERO, 2),
            (TurnEpoch::ZERO, 3),
            (TurnEpoch::new(1), 100),
        ],
        "the blocked frame 4 (and the queued frame 5) must not reach the sink"
    );
    // Frames 0..=3 and the new turn's frame were delivered; frame 4 was not, so it is not
    // counted (counting it before the push was the bug this guards).
    assert_eq!(metrics.snapshot().frames_played, 5);
    assert_eq!(metrics.snapshot().underruns, 0);
    assert_eq!(
        clock.due_sleeps.load(Ordering::Relaxed),
        0,
        "a due deadline touched the timer"
    );
}

#[tokio::test(start_paused = true)]
async fn a_due_frame_doomed_by_a_queued_interrupt_is_neither_released_nor_counted() {
    let clock = Arc::new(CountingClock::default());
    let shared: SharedClock = clock.clone();
    let pacer = phased(&shared, 3);
    let phase = pacer.phase();
    let start_at = shared.now();
    let rig = start(&shared, pacer, None, 4);
    let metrics = rig.metrics.clone();
    rig.send(frame(&*shared, 0, 0, 20)).await;
    // Frame 1 is due at phase + 20 ms. It arrives at phase + 50 ms, so the pacer takes it
    // already overdue (lag 30 ms, within max_lag): the due path, which must not touch the timer.
    shared.sleep_until(start_at + phase + ms(50)).await;
    // Built before the hook is armed, since `frame` reads the clock.
    let doomed = frame(&*shared, 1, 0, 20);
    // The pacer's first `now()` for this frame runs the hook: an interrupt is queued into the
    // pacer's inbox at exactly that point, so the frame is doomed while it is being handled.
    let injector = rig.input.clone();
    *clock.on_next_now.lock().unwrap() = Some(Box::new(move || {
        injector
            .try_send(
                SystemFrame::Interrupt {
                    new_epoch: TurnEpoch::new(1),
                }
                .into(),
            )
            .unwrap();
    }));
    rig.input.send(doomed.into()).await.unwrap();
    let log = rig.stop().await;

    let seqs: Vec<u64> = log.iter().map(|(_, _, seq)| *seq).collect();
    assert_eq!(seqs, vec![0], "the doomed due frame was released");
    assert_eq!(
        metrics.snapshot().frames_played,
        1,
        "the doomed due frame was counted"
    );
    assert_eq!(
        clock.due_sleeps.load(Ordering::Relaxed),
        0,
        "a due frame touched the timer"
    );
}

#[tokio::test(start_paused = true)]
async fn phase_is_seeded_per_session_and_reproducible() {
    // Pure function: in range, reproducible, and spread over the frame.
    let mut seen = std::collections::BTreeSet::new();
    for seed in 0..256 {
        let phase = phase_offset(seed);
        assert!(
            phase < FRAME,
            "seed {seed}: phase {phase:?} outside [0, 20 ms)"
        );
        assert_eq!(phase, phase_offset(seed), "seed {seed} is not reproducible");
        seen.insert(phase);
    }
    assert!(seen.len() >= 15, "only {} distinct phases", seen.len());
    assert_ne!(phase_offset(7), phase_offset(8));

    // Co-started sessions: two pacers with different seeds run at the same time in one paused
    // runtime, and each first frame lands on its own seeded phase, so they wake in different
    // milliseconds.
    let clock = Arc::new(CountingClock::default());
    let shared: SharedClock = clock.clone();
    let begun = shared.now();
    let rig_a = start(&shared, phased(&shared, 7), None, 4);
    let rig_b = start(&shared, phased(&shared, 8), None, 4);
    rig_a.send(frame(&*shared, 0, 0, 20)).await;
    rig_b.send(frame(&*shared, 0, 0, 20)).await;
    let log_a = rig_a.stop().await;
    let log_b = rig_b.stop().await;
    let first_a = log_a[0].0 - begun;
    let first_b = log_b[0].0 - begun;
    assert_eq!(first_a, phase_offset(7));
    assert_eq!(first_b, phase_offset(8));
    assert_ne!(first_a, first_b, "different seeds woke in the same ms");

    // The same seed started again, later, lands on the same phase.
    let begun_again = shared.now();
    let rig_c = start(&shared, phased(&shared, 7), None, 4);
    rig_c.send(frame(&*shared, 0, 0, 20)).await;
    let log_c = rig_c.stop().await;
    assert_eq!(
        log_c[0].0 - begun_again,
        first_a,
        "same seed, different phase"
    );
    assert_eq!(
        clock.due_sleeps.load(Ordering::Relaxed),
        0,
        "a due deadline touched the timer"
    );
}

#[tokio::test(start_paused = true)]
async fn falling_behind_beyond_max_lag_resyncs_and_counts_an_underrun() {
    for phase_on in [false, true] {
        resync_beyond_max_lag(phase_on).await;
    }
}

async fn resync_beyond_max_lag(phase_on: bool) {
    let clock = Arc::new(CountingClock::default());
    let shared: SharedClock = clock.clone();
    // A 300 ms stall on frame 2 is far beyond max_lag (60 ms).
    let rig = start(
        &shared,
        with_phase(&shared, 9, phase_on),
        Some((2, ms(300))),
        1,
    );
    let metrics = rig.metrics.clone();
    for seq in 0..10 {
        rig.send(frame(&*shared, seq, 0, 20)).await;
    }
    let log = rig.stop().await;

    let stats = metrics.snapshot();
    assert_eq!(
        stats.underruns, 1,
        "one resync for one starved stretch (phase on: {phase_on})"
    );
    assert!(
        stats.worst_lag > DEFAULT_MAX_LAG,
        "worst lag {:?}",
        stats.worst_lag
    );
    assert_eq!(stats.frames_played, 10);
    // After the resync the schedule restarts from the clock: the rest of the turn is consecutive
    // 20 ms frames.
    for k in 6..10 {
        assert_eq!(
            log[k].0 - log[k - 1].0,
            FRAME,
            "frame {k} is off the restarted grid (phase on: {phase_on})"
        );
    }
    assert_eq!(
        clock.due_sleeps.load(Ordering::Relaxed),
        0,
        "a due deadline touched the timer"
    );
}

#[tokio::test(start_paused = true)]
async fn a_sink_stall_within_max_lag_is_caught_up_and_the_grid_is_reacquired() {
    let clock = Arc::new(CountingClock::default());
    let shared: SharedClock = clock.clone();
    let pacer = phased(&shared, 9).with_max_lag(ms(200));
    let phase = pacer.phase();
    let start_at = shared.now();
    // The sink stalls 100 ms on frame 2 (recorded at phase + 40 ms, busy until phase + 140 ms).
    // Frames 3..=7 are then released together at phase + 140 ms (each caught up, lag <= 200
    // ms), and the grid is re-acquired from frame 8 on. Had the schedule restarted from the
    // actual release instant, frame 5 would be due at phase + 160 ms and the grid would be lost.
    let rig = start(&shared, pacer, Some((2, ms(100))), 1);
    let metrics = rig.metrics.clone();
    for seq in 0..10 {
        rig.send(frame(&*shared, seq, 0, 20)).await;
    }
    let log = rig.stop().await;

    assert_eq!(metrics.snapshot().underruns, 0);
    assert_eq!(log.len(), 10);
    let first = log[0].0;
    assert_eq!(first - start_at, phase);
    for (k, (at, _, _)) in log.iter().enumerate().take(8).skip(3) {
        assert_eq!(
            *at,
            first + ms(140),
            "frame {k} is not caught up at the stall's end"
        );
    }
    for (k, (at, _, _)) in log.iter().enumerate().skip(8) {
        assert_eq!(*at, grid(first, k as u64), "frame {k} is off the grid");
    }
    assert_eq!(
        clock.due_sleeps.load(Ordering::Relaxed),
        0,
        "a due deadline touched the timer"
    );
}

#[tokio::test(start_paused = true)]
async fn a_late_frame_is_caught_up_and_the_grid_is_kept_when_the_pacer_itself_is_late() {
    let clock = Arc::new(CountingClock::default());
    let shared: SharedClock = clock.clone();
    let pacer = phased(&shared, 4);
    let phase = pacer.phase();
    let start_at = shared.now();
    let rig = start(&shared, pacer, None, 4);
    let metrics = rig.metrics.clone();
    for seq in 0..3 {
        rig.send(frame(&*shared, seq, 0, 20)).await;
    }
    // The source stalls: frames 3.. only arrive at phase + 110 ms. Frame 3 was due at phase + 60
    // ms (lag 50 ms), frame 4 at phase + 80 ms (lag 30), frame 5 at phase + 100 ms (lag 10):
    // each is released at once, and frame 6 (due at phase + 120 ms) is back on the grid.
    shared.sleep_until(start_at + phase + ms(110)).await;
    for seq in 3..10 {
        rig.send(frame(&*shared, seq, 0, 20)).await;
    }
    let log = rig.stop().await;

    assert_eq!(log.len(), 10);
    let first = log[0].0;
    assert_eq!(first - start_at, phase);
    for (k, (at, _, _)) in log.iter().enumerate().take(6).skip(3) {
        assert_eq!(
            *at,
            first + ms(110),
            "late frame {k} was not caught up at once"
        );
    }
    for (k, (at, _, _)) in log.iter().enumerate().skip(6) {
        assert_eq!(
            *at,
            grid(first, k as u64),
            "frame {k} drifted off the grid after the catch-up"
        );
    }
    let stats = metrics.snapshot();
    assert_eq!(stats.underruns, 0, "lag 50 ms is within max_lag");
    assert_eq!(stats.worst_lag, ms(50));
    assert_eq!(clock.due_sleeps.load(Ordering::Relaxed), 0);
}

#[tokio::test(start_paused = true)]
async fn a_turn_end_waits_for_playout_and_the_next_turn_is_not_an_underrun() {
    for phase_on in [false, true] {
        turn_end_waits_for_playout(phase_on).await;
    }
}

async fn turn_end_waits_for_playout(phase_on: bool) {
    let clock = Arc::new(CountingClock::default());
    let shared: SharedClock = clock.clone();
    let pacer = with_phase(&shared, 4, phase_on);
    let phase = first_offset(&pacer, phase_on);
    let start_at = shared.now();
    let rig = start(&shared, pacer, None, 4);
    let metrics = rig.metrics.clone();
    let ends = rig.ends.clone();
    for seq in 0..3 {
        rig.send(frame(&*shared, seq, 0, 20)).await;
    }
    rig.send_tts_ended(0).await;
    // The next turn only starts five seconds later, after a natural silence (no interrupt).
    shared.sleep_until(start_at + Duration::from_secs(5)).await;
    let next_turn_at = shared.now();
    rig.send(frame(&*shared, 3, 1, 20)).await;
    let log = rig.stop().await;

    // Turn 1 plays at phase + 0, 20, 40 ms; its end is held until the last frame has played out
    // (phase + 60 ms), not passed on at once.
    let first = log[0].0;
    assert_eq!(first - start_at, phase, "phase on: {phase_on}");
    let ended = ends.lock().unwrap().clone();
    assert_eq!(
        ended,
        vec![(grid(first, 3), TurnEpoch::ZERO)],
        "phase on: {phase_on}"
    );
    // The next turn starts at its arrival, and the silence is not counted as starvation.
    assert_eq!(
        log[3].0, next_turn_at,
        "the next turn did not start at once (phase on: {phase_on})"
    );
    let stats = metrics.snapshot();
    assert_eq!(
        stats.underruns, 0,
        "a natural turn gap counted as an underrun"
    );
    assert_eq!(stats.frames_played, 4);
    assert_eq!(
        clock.due_sleeps.load(Ordering::Relaxed),
        0,
        "a due deadline touched the timer"
    );
}

#[tokio::test(start_paused = true)]
async fn non_20_ms_frames_are_paced_by_their_own_duration() {
    let clock = Arc::new(CountingClock::default());
    let shared: SharedClock = clock.clone();
    let pacer = phased(&shared, 11);
    let phase = pacer.phase();
    let start_at = shared.now();
    let rig = start(&shared, pacer, None, 4);
    let durations = [10_u64, 10, 40, 10, 20, 40];
    for (seq, ms) in durations.iter().enumerate() {
        rig.send(frame(&*shared, seq as u64, 0, *ms)).await;
    }
    let log = rig.stop().await;

    assert_eq!(log.len(), durations.len());
    assert_eq!(log[0].0 - start_at, phase);
    let mut expected = log[0].0;
    for (k, ms) in durations.iter().enumerate() {
        assert_eq!(
            log[k].0, expected,
            "frame {k} ({ms} ms) is off its schedule"
        );
        expected += Duration::from_millis(*ms);
    }
    assert_eq!(clock.due_sleeps.load(Ordering::Relaxed), 0);
}

#[tokio::test(start_paused = true)]
async fn metrics_handle_outlives_the_pacer_task() {
    let clock = Arc::new(CountingClock::default());
    let shared: SharedClock = clock.clone();
    let rig = start(&shared, phased(&shared, 2), None, 4);
    let metrics = rig.metrics.clone();
    rig.send(frame(&*shared, 0, 0, 20)).await;
    rig.send(frame(&*shared, 1, 0, 20)).await;
    let _ = rig.stop().await;
    let stats = metrics.snapshot();
    assert_eq!(stats.frames_played, 2);
    assert_eq!(stats.underruns, 0);
    assert_eq!(
        clock.due_sleeps.load(Ordering::Relaxed),
        0,
        "a due deadline touched the timer"
    );
}

#[tokio::test(start_paused = true)]
async fn a_late_timer_wake_is_recorded_as_lag_when_the_frame_arrived_early() {
    // Phase zero, so frame 0 is due at once and frame 1 is the first to wait on the timer. The
    // timer fires 15 ms after its deadline (a Windows tick). Every frame is queued up front, so
    // each one reaches the pacer before its deadline. The lateness must still count: worst_lag
    // is the lateness at hand-off, not the lag at arrival (which is negative here).
    let seed = (0_u64..)
        .find(|seed| phase_offset(*seed).is_zero())
        .unwrap();
    let clock = Arc::new(CountingClock {
        late: ms(15),
        ..CountingClock::default()
    });
    let shared: SharedClock = clock.clone();
    let pacer = phased(&shared, seed);
    assert!(pacer.phase().is_zero());
    let start_at = shared.now();
    let rig = start(&shared, pacer, None, 4);
    let metrics = rig.metrics.clone();
    for seq in 0..4 {
        rig.send(frame(&*shared, seq, 0, 20)).await;
    }
    let log = rig.stop().await;

    assert_eq!(log.len(), 4);
    assert_eq!(log[0].0, start_at, "frame 0 is due at once");
    // Frame 1 wakes 15 ms late; the pacer then re-arms its timer on the grid, so frames 2 and 3
    // are back on their grid points (the schedule is absolute, not restarted from the wake).
    assert_eq!(log[1].0, start_at + ms(35), "frame 1 did not wake late");
    assert_eq!(log[2].0, start_at + ms(40));
    assert_eq!(log[3].0, start_at + ms(60));
    let stats = metrics.snapshot();
    assert_eq!(stats.worst_lag, ms(15), "the late wake was not recorded");
    assert_eq!(stats.underruns, 0, "a 15 ms wake is within max_lag");
    assert_eq!(stats.frames_played, 4);
    assert_eq!(clock.due_sleeps.load(Ordering::Relaxed), 0);
}

#[tokio::test(start_paused = true)]
async fn the_grid_is_absolute_when_the_pacer_wakes_after_the_clock_has_moved_past_its_deadline() {
    // The pacer parks on frame 1's deadline (phase + 20 ms). The test then moves the clock to
    // phase + 23 ms in one step, so the pacer wakes 3 ms late. The next frame must land on the
    // absolute grid (deadline + 20 ms), not 3 ms later (wake + 20 ms): a schedule rebuilt from
    // the wake instant would drift by a timer tick per frame on Windows.
    let clock = Arc::new(CountingClock::default());
    let shared: SharedClock = clock.clone();
    let pacer = phased(&shared, 5);
    let phase = pacer.phase();
    let start_at = shared.now();
    let rig = start(&shared, pacer, None, 4);
    let metrics = rig.metrics.clone();
    for seq in 0..3 {
        rig.send(frame(&*shared, seq, 0, 20)).await;
    }
    // Frame 0 is released at phase; by phase + 5 ms the pacer is parked on frame 1's deadline.
    shared.sleep_until(start_at + phase + ms(5)).await;
    // One step past frame 1's deadline. Tokio fires the pacer's timer at the advanced clock.
    tokio::time::advance(ms(18)).await;
    let log = rig.stop().await;

    assert_eq!(log.len(), 3);
    assert_eq!(log[0].0, start_at + phase);
    assert_eq!(
        log[1].0,
        start_at + phase + ms(23),
        "frame 1 was not released at the advanced clock"
    );
    assert_eq!(
        log[2].0,
        start_at + phase + ms(40),
        "frame 2 drifted: the schedule restarted from the wake instant"
    );
    let stats = metrics.snapshot();
    assert_eq!(
        stats.worst_lag,
        ms(3),
        "the 3 ms wake lateness was not recorded"
    );
    assert_eq!(stats.underruns, 0);
    assert_eq!(clock.due_sleeps.load(Ordering::Relaxed), 0);
}

#[tokio::test(start_paused = true)]
async fn a_turn_end_after_the_last_deadline_goes_out_at_once_without_the_timer() {
    for phase_on in [false, true] {
        late_turn_end_without_the_timer(phase_on).await;
    }
}

async fn late_turn_end_without_the_timer(phase_on: bool) {
    // The last frame plays at phase + 40 ms, so the grid point after it (the hold deadline) is
    // phase + 60 ms. The turn's end reaches the pacer at phase + 80 ms, past that deadline: the
    // hold is already due and must not touch the timer (a regression here costs a Windows tick).
    let clock = Arc::new(CountingClock::default());
    let shared: SharedClock = clock.clone();
    let pacer = with_phase(&shared, 4, phase_on);
    let phase = first_offset(&pacer, phase_on);
    let start_at = shared.now();
    let rig = start(&shared, pacer, None, 4);
    let ends = rig.ends.clone();
    for seq in 0..3 {
        rig.send(frame(&*shared, seq, 0, 20)).await;
    }
    shared.sleep_until(start_at + phase + ms(80)).await;
    rig.send_tts_ended(0).await;
    let log = rig.stop().await;

    assert_eq!(log.len(), 3);
    assert_eq!(
        ends.lock().unwrap().clone(),
        vec![(start_at + phase + ms(80), TurnEpoch::ZERO)],
        "the turn end was not forwarded at once (phase on: {phase_on})"
    );
    assert_eq!(
        clock.due_sleeps.load(Ordering::Relaxed),
        0,
        "the due turn-end hold touched the timer"
    );
}

// ---------------------------------------------------------------------------------------------
// The first frame of an answer leaves at once (the default).

/// The seeds of 256 sessions, so that no phase value can hide behind a lucky seed: with the
/// default pacer the first frame of every session leaves at its arrival, the later frames follow
/// on the 20 ms grid anchored at it, and no timer is created for the first frame.
#[tokio::test(start_paused = true)]
async fn the_first_frame_of_a_session_leaves_at_once_whatever_its_seed() {
    let clock = Arc::new(CountingClock::default());
    let shared: SharedClock = clock.clone();
    for seed in 0..256u64 {
        assert!(phase_offset(seed) < FRAME);
        let pacer = OutputPacer::new(shared.clone(), seed);
        let rig = start(&shared, pacer, None, 4);
        let arrived = shared.now();
        rig.send(frame(&*shared, 0, 0, 20)).await;
        rig.send(frame(&*shared, 1, 0, 20)).await;
        rig.send(frame(&*shared, 2, 0, 20)).await;
        let log = rig.stop().await;
        assert_eq!(log.len(), 3, "seed {seed}");
        assert_eq!(
            log[0].0, arrived,
            "seed {seed}: the first frame waited for the phase"
        );
        // Steady state is unchanged: one frame every 20 ms from the first frame.
        assert_eq!(log[1].0, grid(arrived, 1), "seed {seed}");
        assert_eq!(log[2].0, grid(arrived, 2), "seed {seed}");
    }
    assert_eq!(
        clock.due_sleeps.load(Ordering::Relaxed),
        0,
        "a due deadline touched the timer"
    );
}

/// The first frame of the first answer, when the session has been idle for a while (the call
/// started, the user spoke for two seconds, the engine answered): it is released the moment the
/// engine hands it over, not `phase` later. The same with the phase on shows the old delay.
#[tokio::test(start_paused = true)]
async fn the_first_answer_after_an_idle_start_is_not_delayed_by_the_phase() {
    for (on, seed) in [(false, 5u64), (false, 19), (true, 5)] {
        let clock = Arc::new(CountingClock::default());
        let shared: SharedClock = clock.clone();
        let pacer = OutputPacer::new(shared.clone(), seed).with_first_frame_phase(on);
        let phase = pacer.phase();
        assert!(phase > Duration::ZERO, "pick a seed with a phase");
        let rig = start(&shared, pacer, None, 4);
        let metrics = rig.metrics.clone();
        shared.sleep(Duration::from_secs(2)).await; // the caller talks, nothing to play yet
        let arrived = shared.now();
        for seq in 0..5 {
            rig.send(frame(&*shared, seq, 0, 20)).await;
        }
        let log = rig.stop().await;
        let expected = if on { phase } else { Duration::ZERO };
        assert_eq!(log[0].0 - arrived, expected, "phase on: {on}, seed {seed}");
        for (k, (at, _, _)) in log.iter().enumerate() {
            assert_eq!(*at, grid(log[0].0, u64::try_from(k).unwrap()), "frame {k}");
        }
        let stats = metrics.snapshot();
        assert_eq!(stats.underruns, 0);
        assert_eq!(stats.frames_played, 5);
        assert_eq!(stats.worst_lag, Duration::ZERO, "no phantom lag");
    }
}

/// Barge-in clears and the turn after them are unchanged: the interrupt flushes the schedule,
/// the next turn's first frame leaves at once, and a stale frame never plays.
#[tokio::test(start_paused = true)]
async fn an_interrupt_still_flushes_and_the_next_turns_first_frame_leaves_at_once() {
    let clock = Arc::new(CountingClock::default());
    let shared: SharedClock = clock.clone();
    let rig = start(&shared, OutputPacer::new(shared.clone(), 13), None, 4);
    let metrics = rig.metrics.clone();
    let begun = shared.now();
    for seq in 0..10 {
        rig.send(frame(&*shared, seq, 0, 20)).await;
    }
    // Frames 0..=2 are out at 0, 20 and 40 ms; the interrupt lands at 50 ms.
    shared.sleep_until(begun + ms(50)).await;
    rig.send_interrupt(1).await;
    shared.sleep(ms(7)).await;
    let new_turn = shared.now();
    rig.send(frame(&*shared, 10, 1, 20)).await;
    let log = rig.stop().await;

    let old: Vec<u64> = log
        .iter()
        .filter(|(_, e, _)| *e == TurnEpoch::ZERO)
        .map(|(_, _, seq)| *seq)
        .collect();
    assert!(old.len() <= 4 && old.iter().all(|seq| *seq <= 3), "{old:?}");
    let first_new = log
        .iter()
        .find(|(_, e, _)| *e == TurnEpoch::new(1))
        .unwrap();
    assert_eq!(first_new.0, new_turn, "the new turn did not start at once");
    assert_eq!(metrics.snapshot().underruns, 0);
    assert_eq!(
        clock.due_sleeps.load(Ordering::Relaxed),
        0,
        "a due deadline touched the timer"
    );
}
