//! Speculative generation in the `LlmRunner`: a candidate end of turn starts the LLM with
//! its output buffered; the turn's real end commits it (same normalised text, same context) or
//! discards it. Hit, miss, withdrawal, replacement, SideEffect calls held until the commit and
//! run exactly once, ReadOnly calls run ahead, a late note that changes the context, expiry,
//! failures, back-pressure, cancellation consistency and leaks. The LLM is the simulator's
//! `MockLlm`; time is a paused `SimClock`.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods
)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use brisvox_agent::Truncation;
use brisvox_agent::flows::FlowManager;
use brisvox_agent::flows::example::{Restaurant, restaurant_flow};
use brisvox_agent::segment::Language;
use brisvox_agent::tools::{SessionId, ToolEngine, ToolKind};
use brisvox_core::frame::{ControlFrame, ControlKind, Frame, SystemFrame};
use brisvox_core::{Clock, SharedClock, SimClock, TurnEpoch};
use brisvox_engine::{
    BoxFuture, Cut, CutRequest, EngineConfig, LanguageProfile, LanguageSettings, LanguageTracker,
    SpeculationConfig, SpeculationStatsSnapshot, TruncationSource, Utterance, candidate_frame,
    withdrawal_frame,
};
use brisvox_providers::SpokenLanguage;
use brisvox_providers::{FinishReason, LlmEvent, LlmProvider, LlmRequest, LlmStream};
use brisvox_sim::leak::TaskCounter;
use brisvox_sim::mock_llm::{LlmFault, LlmScript};
use common::MS;
use common::llm::{Act, ActLlm, Rig, ToolProbe, lookup_tools};

const SECOND: Duration = Duration::from_secs(1);

fn spec_config() -> EngineConfig {
    let mut config = EngineConfig::DEFAULT;
    config.speculation = SpeculationConfig::ENABLED;
    config
}

fn ep(n: u32) -> TurnEpoch {
    TurnEpoch::new(n)
}

/// The driver's candidate for the open turn `epoch`.
async fn candidate<P: LlmProvider>(rig: &Rig<P>, epoch: u32, text: &str) {
    rig.running
        .input()
        .send(candidate_frame(ep(epoch), text, rig.clock.now()))
        .await
        .unwrap();
}

/// The driver's withdrawal for the open turn `epoch`.
async fn withdraw<P: LlmProvider>(rig: &Rig<P>, epoch: u32) {
    rig.running
        .input()
        .send(withdrawal_frame(ep(epoch), rig.clock.now()))
        .await
        .unwrap();
}

async fn send<P: LlmProvider>(rig: &Rig<P>, frame: Frame) {
    rig.running.input().send(frame).await.unwrap();
}

fn user_turn_started(epoch: u32) -> Frame {
    ControlFrame {
        epoch: ep(epoch),
        kind: ControlKind::UserTurnStarted,
    }
    .into()
}

fn stats<P: LlmProvider>(rig: &Rig<P>) -> SpeculationStatsSnapshot {
    rig.spec_stats.snapshot()
}

fn requests<P: LlmProvider>(rig: &Rig<P>) -> usize {
    rig.llm.requests().len()
}

/// A rig with speculation on and a `lookup` tool of `kind` that takes `delay`.
async fn tool_rig(config: EngineConfig, kind: ToolKind, delay: Duration) -> (Rig, Arc<ToolProbe>) {
    let probe = Arc::new(ToolProbe::default());
    let rig = Rig::new(config, {
        let probe = Arc::clone(&probe);
        move |builder, clock| {
            let (registry, engine) = lookup_tools(clock, &probe, kind, delay, None);
            builder.with_tools(registry, engine)
        }
    })
    .await;
    (rig, probe)
}

fn tool_script(text: &str) -> LlmScript {
    LlmScript::text([text]).with_tool_call("call_1", "lookup", r#"{"q":"table"}"#)
}

// ---- hit ----------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_confirmed_candidate_releases_the_buffered_answer_at_once() {
    let rig = Rig::new(spec_config(), |b, _| b).await;
    // The first token takes 300 ms.
    rig.llm.push(
        LlmScript::text(["  Sure,", " it is sunny."]).with_fault(LlmFault::Stall {
            before_event: 0,
            duration: 300 * MS,
        }),
    );
    candidate(&rig, 1, "what is the weather").await;
    rig.settle(400 * MS).await;

    // The request went out, the answer is complete, and nothing is visible anywhere.
    assert_eq!(requests(&rig), 1);
    assert!(rig.labels().is_empty(), "buffered: {:?}", rig.labels());
    assert!(rig.conversation().is_empty(), "no trace in the context");
    assert_eq!(stats(&rig).started, 1);
    assert_eq!(rig.stats.snapshot().rounds, 0, "no round has started yet");

    // The turn ends with the same words (other case and punctuation): the answer is released
    // within the same instant, not 300 ms later.
    let committed_at = rig.clock.now();
    rig.turn(1, "What is the weather?").await;
    rig.settle(5 * MS).await;
    assert_eq!(rig.clock.now() - committed_at, 5 * MS);
    assert_eq!(
        rig.labels(),
        [
            "user_turn_ended",
            "llm_response_started",
            "text:Sure,", // the leading whitespace of the response is stripped, as always
            "text: it is sunny.",
            "llm_response_ended",
        ]
    );
    assert_eq!(requests(&rig), 1, "no second request");
    assert_eq!(
        rig.conversation(),
        ["user:What is the weather?", "assistant:Sure, it is sunny."]
    );
    rig.context_copy().validate().unwrap();

    let spec = stats(&rig);
    assert_eq!((spec.started, spec.hits, spec.discarded()), (1, 1, 0));
    assert_eq!(spec.hit_rate(), Some(1.0));
    assert_eq!(spec.wasted_tokens(), 0);
    assert!(spec.head_start_ms >= 400, "{spec:?}");
    let llm = rig.stats.snapshot();
    assert_eq!((llm.turns, llm.rounds, llm.requests), (1, 1, 1), "{llm:?}");
    assert_eq!(llm.validate_failures, 0);

    // The timeline shows the request when it really went out and the first token when it
    // really came: 300 ms of provider time, all of it before the turn ended.
    let turn = rig.timeline.current().unwrap();
    let round = turn.llm_round(0).unwrap();
    assert_eq!(round.time_to_first_token, Some(300 * MS));
}

#[tokio::test(start_paused = true)]
async fn the_answer_of_a_hit_equals_the_answer_without_speculation() {
    // Same script, same words: the context and the frames are the same with and without it.
    let run = |config: EngineConfig, speculate: bool| async move {
        let rig = Rig::new(config, |b, _| b).await;
        rig.llm.push(LlmScript::text(["  One", " two", ". Three."]));
        if speculate {
            candidate(&rig, 1, "Count to three.").await;
            rig.settle(100 * MS).await;
        }
        rig.turn(1, "Count to three.").await;
        rig.until_label("llm_response_ended").await;
        (rig.labels(), rig.conversation(), rig.llm.requests())
    };
    let (labels, conversation, requests) = run(EngineConfig::DEFAULT, false).await;
    let (spec_labels, spec_conversation, spec_requests) = run(spec_config(), true).await;
    assert_eq!(labels, spec_labels);
    assert_eq!(conversation, spec_conversation);
    assert_eq!(requests.len(), 1);
    assert_eq!(spec_requests.len(), 1);
    // The request of the speculation is the request the turn would have made (with the words as
    // the candidate spelled them; the context keeps the final transcript's spelling).
    assert_eq!(requests[0], spec_requests[0]);
}

#[tokio::test(start_paused = true)]
async fn the_request_of_a_speculation_carries_the_settled_history() {
    let rig = Rig::new(spec_config(), |b, _| b.with_system_prompt("Be brief.")).await;
    rig.llm.push(LlmScript::text(["One."]));
    rig.llm.push(LlmScript::text(["Two."]));
    rig.turn(1, "a").await;
    rig.until_label("llm_response_ended").await;
    candidate(&rig, 2, "b").await;
    rig.settle(50 * MS).await;
    let sent = rig.llm.requests();
    assert_eq!(sent.len(), 2);
    let texts: Vec<&str> = sent[1].messages.iter().map(|m| &*m.content).collect();
    assert_eq!(texts, ["Be brief.", "a", "One.", "b"]);
    // The real context still ends with the first answer: the candidate is not in it.
    assert_eq!(
        rig.conversation(),
        ["system:Be brief.", "user:a", "assistant:One."]
    );
    rig.turn(2, "b").await;
    rig.until("second answer", |r| {
        r.labels()
            .iter()
            .filter(|l| *l == "llm_response_ended")
            .count()
            == 2
    })
    .await;
    assert_eq!(
        rig.conversation(),
        [
            "system:Be brief.",
            "user:a",
            "assistant:One.",
            "user:b",
            "assistant:Two."
        ]
    );
    assert_eq!(requests(&rig), 2);
}

#[tokio::test(start_paused = true)]
async fn a_candidate_and_its_confirmation_in_the_same_instant_build_one_user_message() {
    // The turn end can reach the runner before the speculative helper has run at all. The
    // request must still be built from the context as it was at the candidate: one user message.
    let rig = Rig::new(spec_config(), |b, _| b).await;
    rig.llm.push(LlmScript::text(["Hi."]));
    rig.llm.push(LlmScript::text(["not used"]));
    candidate(&rig, 1, "hello").await;
    rig.turn(1, "hello").await;
    rig.until_label("llm_response_ended").await;
    let sent = rig.llm.requests();
    assert_eq!(sent.len(), 1, "the speculation was the request");
    let users = sent[0]
        .messages
        .iter()
        .filter(|m| m.role == brisvox_providers::Role::User)
        .count();
    assert_eq!(users, 1, "{:?}", sent[0].messages);
    assert_eq!(rig.conversation(), ["user:hello", "assistant:Hi."]);
    assert_eq!(stats(&rig).hits, 1);
}

// ---- miss, withdrawal, replacement ----------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_changed_transcript_discards_the_speculation_and_runs_the_turn_afresh() {
    let rig = Rig::new(spec_config(), |b, _| b).await;
    rig.llm.push(LlmScript::text(["Booked for two."]));
    rig.llm.push(LlmScript::text(["Booked for four."]));
    candidate(&rig, 1, "book a table for two").await;
    rig.settle(100 * MS).await;
    assert_eq!(requests(&rig), 1);

    rig.turn(1, "book a table for four").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(requests(&rig), 2, "the turn made its own request");
    let texts: Vec<String> = rig
        .labels()
        .into_iter()
        .filter(|l| l.starts_with("text:"))
        .collect();
    assert_eq!(
        texts,
        ["text:Booked for four."],
        "nothing of the speculation"
    );
    assert_eq!(
        rig.conversation(),
        ["user:book a table for four", "assistant:Booked for four."]
    );
    let sent = rig.llm.requests();
    assert_eq!(
        &*sent[1].messages.last().unwrap().content,
        "book a table for four"
    );

    let spec = stats(&rig);
    assert_eq!(
        (spec.started, spec.hits, spec.misses),
        (1, 0, 1),
        "{spec:?}"
    );
    assert_eq!(spec.hit_rate(), Some(0.0));
    // The discarded run streamed a sentence: those tokens were wasted.
    assert!(spec.wasted_tokens() >= 1, "{spec:?}");
}

#[tokio::test(start_paused = true)]
async fn the_wasted_tokens_are_what_the_provider_reported() {
    // A provider that reports usage at `Done`: the metric uses it, not the byte estimate.
    use brisvox_providers::{FinishReason, LlmEvent, LlmRequest, LlmStream, Usage};
    struct Metered;
    impl LlmProvider for Metered {
        fn name(&self) -> &'static str {
            "metered"
        }
        async fn stream(&self, _request: LlmRequest) -> brisvox_providers::Result<LlmStream> {
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            tx.try_send(LlmEvent::TextDelta(Arc::from("Hello.")))
                .unwrap();
            tx.try_send(LlmEvent::Done {
                finish: FinishReason::Stop,
                usage: Some(Usage {
                    input_tokens: 120,
                    cached_input_tokens: 0,
                    output_tokens: 42,
                }),
                provider_data: None,
            })
            .unwrap();
            Ok(LlmStream::new(rx))
        }
    }
    let rig = Rig::with_provider(spec_config(), Metered, |b, _| b).await;
    candidate(&rig, 1, "first words").await;
    rig.settle(50 * MS).await;
    rig.turn(1, "other words").await;
    rig.until_label("llm_response_ended").await;
    let spec = stats(&rig);
    assert_eq!(spec.misses, 1);
    assert_eq!(
        (spec.wasted_output_tokens, spec.wasted_input_tokens),
        (42, 120)
    );
}

#[tokio::test(start_paused = true)]
async fn a_caller_who_resumes_withdraws_the_speculation() {
    let rig = Rig::new(spec_config(), |b, _| b).await;
    rig.llm.push(LlmScript::text(["Too early."]));
    rig.llm.push(LlmScript::text(["The whole answer."]));
    candidate(&rig, 1, "book a table").await;
    rig.settle(100 * MS).await;
    withdraw(&rig, 1).await;
    rig.settle(100 * MS).await;
    assert!(rig.labels().is_empty());
    assert!(rig.conversation().is_empty());
    let spec = stats(&rig);
    assert_eq!((spec.started, spec.withdrawn), (1, 1), "{spec:?}");

    // The user finished the sentence; the turn is answered normally.
    candidate(&rig, 1, "book a table for four").await;
    rig.settle(100 * MS).await;
    rig.turn(1, "book a table for four").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(
        rig.conversation(),
        ["user:book a table for four", "assistant:The whole answer."]
    );
    let spec = stats(&rig);
    assert_eq!(
        (spec.started, spec.hits, spec.withdrawn),
        (2, 1, 1),
        "{spec:?}"
    );
    assert_eq!(spec.hit_rate(), Some(0.5));
}

#[tokio::test(start_paused = true)]
async fn the_same_candidate_keeps_the_run_and_another_replaces_it() {
    let rig = Rig::new(spec_config(), |b, _| b).await;
    rig.llm.push(LlmScript::text(["first"]));
    rig.llm.push(LlmScript::text(["second"]));
    candidate(&rig, 1, "hello there").await;
    rig.settle(50 * MS).await;
    // The same words in another dress: kept, no second request.
    candidate(&rig, 1, "Hello, there!").await;
    rig.settle(50 * MS).await;
    assert_eq!(requests(&rig), 1);
    assert_eq!(stats(&rig).kept, 1);
    // Other words replace it.
    candidate(&rig, 1, "hello there friend").await;
    rig.settle(50 * MS).await;
    assert_eq!(requests(&rig), 2);
    let spec = stats(&rig);
    assert_eq!((spec.started, spec.superseded), (2, 1), "{spec:?}");
    // The replacement is the one that commits.
    rig.turn(1, "hello there friend").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(
        rig.conversation(),
        ["user:hello there friend", "assistant:second"]
    );
    assert_eq!(requests(&rig), 2);
}

#[tokio::test(start_paused = true)]
async fn a_new_user_turn_a_barge_in_and_a_stop_drop_the_speculation() {
    for how in ["user_turn_started", "interrupt", "stop"] {
        let rig = Rig::new(spec_config(), |b, _| b).await;
        rig.llm.push(LlmScript::text(["never heard"]));
        candidate(&rig, 1, "something").await;
        rig.settle(50 * MS).await;
        assert_eq!(stats(&rig).started, 1, "{how}");
        match how {
            "user_turn_started" => send(&rig, user_turn_started(2)).await,
            "interrupt" => rig.interrupt(2).await,
            _ => send(&rig, brisvox_core::frame::SystemFrame::Stop.into()).await,
        }
        rig.settle(50 * MS).await;
        let spec = stats(&rig);
        assert_eq!((spec.abandoned, spec.hits), (1, 0), "{how}: {spec:?}");
        // The context holds nothing of it: no user message, no partial answer.
        assert!(
            rig.conversation().is_empty(),
            "{how}: {:?}",
            rig.conversation()
        );
        rig.context_copy().validate().unwrap();
        assert!(
            !rig.labels().iter().any(|l| l.starts_with("text:")),
            "{how}: {:?}",
            rig.labels()
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_speculation_of_another_epoch_never_answers_a_turn() {
    let rig = Rig::new(spec_config(), |b, _| b).await;
    rig.llm.push(LlmScript::text(["for epoch one"]));
    rig.llm.push(LlmScript::text(["for epoch two"]));
    candidate(&rig, 1, "same words").await;
    rig.settle(50 * MS).await;
    rig.turn(2, "same words").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(
        rig.conversation(),
        ["user:same words", "assistant:for epoch two"]
    );
    assert_eq!(stats(&rig).abandoned, 1);
    assert_eq!(stats(&rig).hits, 0);
}

#[tokio::test(start_paused = true)]
async fn nothing_happens_when_speculation_is_off() {
    let rig = Rig::new(EngineConfig::DEFAULT, |b, _| b).await;
    rig.llm.push(LlmScript::text(["answer"]));
    candidate(&rig, 1, "hello").await;
    withdraw(&rig, 1).await;
    candidate(&rig, 1, "hello again").await;
    rig.settle(SECOND).await;
    assert_eq!(requests(&rig), 0);
    assert_eq!(stats(&rig), SpeculationStatsSnapshot::default());
    assert!(rig.labels().is_empty());
    // The turn is answered as always.
    rig.turn(1, "hello again").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(rig.conversation(), ["user:hello again", "assistant:answer"]);
}

// ---- tools ---------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_side_effect_call_is_held_until_the_commit_and_runs_exactly_once() {
    let (rig, probe) = tool_rig(spec_config(), ToolKind::SideEffect, 100 * MS).await;
    rig.llm.push(tool_script("Booking."));
    rig.llm.push(LlmScript::text(["Done."]));
    candidate(&rig, 1, "book a table").await;
    // Plenty of time for the whole speculative response, and for the tool if it were running.
    rig.settle(2 * SECOND).await;
    assert_eq!(
        probe.started(),
        0,
        "a SideEffect call never runs speculatively"
    );
    let spec = stats(&rig);
    assert_eq!(
        (spec.tool_phases, spec.held_side_effects),
        (1, 1),
        "{spec:?}"
    );
    assert!(rig.labels().is_empty(), "nothing downstream yet");
    assert!(rig.conversation().is_empty());

    rig.turn(1, "book a table").await;
    rig.until("the answer after the tool", |r| {
        r.labels().contains(&"llm_response_ended".to_owned())
    })
    .await;
    assert_eq!((probe.started(), probe.finished()), (1, 1));
    rig.settle(5 * SECOND).await;
    assert_eq!(probe.started(), 1, "never twice");
    assert_eq!(
        rig.conversation(),
        [
            "user:book a table",
            "assistant+calls[lookup]:Booking.",
            "tool:{\"answer\":\"sunny\"}",
            "assistant:Done."
        ]
    );
    rig.context_copy().validate().unwrap();
    // The text before the tool is flushed so it is spoken while the held call runs.
    let labels = rig.labels();
    let flush = labels.iter().position(|l| l == "flush").unwrap();
    let text = labels.iter().position(|l| l == "text:Booking.").unwrap();
    assert!(text < flush, "{labels:?}");
    let spec = stats(&rig);
    assert_eq!((spec.hits, spec.discarded()), (1, 0), "{spec:?}");
    // Two requests: the speculative first round and the round that answers the tool result.
    assert_eq!(requests(&rig), 2);
    let llm = rig.stats.snapshot();
    assert_eq!(
        (llm.tool_rounds, llm.tool_calls, llm.rounds),
        (1, 1, 2),
        "{llm:?}"
    );
    assert_eq!(llm.validate_failures, 0);
}

#[tokio::test(start_paused = true)]
async fn a_discarded_speculation_never_runs_its_side_effect_call() {
    let (rig, probe) = tool_rig(spec_config(), ToolKind::SideEffect, 100 * MS).await;
    rig.llm.push(tool_script("Booking."));
    rig.llm.push(LlmScript::text(["Just talking."]));
    candidate(&rig, 1, "book a table").await;
    rig.settle(SECOND).await;
    // The caller said something else in the end.
    rig.turn(1, "never mind").await;
    rig.until_label("llm_response_ended").await;
    rig.settle(5 * SECOND).await;
    assert_eq!(probe.started(), 0, "the held call was dropped unrun");
    assert_eq!(
        rig.conversation(),
        ["user:never mind", "assistant:Just talking."]
    );
    assert_eq!(stats(&rig).misses, 1);
    rig.context_copy().validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_restarted_speculation_runs_the_side_effect_once_across_both_runs() {
    let (rig, probe) = tool_rig(spec_config(), ToolKind::SideEffect, 100 * MS).await;
    rig.llm.push(tool_script("Booking for two."));
    rig.llm.push(tool_script("Booking for four."));
    rig.llm.push(LlmScript::text(["Done."]));
    candidate(&rig, 1, "book for two").await;
    rig.settle(SECOND).await;
    candidate(&rig, 1, "book for four").await;
    rig.settle(SECOND).await;
    assert_eq!(probe.started(), 0);
    assert_eq!(stats(&rig).held_side_effects, 2);
    rig.turn(1, "book for four").await;
    rig.until_label("llm_response_ended").await;
    rig.settle(5 * SECOND).await;
    assert_eq!(
        (probe.started(), probe.finished()),
        (1, 1),
        "the call id repeats across the two runs of one epoch: still one execution"
    );
    assert_eq!(
        rig.conversation()[1],
        "assistant+calls[lookup]:Booking for four."
    );
}

#[tokio::test(start_paused = true)]
async fn a_read_only_call_runs_ahead_and_its_result_is_used_at_the_commit() {
    let (rig, probe) = tool_rig(spec_config(), ToolKind::ReadOnly, 100 * MS).await;
    rig.llm.push(tool_script("Let me look."));
    rig.llm.push(LlmScript::text(["It is sunny."]));
    candidate(&rig, 1, "what is the weather").await;
    rig.settle(SECOND).await;
    assert_eq!(
        (probe.started(), probe.finished()),
        (1, 1),
        "the ReadOnly call ran during the speculation"
    );
    assert!(rig.labels().is_empty());
    assert!(
        rig.conversation().is_empty(),
        "its result is not in the context yet"
    );

    rig.turn(1, "what is the weather").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(probe.started(), 1, "the commit did not run it again");
    assert_eq!(
        rig.conversation(),
        [
            "user:what is the weather",
            "assistant+calls[lookup]:Let me look.",
            "tool:{\"answer\":\"sunny\"}",
            "assistant:It is sunny."
        ]
    );
    rig.context_copy().validate().unwrap();
    assert_eq!(stats(&rig).hits, 1);
}

#[tokio::test(start_paused = true)]
async fn a_read_only_call_in_flight_is_cancelled_with_a_withdrawn_speculation() {
    let (rig, probe) = tool_rig(spec_config(), ToolKind::ReadOnly, 10 * SECOND).await;
    rig.llm.push(tool_script("Looking."));
    candidate(&rig, 1, "what is the weather").await;
    rig.settle(SECOND).await;
    assert_eq!((probe.started(), probe.finished()), (1, 0), "still running");
    withdraw(&rig, 1).await;
    rig.settle(20 * SECOND).await;
    assert_eq!(
        probe.finished(),
        0,
        "the tool future was dropped with the speculation"
    );
    assert!(rig.conversation().is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_committed_turn_follows_a_slow_read_only_tool_without_an_idle_timeout() {
    // The tool takes 20 s: longer than the 15 s idle timeout of a stream. The speculation is
    // committed while it runs; the round task must wait for the tool, not give up.
    let (rig, probe) = tool_rig(spec_config(), ToolKind::ReadOnly, 20 * SECOND).await;
    rig.llm.push(tool_script("Looking."));
    rig.llm.push(LlmScript::text(["Done."]));
    candidate(&rig, 1, "slow question").await;
    rig.settle(SECOND).await;
    rig.turn(1, "slow question").await;
    rig.settle(30 * SECOND).await;
    assert_eq!((probe.started(), probe.finished()), (1, 1));
    assert_eq!(
        rig.conversation().last().map(String::as_str),
        Some("assistant:Done.")
    );
    assert_eq!(rig.stats.snapshot().idle_timeouts, 0);
}

#[tokio::test(start_paused = true)]
async fn a_late_tool_note_arriving_meanwhile_makes_the_speculation_a_miss() {
    let (rig, probe) = tool_rig(spec_config(), ToolKind::SideEffect, 2 * SECOND).await;
    // Turn 1: a SideEffect call that takes 2 s; the caller barges in while it runs, so its
    // result will come back as a late note.
    rig.llm.push(tool_script("Booking."));
    rig.turn(1, "book a table").await;
    rig.until("the tool started", |_| probe.started() == 1)
        .await;
    rig.interrupt(2).await;
    rig.settle(100 * MS).await;
    // Turn 2: the candidate settles the context now (no note yet) and its request is built.
    rig.llm.push(LlmScript::text(["Speculated answer."]));
    rig.llm.push(LlmScript::text(["Real answer."]));
    candidate(&rig, 2, "did it work").await;
    rig.settle(50 * MS).await;
    assert_eq!(requests(&rig), 2);
    // The tool finishes; its note lands in the engine's mailbox before the turn ends.
    rig.settle(3 * SECOND).await;
    assert_eq!(probe.finished(), 1);
    rig.turn(2, "did it work").await;
    rig.until("the real answer", |r| {
        r.labels().iter().any(|l| l == "text:Real answer.")
    })
    .await;
    let spec = stats(&rig);
    assert_eq!((spec.context_changes, spec.hits), (1, 0), "{spec:?}");
    assert_eq!(requests(&rig), 3, "the turn made its own request");
    // The request of the turn carries the note the speculative one lacked.
    let sent = rig.llm.requests();
    assert!(
        sent[2].messages.len() > sent[1].messages.len(),
        "{} vs {}",
        sent[2].messages.len(),
        sent[1].messages.len()
    );
    rig.context_copy().validate().unwrap();
    assert_eq!(rig.stats.snapshot().validate_failures, 0);
}

// ---- age, failures, back-pressure -----------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_speculation_that_is_never_confirmed_ends_by_itself_and_is_not_committed_late() {
    let mut config = spec_config();
    config.speculation.max_age = SECOND;
    let rig = Rig::new(config, |b, _| b).await;
    // The first token takes 5 s: the helper is still waiting when its age limit passes.
    rig.llm
        .push(LlmScript::text(["slow"]).with_fault(LlmFault::Stall {
            before_event: 0,
            duration: 5 * SECOND,
        }));
    rig.llm.push(LlmScript::text(["fresh"]));
    candidate(&rig, 1, "hello").await;
    rig.settle(2 * SECOND).await;
    // The same words arrive late: older than max_age, so the turn starts afresh.
    rig.turn(1, "hello").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(rig.conversation(), ["user:hello", "assistant:fresh"]);
    let spec = stats(&rig);
    assert_eq!((spec.expired, spec.hits), (1, 0), "{spec:?}");
    assert_eq!(requests(&rig), 2);
}

#[tokio::test(start_paused = true)]
async fn a_failed_speculative_request_is_retried_at_the_commit() {
    let rig = Rig::new(spec_config(), |b, _| b).await;
    rig.llm
        .push(LlmScript::text(["x"]).with_fault(LlmFault::ErrorResponse { status: 503 }));
    rig.llm.push(LlmScript::text(["Recovered."]));
    candidate(&rig, 1, "hello").await;
    rig.settle(SECOND).await;
    assert_eq!(stats(&rig).failures, 1);
    rig.turn(1, "hello").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(rig.conversation(), ["user:hello", "assistant:Recovered."]);
    let llm = rig.stats.snapshot();
    assert_eq!((llm.retries, llm.failed_rounds), (1, 0), "{llm:?}");
    assert_eq!(requests(&rig), 2);
}

#[tokio::test(start_paused = true)]
async fn a_failed_speculative_request_without_retries_fails_the_round_like_any_other() {
    let mut config = spec_config();
    config.llm.retries = 0;
    let mut rig = Rig::new(config, |b, _| b).await;
    rig.llm
        .push(LlmScript::text(["x"]).with_fault(LlmFault::ErrorResponse { status: 503 }));
    candidate(&rig, 1, "hello").await;
    rig.settle(SECOND).await;
    rig.turn(1, "hello").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(rig.conversation(), ["user:hello"]);
    assert_eq!(rig.stats.snapshot().failed_rounds, 1);
    // The machine is told upstream that nothing will be spoken.
    assert_eq!(rig.upstream_frames().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_stream_that_breaks_after_output_keeps_the_partial_text_like_any_round() {
    let rig = Rig::new(spec_config(), |b, _| b).await;
    rig.llm.push(
        LlmScript::text(["Half of", " an answer"])
            .with_fault(LlmFault::Disconnect { after_events: 1 }),
    );
    candidate(&rig, 1, "hello").await;
    rig.settle(SECOND).await;
    rig.turn(1, "hello").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(rig.conversation(), ["user:hello", "assistant:Half of"]);
    assert_eq!(requests(&rig), 1, "no retry after output");
    assert_eq!(rig.stats.snapshot().failed_rounds, 1);
}

#[tokio::test(start_paused = true)]
async fn a_full_buffer_waits_instead_of_dropping() {
    let mut config = spec_config();
    config.speculation.max_buffered_events = 2;
    let rig = Rig::new(config, |b, _| b).await;
    let pieces: Vec<String> = (0..12).map(|n| format!("w{n} ")).collect();
    rig.llm.push(LlmScript::text(pieces.clone()));
    candidate(&rig, 1, "count").await;
    rig.settle(SECOND).await;
    assert!(rig.labels().is_empty());
    rig.turn(1, "count").await;
    rig.until_label("llm_response_ended").await;
    let texts: Vec<String> = rig
        .labels()
        .into_iter()
        .filter_map(|l| l.strip_prefix("text:").map(str::to_owned))
        .collect();
    let expected: Vec<String> = pieces
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if i == 0 {
                p.trim_start().to_owned()
            } else {
                p.clone()
            }
        })
        .collect();
    assert_eq!(texts, expected, "every piece, in order");
    assert_eq!(requests(&rig), 1);
}

// ---- cancellation consistency ------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_barge_in_right_after_the_commit_leaves_a_consistent_context() {
    let rig = Rig::new(spec_config(), |b, _| b).await;
    // A long answer still streaming at the commit.
    rig.llm.push(
        LlmScript::text(["One ", "two ", "three ", "four"]).with_fault(LlmFault::Stall {
            before_event: 2,
            duration: 5 * SECOND,
        }),
    );
    candidate(&rig, 1, "count").await;
    rig.settle(200 * MS).await;
    rig.turn(1, "count").await;
    rig.settle(100 * MS).await;
    rig.interrupt(2).await;
    rig.settle(SECOND).await;
    // The partial answer is what was streamed; the context is valid and nothing is left running.
    let conversation = rig.conversation();
    assert_eq!(conversation[0], "user:count");
    assert_eq!(conversation[1], "assistant:One two ");
    rig.context_copy().validate().unwrap();
    let llm = rig.stats.snapshot();
    assert_eq!(
        (llm.halts, llm.partial_messages, llm.join_timeouts),
        (1, 1, 0),
        "{llm:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_pipeline_leaves_no_task_behind() {
    let clock = SimClock::new();
    let counter = TaskCounter::start_settled(&clock, 20 * MS).await.unwrap();
    {
        let (rig, _probe) = tool_rig(spec_config(), ToolKind::ReadOnly, 10 * SECOND).await;
        rig.llm.push(tool_script("Looking."));
        rig.llm.push(LlmScript::text(["one"]));
        candidate(&rig, 1, "first").await;
        rig.settle(SECOND).await;
        // Replaced, then left running when the pipeline stops.
        candidate(&rig, 1, "second").await;
        rig.settle(SECOND).await;
        let mut running = rig.running;
        running.stop(Duration::from_secs(5)).await.unwrap();
    }
    counter
        .wait_for_baseline(&clock, Duration::from_secs(2))
        .await
        .expect("a speculation helper or a tool task leaked");
}

// ---- review fixes ----------------------------------------------------------------------------------

/// A provider whose first response is `pieces` pieces pushed through a one-slot stream, counting
/// what the consumer actually pulled; later responses are the single piece `Fresh.`.
#[derive(Clone, Default)]
struct Counting {
    pieces: usize,
    calls: Arc<AtomicUsize>,
    delivered: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
}

impl Counting {
    fn new(pieces: usize) -> Self {
        Self {
            pieces,
            ..Self::default()
        }
    }
}

impl LlmProvider for Counting {
    fn name(&self) -> &'static str {
        "counting"
    }

    async fn stream(&self, _request: LlmRequest) -> brisvox_providers::Result<LlmStream> {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let (delivered, closed) = (Arc::clone(&self.delivered), Arc::clone(&self.closed));
        let pieces = if first { self.pieces } else { 1 };
        tokio::spawn(async move {
            for n in 0..pieces {
                let text = if first {
                    format!("w{n} ")
                } else {
                    "Fresh.".to_owned()
                };
                if tx.send(LlmEvent::TextDelta(Arc::from(text))).await.is_err() {
                    closed.store(true, Ordering::SeqCst);
                    return;
                }
                if first {
                    delivered.fetch_add(1, Ordering::SeqCst);
                }
            }
            let done = LlmEvent::Done {
                finish: FinishReason::Stop,
                usage: None,
                provider_data: None,
            };
            if tx.send(done).await.is_err() {
                closed.store(true, Ordering::SeqCst);
            }
        });
        Ok(LlmStream::new(rx))
    }
}

#[tokio::test(start_paused = true)]
async fn the_buffer_is_bounded_the_provider_is_not_drained_ahead_of_the_commit() {
    let mut config = spec_config();
    config.speculation.max_buffered_events = 2;
    let provider = Counting::new(100);
    let rig = Rig::with_provider(config, provider.clone(), |b, _| b).await;
    candidate(&rig, 1, "count").await;
    rig.settle(SECOND).await;
    // The helper stopped pulling once the buffer was full: a few events in flight, never all 100.
    let pulled = provider.delivered.load(Ordering::SeqCst);
    assert!(
        (1..=6).contains(&pulled),
        "an unbounded buffer would have pulled all 100, pulled {pulled}"
    );
    // Nothing was dropped: after the commit every piece arrives, in order.
    rig.turn(1, "count").await;
    rig.until_label("llm_response_ended").await;
    let texts: Vec<String> = rig
        .labels()
        .into_iter()
        .filter_map(|l| l.strip_prefix("text:").map(str::to_owned))
        .collect();
    assert_eq!(texts.len(), 100);
    assert_eq!(texts[99], "w99 ");
    assert_eq!(provider.delivered.load(Ordering::SeqCst), 100);
}

#[tokio::test(start_paused = true)]
async fn a_full_buffer_does_not_keep_the_helper_alive_past_the_age_limit() {
    let mut config = spec_config();
    config.speculation.max_buffered_events = 2;
    config.speculation.max_age = SECOND;
    let provider = Counting::new(100);
    let rig = Rig::with_provider(config, provider.clone(), |b, _| b).await;
    candidate(&rig, 1, "count").await;
    rig.settle(500 * MS).await;
    assert!(
        !provider.closed.load(Ordering::SeqCst),
        "the helper is blocked on its full buffer, still holding the stream"
    );
    // Past the age limit the blocked helper ends and drops the provider stream.
    rig.settle(2 * SECOND).await;
    assert!(
        provider.closed.load(Ordering::SeqCst),
        "the helper held the provider stream open past max_age"
    );
    // The same words arrive late: expired, the turn runs afresh.
    rig.turn(1, "count").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(rig.conversation(), ["user:count", "assistant:Fresh."]);
    let spec = stats(&rig);
    assert_eq!((spec.expired, spec.hits), (1, 0), "{spec:?}");
}

#[tokio::test(start_paused = true)]
async fn a_runner_with_a_flow_never_speculates() {
    let restaurant = Restaurant::new(&[]);
    let flow = restaurant_flow(Language::En, &restaurant).unwrap();
    let registry = Arc::clone(flow.registry());
    let rig = Rig::new(spec_config(), move |b, clock| {
        let shared: SharedClock = Arc::new(*clock);
        let engine = Arc::new(ToolEngine::new(
            Arc::clone(&registry),
            SessionId::new(7),
            shared.clone(),
            brisvox_agent::tools::EngineConfig::default(),
        ));
        b.with_tools(registry, engine)
            .with_flow(FlowManager::new(Arc::new(flow), shared))
            .unwrap()
    })
    .await;
    rig.llm.push(LlmScript::text(["Welcome."]));
    let before = rig.conversation();
    candidate(&rig, 1, "hello there").await;
    rig.settle(SECOND).await;
    let spec = stats(&rig);
    assert_eq!((spec.skipped_flow, spec.started), (1, 0), "{spec:?}");
    assert_eq!(requests(&rig), 0, "no request behind the flow's back");
    assert_eq!(rig.conversation(), before, "the context is untouched");
    // The turn runs as it always does.
    rig.turn(1, "hello there").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(requests(&rig), 1);
    assert_eq!(stats(&rig).hits, 0);
    rig.context_copy().validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_candidate_during_the_previous_answer_is_skipped_and_the_live_round_is_left_alone() {
    let rig = Rig::new(spec_config(), |b, _| b).await;
    rig.llm.push(
        LlmScript::text(["Still ", "talking."]).with_fault(LlmFault::Stall {
            before_event: 1,
            duration: 3 * SECOND,
        }),
    );
    rig.turn(1, "tell me").await;
    rig.settle(200 * MS).await;
    candidate(&rig, 2, "and another thing").await;
    rig.settle(200 * MS).await;
    let spec = stats(&rig);
    assert_eq!((spec.skipped_busy, spec.started), (1, 0), "{spec:?}");
    let llm = rig.stats.snapshot();
    assert_eq!((llm.halts, llm.partial_messages), (0, 0), "{llm:?}");
    // The live answer completes untouched.
    rig.until_label("llm_response_ended").await;
    assert_eq!(
        rig.conversation(),
        ["user:tell me", "assistant:Still talking."]
    );
    assert_eq!(requests(&rig), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_candidate_is_skipped_when_the_round_task_will_not_stop() {
    let provider = ActLlm::default();
    provider.push(Act::Block(1_500 * MS, "late words".into()));
    let mut config = spec_config();
    config.timeouts.join = 400 * MS;
    let rig = Rig::with_provider(config, provider.clone(), |b, _| b).await;
    rig.turn(1, "q1").await;
    // Real time: the blocked request holds its thread, so nothing else makes progress.
    for _ in 0..5_000 {
        if provider.entered() == 1 {
            break;
        }
        std::thread::sleep(MS);
    }
    assert_eq!(provider.entered(), 1);
    let before = rig.conversation();
    rig.interrupt(2).await;
    rig.until("the join to time out", |r| {
        r.stats.snapshot().join_timeouts == 1
    })
    .await;
    // A candidate while the old task is stuck: the context cannot be settled, nothing starts.
    candidate(&rig, 2, "q2").await;
    rig.until("the candidate to be skipped", |r| {
        stats(r).skipped_unsafe == 1
    })
    .await;
    let spec = stats(&rig);
    assert_eq!((spec.started, spec.skipped_unsafe), (0, 1), "{spec:?}");
    assert_eq!(provider.entered(), 1, "no speculative request");
    assert_eq!(rig.conversation(), before, "the context was not touched");
    // Let the stuck task end before the rig goes away.
    rig.settle(2 * SECOND).await;
    assert_eq!(rig.conversation(), before);
}

#[tokio::test(start_paused = true)]
async fn a_provider_that_panics_in_the_speculation_leaves_a_turn_that_retries() {
    let provider = ActLlm::default();
    provider.push(Act::Panic);
    provider.push(Act::Text("Recovered.".into()));
    let rig = Rig::with_provider(spec_config(), provider.clone(), |b, _| b).await;
    candidate(&rig, 1, "hello").await;
    rig.settle(SECOND).await;
    assert_eq!(provider.entered(), 1);
    assert!(rig.labels().is_empty(), "nothing leaked downstream");
    rig.turn(1, "hello").await;
    rig.until_label("llm_response_ended").await;
    // The committed round saw a closed channel, took the cut-off path and retried once.
    assert_eq!(rig.conversation(), ["user:hello", "assistant:Recovered."]);
    let llm = rig.stats.snapshot();
    assert_eq!((llm.streams_cut_off, llm.retries), (1, 1), "{llm:?}");
    assert_eq!(provider.entered(), 2);
    rig.context_copy().validate().unwrap();
}

/// A source that answers every question with the same heard text.
struct HeardPrefix(&'static str);

impl TruncationSource for HeardPrefix {
    fn cut<'a>(&'a self, _request: &'a CutRequest) -> BoxFuture<'a, Cut> {
        let text = self.0;
        Box::pin(async move {
            Cut::Heard(Truncation {
                text: Arc::from(text),
                estimated: false,
                words_heard: text.split_whitespace().count(),
                unmatched_words: 0,
                gaps: 0,
                played: Duration::ZERO,
            })
        })
    }
}

#[tokio::test(start_paused = true)]
async fn the_request_of_a_speculation_carries_only_the_played_text_of_an_interrupted_answer() {
    let run = |speculate: bool| async move {
        let config = if speculate {
            spec_config()
        } else {
            EngineConfig::DEFAULT
        };
        let rig = Rig::new(config, |b, _| {
            b.with_truncation(Arc::new(HeardPrefix("Part")))
        })
        .await;
        rig.llm.push(
            LlmScript::text(["Part", " and the unplayed rest."]).with_fault(LlmFault::Stall {
                before_event: 1,
                duration: 5 * SECOND,
            }),
        );
        rig.llm.push(LlmScript::text(["Second."]));
        rig.turn(1, "a").await;
        rig.settle(200 * MS).await;
        rig.interrupt(2).await;
        rig.settle(100 * MS).await;
        if speculate {
            candidate(&rig, 2, "b").await;
            rig.settle(100 * MS).await;
            // The speculative request was built after the cut: only what was heard.
            let sent = rig.llm.requests();
            assert_eq!(sent.len(), 2);
            let texts: Vec<&str> = sent[1].messages.iter().map(|m| &*m.content).collect();
            assert_eq!(texts, ["a", "Part", "b"]);
        }
        rig.turn(2, "b").await;
        // The interrupted first answer never ended; the second one does.
        rig.until_label("llm_response_ended").await;
        (rig.conversation(), rig.llm.requests(), stats(&rig))
    };
    let (plain, plain_requests, _) = run(false).await;
    let (speculated, spec_requests, spec) = run(true).await;
    assert_eq!(
        plain,
        ["user:a", "assistant:Part", "user:b", "assistant:Second."]
    );
    assert_eq!(speculated, plain, "the final context equals the plain run");
    assert_eq!(spec_requests[1], plain_requests[1]);
    assert_eq!(spec.hits, 1, "{spec:?}");
}

#[tokio::test(start_paused = true)]
async fn a_cancel_counts_the_speculation_it_kills() {
    let rig = Rig::new(spec_config(), |b, _| b).await;
    rig.llm
        .push(LlmScript::text(["Some words that cost tokens."]));
    candidate(&rig, 1, "hello").await;
    rig.settle(100 * MS).await;
    send(&rig, SystemFrame::Cancel.into()).await;
    rig.settle(100 * MS).await;
    let spec = stats(&rig);
    assert_eq!(
        (spec.started, spec.abandoned, spec.hits),
        (1, 1, 0),
        "{spec:?}"
    );
    assert!(spec.wasted_tokens() >= 1, "{spec:?}");
}

#[tokio::test(start_paused = true)]
async fn the_timeline_has_the_first_token_while_a_committed_answer_is_still_streaming() {
    let rig = Rig::new(spec_config(), |b, _| b).await;
    rig.llm.push(
        LlmScript::text(["One ", "two ", "three"]).with_fault(LlmFault::Stall {
            before_event: 2,
            duration: 5 * SECOND,
        }),
    );
    candidate(&rig, 1, "count").await;
    rig.settle(200 * MS).await;
    rig.turn(1, "count").await;
    rig.settle(100 * MS).await;
    // The response is not done, but its first token is on the timeline already.
    assert!(!rig.labels().contains(&"llm_response_ended".to_owned()));
    let turn = rig.timeline.current().unwrap();
    assert!(turn.llm_round(0).unwrap().time_to_first_token.is_some());
    // A barge-in mid-response keeps it.
    rig.interrupt(2).await;
    rig.settle(SECOND).await;
    let turn = rig.timeline.current().unwrap();
    let round = turn.llm_round(0).unwrap();
    assert!(round.time_to_first_token.is_some());
}

#[tokio::test(start_paused = true)]
async fn a_discarded_malformed_speculation_does_not_cost_the_real_run_its_retry_budget() {
    // The default budget allows two malformed rounds per turn. The discarded speculation had one
    // of its own; the real run (two malformed rounds, then a good answer) must still succeed,
    // exactly as it would without speculation.
    let (rig, probe) = tool_rig(spec_config(), ToolKind::ReadOnly, 10 * MS).await;
    let malformed = || tool_script("Hmm.").with_fault(LlmFault::MalformedToolArguments);
    rig.llm.push(malformed());
    rig.llm.push(malformed());
    rig.llm.push(malformed());
    rig.llm.push(LlmScript::text(["Fixed."]));
    candidate(&rig, 1, "first words").await;
    rig.settle(SECOND).await;
    assert_eq!(requests(&rig), 1);
    rig.turn(1, "other words").await;
    rig.until_label("llm_response_ended").await;
    let llm = rig.stats.snapshot();
    assert_eq!(
        (llm.tool_budget_exhausted, llm.fallbacks),
        (0, 0),
        "{llm:?}"
    );
    assert_eq!(probe.started(), 0, "no malformed call ever ran");
    assert_eq!(
        rig.conversation().last().map(String::as_str),
        Some("assistant:Fixed.")
    );
    assert_eq!(requests(&rig), 4);
}

#[tokio::test(start_paused = true)]
async fn a_candidate_is_skipped_while_the_previous_answer_is_still_playing() {
    // The cut to what was heard is taken at the candidate and consumes the records: while the
    // playback position still moves, a plain turn would cut later, so nothing is speculated.
    let book = brisvox_engine::PlayoutBook::shared(8);
    let rig = Rig::new(spec_config(), {
        let book = Arc::clone(&book);
        move |b, _| b.with_book(book)
    })
    .await;
    rig.llm.push(LlmScript::text(["Hello there."]));
    rig.llm.push(LlmScript::text(["Second."]));
    rig.turn(1, "a").await;
    rig.until_label("llm_response_ended").await;
    // The answer's audio is handed to the transport but not played yet.
    book.register_utterance(brisvox_engine::RoundKey::new(ep(1), 0), 1);
    book.add_audio(1, SECOND);
    assert!(!book.is_settled_through(ep(2)));
    candidate(&rig, 2, "b").await;
    rig.settle(100 * MS).await;
    let spec = stats(&rig);
    assert_eq!((spec.skipped_busy, spec.started), (1, 0), "{spec:?}");
    assert_eq!(requests(&rig), 1);
    // Once the audio is played (all released), the next candidate speculates.
    book.mark_lost(1);
    assert!(book.is_settled_through(ep(2)));
    candidate(&rig, 2, "b").await;
    rig.settle(100 * MS).await;
    assert_eq!(stats(&rig).started, 1);
}

// ---- review fixes (second round) --------------------------------------------------------------------

/// A rig with speculation on and a `lookup` tool of `kind` that takes `delay` and asks for a
/// filler after `filler_after`.
async fn tool_rig_with_filler(
    config: EngineConfig,
    kind: ToolKind,
    delay: Duration,
    filler_after: Duration,
) -> (Rig, Arc<ToolProbe>) {
    let probe = Arc::new(ToolProbe::default());
    let rig = Rig::new(config, {
        let probe = Arc::clone(&probe);
        move |builder, clock| {
            let (registry, engine) = lookup_tools(clock, &probe, kind, delay, Some(filler_after));
            builder.with_tools(registry, engine)
        }
    })
    .await;
    (rig, probe)
}

#[tokio::test(start_paused = true)]
async fn a_committed_speculative_tool_phase_speaks_the_filler_while_the_held_call_runs() {
    // The held SideEffect call takes 2 s once the commit runs it, and the tool asks for a filler
    // after 500 ms: the caller hears "One moment" meanwhile, exactly as without speculation.
    let (rig, probe) =
        tool_rig_with_filler(spec_config(), ToolKind::SideEffect, 2 * SECOND, 500 * MS).await;
    rig.llm.push(tool_script("Booking."));
    rig.llm.push(LlmScript::text(["Done."]));
    candidate(&rig, 1, "book a table").await;
    rig.settle(2 * SECOND).await;
    assert_eq!(probe.started(), 0, "held until the commit");
    assert!(rig.labels().is_empty());

    let committed_at = rig.clock.now();
    rig.turn(1, "book a table").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(
        rig.labels(),
        [
            "user_turn_ended",
            "llm_response_started",
            "text:Booking.",
            "flush", // the text before the tool, spoken while the held call runs
            "llm_response_started", // the filler is a round of its own
            "text:One moment, please.",
            "flush",
            "llm_response_started",
            "text:Done.",
            "llm_response_ended",
        ]
    );
    assert_eq!(rig.stats.snapshot().fillers, 1);
    assert_eq!((probe.started(), probe.finished()), (1, 1));
    assert!(
        rig.clock.now() - committed_at >= 2 * SECOND,
        "the answer waited for the booking"
    );
    // The spoken filler is part of the conversation, after the result.
    assert_eq!(
        rig.conversation(),
        [
            "user:book a table",
            "assistant+calls[lookup]:Booking.",
            "tool:{\"answer\":\"sunny\"}",
            "assistant:One moment, please.",
            "assistant:Done."
        ]
    );
    rig.context_copy().validate().unwrap();
    assert_eq!(stats(&rig).hits, 1);
}

#[tokio::test(start_paused = true)]
async fn a_read_only_call_that_finished_during_the_speculation_needs_no_filler() {
    // The ReadOnly call ran (100 ms) while the turn was still waiting: at the commit its result
    // is there, so no filler is due even though the tool would ask for one after 500 ms.
    let (rig, probe) =
        tool_rig_with_filler(spec_config(), ToolKind::ReadOnly, 100 * MS, 500 * MS).await;
    rig.llm.push(tool_script("Let me look."));
    rig.llm.push(LlmScript::text(["It is sunny."]));
    candidate(&rig, 1, "what is the weather").await;
    rig.settle(SECOND).await;
    assert_eq!((probe.started(), probe.finished()), (1, 1));
    rig.turn(1, "what is the weather").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(rig.stats.snapshot().fillers, 0);
    assert!(
        !rig.labels().iter().any(|l| l.contains("One moment")),
        "{:?}",
        rig.labels()
    );
}

#[tokio::test(start_paused = true)]
async fn a_side_effect_call_still_runs_after_a_miss_whose_speculation_had_held_the_same_call() {
    // The speculation (for "book a table for two") held `call_1`; the caller's final words were
    // different, so it was discarded. The plain run of the turn issues the SAME call id `call_1`
    // (ids are per epoch and sequence) and its SideEffect must run: a held call must leave no
    // trace in the idempotency record.
    let (rig, probe) = tool_rig(spec_config(), ToolKind::SideEffect, 100 * MS).await;
    rig.llm.push(tool_script("Booking for two."));
    rig.llm.push(tool_script("Booking for four."));
    rig.llm.push(LlmScript::text(["Done."]));
    candidate(&rig, 1, "book a table for two").await;
    rig.settle(SECOND).await;
    let spec = stats(&rig);
    assert_eq!(
        (spec.held_side_effects, probe.started()),
        (1, 0),
        "{spec:?}"
    );

    rig.turn(1, "book a table for four").await;
    rig.until_label("llm_response_ended").await;
    rig.settle(5 * SECOND).await;
    let spec = stats(&rig);
    assert_eq!((spec.misses, spec.hits), (1, 0), "{spec:?}");
    assert_eq!(
        (probe.started(), probe.finished()),
        (1, 1),
        "the booking of the plain run happened, once"
    );
    assert_eq!(
        rig.conversation(),
        [
            "user:book a table for four",
            "assistant+calls[lookup]:Booking for four.",
            "tool:{\"answer\":\"sunny\"}",
            "assistant:Done."
        ]
    );
    assert_eq!(requests(&rig), 3);
    rig.context_copy().validate().unwrap();
    assert_eq!(rig.stats.snapshot().validate_failures, 0);
}

#[tokio::test(start_paused = true)]
async fn a_helper_whose_provider_never_answers_times_out_and_the_turn_retries() {
    let mut config = spec_config();
    config.timeouts.llm_first_token = 300 * MS;
    let rig = Rig::new(config, |b, _| b).await;
    // The first request stalls far past the first-token limit; the retry answers.
    rig.llm
        .push(LlmScript::text(["never"]).with_fault(LlmFault::Stall {
            before_event: 0,
            duration: 60 * SECOND,
        }));
    rig.llm.push(LlmScript::text(["Recovered."]));
    candidate(&rig, 1, "hello").await;
    rig.settle(SECOND).await;
    let llm = rig.stats.snapshot();
    assert_eq!(llm.first_token_timeouts, 1, "{llm:?}");
    assert_eq!(stats(&rig).failures, 1);
    assert!(rig.labels().is_empty(), "nothing leaked downstream");

    rig.turn(1, "hello").await;
    rig.until_label("llm_response_ended").await;
    // The committed round found the speculative attempt failed before any output and retried.
    assert_eq!(rig.conversation(), ["user:hello", "assistant:Recovered."]);
    let llm = rig.stats.snapshot();
    assert_eq!((llm.retries, llm.failed_rounds), (1, 0), "{llm:?}");
    assert_eq!(requests(&rig), 2);
    rig.context_copy().validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_helper_whose_stream_goes_quiet_after_output_times_out_and_the_partial_text_stays() {
    let mut config = spec_config();
    config.timeouts.llm_idle = 300 * MS;
    let rig = Rig::new(config, |b, _| b).await;
    rig.llm.push(
        LlmScript::text(["Half of", " an answer"]).with_fault(LlmFault::Stall {
            before_event: 1,
            duration: 60 * SECOND,
        }),
    );
    candidate(&rig, 1, "hello").await;
    rig.settle(SECOND).await;
    let llm = rig.stats.snapshot();
    assert_eq!(
        (llm.idle_timeouts, llm.first_token_timeouts),
        (1, 0),
        "{llm:?}"
    );
    assert_eq!(stats(&rig).failures, 1);
    assert!(rig.labels().is_empty(), "buffered, not spoken");

    rig.turn(1, "hello").await;
    rig.until_label("llm_response_ended").await;
    // Output was buffered: the partial text is kept and the round is a failed one, no retry.
    assert_eq!(rig.conversation(), ["user:hello", "assistant:Half of"]);
    let llm = rig.stats.snapshot();
    assert_eq!((llm.failed_rounds, llm.retries), (1, 0), "{llm:?}");
    assert_eq!(requests(&rig), 1);
}

// ---- language switches ----------------------------------------------------------------------

fn tracker() -> Arc<LanguageTracker> {
    let profiles = vec![
        LanguageProfile::new("en", Language::En, "en-voice"),
        LanguageProfile::new("tr", Language::Tr, "tr-voice"),
    ];
    Arc::new(LanguageTracker::new(Arc::new(
        LanguageSettings::new(profiles, "en").unwrap(),
    )))
}

/// The caller said two Turkish utterances: the tracker switches.
fn caller_switches_to_turkish(tracker: &LanguageTracker) {
    for text in ["bana yardım eder misin", "siparişim nerede acaba"] {
        let mut utterance = Utterance::new();
        utterance.push(text, Some(&SpokenLanguage::new("tr")));
        let _ = tracker.observe(&utterance);
    }
    assert_eq!(tracker.current_code(), "tr");
}

fn system_of(request: &LlmRequest) -> String {
    request.messages.first().unwrap().content.to_string()
}

#[tokio::test(start_paused = true)]
async fn a_speculation_started_before_a_language_switch_is_discarded_and_the_turn_asks_again() {
    let tracker = tracker();
    let rig = Rig::new(spec_config(), {
        let tracker = Arc::clone(&tracker);
        move |b, _| b.with_system_prompt("Be brief.").with_languages(tracker)
    })
    .await;
    rig.llm.push(LlmScript::text(["Of course."])); // the speculative answer, in English
    rig.llm.push(LlmScript::text(["Tabii."])); // the real answer
    candidate(&rig, 1, "siparişim nerede acaba").await;
    rig.settle(100 * MS).await;
    assert_eq!(requests(&rig), 1);

    // The turn's own final reaches the language tracker before the turn ends: this very turn
    // makes the second Turkish utterance in a row.
    caller_switches_to_turkish(&tracker);
    rig.turn(1, "siparişim nerede acaba").await;
    rig.until_label("llm_response_ended").await;

    let spec = stats(&rig);
    assert_eq!(
        (spec.started, spec.hits, spec.misses),
        (1, 0, 1),
        "{spec:?}"
    );
    let sent = rig.llm.requests();
    assert_eq!(sent.len(), 2, "the turn asked again");
    assert!(system_of(&sent[0]).ends_with("Reply in English."));
    assert!(system_of(&sent[1]).ends_with("Reply in Turkish."));
    // What was said is in the context, with no trace of the pinned line.
    assert_eq!(
        rig.conversation(),
        [
            "system:Be brief.",
            "user:siparişim nerede acaba",
            "assistant:Tabii."
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_switch_after_the_turn_ended_is_for_the_next_turn_and_the_speculation_stands() {
    let tracker = tracker();
    let rig = Rig::new(spec_config(), {
        let tracker = Arc::clone(&tracker);
        move |b, _| b.with_system_prompt("Be brief.").with_languages(tracker)
    })
    .await;
    for reply in ["Of course.", "Tabii."] {
        rig.llm.push(LlmScript::text([reply]));
    }
    candidate(&rig, 1, "where is my order").await;
    rig.settle(100 * MS).await;
    rig.turn(1, "where is my order").await;
    rig.settle(5 * MS).await;
    // The language of epoch 1 was locked when its speculation was judged.
    caller_switches_to_turkish(&tracker);
    rig.until_label("llm_response_ended").await;
    let spec = stats(&rig);
    assert_eq!((spec.hits, spec.misses), (1, 0), "{spec:?}");

    rig.turn(2, "siparişim nerede acaba").await;
    rig.settle(100 * MS).await;
    let sent = rig.llm.requests();
    assert_eq!(sent.len(), 2);
    assert!(system_of(&sent[0]).ends_with("Reply in English."));
    assert!(system_of(&sent[1]).ends_with("Reply in Turkish."));
}

/// A slow tool and a rig whose call has two languages; `turkish` switches the tracker first.
async fn filler_run(turkish: bool) -> Vec<String> {
    let tracker = tracker();
    if turkish {
        caller_switches_to_turkish(&tracker);
    }
    let probe = Arc::new(ToolProbe::default());
    let rig = Rig::new(EngineConfig::DEFAULT, {
        let (tracker, probe) = (Arc::clone(&tracker), Arc::clone(&probe));
        move |builder, clock| {
            let (registry, engine) = lookup_tools(
                clock,
                &probe,
                ToolKind::ReadOnly,
                3 * SECOND,
                Some(500 * MS),
            );
            builder.with_tools(registry, engine).with_languages(tracker)
        }
    })
    .await;
    rig.llm.push(tool_script("Let me look."));
    rig.llm.push(LlmScript::text(["Done."]));
    rig.turn(1, "find my table").await;
    rig.settle(6 * SECOND).await;
    rig.labels()
}

#[tokio::test(start_paused = true)]
async fn the_tool_filler_is_spoken_in_the_default_language_and_skipped_in_another() {
    let english = filler_run(false).await;
    assert!(
        english
            .iter()
            .any(|l| l.contains(brisvox_engine::DEFAULT_FILLER)),
        "{english:?}"
    );
    let turkish = filler_run(true).await;
    assert!(
        !turkish
            .iter()
            .any(|l| l.contains(brisvox_engine::DEFAULT_FILLER)),
        "{turkish:?}"
    );
    assert!(turkish.iter().any(|l| l.contains("Done.")), "{turkish:?}");
}
