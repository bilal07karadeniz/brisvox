//! Self-wake (`SystemFrame::Wake`), pipeline statistics, the upstream-push rule and the
//! `UtteranceStarted` control frame.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::*;
use crate::clock::{Clock, SimClock};
use crate::epoch::TurnEpoch;
use crate::frame::{
    AudioFrame, ControlFrame, ControlKind, DataFrame, Frame, MAX_FRAME_SIZE, MetricFrame,
    MetricKind, SampleRate, SystemFrame,
};
use crate::queue::{OverflowPolicies, QueueConfig};

const SECOND: Duration = Duration::from_secs(1);
const MS: Duration = Duration::from_millis(1);

#[allow(clippy::disallowed_methods)] // frames need some timestamp
fn audio(seq: u64, epoch: u32) -> Frame {
    AudioFrame::new(
        Bytes::new(),
        SampleRate::Hz16000,
        Instant::now(),
        seq,
        TurnEpoch::new(epoch),
    )
    .unwrap()
    .into()
}

fn control(kind: ControlKind) -> Frame {
    ControlFrame {
        epoch: TurnEpoch::ZERO,
        kind,
    }
    .into()
}

fn wake(token: u32) -> Frame {
    SystemFrame::Wake { token }.into()
}

fn label(frame: &Frame) -> String {
    match frame {
        Frame::Data(DataFrame::Audio(a)) => format!("audio{}", a.seq()),
        Frame::System(SystemFrame::Wake { token }) => format!("wake{token}"),
        other => other.name().to_owned(),
    }
}

/// Aborts its task when dropped (the engine's `TaskGuard`, which core cannot depend on).
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

type Log = Arc<Mutex<Vec<String>>>;

fn snapshot(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

// ---------------------------------------------------------------------------------------------
// Wake

/// On `Start` spawns a helper that wakes this processor after `delay`; records every wake with
/// the simulated time it was handled at.
struct Sleeper {
    clock: SimClock,
    delay: Duration,
    log: Log,
    helper: Option<AbortOnDrop>,
}

impl Processor for Sleeper {
    fn name(&self) -> &'static str {
        "sleeper"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        match &frame {
            Frame::System(SystemFrame::Start) => {
                let me = ctx.self_sender().expect("attached");
                let (clock, delay) = (self.clock, self.delay);
                self.helper = Some(AbortOnDrop(tokio::spawn(async move {
                    clock.sleep(delay).await;
                    me.send(wake(7)).await.unwrap();
                })));
            }
            Frame::System(SystemFrame::Wake { token }) => {
                self.log.lock().unwrap().push(format!(
                    "wake{token}@{}ms",
                    self.clock.elapsed().as_millis()
                ));
                return Ok(());
            }
            _ => {}
        }
        ctx.forward(frame, dir).await
    }
}

#[tokio::test(start_paused = true)]
async fn helper_task_wakes_a_waiting_processor_at_the_exact_time() {
    let clock = SimClock::new();
    let log = Log::default();
    let pipeline = Pipeline::new("p").then(Sleeper {
        clock,
        delay: Duration::from_millis(100),
        log: Arc::clone(&log),
        helper: None,
    });
    let mut running = pipeline
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    clock.sleep(Duration::from_millis(99)).await;
    assert!(snapshot(&log).is_empty(), "woke too early");
    clock.sleep(Duration::from_millis(5)).await;
    assert_eq!(snapshot(&log), ["wake7@100ms"]);
    running.stop(SECOND).await.unwrap();
}

/// Blocks on the first audio frame until released; stores its self sender at `Start`.
struct Gate {
    log: Log,
    release: Arc<Notify>,
    me: Arc<Mutex<Option<FrameSender>>>,
}

impl Processor for Gate {
    fn name(&self) -> &'static str {
        "gate"
    }

    async fn process(
        &mut self,
        frame: Frame,
        _dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        if matches!(frame, Frame::System(SystemFrame::Start)) {
            *self.me.lock().unwrap() = ctx.self_sender();
        }
        let name = label(&frame);
        if name == "audio0" {
            self.release.notified().await;
        }
        self.log.lock().unwrap().push(name);
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn wake_overtakes_queued_data() {
    let log = Log::default();
    let release = Arc::new(Notify::new());
    let me = Arc::new(Mutex::new(None));
    let gate = Gate {
        log: Arc::clone(&log),
        release: Arc::clone(&release),
        me: Arc::clone(&me),
    };
    let mut running = Pipeline::new("p")
        .then(gate)
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    for seq in 0..4 {
        running.input().send(audio(seq, 0)).await.unwrap();
    }
    tokio::task::yield_now().await;
    // audio0 is being held; audio1..3 are queued. The wake is sent last.
    let me = me.lock().unwrap().clone().expect("self sender stored");
    me.try_send(wake(9)).unwrap();
    release.notify_one();
    running.stop(SECOND).await.unwrap();
    assert_eq!(
        snapshot(&log),
        [
            "start", "audio0", "wake9", "audio1", "audio2", "audio3", "stop"
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn wake_is_never_forwarded_to_a_neighbour() {
    /// Forwards everything it gets, wake included (a stage that forgot to special-case it).
    struct Forwarder;
    impl Processor for Forwarder {
        fn name(&self) -> &'static str {
            "forwarder"
        }
        async fn process(
            &mut self,
            frame: Frame,
            dir: Direction,
            ctx: &mut ProcessorContext,
        ) -> Result<()> {
            ctx.forward(frame, dir).await
        }
    }
    struct Recorder(Log);
    impl Processor for Recorder {
        fn name(&self) -> &'static str {
            "recorder"
        }
        async fn process(
            &mut self,
            frame: Frame,
            _dir: Direction,
            _ctx: &mut ProcessorContext,
        ) -> Result<()> {
            self.0.lock().unwrap().push(label(&frame));
            Ok(())
        }
    }
    let log = Log::default();
    let mut running = Pipeline::new("p")
        .then(Forwarder)
        .then(Recorder(Arc::clone(&log)))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    running.input().send(wake(1)).await.unwrap();
    running.input().send(audio(0, 0)).await.unwrap();
    running.stop(SECOND).await.unwrap();
    assert_eq!(snapshot(&log), ["start", "audio0", "stop"]);
}

#[test]
fn wake_is_never_purged_and_frame_stays_small() {
    let mut inbox = Inbox::new(QueueConfig::DEFAULT);
    let tx = inbox.sender(Direction::Downstream);
    tx.try_send(audio(0, 0)).unwrap();
    tx.try_send(wake(3)).unwrap();
    assert!(!wake(3).is_stale(TurnEpoch::new(99)));
    assert_eq!(inbox.purge_stale(TurnEpoch::new(5)), 1);
    let (frame, _) = inbox.try_recv().unwrap();
    assert_eq!(frame, wake(3));
    assert!(std::mem::size_of::<Frame>() <= MAX_FRAME_SIZE);
}

/// Spawns one ticking helper per `Start` that wakes the processor forever; counts ticks.
/// With `ignore_send_errors` the helper never notices that the pipeline is gone (it keeps
/// ticking into a closed inbox), so only an abort can end it.
struct Ticker {
    ticks: Arc<AtomicU64>,
    clock: SimClock,
    ignore_send_errors: bool,
    helper: Option<AbortOnDrop>,
}

impl Processor for Ticker {
    fn name(&self) -> &'static str {
        "ticker"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        match &frame {
            Frame::System(SystemFrame::Start) => {
                let me = ctx.self_sender().expect("attached");
                let (clock, ignore) = (self.clock, self.ignore_send_errors);
                self.helper = Some(AbortOnDrop(tokio::spawn(async move {
                    loop {
                        clock.sleep(MS).await;
                        if me.send(wake(1)).await.is_err() && !ignore {
                            return;
                        }
                    }
                })));
            }
            Frame::System(SystemFrame::Wake { .. }) => {
                self.ticks.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            _ => {}
        }
        ctx.forward(frame, dir).await
    }
}

#[tokio::test(start_paused = true)]
async fn helper_tasks_abort_when_the_pipeline_is_dropped() {
    let clock = SimClock::new();
    let baseline = Handle::current().metrics().num_alive_tasks();
    let ticks = Arc::new(AtomicU64::new(0));
    let running = Pipeline::new("p")
        .then(Ticker {
            ticks: Arc::clone(&ticks),
            clock,
            ignore_send_errors: false,
            helper: None,
        })
        // This helper survives a closed inbox: only the abort on drop can end it.
        .then(Ticker {
            ticks: Arc::clone(&ticks),
            clock,
            ignore_send_errors: true,
            helper: None,
        })
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    clock.sleep(Duration::from_millis(10)).await;
    // Two processor tasks and two helpers.
    assert_eq!(Handle::current().metrics().num_alive_tasks(), baseline + 4);
    let before = ticks.load(Ordering::Relaxed);
    assert!(
        before >= 16,
        "both tickers should have ticked, got {before}"
    );

    drop(running);
    clock.sleep(Duration::from_millis(5)).await;
    assert_eq!(
        Handle::current().metrics().num_alive_tasks(),
        baseline,
        "helper tasks leaked after the pipeline was dropped"
    );
    let after = ticks.load(Ordering::Relaxed);
    clock.sleep(Duration::from_millis(20)).await;
    assert_eq!(ticks.load(Ordering::Relaxed), after, "ticks after drop");
}

// ---------------------------------------------------------------------------------------------
// Stats

/// Never finishes processing an audio frame; forwards the rest.
struct Stuck;

impl Processor for Stuck {
    fn name(&self) -> &'static str {
        "stuck"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        if label(&frame).starts_with("audio") {
            std::future::pending::<()>().await;
        }
        ctx.forward(frame, dir).await
    }
}

/// Answers every audio frame with an audio frame of epoch 0 (stale once the epoch moved on).
struct Emitter;

impl Processor for Emitter {
    fn name(&self) -> &'static str {
        "emitter"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        if matches!(frame, Frame::Data(DataFrame::Audio(_))) {
            return ctx.push(audio(100, 0), Direction::Downstream).await;
        }
        ctx.forward(frame, dir).await
    }
}

#[tokio::test(start_paused = true)]
async fn stats_count_overflow_policy_drops_and_stale_drops() {
    let two_slots = QueueConfig {
        system_capacity: 8,
        data_capacity: 2,
        policies: OverflowPolicies::REALTIME_INPUT,
    };
    let running = Pipeline::new("p")
        .with_queue_config(two_slots)
        .then(Stuck)
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    let metric = || -> Frame {
        DataFrame::Metric(MetricFrame {
            epoch: TurnEpoch::ZERO,
            stage: "x",
            kind: MetricKind::ProcessingTime,
            value: MS,
        })
        .into()
    };
    // audio0 is taken by the stuck task; audio1/2 fill the lane; audio3/4 each evict the oldest
    // queued audio (DropOldest); the metric meets a full lane (DropNewest).
    running.input().send(audio(0, 0)).await.unwrap();
    tokio::task::yield_now().await;
    for seq in 1..=4 {
        running.input().send(audio(seq, 0)).await.unwrap();
    }
    running.input().send(metric()).await.unwrap();
    let stats = running.stats();
    let stuck = stats.stage("stuck").unwrap();
    assert_eq!(stuck.queue.dropped_oldest, 2);
    assert_eq!(stuck.queue.dropped_newest, 1);
    assert_eq!(stuck.queued, 2);
    assert_eq!(stats.total_dropped(), 3);
    assert_eq!(stuck.stale_dropped, 0);
    drop(running);

    // Stale drops: the emitter pushes epoch-0 audio after the interrupt moved it to epoch 2.
    let mut running = Pipeline::new("q")
        .then(Emitter)
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    running
        .input()
        .send(
            SystemFrame::Interrupt {
                new_epoch: TurnEpoch::new(2),
            }
            .into(),
        )
        .await
        .unwrap();
    running.input().send(audio(0, 2)).await.unwrap();
    running.input().send(audio(1, 2)).await.unwrap();
    running.stop(SECOND).await.unwrap();
    let stats = running.stats();
    assert_eq!(stats.stage("emitter").unwrap().stale_dropped, 2);
    assert_eq!(stats.total_stale_dropped(), 2);
    assert_eq!(stats.stage("emitter").unwrap().queued, 0);
    assert!(stats.stage("nope").is_none());
}

// ---------------------------------------------------------------------------------------------
// Upstream pushes (the rule documented in processor.rs)

const FRAMES: u64 = 300;
/// Simulated time for in-flight upstream frames to drain once the tail is done.
const SETTLE: Duration = Duration::from_millis(50);

fn tight_queues() -> QueueConfig {
    QueueConfig {
        system_capacity: 2,
        data_capacity: 1,
        policies: OverflowPolicies::DEFAULT,
    }
}

/// Which way the middle and last stage push upstream frames.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UpstreamPush {
    /// The documented rule: non-blocking `try_send`, a full lane counts as a drop.
    TrySend,
    /// The forbidden pattern: an awaited push from inside `process`.
    Awaited,
}

#[derive(Clone, Default)]
struct Counts {
    delivered: Arc<AtomicU64>,
    /// Upstream pushes (any stage) that found a full lane and were shed.
    upstream_dropped: Arc<AtomicU64>,
    /// Upstream frames the tail originated or passed on (the first hop of every upstream frame).
    tail_attempts: Arc<AtomicU64>,
    /// Upstream frames that reached the head, the end of the chain.
    head_received: Arc<AtomicU64>,
}

/// First stage: forwards downstream and consumes (and counts) upstream frames.
struct Head(Counts);

impl Processor for Head {
    fn name(&self) -> &'static str {
        "head"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        if dir == Direction::Upstream {
            self.0.head_received.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        ctx.forward(frame, dir).await
    }
}

/// Pushes `frame` upstream the way `mode` says.
async fn push_up(
    mode: UpstreamPush,
    counts: &Counts,
    frame: Frame,
    ctx: &mut ProcessorContext,
) -> Result<()> {
    match mode {
        UpstreamPush::Awaited => ctx.push(frame, Direction::Upstream).await,
        UpstreamPush::TrySend => {
            if let Some(up) = ctx.sender(Direction::Upstream)
                && up.try_send(frame).is_err()
            {
                counts.upstream_dropped.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }
    }
}

/// Middle stage: forwards downstream (awaited) and upstream control frames per `mode`.
struct Middle {
    mode: UpstreamPush,
    counts: Counts,
}

impl Processor for Middle {
    fn name(&self) -> &'static str {
        "middle"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        if dir == Direction::Upstream && matches!(frame, Frame::Control(_)) {
            return push_up(self.mode, &self.counts, frame, ctx).await;
        }
        ctx.forward(frame, dir).await
    }
}

/// Last stage: slow (1 ms per audio frame); answers each with an upstream control frame and
/// passes upstream control frames from outside (the pump) on.
struct SlowTail {
    mode: UpstreamPush,
    clock: SimClock,
    counts: Counts,
}

impl Processor for SlowTail {
    fn name(&self) -> &'static str {
        "tail"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        match (&frame, dir) {
            (Frame::Data(DataFrame::Audio(_)), Direction::Downstream) => {
                self.clock.sleep(MS).await;
                self.counts.delivered.fetch_add(1, Ordering::Relaxed);
                self.counts.tail_attempts.fetch_add(1, Ordering::Relaxed);
                let ended = control(ControlKind::TtsEnded);
                push_up(self.mode, &self.counts, ended, ctx).await
            }
            (Frame::Control(_), Direction::Upstream) => {
                self.counts.tail_attempts.fetch_add(1, Ordering::Relaxed);
                push_up(self.mode, &self.counts, frame, ctx).await
            }
            _ => Ok(()),
        }
    }
}

/// Runs the three-stage chain with `FRAMES` audio frames; with `helper`, a task outside the
/// pipeline (the pump) also sends upstream control frames into the tail, awaiting (which is
/// allowed outside `process`).
async fn run_chain(mode: UpstreamPush, helper: bool) -> (Result<()>, Counts) {
    let clock = SimClock::new();
    let counts = Counts::default();
    let mut running = Pipeline::new("chain")
        .with_queue_config(tight_queues())
        .then(Head(counts.clone()))
        .then(Middle {
            mode,
            counts: counts.clone(),
        })
        .then(SlowTail {
            mode,
            clock,
            counts: counts.clone(),
        })
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    let helper_task = helper.then(|| {
        // "Upstream from a helper task": the pump-style sender awaits freely.
        let up = running.upstream_input().clone();
        AbortOnDrop(tokio::spawn(async move {
            for _ in 0..FRAMES {
                if up.send(control(ControlKind::TtsStarted)).await.is_err() {
                    return;
                }
                clock.sleep(MS).await;
            }
        }))
    });
    let input = running.input().clone();
    let feeder = AbortOnDrop(tokio::spawn(async move {
        for seq in 0..FRAMES {
            if input.send(audio(seq, 0)).await.is_err() {
                return;
            }
        }
    }));
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        // Wait until the tail has handled everything, then stop gracefully.
        while counts.delivered.load(Ordering::Relaxed) < FRAMES {
            clock.sleep(MS).await;
        }
        // Let the upstream frames still in flight reach the head before counting them.
        clock.sleep(SETTLE).await;
        running.stop(Duration::from_secs(5)).await
    })
    .await
    .unwrap_or(Err(Error::Timeout {
        what: "chain",
        after: Duration::from_secs(30),
    }));
    drop((helper_task, feeder));
    (result, counts)
}

#[tokio::test(start_paused = true)]
async fn bidirectional_chain_with_slow_consumer_never_deadlocks() {
    for helper in [false, true] {
        let (result, counts) = run_chain(UpstreamPush::TrySend, helper).await;
        result.unwrap();
        assert_eq!(counts.delivered.load(Ordering::Relaxed), FRAMES);
        // Upstream notifications may be shed under pressure (counted), never waited for. Every
        // upstream frame is either shed on some hop or reaches the head, and the head does
        // see some of them (a sender that is always "full" would shed all of them).
        let attempts = counts.tail_attempts.load(Ordering::Relaxed);
        let received = counts.head_received.load(Ordering::Relaxed);
        let dropped = counts.upstream_dropped.load(Ordering::Relaxed);
        assert!(
            attempts >= FRAMES,
            "helper={helper}: tail attempts {attempts}"
        );
        assert!(received > 0, "helper={helper}: no upstream frame arrived");
        assert_eq!(
            received + dropped,
            attempts,
            "helper={helper}: every upstream frame is delivered or counted as shed"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn awaited_upstream_push_from_process_deadlocks_the_same_chain() {
    // The control for the rule: the same chain with awaited upstream pushes inside `process`
    // wedges (the tail never gets through all frames).
    let (result, counts) = run_chain(UpstreamPush::Awaited, true).await;
    assert!(
        matches!(result, Err(Error::Timeout { .. })),
        "expected a deadlock, got {result:?}"
    );
    assert!(counts.delivered.load(Ordering::Relaxed) < FRAMES);
}

// ---------------------------------------------------------------------------------------------
// UtteranceStarted shares the data lane with audio

#[test]
fn utterance_started_is_ordered_with_audio_and_fits_the_cap() {
    assert!(std::mem::size_of::<ControlFrame>() <= 24);
    assert!(std::mem::size_of::<Frame>() <= MAX_FRAME_SIZE);
    let started = |utterance| {
        Frame::from(ControlFrame {
            epoch: TurnEpoch::new(1),
            kind: ControlKind::UtteranceStarted { utterance },
        })
    };
    assert_eq!(started(5).name(), "utterance_started");
    assert!(!started(5).is_stale(TurnEpoch::new(1)));
    assert!(started(5).is_stale(TurnEpoch::new(2)));

    let mut inbox = Inbox::new(QueueConfig {
        system_capacity: 4,
        data_capacity: 4,
        policies: OverflowPolicies::REALTIME_INPUT,
    });
    let tx = inbox.sender(Direction::Downstream);
    let sent = [
        started(1),
        audio(0, 1),
        audio(1, 1),
        started(2),
        audio(2, 1),
    ];
    // Lane holds 4: the fifth frame (audio) evicts the oldest queued audio, never the control
    // frame, and order is otherwise first-in first-out.
    for frame in sent.clone() {
        tx.try_send(frame).unwrap();
    }
    let got: Vec<String> = std::iter::from_fn(|| inbox.try_recv().ok())
        .map(|(f, _)| match &f {
            Frame::Control(ControlFrame {
                kind: ControlKind::UtteranceStarted { utterance },
                ..
            }) => format!("u{utterance}"),
            other => label(other),
        })
        .collect();
    assert_eq!(got, ["u1", "audio1", "u2", "audio2"]);
}

// ---------------------------------------------------------------------------------------------
// Inbox-close semantics and wake direction (D-A)

/// Records every frame with its direction; with `take_self_sender`, asks for a self sender at
/// `Start` and keeps it.
struct Probe {
    log: Log,
    take_self_sender: bool,
    kept: Arc<Mutex<Option<FrameSender>>>,
}

impl Processor for Probe {
    fn name(&self) -> &'static str {
        "probe"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        if matches!(frame, Frame::System(SystemFrame::Start)) && self.take_self_sender {
            *self.kept.lock().unwrap() = ctx.self_sender();
        }
        self.log
            .lock()
            .unwrap()
            .push(format!("{}/{dir:?}", label(&frame)));
        Ok(())
    }
}

type KeptSender = Arc<Mutex<Option<FrameSender>>>;

fn probe_task(take_self_sender: bool) -> (FrameSender, KeptSender, Log, JoinHandle<Result<()>>) {
    let inbox = Inbox::new(QueueConfig::DEFAULT);
    let tx = inbox.sender(Direction::Downstream);
    let log = Log::default();
    let kept = Arc::new(Mutex::new(None));
    let probe = Probe {
        log: Arc::clone(&log),
        take_self_sender,
        kept: Arc::clone(&kept),
    };
    let task = tokio::spawn(crate::processor::run_processor(
        probe,
        inbox,
        ProcessorContext::new(None, None),
        CancellationToken::new(),
    ));
    (tx, kept, log, task)
}

#[tokio::test(start_paused = true)]
async fn a_processor_that_never_asks_for_a_self_sender_still_sees_its_inbox_close() {
    let (tx, kept, log, task) = probe_task(false);
    tx.send(SystemFrame::Start.into()).await.unwrap();
    drop(tx);
    // Upstream ended: the task finishes by itself, there is no hidden sender holding it open.
    tokio::time::timeout(SECOND, task)
        .await
        .expect("inbox must close when every sender is gone")
        .unwrap()
        .unwrap();
    assert!(kept.lock().unwrap().is_none());
    assert_eq!(snapshot(&log), ["start/Downstream"]);
}

#[tokio::test(start_paused = true)]
async fn a_self_sender_keeps_the_inbox_open_until_it_is_dropped() {
    let (tx, kept, log, mut task) = probe_task(true);
    tx.send(SystemFrame::Start.into()).await.unwrap();
    drop(tx);
    SimClock::new().sleep(SECOND).await;
    assert!(
        !task.is_finished(),
        "the helper's sender holds the inbox open"
    );

    // The wake arrives downstream, like any frame sent through the self sender.
    let me = kept.lock().unwrap().take().expect("self sender kept");
    me.send(wake(5)).await.unwrap();
    SimClock::new().sleep(MS).await;
    assert_eq!(snapshot(&log), ["start/Downstream", "wake5/Downstream"]);
    assert!(!task.is_finished());

    drop(me);
    tokio::time::timeout(SECOND, &mut task)
        .await
        .expect("inbox closes once the last self sender is dropped")
        .unwrap()
        .unwrap();
}
