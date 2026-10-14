//! Pipeline composition and lifecycle tests.

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::time::Instant;

use super::*;
use crate::epoch::TurnEpoch;
use crate::frame::{AudioFrame, DataFrame, Frame, SampleRate};
use crate::queue::OverflowPolicies;

const SECOND: Duration = Duration::from_secs(1);

fn audio(seq: u64) -> Frame {
    audio_in(seq, 0)
}

#[allow(clippy::disallowed_methods)] // frames need some timestamp
fn audio_in(seq: u64, epoch: u32) -> Frame {
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

fn label(frame: &Frame) -> String {
    match frame {
        Frame::Data(DataFrame::Audio(a)) => format!("audio{}", a.seq()),
        other => other.name().to_owned(),
    }
}

/// Forwards everything.
struct Pass(&'static str);

impl Processor for Pass {
    fn name(&self) -> &'static str {
        self.0
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

/// Records what it processes (downstream), then forwards.
#[derive(Clone, Default)]
struct Collector(Arc<Mutex<Vec<String>>>);

impl Collector {
    fn seen(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

impl Processor for Collector {
    fn name(&self) -> &'static str {
        "collector"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        if dir == Direction::Downstream {
            self.0.lock().unwrap().push(label(&frame));
        }
        ctx.forward(frame, dir).await
    }
}

/// Yields between frames so queues actually fill up.
struct Slow;

impl Processor for Slow {
    fn name(&self) -> &'static str {
        "slow"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        ctx.forward(frame, dir).await
    }
}

/// Fails on audio frame `at`.
struct Failer {
    at: u64,
}

impl Processor for Failer {
    fn name(&self) -> &'static str {
        "failer"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        if label(&frame) == format!("audio{}", self.at) {
            return Err(Error::InvalidAudio("boom"));
        }
        ctx.forward(frame, dir).await
    }
}

/// Panics on the first audio frame.
struct Panicker;

impl Processor for Panicker {
    fn name(&self) -> &'static str {
        "panicker"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        if label(&frame).starts_with("audio") {
            panic!("processor bug");
        }
        ctx.forward(frame, dir).await
    }
}

/// Never finishes processing an audio frame.
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

/// Swallows `Stop` (a buggy processor), forwards everything else.
struct Swallow;

impl Processor for Swallow {
    fn name(&self) -> &'static str {
        "swallow"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        if frame.name() == "stop" {
            return Ok(());
        }
        ctx.forward(frame, dir).await
    }
}

fn small_queues() -> QueueConfig {
    QueueConfig {
        system_capacity: 2,
        data_capacity: 2,
        policies: OverflowPolicies::DEFAULT,
    }
}

fn expected(n: u64) -> Vec<String> {
    let mut v = vec!["start".to_owned()];
    v.extend((0..n).map(|i| format!("audio{i}")));
    v.push("stop".to_owned());
    v
}

#[tokio::test(start_paused = true)]
async fn graceful_stop_drains_everything_in_order() {
    let collector = Collector::default();
    let pipeline = Pipeline::new("p")
        .with_queue_config(small_queues())
        .then(Pass("a"))
        .then(Slow)
        .then(Pass("b"))
        .then(collector.clone());
    let mut running = pipeline
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    for i in 0..200 {
        running.input().send(audio(i)).await.unwrap();
    }
    running.stop(SECOND).await.unwrap();
    assert_eq!(collector.seen(), expected(200));
}

#[tokio::test(start_paused = true)]
async fn nested_pipeline_behaves_like_flat() {
    let flat_seen = Collector::default();
    let nested_seen = Collector::default();
    let flat = Pipeline::new("flat")
        .then(Pass("a"))
        .then(Pass("b"))
        .then(Pass("c"))
        .then(flat_seen.clone());
    let nested = Pipeline::new("outer")
        .then(Pass("a"))
        .then_pipeline(
            Pipeline::new("mid")
                .then(Pass("b"))
                .then_pipeline(Pipeline::new("inner").then(Pass("c"))),
        )
        .then(nested_seen.clone());
    assert_eq!(nested.processor_count(), flat.processor_count());

    for pipeline in [flat, nested] {
        let mut running = pipeline
            .start(Links::default(), &CancellationToken::new())
            .await
            .unwrap();
        for i in 0..50 {
            running.input().send(audio(i)).await.unwrap();
        }
        running.stop(SECOND).await.unwrap();
    }
    assert_eq!(flat_seen.seen(), expected(50));
    assert_eq!(nested_seen.seen(), flat_seen.seen());
}

#[tokio::test(start_paused = true)]
async fn edge_links_receive_frames_leaving_the_pipeline() {
    let mut out = Inbox::new(QueueConfig::DEFAULT);
    let mut back = Inbox::new(QueueConfig::DEFAULT);
    let links = Links {
        downstream: Some(out.sender(Direction::Downstream)),
        upstream: Some(back.sender(Direction::Upstream)),
    };
    let mut running = Pipeline::new("p")
        .then(Pass("a"))
        .then(Pass("b"))
        .start(links, &CancellationToken::new())
        .await
        .unwrap();
    running.input().send(audio(1)).await.unwrap();
    running.upstream_input().send(audio(2)).await.unwrap();
    // Upstream frames must arrive before stopping: graceful stop finishes upstream-first, and
    // frames pushed upstream into an already-stopped processor are discarded by design.
    let (frame, dir) = back.recv().await.unwrap();
    assert_eq!(
        (label(&frame), dir),
        ("audio2".to_owned(), Direction::Upstream)
    );
    running.stop(SECOND).await.unwrap();

    let mut downstream = Vec::new();
    while let Ok((frame, dir)) = out.try_recv() {
        assert_eq!(dir, Direction::Downstream);
        downstream.push(label(&frame));
    }
    // `out` is an inbox: its system lane overtakes data, so arrival order is not observable
    // here (order through stop is covered by `graceful_stop_drains_everything_in_order`).
    downstream.sort();
    assert_eq!(downstream, ["audio1", "start", "stop"]);
}

#[tokio::test(start_paused = true)]
async fn cancel_is_immediate_even_when_blocked() {
    let parent = CancellationToken::new();
    let mut running = Pipeline::new("p")
        .then(Pass("a"))
        .then(Stuck)
        .then(Pass("b"))
        .start(Links::default(), &parent)
        .await
        .unwrap();
    running.input().send(audio(0)).await.unwrap();
    running.input().send(audio(1)).await.unwrap();
    tokio::task::yield_now().await;
    // Cancelling the parent token cancels the pipeline.
    parent.cancel();
    tokio::time::timeout(SECOND, running.wait())
        .await
        .expect("cancel hung")
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn first_error_cancels_the_rest_and_is_returned() {
    let mut running = Pipeline::new("p")
        .then(Pass("a"))
        .then(Failer { at: 3 })
        .then(Stuck)
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    for i in 0..10 {
        // Sends may fail once the pipeline is torn down; that is expected here.
        let _ = running.input().send(audio(i)).await;
    }
    let result = tokio::time::timeout(SECOND, running.wait())
        .await
        .expect("supervisor hung");
    assert_eq!(result, Err(Error::InvalidAudio("boom")));
}

#[tokio::test(start_paused = true)]
async fn panic_is_contained_and_reported() {
    let mut running = Pipeline::new("p")
        .then(Pass("a"))
        .then(Panicker)
        .then(Pass("b"))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    running.input().send(audio(0)).await.unwrap();
    let result = tokio::time::timeout(SECOND, running.wait())
        .await
        .expect("supervisor hung");
    assert_eq!(result, Err(Error::ProcessorPanicked("panicker")));
}

#[tokio::test(start_paused = true)]
async fn stop_times_out_and_cancels_when_a_processor_swallows_stop() {
    let mut running = Pipeline::new("p")
        .then(Swallow)
        .then(Pass("b"))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    let result = running.stop(SECOND).await;
    assert_eq!(
        result,
        Err(Error::Timeout {
            what: "pipeline stop",
            after: SECOND
        })
    );
    assert!(running.tasks.is_empty());
}

#[tokio::test]
async fn empty_pipeline_is_rejected() {
    let result = Pipeline::new("empty")
        .start(Links::default(), &CancellationToken::new())
        .await;
    assert!(matches!(result, Err(Error::InvalidPipeline(_))));
}

fn tiny_queues() -> QueueConfig {
    QueueConfig {
        system_capacity: 1,
        data_capacity: 1,
        policies: OverflowPolicies::DEFAULT,
    }
}

#[tokio::test(start_paused = true)]
async fn cancel_frame_drops_backlog_and_ends_every_task() {
    let collector = Collector::default();
    let mut running = Pipeline::new("p")
        .then(Slow)
        .then(collector.clone())
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    let input = running.input().clone();
    let feeder = tokio::spawn(async move {
        for i in 0..50 {
            if input.send(audio(i)).await.is_err() {
                break;
            }
        }
    });
    tokio::task::yield_now().await;
    running
        .input()
        .send(SystemFrame::Cancel.into())
        .await
        .unwrap();
    tokio::time::timeout(SECOND, running.wait())
        .await
        .expect("cancel frame did not end the pipeline")
        .unwrap();
    feeder.await.unwrap();
    let seen = collector.seen();
    assert_eq!(seen.last().map(String::as_str), Some("cancel"), "{seen:?}");
    let audio_seen = seen.iter().filter(|s| s.starts_with("audio")).count();
    assert!(audio_seen < 50, "backlog was not dropped: {audio_seen}");
}

#[tokio::test(start_paused = true)]
async fn cancel_releases_processors_blocked_on_backpressure() {
    let parent = CancellationToken::new();
    let mut running = Pipeline::new("p")
        .with_queue_config(tiny_queues())
        .then(Pass("a"))
        .then(Pass("b"))
        .then(Stuck)
        .start(Links::default(), &parent)
        .await
        .unwrap();
    let input = running.input().clone();
    let flood = tokio::spawn(async move {
        for i in 0..20 {
            if input.send(audio(i)).await.is_err() {
                break;
            }
        }
    });
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    // a and b are parked pushing into full neighbours; the flood is parked too.
    assert!(!flood.is_finished());
    running.cancel();
    tokio::time::timeout(SECOND, running.wait())
        .await
        .expect("cancel hung on backpressure")
        .unwrap();
    tokio::time::timeout(SECOND, flood)
        .await
        .expect("sender stayed blocked")
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn stop_is_bounded_even_when_the_system_lane_is_full() {
    let mut running = Pipeline::new("p")
        .with_queue_config(tiny_queues())
        .then(Stuck)
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    // Stuck parks on the audio frame; its system lane (capacity 1) gets filled.
    running.input().send(audio(0)).await.unwrap();
    tokio::task::yield_now().await;
    running
        .input()
        .send(SystemFrame::Start.into())
        .await
        .unwrap();
    let result = tokio::time::timeout(2 * SECOND, running.stop(SECOND))
        .await
        .expect("stop() itself hung");
    assert_eq!(
        result,
        Err(Error::Timeout {
            what: "pipeline stop",
            after: SECOND
        })
    );
    assert!(running.tasks.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn root_cause_error_wins_over_secondary_channel_closed() {
    for _ in 0..50 {
        let mut running = Pipeline::new("p")
            .with_queue_config(tiny_queues())
            .then(Pass("a"))
            .then(Failer { at: 0 })
            .then(Pass("b"))
            .start(Links::default(), &CancellationToken::new())
            .await
            .unwrap();
        for i in 0..5 {
            let _ = running.input().send(audio(i)).await;
        }
        let result = tokio::time::timeout(SECOND, running.wait())
            .await
            .expect("supervisor hung");
        assert_eq!(result, Err(Error::InvalidAudio("boom")));
    }
}

// ---------------------------------------------------------------------------------------------
// Parallel branches

/// On each audio frame, emits a VAD event downstream (a frame originating inside a branch).
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
        if dir == Direction::Downstream && label(&frame).starts_with("audio") {
            #[allow(clippy::disallowed_methods)] // test timestamp
            let at = Instant::now();
            ctx.push(SystemFrame::UserStartedSpeaking { at }.into(), dir)
                .await?;
        }
        ctx.forward(frame, dir).await
    }
}

/// Consumes data frames; forwards everything else.
struct Consume;

impl Processor for Consume {
    fn name(&self) -> &'static str {
        "consume"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        if frame.level() == crate::frame::FrameLevel::Data {
            return Ok(());
        }
        ctx.forward(frame, dir).await
    }
}

fn count(seen: &[String], prefix: &str) -> usize {
    seen.iter().filter(|s| s.starts_with(prefix)).count()
}

fn flush(epoch: u32) -> Frame {
    crate::frame::ControlFrame {
        epoch: TurnEpoch::new(epoch),
        kind: crate::frame::ControlKind::Flush,
    }
    .into()
}

#[tokio::test(start_paused = true)]
async fn parallel_collapses_broadcast_frames() {
    let (c0, c1, out) = (
        Collector::default(),
        Collector::default(),
        Collector::default(),
    );
    let pipeline = Pipeline::new("p")
        .then(Pass("in"))
        .then_parallel([
            Pipeline::new("b0").then(c0.clone()),
            Pipeline::new("b1").then(c1.clone()),
        ])
        .then(out.clone());
    assert_eq!(pipeline.processor_count(), 4);
    let mut running = pipeline
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    for i in 0..3 {
        running.input().send(audio(i)).await.unwrap();
    }
    running.input().send(flush(7)).await.unwrap();
    running.stop(SECOND).await.unwrap();

    for branch in [&c0, &c1] {
        assert_eq!(
            branch.seen(),
            ["start", "audio0", "audio1", "audio2", "flush", "stop"]
        );
    }
    let seen = out.seen();
    assert_eq!(seen.first().map(String::as_str), Some("start"), "{seen:?}");
    assert_eq!(seen.last().map(String::as_str), Some("stop"), "{seen:?}");
    assert_eq!(count(&seen, "start"), 1, "{seen:?}");
    assert_eq!(count(&seen, "flush"), 1, "{seen:?}");
    assert_eq!(count(&seen, "stop"), 1, "{seen:?}");
    // Data is not collapsed: both pass-through branches deliver it.
    assert_eq!(count(&seen, "audio"), 6, "{seen:?}");
}

#[tokio::test(start_paused = true)]
async fn stop_waits_for_the_slowest_branch() {
    const N: u64 = 40;
    let out = Collector::default();
    let slow = (0..5).fold(Pipeline::new("slow"), |p, _| p.then(Slow));
    let mut running = Pipeline::new("p")
        .with_queue_config(small_queues())
        .then_parallel([slow, Pipeline::new("fast").then(Pass("fast"))])
        .then(out.clone())
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    for i in 0..N {
        running.input().send(audio(i)).await.unwrap();
    }
    running.stop(SECOND).await.unwrap();
    let seen = out.seen();
    assert_eq!(seen.last().map(String::as_str), Some("stop"), "{seen:?}");
    assert_eq!(count(&seen, "audio"), usize::try_from(2 * N).unwrap());
    assert_eq!(count(&seen, "stop"), 1);
}

#[tokio::test(start_paused = true)]
async fn frames_originating_in_a_branch_pass_through() {
    let out = Collector::default();
    let mut running = Pipeline::new("p")
        .then_parallel([
            Pipeline::new("vad").then(Emitter).then(Consume),
            Pipeline::new("stt").then(Pass("stt")),
        ])
        .then(out.clone())
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    for i in 0..2 {
        running.input().send(audio(i)).await.unwrap();
    }
    running.stop(SECOND).await.unwrap();
    let seen = out.seen();
    assert_eq!(count(&seen, "user_started_speaking"), 2, "{seen:?}");
    assert_eq!(count(&seen, "audio"), 2, "{seen:?}");
    assert_eq!(count(&seen, "start"), 1, "{seen:?}");
}

#[tokio::test(start_paused = true)]
async fn upstream_copies_are_collapsed_at_the_fanout() {
    let mut back = Inbox::new(QueueConfig::DEFAULT);
    let links = Links {
        downstream: None,
        upstream: Some(back.sender(Direction::Upstream)),
    };
    let mut running = Pipeline::new("p")
        .then_parallel([
            Pipeline::new("b0").then(Pass("b0")),
            Pipeline::new("b1").then(Pass("b1")),
        ])
        .start(links, &CancellationToken::new())
        .await
        .unwrap();
    let interrupt: Frame = SystemFrame::Interrupt {
        new_epoch: TurnEpoch::new(5),
    }
    .into();
    running.upstream_input().send(interrupt).await.unwrap();
    running.upstream_input().send(audio_in(9, 5)).await.unwrap();
    let mut got = Vec::new();
    for _ in 0..3 {
        let (frame, _) = tokio::time::timeout(SECOND, back.recv())
            .await
            .expect("upstream frame missing")
            .unwrap();
        got.push(label(&frame));
    }
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(back.try_recv().is_err(), "unexpected extra upstream copy");
    got.sort();
    assert_eq!(got, ["audio9", "audio9", "interrupt"]);
    running.stop(SECOND).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn parallel_cancel_token_and_cancel_frame() {
    // Token: a branch stuck mid-frame does not block cancellation.
    let mut running = Pipeline::new("p")
        .then_parallel([
            Pipeline::new("stuck").then(Stuck),
            Pipeline::new("ok").then(Pass("ok")),
        ])
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    running.input().send(audio(0)).await.unwrap();
    tokio::task::yield_now().await;
    running.cancel();
    tokio::time::timeout(SECOND, running.wait())
        .await
        .expect("cancel hung")
        .unwrap();

    // Cancel frame: forwarded once past the merge, every task ends without errors.
    let out = Collector::default();
    let mut running = Pipeline::new("p")
        .then_parallel([
            Pipeline::new("b0").then(Pass("b0")),
            Pipeline::new("b1").then(Slow),
        ])
        .then(out.clone())
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    for i in 0..5 {
        running.input().send(audio(i)).await.unwrap();
    }
    running
        .input()
        .send(SystemFrame::Cancel.into())
        .await
        .unwrap();
    tokio::time::timeout(SECOND, running.wait())
        .await
        .expect("cancel frame hung")
        .unwrap();
    let seen = out.seen();
    assert_eq!(count(&seen, "cancel"), 1, "{seen:?}");
    assert_eq!(seen.last().map(String::as_str), Some("cancel"), "{seen:?}");
}

#[tokio::test(start_paused = true)]
async fn parallel_nested_inside_a_pipeline() {
    let out = Collector::default();
    let inner = Pipeline::new("inner").then(Pass("pre")).then_parallel([
        Pipeline::new("b0").then(Pass("b0")),
        Pipeline::new("b1").then(Consume),
        Pipeline::new("b2").then_parallel([
            Pipeline::new("b2a").then(Consume),
            Pipeline::new("b2b").then(Consume),
        ]),
    ]);
    let pipeline = Pipeline::new("outer")
        .then(Pass("a"))
        .then_pipeline(inner)
        .then(out.clone());
    assert_eq!(pipeline.processor_count(), 7);
    let mut running = pipeline
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    for i in 0..4 {
        running.input().send(audio(i)).await.unwrap();
    }
    running.stop(SECOND).await.unwrap();
    assert_eq!(out.seen(), expected(4));
}

#[tokio::test]
async fn invalid_parallel_shapes_are_rejected() {
    let no_branches = Pipeline::new("p").then_parallel(Vec::new());
    assert!(matches!(
        no_branches
            .start(Links::default(), &CancellationToken::new())
            .await,
        Err(Error::InvalidPipeline(_))
    ));
    let empty_branch = Pipeline::new("p")
        .then_parallel([Pipeline::new("empty"), Pipeline::new("ok").then(Pass("ok"))]);
    assert!(matches!(
        empty_branch
            .start(Links::default(), &CancellationToken::new())
            .await,
        Err(Error::InvalidPipeline(_))
    ));
}

/// Forwards each frame, sleeps a little real time, then emits a second copy of data frames:
/// the second push can land after a Cancel overtook it (multi-thread race tests only).
struct Burst;

impl Processor for Burst {
    fn name(&self) -> &'static str {
        "burst"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        let is_data = frame.level() == crate::frame::FrameLevel::Data;
        ctx.forward(frame.clone(), dir).await?;
        crate::clock::SystemClock
            .sleep(Duration::from_millis(2))
            .await;
        if is_data {
            ctx.push(frame, dir).await?;
        }
        Ok(())
    }
}

/// Swallows control frames (a misbehaving branch); forwards everything else.
struct SwallowControl;

impl Processor for SwallowControl {
    fn name(&self) -> &'static str {
        "swallow_control"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        if frame.level() == crate::frame::FrameLevel::Control {
            return Ok(());
        }
        ctx.forward(frame, dir).await
    }
}

use crate::clock::Clock as _;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_frame_with_a_slow_branch_ends_cleanly() {
    for _ in 0..10 {
        let out = Collector::default();
        let mut running = Pipeline::new("p")
            .then_parallel([
                Pipeline::new("fast").then(Pass("fast")),
                Pipeline::new("slow").then(Burst),
            ])
            .then(out.clone())
            .start(Links::default(), &CancellationToken::new())
            .await
            .unwrap();
        for i in 0..5 {
            running.input().send(audio(i)).await.unwrap();
        }
        crate::clock::SystemClock
            .sleep(Duration::from_millis(3))
            .await;
        running
            .input()
            .send(SystemFrame::Cancel.into())
            .await
            .unwrap();
        let result = tokio::time::timeout(SECOND, running.wait())
            .await
            .expect("cancel hung");
        assert_eq!(result, Ok(()));
        assert_eq!(count(&out.seen(), "cancel"), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_with_real_concurrency_waits_for_every_branch() {
    for _ in 0..5 {
        let out = Collector::default();
        let mut running = Pipeline::new("p")
            .then_parallel([
                Pipeline::new("fast").then(Pass("fast")),
                Pipeline::new("slow").then(Burst),
            ])
            .then(out.clone())
            .start(Links::default(), &CancellationToken::new())
            .await
            .unwrap();
        for i in 0..10 {
            running.input().send(audio(i)).await.unwrap();
        }
        running.input().send(flush(3)).await.unwrap();
        running.stop(10 * SECOND).await.unwrap();
        let seen = out.seen();
        assert_eq!(seen.last().map(String::as_str), Some("stop"), "{seen:?}");
        // fast: 10, slow (Burst emits each data frame twice): 20.
        assert_eq!(count(&seen, "audio"), 30, "{seen:?}");
        assert_eq!(count(&seen, "flush"), 1, "{seen:?}");
        assert_eq!(count(&seen, "start"), 1, "{seen:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_swallowing_branch_does_not_eat_later_broadcasts() {
    let out = Collector::default();
    let mut running = Pipeline::new("p")
        .then_parallel([
            Pipeline::new("ok").then(Pass("ok")),
            Pipeline::new("bad").then(SwallowControl),
        ])
        .then(out.clone())
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    running.input().send(flush(7)).await.unwrap();
    running.input().send(flush(7)).await.unwrap();
    running.stop(SECOND).await.unwrap();
    assert_eq!(count(&out.seen(), "flush"), 2, "{:?}", out.seen());
}

#[tokio::test(start_paused = true)]
async fn ledger_stays_bounded_with_a_swallowing_branch() {
    let out = Collector::default();
    let mut running = Pipeline::new("p")
        .then_parallel([
            Pipeline::new("ok").then(Pass("ok")),
            Pipeline::new("bad").then(SwallowControl),
        ])
        .then(out.clone())
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    for epoch in 0..600 {
        running.input().send(flush(epoch)).await.unwrap();
    }
    running.stop(SECOND).await.unwrap();
    // Every broadcast forwarded exactly once despite 600 > 256 incomplete ledger entries.
    assert_eq!(count(&out.seen(), "flush"), 600);
}

#[tokio::test(start_paused = true)]
async fn nested_parallel_blocks_collapse_control_and_interrupts_once() {
    let out = Collector::default();
    let mut running = Pipeline::new("p")
        .then_parallel([
            Pipeline::new("b0").then(Pass("b0")),
            Pipeline::new("b1").then_parallel([
                Pipeline::new("b1a").then(Consume),
                Pipeline::new("b1b").then(Consume),
            ]),
        ])
        .then(out.clone())
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
    // Frames of the new turn (older ones would now be dropped as stale).
    running.input().send(flush(2)).await.unwrap();
    running.input().send(audio_in(0, 2)).await.unwrap();
    running.stop(SECOND).await.unwrap();
    let seen = out.seen();
    assert_eq!(count(&seen, "flush"), 1, "{seen:?}");
    assert_eq!(count(&seen, "interrupt"), 1, "{seen:?}");
    assert_eq!(count(&seen, "audio"), 1, "{seen:?}");
    assert_eq!(count(&seen, "start"), 1, "{seen:?}");
    assert_eq!(seen.last().map(String::as_str), Some("stop"));
}

#[tokio::test(start_paused = true)]
async fn empty_nested_pipelines_are_skipped() {
    let out = Collector::default();
    let mut running = Pipeline::new("p")
        .then(Pass("a"))
        .then_pipeline(Pipeline::new("empty"))
        .then(out.clone())
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    running.input().send(audio(0)).await.unwrap();
    running.stop(SECOND).await.unwrap();
    assert_eq!(out.seen(), expected(1));
}

#[tokio::test]
async fn more_than_64_branches_is_rejected() {
    let branches = (0..65).map(|_| Pipeline::new("b").then(Pass("b")));
    let result = Pipeline::new("p")
        .then_parallel(branches)
        .start(Links::default(), &CancellationToken::new())
        .await;
    assert!(matches!(result, Err(Error::InvalidPipeline(_))));
}

// ---------------------------------------------------------------------------------------------
// Turn epochs and interruption

/// A transport-sink stand-in with the default epoch policy: records (label, epoch) of every
/// frame it processes.
/// (frame label, epoch) as processed by the sink.
type Record = (String, Option<u32>);

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<Record>>>);

impl Sink {
    fn records(&self) -> Vec<Record> {
        self.0.lock().unwrap().clone()
    }
}

fn epoch_of(frame: &Frame) -> Option<u32> {
    match frame {
        Frame::System(SystemFrame::Interrupt { new_epoch }) => Some(new_epoch.get()),
        other => other.epoch().map(TurnEpoch::get),
    }
}

impl Processor for Sink {
    fn name(&self) -> &'static str {
        "sink"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        if dir == Direction::Downstream {
            let name = frame.name().to_owned();
            self.0.lock().unwrap().push((name, epoch_of(&frame)));
        }
        ctx.forward(frame, dir).await
    }
}

/// Like `Collector`, but opts out of epoch filtering.
struct Recorder(Collector);

impl Processor for Recorder {
    fn name(&self) -> &'static str {
        "recorder"
    }

    fn epoch_policy(&self) -> crate::processor::EpochPolicy {
        crate::processor::EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        self.0.process(frame, dir, ctx).await
    }
}

fn interrupt(epoch: u32) -> Frame {
    SystemFrame::Interrupt {
        new_epoch: TurnEpoch::new(epoch),
    }
    .into()
}

fn tool_result(epoch: u32) -> Frame {
    DataFrame::ToolResult(Box::new(crate::frame::ToolResult {
        epoch: TurnEpoch::new(epoch),
        call_id: Arc::from("call"),
        content: Arc::from("{}"),
        is_error: false,
    }))
    .into()
}

/// Checks the interruption invariant on a sink record: once an interrupt to epoch e has been
/// processed, no epoch-tagged frame older than e is processed (tool results exempt).
fn check_no_stale_after_interrupt(records: &[Record]) -> std::result::Result<(), String> {
    let mut current = 0;
    for (name, epoch) in records {
        match (name.as_str(), epoch) {
            ("interrupt", Some(e)) => current = current.max(*e),
            ("tool_result", _) | (_, None) => {}
            (_, Some(e)) if *e < current => {
                return Err(format!("{name}@{e} processed after interrupt to {current}"));
            }
            _ => {}
        }
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn queued_stale_frames_are_not_processed_after_an_interrupt() {
    let sink = Sink::default();
    let mut running = Pipeline::new("p")
        .then(Slow)
        .then(Pass("tts"))
        .then(sink.clone())
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    for i in 0..30 {
        running.input().send(audio_in(i, 0)).await.unwrap();
    }
    running.input().send(interrupt(1)).await.unwrap();
    running.input().send(tool_result(0)).await.unwrap();
    for i in 0..5 {
        running.input().send(audio_in(100 + i, 1)).await.unwrap();
    }
    running.stop(SECOND).await.unwrap();

    let records = sink.records();
    check_no_stale_after_interrupt(&records).unwrap();
    let old = records
        .iter()
        .filter(|r| r.0 == "audio" && r.1 == Some(0))
        .count();
    let new = records
        .iter()
        .filter(|r| r.0 == "audio" && r.1 == Some(1))
        .count();
    assert!(old < 30, "the interrupt purged nothing: {records:?}");
    assert_eq!(new, 5, "{records:?}");
    assert_eq!(records.iter().filter(|r| r.0 == "tool_result").count(), 1);
    assert_eq!(records.iter().filter(|r| r.0 == "interrupt").count(), 1);
}

#[tokio::test(start_paused = true)]
async fn keep_all_processors_see_stale_frames() {
    let seen = Collector::default();
    let mut running = Pipeline::new("p")
        .then(Recorder(seen.clone()))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    running.input().send(interrupt(1)).await.unwrap();
    running.input().send(audio_in(7, 0)).await.unwrap();
    running.stop(SECOND).await.unwrap();
    assert!(
        seen.seen().contains(&"audio7".to_owned()),
        "{:?}",
        seen.seen()
    );
}

#[derive(Debug, Clone, Copy)]
enum EpochOp {
    Audio { stale: bool },
    Tool { stale: bool },
    Interrupt,
    Yield,
}

fn epoch_ops() -> impl proptest::strategy::Strategy<Value = Vec<EpochOp>> {
    use proptest::prelude::*;
    prop::collection::vec(
        prop_oneof![
            6 => any::<bool>().prop_map(|stale| EpochOp::Audio { stale }),
            1 => any::<bool>().prop_map(|stale| EpochOp::Tool { stale }),
            2 => Just(EpochOp::Interrupt),
            2 => Just(EpochOp::Yield),
        ],
        1..80,
    )
}

/// Drives one random scenario; returns a description of the first violation, if any.
fn run_epoch_scenario(ops: &[EpochOp]) -> std::result::Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        let sink = Sink::default();
        let mut running = Pipeline::new("p")
            .with_queue_config(small_queues())
            .then(Pass("a"))
            .then(Slow)
            .then(Pass("b"))
            .then(sink.clone())
            .start(Links::default(), &CancellationToken::new())
            .await
            .map_err(|e| e.to_string())?;
        let (mut epoch, mut seq) = (0u32, 0u64);
        let (mut tools, mut fresh_after_last_interrupt) = (0usize, 0usize);
        for op in ops {
            let tag = |stale: bool| {
                if stale {
                    epoch.saturating_sub(1)
                } else {
                    epoch
                }
            };
            match *op {
                EpochOp::Audio { stale } => {
                    seq += 1;
                    let e = tag(stale);
                    if e == epoch {
                        fresh_after_last_interrupt += 1;
                    }
                    running
                        .input()
                        .send(audio_in(seq, e))
                        .await
                        .map_err(|e| e.to_string())?;
                }
                EpochOp::Tool { stale } => {
                    tools += 1;
                    running
                        .input()
                        .send(tool_result(tag(stale)))
                        .await
                        .map_err(|e| e.to_string())?;
                }
                EpochOp::Interrupt => {
                    epoch += 1;
                    fresh_after_last_interrupt = 0;
                    running
                        .input()
                        .send(interrupt(epoch))
                        .await
                        .map_err(|e| e.to_string())?;
                }
                EpochOp::Yield => {
                    for _ in 0..3 {
                        tokio::task::yield_now().await;
                    }
                }
            }
        }
        running.stop(SECOND).await.map_err(|e| e.to_string())?;
        let records = sink.records();
        check_no_stale_after_interrupt(&records)?;
        // Liveness: nothing current or exempt was lost.
        let delivered_tools = records.iter().filter(|r| r.0 == "tool_result").count();
        if delivered_tools != tools {
            return Err(format!("tool results lost: {delivered_tools}/{tools}"));
        }
        let fresh = records
            .iter()
            .filter(|r| r.0 == "audio" && r.1 == Some(epoch))
            .count();
        if fresh < fresh_after_last_interrupt {
            return Err(format!(
                "current-epoch audio lost: {fresh}/{fresh_after_last_interrupt}"
            ));
        }
        Ok(())
    })
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]

    /// After an interrupt, no frame of an older epoch reaches the sink; current-epoch frames and
    /// tool results are never lost.
    #[test]
    fn no_stale_frame_reaches_the_sink_after_an_interrupt(ops in epoch_ops()) {
        let outcome = run_epoch_scenario(&ops);
        proptest::prop_assert!(outcome.is_ok(), "{:?}", outcome);
    }
}

/// Emits `Interrupt(1)` in both directions when it sees audio `at` (a barge-in detected
/// mid-pipeline), then keeps forwarding normally.
struct Originator {
    at: u64,
    both_ways: bool,
    /// Records what this processor itself processes (when it is the last stage).
    log: Option<Sink>,
}

impl Processor for Originator {
    fn name(&self) -> &'static str {
        "originator"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        if let Some(log) = &self.log {
            log.0
                .lock()
                .unwrap()
                .push((frame.name().to_owned(), epoch_of(&frame)));
        }
        let trigger = matches!(&frame, Frame::Data(DataFrame::Audio(a)) if a.seq() == self.at);
        ctx.forward(frame, dir).await?;
        if trigger {
            ctx.push(interrupt(1), Direction::Downstream).await?;
            if self.both_ways {
                ctx.push(interrupt(1), Direction::Upstream).await?;
            }
        }
        Ok(())
    }
}

fn audio_count_at(records: &[Record], epoch: u32) -> usize {
    records
        .iter()
        .filter(|r| r.0 == "audio" && r.1 == Some(epoch))
        .count()
}

#[tokio::test(start_paused = true)]
async fn a_processor_originating_an_interrupt_purges_its_own_backlog() {
    let sink = Sink::default();
    let mut back = Inbox::new(QueueConfig::DEFAULT);
    let links = Links {
        downstream: None,
        upstream: Some(back.sender(Direction::Upstream)),
    };
    let mut running = Pipeline::new("p")
        .then(Pass("a"))
        .then(Originator {
            at: 10,
            both_ways: true,
            log: None,
        })
        .then(Slow)
        .then(sink.clone())
        .start(links, &CancellationToken::new())
        .await
        .unwrap();
    for i in 0..30 {
        running.input().send(audio_in(i, 0)).await.unwrap();
    }
    for i in 0..3 {
        running.input().send(audio_in(100 + i, 1)).await.unwrap();
    }
    // The upstream copy leaves the pipeline exactly once.
    let (frame, _) = tokio::time::timeout(SECOND, back.recv())
        .await
        .expect("interrupt did not travel upstream")
        .unwrap();
    assert_eq!(frame.name(), "interrupt");
    running.stop(SECOND).await.unwrap();
    assert!(back.try_recv().is_err(), "duplicate upstream interrupt");

    let records = sink.records();
    check_no_stale_after_interrupt(&records).unwrap();
    // Audio 0..=10 may be delivered (sent before the barge-in); nothing after it of epoch 0.
    assert!(audio_count_at(&records, 0) <= 11, "{records:?}");
    assert_eq!(audio_count_at(&records, 1), 3, "{records:?}");
    assert_eq!(records.iter().filter(|r| r.0 == "interrupt").count(), 1);
}

#[tokio::test(start_paused = true)]
async fn parallel_blocks_apply_the_epoch_gate() {
    let sink = Sink::default();
    let mut running = Pipeline::new("p")
        .then_parallel([
            Pipeline::new("b0").then(Pass("b0")),
            Pipeline::new("b1").then(Slow),
        ])
        .then(sink.clone())
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    for i in 0..3 {
        running.input().send(audio_in(i, 0)).await.unwrap();
    }
    running.input().send(interrupt(1)).await.unwrap();
    running.input().send(flush(0)).await.unwrap();
    running.input().send(audio_in(50, 0)).await.unwrap();
    running.input().send(tool_result(0)).await.unwrap();
    running.input().send(flush(1)).await.unwrap();
    running.input().send(flush(1)).await.unwrap();
    running.input().send(audio_in(60, 1)).await.unwrap();
    running.stop(SECOND).await.unwrap();

    let records = sink.records();
    check_no_stale_after_interrupt(&records).unwrap();
    let count = |name: &str, epoch: Option<u32>| {
        records
            .iter()
            .filter(|r| r.0 == name && (epoch.is_none() || r.1 == epoch))
            .count()
    };
    assert_eq!(count("interrupt", None), 1, "{records:?}");
    // Two equal broadcasts after the interrupt: each collapsed to one (ledger entries complete).
    assert_eq!(count("flush", Some(1)), 2, "{records:?}");
    assert_eq!(count("flush", Some(0)), 0, "{records:?}");
    // Data is not collapsed: the stale-exempt tool result comes out of both branches.
    assert_eq!(count("tool_result", None), 2, "{records:?}");
    assert_eq!(count("audio", Some(1)), 2, "{records:?}");
}

#[tokio::test(start_paused = true)]
async fn an_interrupt_from_one_branch_gates_the_others_at_the_merge() {
    let sink = Sink::default();
    let mut running = Pipeline::new("p")
        .then_parallel([
            Pipeline::new("vad").then(Originator {
                at: 0,
                both_ways: false,
                log: None,
            }),
            Pipeline::new("slow").then(Slow).then(Slow),
        ])
        .then(sink.clone())
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    for i in 0..10 {
        running.input().send(audio_in(i, 0)).await.unwrap();
    }
    running.stop(SECOND).await.unwrap();
    let records = sink.records();
    check_no_stale_after_interrupt(&records).unwrap();
    assert_eq!(
        records.iter().filter(|r| r.0 == "interrupt").count(),
        1,
        "{records:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_gate_alone_keeps_stale_frames_from_process() {
    // A single processor fed directly, with stale frames sent only after it has consumed the
    // interrupt: no purge and no upstream push filter can help, only its own gate.
    let seen = Sink::default();
    let inbox = Inbox::new(QueueConfig::DEFAULT);
    let tx = inbox.sender(Direction::Downstream);
    let task = tokio::spawn(run_processor(
        seen.clone(),
        inbox,
        ProcessorContext::default(),
        CancellationToken::new(),
    ));
    let settle = || async {
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
    };
    tx.send(interrupt(2)).await.unwrap();
    settle().await;
    for frame in [
        audio_in(1, 1),
        tool_result(1),
        audio_in(2, 2),
        interrupt(2),
        interrupt(1),
    ] {
        tx.send(frame).await.unwrap();
        settle().await;
    }
    drop(tx);
    tokio::time::timeout(SECOND, task)
        .await
        .expect("processor did not finish")
        .unwrap()
        .unwrap();
    assert_eq!(
        seen.records(),
        [
            ("interrupt".to_owned(), Some(2)),
            ("tool_result".to_owned(), Some(1)),
            ("audio".to_owned(), Some(2)),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn an_originating_last_stage_stops_processing_its_stale_backlog() {
    let log = Sink::default();
    let mut running = Pipeline::new("p")
        .then(Slow)
        .then(Originator {
            at: 3,
            both_ways: false,
            log: Some(log.clone()),
        })
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    for i in 0..30 {
        running.input().send(audio_in(i, 0)).await.unwrap();
    }
    running.stop(SECOND).await.unwrap();
    let records = log.records();
    // It processed audio 0..=3, originated the interrupt, and then nothing of epoch 0.
    assert_eq!(audio_count_at(&records, 0), 4, "{records:?}");
}
