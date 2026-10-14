//! After cancel (or graceful stop) every task a pipeline spawned is gone.

use std::time::Duration;

use brisvox_core::{
    Direction, Frame, Links, Pipeline, Processor, ProcessorContext, Result, SimClock,
};
use brisvox_sim::leak::TaskCounter;
use tokio_util::sync::CancellationToken;

/// Forwards everything.
struct Pass;

impl Processor for Pass {
    fn name(&self) -> &'static str {
        "pass"
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

fn pipeline(stages: usize) -> Pipeline {
    (0..stages).fold(Pipeline::new("leak"), |p, _| p.then(Pass))
}

#[tokio::test(start_paused = true)]
async fn cancel_leaves_no_tasks_behind() {
    let clock = SimClock::new();
    let counter = TaskCounter::start().unwrap();
    let parent = CancellationToken::new();
    let mut pipelines = Vec::new();
    for _ in 0..20 {
        pipelines.push(pipeline(8).start(Links::default(), &parent).await.unwrap());
    }
    assert_eq!(counter.alive(), counter.baseline() + 20 * 8);

    // Cancel without joining: the helper alone must observe every task ending.
    parent.cancel();
    counter
        .wait_for_baseline(&clock, Duration::from_secs(1))
        .await
        .unwrap();
    drop(pipelines);
}

#[tokio::test(start_paused = true)]
async fn graceful_stop_leaves_no_tasks_behind() {
    let clock = SimClock::new();
    let counter = TaskCounter::start().unwrap();
    let mut running = pipeline(5)
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    running.stop(Duration::from_secs(1)).await.unwrap();
    counter
        .wait_for_baseline(&clock, Duration::from_secs(1))
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn dropping_a_running_pipeline_leaves_no_tasks_behind() {
    let clock = SimClock::new();
    let counter = TaskCounter::start().unwrap();
    let running = pipeline(5)
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    drop(running);
    counter
        .wait_for_baseline(&clock, Duration::from_secs(1))
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn leak_is_detected() {
    let clock = SimClock::new();
    let counter = TaskCounter::start().unwrap();
    let leaked = tokio::spawn(std::future::pending::<()>());
    let result = counter
        .wait_for_baseline(&clock, Duration::from_millis(50))
        .await;
    assert!(matches!(
        result,
        Err(brisvox_sim::Error::TaskLeak { alive, baseline }) if alive == baseline + 1
    ));
    leaked.abort();
}

#[tokio::test(start_paused = true)]
async fn parallel_blocks_leave_no_tasks_behind() {
    let clock = SimClock::new();
    let counter = TaskCounter::start().unwrap();
    let branches = || (0..3).map(|_| pipeline(2));
    // Cancel path.
    let running = Pipeline::new("par")
        .then_parallel(branches())
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    running.cancel();
    counter
        .wait_for_baseline(&clock, Duration::from_secs(1))
        .await
        .unwrap();
    drop(running);
    // Graceful stop path.
    let mut running = Pipeline::new("par")
        .then_parallel(branches())
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    running.stop(Duration::from_secs(1)).await.unwrap();
    counter
        .wait_for_baseline(&clock, Duration::from_secs(1))
        .await
        .unwrap();
}
