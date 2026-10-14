//! Speculation candidates from the `TurnDriver`: when the driver offers the runner a
//! candidate end of turn (the final transcript of a stop, the partial text at the VAD stop),
//! when it does not (feature off, equal text, budget spent), and when it withdraws (the caller
//! resumes, the end is released empty). The turn's real end is unchanged.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_agent::TurnPolicy;
use brisvox_core::frame::{
    ControlFrame, ControlKind, DataFrame, Frame, SystemFrame, TranscriptFrame,
};
use brisvox_core::{
    Clock, Direction, EpochPolicy, Links, Pipeline, Processor, ProcessorContext, RunningPipeline,
    SharedClock, SimClock, TimelineRecorder, TurnEpoch,
};
use brisvox_engine::{
    EngineConfig, InterruptionConfig, SpeculationConfig, TurnDriver, TurnDriverConfig,
    TurnDriverStats,
};
use common::MS;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Records every downstream frame with its time.
struct Tail {
    seen: Arc<Mutex<Vec<(Instant, Frame)>>>,
    clock: SimClock,
}

impl Processor for Tail {
    fn name(&self) -> &'static str {
        "tail"
    }

    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        frame: Frame,
        direction: Direction,
        _ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        if direction == Direction::Downstream {
            self.seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((self.clock.now(), frame));
        }
        Ok(())
    }
}

fn label(frame: &Frame) -> String {
    match frame {
        Frame::Control(ControlFrame { epoch, kind }) => format!("{}:{}", kind.name(), epoch.get()),
        Frame::Data(DataFrame::Transcript(t)) => {
            format!("transcript:{}:{}:{}", t.epoch.get(), t.is_final, t.text)
        }
        Frame::System(SystemFrame::Interrupt { new_epoch }) => {
            format!("interrupt:{}", new_epoch.get())
        }
        other => other.name().to_owned(),
    }
}

struct Rig {
    running: RunningPipeline,
    seen: Arc<Mutex<Vec<(Instant, Frame)>>>,
    stats: TurnDriverStats,
    clock: SimClock,
    origin: Instant,
}

const FIXED: TurnPolicy = TurnPolicy::fixed(Duration::from_millis(700));

fn config(speculation: SpeculationConfig) -> TurnDriverConfig {
    let mut engine = EngineConfig::DEFAULT;
    engine.turn = FIXED;
    engine.interruption = InterruptionConfig::OFF;
    engine.speculation = speculation;
    TurnDriverConfig::from_engine(&engine)
}

/// Candidates at the final transcript of a stop only.
fn finals_only() -> SpeculationConfig {
    SpeculationConfig {
        on_partial: false,
        ..SpeculationConfig::ENABLED
    }
}

impl Rig {
    async fn new(config: TurnDriverConfig) -> Self {
        let clock = SimClock::new();
        clock.sleep(MS).await;
        let shared: SharedClock = Arc::new(clock);
        let driver = TurnDriver::new(config, shared, TimelineRecorder::new());
        let stats = driver.stats();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let running = Pipeline::new("turn")
            .then(driver)
            .then(Tail {
                seen: Arc::clone(&seen),
                clock,
            })
            .start(Links::default(), &CancellationToken::new())
            .await
            .unwrap();
        let rig = Self {
            running,
            seen,
            stats,
            clock,
            origin: clock.now(),
        };
        rig.settle().await;
        rig
    }

    async fn settle(&self) {
        for _ in 0..40 {
            tokio::task::yield_now().await;
        }
    }

    async fn at(&self, ms: u64) {
        brisvox_core::clock::wait_until(&self.clock, self.origin + Duration::from_millis(ms)).await;
        self.settle().await;
    }

    fn t(&self, ms: u64) -> Instant {
        self.origin + Duration::from_millis(ms)
    }

    async fn send(&self, frame: Frame) {
        self.running.input().send(frame).await.unwrap();
        self.settle().await;
    }

    async fn vad_start(&self, at_ms: u64) {
        self.send(SystemFrame::UserStartedSpeaking { at: self.t(at_ms) }.into())
            .await;
    }

    async fn vad_stop(&self, at_ms: u64) {
        self.send(SystemFrame::UserStoppedSpeaking { at: self.t(at_ms) }.into())
            .await;
    }

    fn transcript(&self, text: &str, is_final: bool) -> Frame {
        DataFrame::Transcript(TranscriptFrame {
            epoch: TurnEpoch::ZERO,
            text: Arc::from(text),
            is_final,
            received_at: self.clock.now(),
        })
        .into()
    }

    async fn interim(&self, text: &str) {
        self.send(self.transcript(text, false)).await;
    }

    async fn final_segment(&self, text: &str) {
        self.send(self.transcript(text, true)).await;
    }

    async fn utterance_final(&self) {
        self.send(
            ControlFrame {
                epoch: TurnEpoch::ZERO,
                kind: ControlKind::UtteranceFinal,
            }
            .into(),
        )
        .await;
    }

    async fn final_of(&self, texts: &[&str]) {
        for text in texts {
            self.final_segment(text).await;
        }
        self.utterance_final().await;
    }

    /// `(milliseconds after the origin, label)` of every downstream frame.
    fn frames(&self) -> Vec<(u64, String)> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, f)| !matches!(f, Frame::System(SystemFrame::Start)))
            .map(|(at, f)| {
                let ms = at.saturating_duration_since(self.origin).as_millis();
                (u64::try_from(ms).unwrap(), label(f))
            })
            .collect()
    }

    fn labels(&self) -> Vec<String> {
        self.frames().into_iter().map(|(_, l)| l).collect()
    }
}

#[tokio::test(start_paused = true)]
async fn the_final_of_a_stop_is_offered_before_the_turn_ends() {
    let rig = Rig::new(config(finals_only())).await;
    rig.vad_start(0).await;
    rig.at(100).await;
    rig.interim("book").await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    // No candidate at the stop: only finals count here.
    assert_eq!(rig.labels(), ["user_turn_started:0"]);
    rig.at(1250).await;
    rig.final_of(&["Book a table."]).await;
    rig.at(3000).await;
    assert_eq!(
        rig.frames(),
        vec![
            (0, "user_turn_started:0".to_owned()),
            (1250, "transcript:0:false:Book a table.".to_owned()),
            (1700, "transcript:0:true:Book a table.".to_owned()),
            (1700, "user_turn_ended:0".to_owned()),
        ],
        "the candidate comes with the final (450 ms before the end); the end is unchanged"
    );
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.speculations_offered, stats.speculations_withdrawn),
        (1, 0),
        "{stats:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_partial_text_is_offered_at_the_stop_and_again_only_if_the_final_differs() {
    let rig = Rig::new(config(SpeculationConfig::ENABLED)).await;
    rig.vad_start(0).await;
    rig.at(500).await;
    rig.interim("book a").await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    // The candidate is the latest interim at the stop.
    assert_eq!(
        rig.frames()[1..],
        [(1100, "transcript:0:false:book a".to_owned())]
    );
    rig.at(1250).await;
    // The final says more: a new candidate.
    rig.final_of(&["book a table"]).await;
    assert_eq!(rig.labels()[2], "transcript:0:false:book a table");
    assert_eq!(rig.stats.snapshot().speculations_offered, 2);
    rig.at(3000).await;
    assert_eq!(
        rig.labels()[3..],
        ["transcript:0:true:book a table", "user_turn_ended:0"]
    );
}

#[tokio::test(start_paused = true)]
async fn a_final_that_says_the_same_in_other_words_is_not_offered_again() {
    let rig = Rig::new(config(SpeculationConfig::ENABLED)).await;
    rig.vad_start(0).await;
    rig.at(500).await;
    rig.interim("book a table for four").await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1250).await;
    // Same words, a capital and a full stop: no second candidate.
    rig.final_of(&["Book a table for four."]).await;
    rig.at(3000).await;
    assert_eq!(
        rig.labels(),
        [
            "user_turn_started:0",
            "transcript:0:false:book a table for four",
            "transcript:0:true:Book a table for four.",
            "user_turn_ended:0",
        ]
    );
    assert_eq!(rig.stats.snapshot().speculations_offered, 1);
}

#[tokio::test(start_paused = true)]
async fn the_final_segments_heard_so_far_beat_a_stale_interim() {
    let rig = Rig::new(config(SpeculationConfig::ENABLED)).await;
    rig.vad_start(0).await;
    rig.at(500).await;
    rig.interim("book a").await;
    // The provider already finalized the stretch; its interim is older than that.
    rig.final_segment("book a table").await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    assert_eq!(rig.labels()[1], "transcript:0:false:book a table");
}

#[tokio::test(start_paused = true)]
async fn a_caller_who_resumes_withdraws_the_candidate_and_the_longer_text_follows() {
    let rig = Rig::new(config(finals_only())).await;
    rig.vad_start(0).await;
    rig.at(600).await;
    rig.vad_stop(500).await;
    rig.at(700).await;
    rig.final_of(&["I want to book a table"]).await;
    assert_eq!(rig.labels()[1], "transcript:0:false:I want to book a table");
    rig.at(900).await;
    rig.vad_start(900).await; // they go on: the same turn, the end was not decided yet
    assert_eq!(
        rig.labels()[2],
        "transcript:0:false:",
        "an empty non-final transcript is the withdrawal"
    );
    rig.at(1600).await;
    rig.vad_stop(1500).await;
    rig.at(1700).await;
    rig.final_of(&["for four"]).await;
    rig.at(4000).await;
    assert_eq!(
        rig.labels()[3..],
        [
            "transcript:0:false:I want to book a table for four",
            "transcript:0:true:I want to book a table for four",
            "user_turn_ended:0",
        ]
    );
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.speculations_offered, stats.speculations_withdrawn),
        (2, 1),
        "{stats:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn nothing_is_offered_when_the_feature_is_off() {
    let rig = Rig::new(config(SpeculationConfig::OFF)).await;
    rig.vad_start(0).await;
    rig.at(500).await;
    rig.interim("book a").await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1250).await;
    rig.final_of(&["book a table"]).await;
    rig.vad_start(1300).await;
    rig.at(4000).await;
    // Exactly the frames of a driver that has never heard of speculation.
    assert!(
        rig.labels()
            .iter()
            .all(|l| !l.starts_with("transcript:0:false")),
        "{:?}",
        rig.labels()
    );
    assert_eq!(rig.stats.snapshot().speculations_offered, 0);
}

#[tokio::test(start_paused = true)]
async fn the_budget_per_turn_bounds_the_candidates() {
    let rig = Rig::new(config(SpeculationConfig {
        max_per_turn: 1,
        ..SpeculationConfig::ENABLED
    }))
    .await;
    rig.vad_start(0).await;
    rig.at(500).await;
    rig.interim("book a").await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1250).await;
    rig.final_of(&["book a table"]).await;
    rig.at(3000).await;
    assert_eq!(
        rig.labels(),
        [
            "user_turn_started:0",
            "transcript:0:false:book a",
            "transcript:0:true:book a table",
            "user_turn_ended:0",
        ]
    );
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.speculations_offered, stats.speculations_capped),
        (1, 1),
        "{stats:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn an_empty_turn_withdraws_the_candidate_it_offered() {
    let rig = Rig::new(config(SpeculationConfig::ENABLED)).await;
    rig.vad_start(0).await;
    rig.at(500).await;
    rig.interim("uh").await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1250).await;
    // The provider heard nothing worth a turn: an UtteranceFinal without text.
    rig.final_of(&[]).await;
    rig.at(3000).await;
    assert_eq!(
        rig.labels(),
        [
            "user_turn_started:0",
            "transcript:0:false:uh",
            "transcript:0:false:",
        ]
    );
    let stats = rig.stats.snapshot();
    assert_eq!(
        (
            stats.speculations_offered,
            stats.speculations_withdrawn,
            stats.empty_turns
        ),
        (1, 1, 1),
        "{stats:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_next_turn_starts_with_a_fresh_budget_and_no_stale_candidate() {
    let rig = Rig::new(config(SpeculationConfig {
        max_per_turn: 1,
        ..finals_only()
    }))
    .await;
    for (start, text) in [(0u64, "first question"), (4000, "second question")] {
        rig.vad_start(start).await;
        rig.at(start + 600).await;
        rig.vad_stop(start + 500).await;
        rig.at(start + 700).await;
        rig.final_of(&[text]).await;
        rig.at(start + 3500).await;
    }
    let labels = rig.labels();
    assert_eq!(
        labels
            .iter()
            .filter(|l| l.starts_with("transcript:") && l.contains(":false:"))
            .count(),
        2,
        "{labels:?}"
    );
    assert!(
        labels.contains(&"transcript:1:false:second question".to_owned()),
        "{labels:?}"
    );
}

// ---- the stable-partial gate ----

/// Partial candidates only after the caller stayed silent, and the text unchanged, for 200 ms.
fn gated() -> SpeculationConfig {
    SpeculationConfig {
        partial_stable_for: Duration::from_millis(200),
        ..SpeculationConfig::ENABLED
    }
}

#[tokio::test(start_paused = true)]
async fn a_stable_partial_is_offered_once_the_gate_has_elapsed() {
    let rig = Rig::new(config(gated())).await;
    rig.vad_start(0).await;
    rig.at(500).await;
    rig.interim("book a table").await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1299).await;
    assert_eq!(rig.labels(), ["user_turn_started:0"], "still waiting");
    rig.at(1300).await;
    assert_eq!(
        rig.frames()[1..],
        [(1300, "transcript:0:false:book a table".to_owned())],
        "offered 200 ms after the stop was seen, the text unchanged since 500"
    );
    // The final says the same: the candidate stands, nothing more is offered.
    rig.at(1400).await;
    rig.final_of(&["Book a table."]).await;
    rig.at(3000).await;
    assert_eq!(
        rig.labels()[2..],
        ["transcript:0:true:Book a table.", "user_turn_ended:0"]
    );
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.speculations_offered, stats.speculations_withdrawn),
        (1, 0),
        "{stats:?}"
    );
}

/// The caller keeps talking inside the gate: not one request, not even a withdrawn one. Without
/// the gate the same call offers the partial at the stop and withdraws it at the resumption.
#[tokio::test(start_paused = true)]
async fn a_caller_who_resumes_inside_the_gate_costs_no_candidate_at_all() {
    for (speculation, offered, withdrawn) in [(SpeculationConfig::ENABLED, 2, 1), (gated(), 1, 0)] {
        let rig = Rig::new(config(speculation)).await;
        rig.vad_start(0).await;
        rig.at(500).await;
        rig.interim("I want to book").await;
        rig.at(1100).await;
        rig.vad_stop(1000).await;
        rig.at(1250).await; // the gate (due at 1300) has not elapsed
        rig.vad_start(1200).await; // they go on
        rig.at(1300).await;
        rig.final_of(&["I want to book"]).await; // the late final of the first stop
        rig.at(1800).await;
        rig.interim("a table for four").await;
        rig.at(2100).await;
        rig.vad_stop(2000).await;
        rig.at(2400).await;
        rig.final_of(&["a table for four"]).await;
        rig.at(4000).await;
        let labels = rig.labels();
        assert_eq!(
            labels.last().map(String::as_str),
            Some("user_turn_ended:0"),
            "{labels:?}"
        );
        let stats = rig.stats.snapshot();
        assert_eq!(
            (stats.speculations_offered, stats.speculations_withdrawn),
            (offered, withdrawn),
            "gate {:?}: {labels:?}",
            speculation.partial_stable_for
        );
        if speculation.partial_stable_for.is_zero() {
            assert_eq!(labels[1], "transcript:0:false:I want to book");
        } else {
            // The only candidate is the stable text of the second stop, offered 200 ms after it.
            assert_eq!(
                rig.frames()[1],
                (
                    2300,
                    "transcript:0:false:I want to book a table for four".to_owned()
                )
            );
        }
    }
}

/// An interim that changes the text while the gate runs restarts it (the STT is still moving).
#[tokio::test(start_paused = true)]
async fn a_partial_that_changes_during_the_gate_restarts_it() {
    let rig = Rig::new(config(gated())).await;
    rig.vad_start(0).await;
    rig.at(500).await;
    rig.interim("book a").await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1250).await;
    rig.interim("book a table").await; // restarts the wait: due at 1450, not 1300
    rig.at(1449).await;
    assert_eq!(rig.labels(), ["user_turn_started:0"]);
    rig.at(1450).await;
    assert_eq!(
        rig.frames()[1..],
        [(1450, "transcript:0:false:book a table".to_owned())]
    );
    // The same words again (other case, a full stop) change nothing and offer nothing new.
    rig.interim("Book a table.").await;
    rig.at(2000).await;
    assert_eq!(rig.stats.snapshot().speculations_offered, 1);
}

/// The STT final of the stop supersedes the waiting partial: one candidate, from the final.
#[tokio::test(start_paused = true)]
async fn the_final_of_the_stop_supersedes_a_partial_waiting_for_the_gate() {
    let config = config(SpeculationConfig {
        partial_stable_for: Duration::from_millis(400),
        ..SpeculationConfig::ENABLED
    });
    let rig = Rig::new(config).await;
    rig.vad_start(0).await;
    rig.at(500).await;
    rig.interim("book a").await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1250).await;
    rig.final_of(&["book a table"]).await;
    rig.at(1600).await; // past the gate (1500): the partial must not come after the final
    rig.at(3000).await;
    assert_eq!(
        rig.labels(),
        [
            "user_turn_started:0",
            "transcript:0:false:book a table",
            "transcript:0:true:book a table",
            "user_turn_ended:0",
        ]
    );
    assert_eq!(rig.stats.snapshot().speculations_offered, 1);
}

/// The adaptive policy ends the turn within milliseconds of the stop and holds it for the final;
/// the gated partial must still be offered then (the ungated one is offered before the end).
/// A short fixed silence stands in for the verdict here.
#[tokio::test(start_paused = true)]
async fn the_gated_partial_is_offered_while_the_ended_turn_waits_for_its_final() {
    let mut engine = EngineConfig::DEFAULT;
    engine.turn = TurnPolicy::fixed(Duration::from_millis(100));
    engine.interruption = InterruptionConfig::OFF;
    engine.speculation = gated();
    let rig = Rig::new(TurnDriverConfig::from_engine(&engine)).await;
    rig.vad_start(0).await;
    rig.at(500).await;
    rig.interim("book a table").await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1250).await; // the silence (100 ms) ended the turn; it waits for the final
    assert_eq!(rig.labels(), ["user_turn_started:0"]);
    rig.at(1300).await;
    assert_eq!(
        rig.frames()[1..],
        [(1300, "transcript:0:false:book a table".to_owned())],
        "offered at the gate although the turn already ended"
    );
    rig.at(1400).await;
    rig.final_of(&["Book a table."]).await;
    rig.at(2000).await;
    assert_eq!(
        rig.labels()[2..],
        ["transcript:0:true:Book a table.", "user_turn_ended:0"]
    );
    assert_eq!(rig.stats.snapshot().speculations_offered, 1);
}

/// The gate outlasts the final wait: the held end is released at the timeout without a final and
/// the response has started when the gate elapses. The late gate must offer nothing and so must
/// withdraw nothing (no candidate for a turn that is answered already).
#[tokio::test(start_paused = true)]
async fn a_gate_that_elapses_after_the_held_turn_was_released_offers_nothing() {
    let mut engine = EngineConfig::DEFAULT;
    engine.turn = TurnPolicy::fixed(Duration::from_millis(100));
    engine.interruption = InterruptionConfig::OFF;
    engine.speculation = SpeculationConfig {
        partial_stable_for: Duration::from_millis(600),
        ..SpeculationConfig::ENABLED
    };
    let mut config = TurnDriverConfig::from_engine(&engine);
    config.final_wait = Duration::from_millis(150);
    config.late_final_wait = Duration::ZERO;
    let rig = Rig::new(config).await;
    rig.vad_start(0).await;
    rig.at(500).await;
    rig.interim("book a table").await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    // The silence (100 ms) ends the turn at 1200; the final never comes: released at 1350.
    rig.at(1500).await;
    assert_eq!(
        rig.labels(),
        [
            "user_turn_started:0",
            "transcript:0:true:book a table",
            "user_turn_ended:0",
        ],
        "released on the interim, the gate (due at 1700) still running"
    );
    rig.at(2500).await; // the gate has elapsed
    assert_eq!(rig.labels().len(), 3, "{:?}", rig.labels());
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.speculations_offered, stats.speculations_withdrawn),
        (0, 0),
        "{stats:?}"
    );
}

/// The caller pauses, goes on and pauses again; the STT final of the FIRST stop is late and
/// arrives while the gate of the second stop runs. It must not cancel that gate: the second stop
/// still owes its final, so nothing else would offer the partial before the final of the second
/// stop (the head start the gate exists to keep would be lost).
#[tokio::test(start_paused = true)]
async fn a_late_final_of_an_earlier_stop_does_not_cancel_the_gate_of_a_later_stop() {
    let rig = Rig::new(config(gated())).await;
    rig.vad_start(0).await;
    rig.at(500).await;
    rig.interim("I want to book").await;
    rig.at(1100).await;
    rig.vad_stop(1000).await; // gate due at 1300 ...
    rig.at(1250).await;
    rig.vad_start(1200).await; // ... cancelled: they go on
    rig.at(1800).await;
    rig.interim("I want to book a table").await;
    rig.at(2100).await;
    rig.vad_stop(2000).await; // the gate of this stop is due at 2300
    rig.at(2200).await;
    rig.final_of(&["I want to book a table"]).await; // the late final of the first stop
    rig.at(2299).await;
    assert_eq!(rig.labels(), ["user_turn_started:0"], "still waiting");
    rig.at(2300).await;
    assert_eq!(
        rig.frames()[1..],
        [(2300, "transcript:0:false:I want to book a table".to_owned())],
        "the gate of the second stop survived the first final"
    );
    rig.at(2500).await;
    rig.final_of(&[]).await; // the final of the second stop adds nothing
    rig.at(4000).await;
    let labels = rig.labels();
    assert_eq!(
        labels.last().map(String::as_str),
        Some("user_turn_ended:0"),
        "{labels:?}"
    );
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.speculations_offered, stats.speculations_withdrawn),
        (1, 0),
        "{stats:?}"
    );
}
