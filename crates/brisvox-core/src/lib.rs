//! Core building blocks of the brisvox engine: the `Frame` model, the `Processor` trait,
//! pipeline routing, the injected `Clock`, turn epochs and the turn timeline.
//!
//! Every other brisvox crate depends on this one; it depends on no other brisvox crate.

#![forbid(unsafe_code)]

pub mod clock;
pub mod epoch;
pub mod error;
pub mod frame;
pub mod pipeline;
pub mod processor;
pub mod queue;
pub mod telemetry;
pub mod timeline;

pub use clock::{Clock, SharedClock, SimClock, SystemClock};
pub use epoch::{Admit, EpochGate, TurnEpoch};
pub use error::{Error, Result};
pub use frame::{AudioFrame, ControlFrame, ControlKind, DataFrame, Frame, FrameLevel, SystemFrame};
pub use pipeline::{Links, Pipeline, PipelineStats, RunningPipeline, StageStats};
pub use processor::{Direction, EpochPolicy, Processor, ProcessorContext, run_processor};
pub use queue::{
    EpochWatch, FrameSender, Inbox, InboxProbe, OverflowPolicies, OverflowPolicy, QueueConfig,
    QueueStats,
};
pub use telemetry::Secret;
pub use timeline::{
    LlmRoundMetrics, MAX_LLM_ROUNDS, TimelineRecorder, TimelineSink, TimelineStats, TurnEvent,
    TurnMetrics, TurnRecord,
};

#[cfg(test)]
mod tests {
    /// Guards the lint policy: clippy.toml must keep exempting tests from the workspace
    /// no-panic lints (`unwrap_used`, `expect_used`, `indexing_slicing`, `panic`).
    #[test]
    fn lint_policy_allows_panicking_constructs_in_tests() {
        let v: Vec<u8> = (1..=3).collect();
        assert_eq!(v[0], 1);
        let parsed: u8 = "7".parse().unwrap();
        let again: u8 = "7".parse().expect("valid u8");
        if parsed != again {
            panic!("parse mismatch");
        }
    }
}
