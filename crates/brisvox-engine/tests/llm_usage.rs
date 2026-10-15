//! The LLM runner counts the usage the provider reports, once per response, for every response
//! the call pays for: normal and tool-loop rounds, and speculative rounds whether the turn
//! commits or discards them. A response that finishes without usage is counted apart. The
//! provider is the simulator's `MockLlm` with scripted usage; time is a paused `SimClock`.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use brisvox_agent::tools::ToolKind;
use brisvox_core::{Clock, TurnEpoch};
use brisvox_engine::{EngineConfig, LlmStatsSnapshot, SpeculationConfig, candidate_frame};
use brisvox_providers::Usage;
use brisvox_sim::mock_llm::LlmScript;
use common::MS;
use common::llm::{Rig, ToolProbe, lookup_tools};

const fn usage(input: u32, cached: u32, output: u32) -> Usage {
    Usage {
        input_tokens: input,
        cached_input_tokens: cached,
        output_tokens: output,
    }
}

fn tokens(s: LlmStatsSnapshot) -> (u64, u64, u64, u64) {
    (
        s.input_tokens,
        s.cached_input_tokens,
        s.output_tokens,
        s.responses_without_usage,
    )
}

async fn tool_rig(config: EngineConfig) -> Rig {
    let probe = Arc::new(ToolProbe::default());
    Rig::new(config, move |builder, clock| {
        let (registry, engine) =
            lookup_tools(clock, &probe, ToolKind::ReadOnly, Duration::ZERO, None);
        builder.with_tools(registry, engine)
    })
    .await
}

#[tokio::test(start_paused = true)]
async fn usage_of_every_round_is_summed_and_a_response_without_usage_is_counted_apart() {
    let rig = tool_rig(EngineConfig::DEFAULT).await;
    // Turn 1: a tool round (usage), then the answer (usage). Turn 2: an answer with no usage.
    rig.llm.push(
        LlmScript::text(["Let me look."])
            .with_tool_call("call_1", "lookup", r#"{"q":"weather"}"#)
            .with_usage(usage(100, 40, 10)),
    );
    rig.llm
        .push(LlmScript::text(["Sunny."]).with_usage(usage(150, 100, 20)));
    rig.llm
        .push(LlmScript::text(["Anything else?"]).without_usage());

    rig.turn(1, "what is the weather").await;
    rig.until_label("llm_response_ended").await;
    let after_first = rig.stats.snapshot();
    assert_eq!(after_first.rounds, 2, "{after_first:?}");
    assert_eq!(tokens(after_first), (250, 140, 30, 0), "{after_first:?}");

    rig.turn(2, "no thanks").await;
    rig.until("the second answer", |rig| {
        rig.labels()
            .iter()
            .filter(|l| *l == "llm_response_ended")
            .count()
            == 2
    })
    .await;
    let after_second = rig.stats.snapshot();
    assert_eq!(after_second.rounds, 3, "{after_second:?}");
    assert_eq!(
        tokens(after_second),
        (250, 140, 30, 1),
        "the response without usage adds no tokens and is counted: {after_second:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_usage_of_a_discarded_speculation_is_counted_and_a_committed_one_only_once() {
    let mut config = EngineConfig::DEFAULT;
    config.speculation = SpeculationConfig::ENABLED;
    let rig = Rig::new(config, |b, _| b).await;
    // The first speculation is discarded (the caller said something else), the turn then runs
    // its own request; the second speculation is committed by the turn.
    rig.llm
        .push(LlmScript::text(["Booked for two."]).with_usage(usage(120, 0, 42)));
    rig.llm
        .push(LlmScript::text(["Booked for four."]).with_usage(usage(130, 20, 7)));
    rig.llm
        .push(LlmScript::text(["Done."]).with_usage(usage(10, 0, 3)));

    let epoch = |n: u32| TurnEpoch::new(n);
    rig.running
        .input()
        .send(candidate_frame(
            epoch(1),
            "book a table for two",
            rig.clock.now(),
        ))
        .await
        .unwrap();
    rig.settle(100 * MS).await;
    // Billed already: the helper's response is complete although no turn has used it.
    assert_eq!(
        tokens(rig.stats.snapshot()),
        (120, 0, 42, 0),
        "a speculative response is counted when it ends"
    );

    rig.turn(1, "book a table for four").await;
    rig.until_label("llm_response_ended").await;
    let spec = rig.spec_stats.snapshot();
    assert_eq!((spec.misses, spec.hits), (1, 0), "{spec:?}");
    assert_eq!(
        tokens(rig.stats.snapshot()),
        (250, 20, 49, 0),
        "the discarded response and the turn's own response"
    );

    // A candidate that the turn confirms: the helper's response is the turn's, counted once.
    rig.running
        .input()
        .send(candidate_frame(epoch(2), "thank you", rig.clock.now()))
        .await
        .unwrap();
    rig.settle(100 * MS).await;
    rig.turn(2, "Thank you").await;
    rig.until("the committed answer", |rig| {
        rig.labels()
            .iter()
            .filter(|l| *l == "llm_response_ended")
            .count()
            == 2
    })
    .await;
    let spec = rig.spec_stats.snapshot();
    assert_eq!((spec.misses, spec.hits), (1, 1), "{spec:?}");
    let llm = rig.stats.snapshot();
    assert_eq!(llm.requests, 3, "{llm:?}");
    assert_eq!(tokens(llm), (260, 20, 52, 0), "{llm:?}");
}
