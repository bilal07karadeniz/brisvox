//! `LlmRunner`: the round task, the assembler, the tool loop with fillers, the
//! per-round timeline, barge-in, errors and retries, the played-text cut through a
//! `TruncationSource`, the offload of large responses and the flow hook. The LLM is the
//! simulator's `MockLlm`; time is a paused `SimClock`.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods
)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_agent::Truncation;
use brisvox_agent::context::Message;
use brisvox_agent::flows::FlowManager;
use brisvox_agent::flows::example::{Restaurant, restaurant_flow};
use brisvox_agent::segment::Language;
use brisvox_agent::tools::{SessionId, ToolEngine, ToolKind};
use brisvox_core::frame::{ControlFrame, ControlKind, Frame, SystemFrame};
use brisvox_core::{Clock, SharedClock, SimClock, TurnEpoch};
use brisvox_engine::{
    BoxFuture, Cut, CutRequest, EngineConfig, LlmRunner, Offload, PlayoutBook, RoundKey,
    TruncationSource,
};
use brisvox_providers::{LlmProvider, LlmRequest, LlmStream, Role};
use brisvox_sim::mock_llm::{LlmFault, LlmScript, MockLlm};
use common::MS;
use common::llm::{Act, ActLlm, Gate, Rig, ToolProbe, lookup_tools};

const SECOND: Duration = Duration::from_secs(1);

fn tool_script(text: &str) -> LlmScript {
    LlmScript::text([text]).with_tool_call("call_1", "lookup", r#"{"q":"weather"}"#)
}

fn requests(rig: &Rig) -> Vec<LlmRequest> {
    rig.llm.requests()
}

fn roles(request: &LlmRequest) -> Vec<Role> {
    request.messages.iter().map(|m| m.role).collect()
}

fn count<P: LlmProvider>(rig: &Rig<P>, label: &str) -> usize {
    rig.labels().iter().filter(|l| *l == label).count()
}

/// A rig whose runner has the `lookup` tool (and optionally a playout book).
async fn tool_rig(
    config: EngineConfig,
    kind: ToolKind,
    delay: Duration,
    filler_after: Option<Duration>,
    book: Option<Arc<PlayoutBook>>,
) -> (Rig, Arc<ToolProbe>) {
    let probe = Arc::new(ToolProbe::default());
    let rig = Rig::new(config, {
        let probe = Arc::clone(&probe);
        move |builder, clock| {
            let (registry, engine) = lookup_tools(clock, &probe, kind, delay, filler_after);
            let builder = builder.with_tools(registry, engine);
            match book {
                Some(book) => builder.with_book(book),
                None => builder,
            }
        }
    })
    .await;
    (rig, probe)
}

// ---- a plain turn ----------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_plain_turn_streams_each_delta_at_once_and_pushes_one_assistant_message() {
    let mut rig = Rig::default().await;
    rig.llm.push(LlmScript::text(["  Hello", " there."]));
    rig.turn(1, "hi").await;
    rig.until_label("llm_response_ended").await;

    assert_eq!(
        rig.labels(),
        [
            "user_turn_ended",
            "llm_response_started",
            "text:Hello", // the leading whitespace of the response is stripped once
            "text: there.",
            "llm_response_ended",
        ]
    );
    assert_eq!(rig.conversation(), ["user:hi", "assistant:Hello there."]);
    rig.context_copy().validate().unwrap();
    let sent = requests(&rig);
    assert_eq!(sent.len(), 1);
    assert_eq!(roles(&sent[0]), [Role::User]);
    assert!(
        rig.upstream_frames().is_empty(),
        "the bot spoke: no upstream end"
    );
    let stats = rig.stats.snapshot();
    assert_eq!((stats.turns, stats.rounds, stats.requests), (1, 1, 1));
    assert_eq!((stats.finishes_inline, stats.silent_turns), (1, 0));
    assert_eq!(stats.failed_rounds, 0);
}

#[tokio::test(start_paused = true)]
async fn leading_whitespace_is_stripped_once_even_across_whitespace_only_deltas() {
    let rig = Rig::default().await;
    rig.llm.push(LlmScript::text(["  ", "\n Hi", " you"]));
    rig.turn(1, "x").await;
    rig.until_label("llm_response_ended").await;
    let texts: Vec<String> = rig
        .labels()
        .into_iter()
        .filter(|l| l.starts_with("text:"))
        .collect();
    assert_eq!(texts, ["text:Hi", "text: you"]);
    assert_eq!(rig.conversation(), ["user:x", "assistant:Hi you"]);
}

#[tokio::test(start_paused = true)]
async fn the_timeline_records_the_request_first_token_and_done_of_a_round() {
    let rig = Rig::default().await;
    // The first delta comes 300 ms after the request, the second 200 ms after that, `Done` at
    // once: first token at 300 ms, done at 500 ms. A mark stamped at the wrong place (the
    // request, `Done`, the end of the finish) would read differently.
    rig.llm.push(
        LlmScript::text(["Hel", "lo."])
            .with_fault(LlmFault::Stall {
                before_event: 0,
                duration: 300 * MS,
            })
            .with_fault(LlmFault::Stall {
                before_event: 1,
                duration: 200 * MS,
            }),
    );
    rig.turn(1, "hi").await;
    rig.until_label("llm_response_ended").await;
    let turn = rig.timeline.current().unwrap();
    assert_eq!(turn.llm_round_count(), 1);
    let round = turn.llm_round(0).unwrap();
    assert_eq!(round.time_to_first_token, Some(300 * MS));
    assert_eq!(round.duration, Some(500 * MS));
}

#[tokio::test(start_paused = true)]
async fn a_retried_request_is_one_timeline_round_stamped_at_the_first_request() {
    let mut config = EngineConfig::DEFAULT;
    config.llm.retry_delay = 100 * MS;
    let rig = Rig::new(config, |b, _| b).await;
    rig.llm
        .push(LlmScript::text(["x"]).with_fault(LlmFault::ErrorResponse { status: 503 }));
    rig.llm.push(LlmScript::text(["Recovered."]));
    rig.turn(1, "hi").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(rig.stats.snapshot().retries, 1);
    let turn = rig.timeline.current().unwrap();
    assert_eq!(turn.llm_round_count(), 1, "a retry is not a new round");
    // The request mark is the first attempt's: the retry delay counts toward the first token.
    let round = turn.llm_round(0).unwrap();
    assert!(round.time_to_first_token.unwrap() >= 100 * MS, "{round:?}");
}

#[tokio::test(start_paused = true)]
async fn an_empty_response_ends_the_turn_and_tells_the_machine_upstream() {
    let mut rig = Rig::default().await;
    // The mock's fallback answer is empty: no text, no calls.
    rig.turn(1, "hi").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(
        rig.labels(),
        [
            "user_turn_ended",
            "llm_response_started",
            "llm_response_ended"
        ]
    );
    assert_eq!(
        rig.conversation(),
        ["user:hi"],
        "no empty assistant message"
    );
    let upstream = rig.upstream_frames();
    assert!(
        matches!(
            upstream.as_slice(),
            [Frame::Control(ControlFrame {
                epoch,
                kind: ControlKind::TtsEnded
            })] if *epoch == TurnEpoch::new(1)
        ),
        "{upstream:?}"
    );
    let stats = rig.stats.snapshot();
    assert_eq!((stats.silent_turns, stats.upstream_tts_ended), (1, 1));
}

#[tokio::test(start_paused = true)]
async fn a_turn_end_without_a_transcript_is_answered_upstream_and_starts_no_round() {
    let mut rig = Rig::default().await;
    rig.running
        .input()
        .send(
            ControlFrame {
                epoch: TurnEpoch::new(1),
                kind: ControlKind::UserTurnEnded,
            }
            .into(),
        )
        .await
        .unwrap();
    rig.settle(SECOND).await;
    assert_eq!(rig.labels(), ["user_turn_ended"]);
    assert_eq!(requests(&rig).len(), 0);
    assert_eq!(rig.upstream_frames().len(), 1);
    assert_eq!(rig.stats.snapshot().empty_turns, 1);
}

#[tokio::test(start_paused = true)]
async fn the_system_prompt_stays_the_first_message_of_every_request() {
    let rig = Rig::new(EngineConfig::DEFAULT, |b, _| {
        b.with_system_prompt("Be brief.")
    })
    .await;
    rig.llm.push(LlmScript::text(["One."]));
    rig.llm.push(LlmScript::text(["Two."]));
    rig.turn(1, "a").await;
    rig.until_label("llm_response_ended").await;
    rig.turn(2, "b").await;
    rig.until("second answer", |r| count(r, "llm_response_ended") == 2)
        .await;
    let sent = requests(&rig);
    assert_eq!(sent.len(), 2);
    for request in &sent {
        assert_eq!(request.messages[0].role, Role::System);
        assert_eq!(&*request.messages[0].content, "Be brief.");
    }
    assert_eq!(
        roles(&sent[1]),
        [Role::System, Role::User, Role::Assistant, Role::User]
    );
}

#[tokio::test(start_paused = true)]
async fn the_provider_is_warmed_at_start() {
    struct Warm(MockLlm, Arc<AtomicUsize>);
    impl LlmProvider for Warm {
        fn name(&self) -> &'static str {
            "warm"
        }
        async fn stream(&self, request: LlmRequest) -> brisvox_providers::Result<LlmStream> {
            self.0.stream(request).await
        }
        async fn warm(&self) -> brisvox_providers::Result<()> {
            self.1.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    let clock = SimClock::new();
    let shared: SharedClock = Arc::new(clock);
    let warmed = Arc::new(AtomicUsize::new(0));
    let runner = LlmRunner::builder(
        Arc::new(Warm(MockLlm::new(shared.clone()), Arc::clone(&warmed))),
        &EngineConfig::DEFAULT,
        shared,
        brisvox_core::TimelineRecorder::new(),
    )
    .build();
    let stats = runner.stats();
    let _running = brisvox_core::Pipeline::new("warm")
        .then(runner)
        .start(
            brisvox_core::Links::default(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(warmed.load(Ordering::SeqCst), 1);
    assert_eq!(stats.snapshot().warms, 1);
}

// ---- the tool loop ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_tool_turn_runs_the_tool_between_two_rounds() {
    let book = PlayoutBook::shared(16);
    let (rig, probe) = tool_rig(
        EngineConfig::DEFAULT,
        ToolKind::ReadOnly,
        100 * MS,
        None,
        Some(Arc::clone(&book)),
    )
    .await;
    rig.llm.push(tool_script("Let me check."));
    rig.llm.push(LlmScript::text(["It is sunny."]));
    rig.turn(1, "weather?").await;
    rig.until_label("llm_response_ended").await;

    assert_eq!(
        rig.labels(),
        [
            "user_turn_ended",
            "llm_response_started",
            "text:Let me check.",
            "flush",
            "llm_response_started",
            "text:It is sunny.",
            "llm_response_ended",
        ]
    );
    assert_eq!(probe.started(), 1);
    let conversation = rig.conversation();
    assert_eq!(conversation[0], "user:weather?");
    assert_eq!(conversation[1], "assistant+calls[lookup]:Let me check.");
    assert!(conversation[2].starts_with("tool:"), "{conversation:?}");
    assert!(conversation[2].contains("sunny"));
    assert_eq!(conversation[3], "assistant:It is sunny.");
    rig.context_copy().validate().unwrap();

    // The second request replays the assistant message with the provider's call id and the
    // result under the same id.
    let sent = requests(&rig);
    assert_eq!(sent.len(), 2);
    assert_eq!(roles(&sent[1]), [Role::User, Role::Assistant, Role::Tool]);
    assert_eq!(&*sent[1].messages[1].tool_calls[0].id, "call_1");
    assert_eq!(&*sent[1].messages[1].tool_calls[0].name, "lookup");
    assert_eq!(
        &*sent[1].messages[1].tool_calls[0].arguments,
        r#"{"q":"weather"}"#
    );
    assert_eq!(sent[1].messages[2].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(sent[0].tools.len(), 1);
    assert_eq!(&*sent[0].tools[0].name, "lookup");

    // Two timed rounds, each with first token and done.
    let turn = rig.timeline.current().unwrap();
    assert_eq!(turn.llm_round_count(), 2);
    for round in turn.llm_rounds() {
        assert!(round.time_to_first_token.is_some() && round.duration.is_some());
    }

    // Both rounds are in the playout book, each linked to its context message.
    let first = book.snapshot(RoundKey::new(TurnEpoch::new(1), 0)).unwrap();
    let second = book.snapshot(RoundKey::new(TurnEpoch::new(1), 1)).unwrap();
    assert_eq!((first.msg_index, second.msg_index), (Some(1), Some(3)));

    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.tool_rounds, stats.tool_calls, stats.rounds),
        (1, 1, 2)
    );
}

#[tokio::test(start_paused = true)]
async fn a_slow_tool_gets_a_filler_round_that_enters_the_context_after_the_results() {
    let book = PlayoutBook::shared(16);
    let (rig, probe) = tool_rig(
        EngineConfig::DEFAULT,
        ToolKind::ReadOnly,
        2 * SECOND,
        Some(700 * MS),
        Some(Arc::clone(&book)),
    )
    .await;
    rig.llm.push(tool_script("Let me check."));
    rig.llm.push(LlmScript::text(["It is sunny."]));
    rig.turn(1, "weather?").await;
    rig.until_label("llm_response_ended").await;

    assert_eq!(
        rig.labels(),
        [
            "user_turn_ended",
            "llm_response_started",
            "text:Let me check.",
            "flush",
            "llm_response_started", // the filler is a round of its own
            "text:One moment, please.",
            "flush",
            "llm_response_started",
            "text:It is sunny.",
            "llm_response_ended",
        ]
    );
    assert_eq!(probe.finished(), 1);
    // The spoken filler is part of the conversation, after the tool result (never between
    // a call and its result) and before the answer; the second request did not end on it.
    let conversation = rig.conversation();
    assert_eq!(conversation.len(), 5, "{conversation:?}");
    assert_eq!(conversation[3], "assistant:One moment, please.");
    assert_eq!(conversation[4], "assistant:It is sunny.");
    assert!(
        requests(&rig)[1]
            .messages
            .iter()
            .all(|m| !m.content.contains("One moment")),
        "the filler is placed after the request that followed it was sent"
    );
    rig.context_copy().validate().unwrap();
    // The filler has its own ordinal and its own message (cut to what was heard like any
    // round); the model's second round follows.
    let filler = book.snapshot(RoundKey::new(TurnEpoch::new(1), 1)).unwrap();
    assert_eq!(filler.msg_index, Some(3));
    let second = book.snapshot(RoundKey::new(TurnEpoch::new(1), 2)).unwrap();
    assert_eq!(second.msg_index, Some(4));
    assert_eq!(rig.stats.snapshot().fillers_in_context, 1);
    // The timeline times only the model's two requests.
    assert_eq!(rig.timeline.current().unwrap().llm_round_count(), 2);
    assert_eq!(rig.stats.snapshot().fillers, 1);
}

#[tokio::test(start_paused = true)]
async fn a_fast_tool_gets_no_filler() {
    let (rig, _) = tool_rig(
        EngineConfig::DEFAULT,
        ToolKind::ReadOnly,
        100 * MS,
        Some(700 * MS),
        None,
    )
    .await;
    rig.llm.push(tool_script("One sec."));
    rig.llm.push(LlmScript::text(["Done."]));
    rig.turn(1, "go").await;
    rig.until_label("llm_response_ended").await;
    assert!(rig.labels().iter().all(|l| !l.contains("One moment")));
    assert_eq!(rig.stats.snapshot().fillers, 0);
}

#[tokio::test(start_paused = true)]
async fn the_number_of_tool_rounds_is_limited_and_a_fallback_line_is_spoken() {
    let mut config = EngineConfig::DEFAULT;
    config.llm.max_tool_rounds = 2;
    let (rig, probe) = tool_rig(config, ToolKind::ReadOnly, 10 * MS, None, None).await;
    for _ in 0..4 {
        rig.llm.push(tool_script("Again."));
    }
    rig.turn(1, "loop").await;
    rig.until_label("llm_response_ended").await;

    assert_eq!(probe.started(), 2, "only two rounds ran their tools");
    let labels = rig.labels();
    let n = labels.len();
    // The text of the over-limit round ("Again.") is flushed before the fallback line starts, so
    // the two are separate utterances.
    assert_eq!(
        labels[n - 5..],
        [
            "text:Again.".to_owned(),
            "flush".to_owned(),
            "llm_response_started".to_owned(),
            format!("text:{}", brisvox_engine::DEFAULT_FALLBACK),
            "llm_response_ended".to_owned()
        ]
    );
    // Only the two model rounds that ran tools and the over-limit one are timed; the fallback
    // line is not a request.
    assert_eq!(rig.timeline.current().unwrap().llm_round_count(), 3);
    let conversation = rig.conversation();
    assert_eq!(
        conversation.last().unwrap(),
        &format!("assistant:{}", brisvox_engine::DEFAULT_FALLBACK)
    );
    // The call of the third response is answered without running, so the context is valid.
    assert!(
        conversation
            .iter()
            .any(|m| m.starts_with("tool!:") && m.contains("tool_round_limit")),
        "{conversation:?}"
    );
    rig.context_copy().validate().unwrap();
    let stats = rig.stats.snapshot();
    assert_eq!((stats.tool_round_limit_hits, stats.fallbacks), (1, 1));
    assert_eq!(requests(&rig).len(), 3);
}

#[tokio::test(start_paused = true)]
async fn the_engines_retry_budget_ends_the_tool_loop_with_the_fallback_line() {
    let (rig, probe) = tool_rig(
        EngineConfig::DEFAULT,
        ToolKind::ReadOnly,
        10 * MS,
        None,
        None,
    )
    .await;
    // Every response asks for the tool with arguments that are cut off: never valid JSON.
    for _ in 0..4 {
        rig.llm
            .push(tool_script("Hmm.").with_fault(LlmFault::MalformedToolArguments));
    }
    rig.turn(1, "go").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(probe.started(), 0, "no malformed call ever ran");
    let stats = rig.stats.snapshot();
    assert_eq!((stats.tool_budget_exhausted, stats.fallbacks), (1, 1));
    assert!(
        rig.labels()
            .contains(&format!("text:{}", brisvox_engine::DEFAULT_FALLBACK))
    );
    rig.context_copy().validate().unwrap();
    // Two retries were allowed, the third malformed round is the one that exhausts the budget.
    assert_eq!(requests(&rig).len(), 3);
}

#[tokio::test(start_paused = true)]
async fn calls_without_a_tool_engine_are_answered_with_errors() {
    let rig = Rig::default().await;
    rig.llm.push(tool_script("Let me look."));
    rig.llm.push(LlmScript::text(["I cannot."]));
    rig.turn(1, "go").await;
    rig.until_label("llm_response_ended").await;
    rig.context_copy().validate().unwrap();
    assert!(
        rig.conversation()
            .iter()
            .any(|m| m.starts_with("tool!:") && m.contains("unknown_tool"))
    );
    assert_eq!(rig.stats.snapshot().tools_unavailable, 1);
}

// ---- errors, stalls and retries -----------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_failed_request_is_retried_once_before_any_output() {
    let rig = Rig::default().await;
    rig.llm
        .push(LlmScript::text(["x"]).with_fault(LlmFault::ErrorResponse { status: 503 }));
    rig.llm.push(LlmScript::text(["Recovered."]));
    rig.turn(1, "hi").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(
        rig.labels(),
        [
            "user_turn_ended",
            "llm_response_started",
            "text:Recovered.",
            "llm_response_ended"
        ],
        "one LlmResponseStarted for the round, not one per attempt"
    );
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.retries, stats.request_errors, stats.requests),
        (1, 1, 2)
    );
    assert_eq!(stats.failed_rounds, 0);
}

#[tokio::test(start_paused = true)]
async fn without_retries_an_error_ends_the_turn_quietly_and_the_next_turn_recovers() {
    let mut config = EngineConfig::DEFAULT;
    config.llm.retries = 0;
    let mut rig = Rig::new(config, |b, _| b).await;
    rig.llm
        .push(LlmScript::text(["x"]).with_fault(LlmFault::ErrorResponse { status: 503 }));
    rig.llm.push(LlmScript::text(["Recovered."]));
    rig.turn(1, "first").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(
        rig.labels(),
        [
            "user_turn_ended",
            "llm_response_started",
            "llm_response_ended"
        ]
    );
    assert_eq!(rig.upstream_frames().len(), 1, "the machine hears TtsEnded");
    assert_eq!(rig.stats.snapshot().failed_rounds, 1);

    rig.turn(2, "second").await;
    rig.until("recovery", |r| count(r, "llm_response_ended") == 2)
        .await;
    assert_eq!(
        rig.conversation(),
        ["user:first", "user:second", "assistant:Recovered."]
    );
    rig.context_copy().validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn nothing_is_retried_after_output_and_the_partial_text_is_kept() {
    let mut rig = Rig::default().await;
    rig.llm
        .push(LlmScript::text(["Hel", "lo"]).with_fault(LlmFault::Disconnect { after_events: 1 }));
    rig.llm.push(LlmScript::text(["never used"]));
    rig.turn(1, "hi").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(
        rig.labels(),
        [
            "user_turn_ended",
            "llm_response_started",
            "text:Hel",
            "llm_response_ended"
        ]
    );
    assert_eq!(rig.conversation(), ["user:hi", "assistant:Hel"]);
    assert_eq!(requests(&rig).len(), 1, "no retry once the caller heard it");
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.retries, stats.failed_rounds, stats.stream_errors),
        (0, 1, 1)
    );
    assert!(
        rig.upstream_frames().is_empty(),
        "text was spoken: TTS ends the response"
    );
}

#[tokio::test(start_paused = true)]
async fn a_stalled_first_token_times_out_retries_once_and_ends_quietly() {
    let mut config = EngineConfig::DEFAULT;
    config.timeouts.llm_first_token = 200 * MS;
    let mut rig = Rig::new(config, |b, _| b).await;
    let stall = || {
        LlmScript::text(["late"]).with_fault(LlmFault::Stall {
            before_event: 0,
            duration: Duration::from_secs(30),
        })
    };
    rig.llm.push(stall());
    rig.llm.push(stall());
    rig.llm.push(LlmScript::text(["Back."]));
    rig.turn(1, "hi").await;
    rig.until_label("llm_response_ended").await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (
            stats.first_token_timeouts,
            stats.retries,
            stats.failed_rounds
        ),
        (2, 1, 1)
    );
    assert_eq!(rig.upstream_frames().len(), 1);
    assert_eq!(rig.conversation(), ["user:hi"]);

    rig.turn(2, "again").await;
    rig.until("recovery", |r| count(r, "llm_response_ended") == 2)
        .await;
    assert_eq!(rig.conversation().last().unwrap(), "assistant:Back.");
}

#[tokio::test(start_paused = true)]
async fn a_stream_that_goes_silent_after_output_ends_the_turn_with_the_partial_text() {
    let mut config = EngineConfig::DEFAULT;
    config.timeouts.llm_idle = 500 * MS;
    let rig = Rig::new(config, |b, _| b).await;
    rig.llm
        .push(LlmScript::text(["Hel", "lo"]).with_fault(LlmFault::Stall {
            before_event: 1,
            duration: Duration::from_secs(30),
        }));
    rig.turn(1, "hi").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(rig.conversation(), ["user:hi", "assistant:Hel"]);
    let stats = rig.stats.snapshot();
    assert_eq!((stats.idle_timeouts, stats.retries), (1, 0));
}

// ---- barge-in ----------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_barge_in_keeps_the_raw_text_so_far_stops_the_round_and_the_next_turn_goes_on() {
    let rig = Rig::default().await;
    rig.llm.push(
        LlmScript::text(["Hello ", "wor", "ld"]).with_fault(LlmFault::Stall {
            before_event: 2,
            duration: Duration::from_secs(30),
        }),
    );
    rig.turn(1, "hi").await;
    rig.until_label("text:wor").await;
    rig.interrupt(2).await;
    rig.until_label("interrupt").await;

    assert_eq!(rig.conversation(), ["user:hi", "assistant:Hello wor"]);
    rig.context_copy().validate().unwrap();
    let stats = rig.stats.snapshot();
    assert_eq!((stats.halts, stats.partial_messages), (1, 1));
    rig.settle(60 * SECOND).await;
    assert_eq!(count(&rig, "llm_response_ended"), 0, "the turn was cut");
    assert_eq!(
        rig.labels().last().map(String::as_str),
        Some("interrupt"),
        "nothing follows the interrupt: {:?}",
        rig.labels()
    );

    rig.llm.push(LlmScript::text(["Fine."]));
    rig.turn(2, "again").await;
    rig.until_label("llm_response_ended").await;
    let sent = requests(&rig);
    assert_eq!(sent.len(), 2);
    assert_eq!(roles(&sent[1]), [Role::User, Role::Assistant, Role::User]);
    assert_eq!(&*sent[1].messages[1].content, "Hello wor");
}

#[tokio::test(start_paused = true)]
async fn a_side_effect_tool_runs_once_when_the_caller_barges_in_during_it_and_its_late_note_comes_once()
 {
    let (rig, probe) = tool_rig(
        EngineConfig::DEFAULT,
        ToolKind::SideEffect,
        3 * SECOND,
        None,
        None,
    )
    .await;
    rig.llm.push(tool_script("Booking it."));
    rig.llm.push(LlmScript::text(["Okay."]));
    rig.llm.push(LlmScript::text(["Anything else?"]));
    rig.turn(1, "book").await;
    rig.until("tool started", |_| probe.started() == 1).await;
    rig.settle(500 * MS).await;
    rig.interrupt(2).await;
    rig.until_label("interrupt").await;

    // The open call was closed with an error result so the context is valid, and the tool body
    // is still running on its own task.
    let conversation = rig.conversation();
    assert_eq!(conversation.len(), 3, "{conversation:?}");
    assert_eq!(conversation[1], "assistant+calls[lookup]:Booking it.");
    assert!(
        conversation[2].starts_with("tool!:interrupted"),
        "{conversation:?}"
    );
    rig.context_copy().validate().unwrap();
    assert!(rig.context_copy().pending_tool_calls().is_empty());
    assert_eq!(probe.finished(), 0);
    let stats = rig.stats.snapshot();
    assert_eq!((stats.halts, stats.interrupted_tool_calls), (1, 1));

    // The tool completes after the barge-in; the next turn reports its result once.
    rig.settle(4 * SECOND).await;
    assert_eq!((probe.started(), probe.finished()), (1, 1));
    rig.turn(2, "hello").await;
    rig.until_label("llm_response_ended").await;
    let notes = |rig: &Rig| {
        rig.conversation()
            .iter()
            .filter(|m| m.contains("Late result of tool call"))
            .count()
    };
    assert_eq!(notes(&rig), 1, "{:?}", rig.conversation());
    let second = &requests(&rig)[1];
    assert!(
        second
            .messages
            .iter()
            .any(|m| m.role == Role::System && m.content.contains("sunny")),
        "the request of the next turn carries the note: {second:?}"
    );

    // And it is not reported again; the tool never ran a second time.
    rig.turn(3, "more").await;
    rig.until("third answer", |r| count(r, "llm_response_ended") == 2)
        .await;
    assert_eq!(notes(&rig), 1);
    assert_eq!(probe.started(), 1, "the SideEffect body ran exactly once");
    assert_eq!(rig.stats.snapshot().late_notes, 1);
    rig.context_copy().validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_read_only_tool_is_dropped_when_the_caller_barges_in() {
    let (rig, probe) = tool_rig(
        EngineConfig::DEFAULT,
        ToolKind::ReadOnly,
        3 * SECOND,
        None,
        None,
    )
    .await;
    rig.llm.push(tool_script("Looking."));
    rig.turn(1, "look").await;
    rig.until("tool started", |_| probe.started() == 1).await;
    rig.interrupt(2).await;
    rig.until_label("interrupt").await;
    rig.settle(5 * SECOND).await;
    assert_eq!(probe.finished(), 0, "the read-only future was dropped");
    rig.context_copy().validate().unwrap();
    assert!(rig.context_copy().pending_tool_calls().is_empty());
    // No late note for a read-only tool.
    rig.llm.push(LlmScript::text(["Hi."]));
    rig.turn(2, "next").await;
    rig.until_label("llm_response_ended").await;
    assert!(
        rig.conversation()
            .iter()
            .all(|m| !m.contains("Late result")),
        "{:?}",
        rig.conversation()
    );
}

// ---- played text ---------------------------------------------------------------------------------

/// A source that records what it is asked and answers by round ordinal.
struct Scripted {
    seen: Mutex<Vec<CutRequest>>,
    answers: fn(&CutRequest) -> Cut,
}

impl Scripted {
    fn new(answers: fn(&CutRequest) -> Cut) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::default(),
            answers,
        })
    }

    fn seen(&self) -> Vec<CutRequest> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl TruncationSource for Scripted {
    fn cut<'a>(&'a self, request: &'a CutRequest) -> BoxFuture<'a, Cut> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
        let answer = (self.answers)(request);
        Box::pin(async move { answer })
    }
}

fn heard(text: &str) -> Truncation {
    Truncation {
        text: Arc::from(text),
        estimated: false,
        words_heard: text.split_whitespace().count(),
        unmatched_words: 0,
        gaps: 0,
        played: Duration::ZERO,
    }
}

#[tokio::test(start_paused = true)]
async fn the_cut_runs_last_round_first_and_a_removed_message_does_not_shift_the_earlier_cut() {
    let source = Scripted::new(|request| match request.key.round {
        // The second round was never heard: its message goes away.
        1 => Cut::Heard(heard("")),
        // The first round was cut after "Let me".
        _ => Cut::Heard(heard("Let me")),
    });
    let probe = Arc::new(ToolProbe::default());
    let rig = Rig::new(EngineConfig::DEFAULT, {
        let (probe, source) = (Arc::clone(&probe), Arc::clone(&source));
        move |b, clock| {
            let (registry, engine) = lookup_tools(clock, &probe, ToolKind::ReadOnly, 10 * MS, None);
            b.with_tools(registry, engine).with_truncation(source)
        }
    })
    .await;
    rig.llm.push(tool_script("Let me check."));
    rig.llm.push(LlmScript::text(["It is sunny."]));
    rig.turn(1, "weather?").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(rig.conversation().len(), 4);
    assert!(
        source.seen().is_empty(),
        "nothing is cut before the next turn"
    );

    // The caller barged in; the cut happens when the next turn ends.
    rig.interrupt(2).await;
    rig.until_label("interrupt").await;
    rig.llm.push(LlmScript::text(["Sure."]));
    rig.turn(2, "stop").await;
    rig.until("answer", |r| count(r, "llm_response_ended") == 2)
        .await;

    let seen = source.seen();
    assert_eq!(
        seen.iter().map(|r| r.key.round).collect::<Vec<_>>(),
        [1, 0],
        "last round first"
    );
    assert!(seen.iter().all(|r| r.interrupted_at.is_some()));
    assert_eq!(&*seen[0].sent, "It is sunny.");
    assert_eq!(&*seen[1].sent, "Let me check.");
    let conversation = rig.conversation();
    assert_eq!(conversation[0], "user:weather?");
    assert_eq!(conversation[1], "assistant+calls[lookup]:Let me");
    assert!(conversation[2].starts_with("tool:"));
    assert_eq!(conversation[3], "user:stop");
    assert_eq!(conversation[4], "assistant:Sure.");
    rig.context_copy().validate().unwrap();
    // The request of turn two (the third request) was built from the cut context: the model sees
    // only what the caller heard, with the tool result still paired right after its call.
    let sent = requests(&rig);
    assert_eq!(sent.len(), 3);
    let third = &sent[2];
    assert_eq!(
        roles(third),
        [Role::User, Role::Assistant, Role::Tool, Role::User]
    );
    assert_eq!(&*third.messages[1].content, "Let me");
    assert_eq!(third.messages[1].tool_calls.len(), 1);
    assert!(
        third
            .messages
            .iter()
            .all(|m| !m.content.contains("It is sunny")),
        "the unheard second round must not reach the model: {third:?}"
    );
    assert_eq!(&*third.messages[3].content, "stop");
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.cuts_heard, stats.cuts_removed, stats.cuts_shortened),
        (2, 1, 1)
    );
    assert_eq!(rig.truncations.lock().unwrap().truncations, 2);
}

#[tokio::test(start_paused = true)]
async fn a_response_that_ended_by_itself_is_asked_about_without_an_interrupt_time() {
    let source = Scripted::new(|request| Cut::HeardInFull(heard(request.sent.trim_end())));
    let rig = Rig::new(EngineConfig::DEFAULT, {
        let source = Arc::clone(&source);
        move |b, _| b.with_truncation(source)
    })
    .await;
    rig.llm.push(LlmScript::text(["All of it. "]));
    rig.llm.push(LlmScript::text(["Next."]));
    rig.turn(1, "a").await;
    rig.until_label("llm_response_ended").await;
    rig.turn(2, "b").await;
    rig.until("second", |r| count(r, "llm_response_ended") == 2)
        .await;
    let seen = source.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].interrupted_at, None);
    // Heard in full: applied (trailing whitespace trimmed) and not counted as a cut.
    assert_eq!(rig.conversation()[1], "assistant:All of it.");
    assert_eq!(
        &*requests(&rig)[1].messages[1].content,
        "All of it.",
        "the next request carries the applied text"
    );
    let stats = rig.stats.snapshot();
    assert_eq!((stats.cuts_whole, stats.cuts_heard), (1, 0));
    assert_eq!(rig.truncations.lock().unwrap().truncations, 0);
}

#[tokio::test(start_paused = true)]
async fn the_default_source_keeps_every_message_as_written() {
    let rig = Rig::default().await;
    rig.llm.push(LlmScript::text(["A full answer."]));
    rig.llm.push(LlmScript::text(["Next."]));
    rig.turn(1, "a").await;
    rig.until_label("llm_response_ended").await;
    rig.interrupt(2).await;
    rig.turn(2, "b").await;
    rig.until("second", |r| count(r, "llm_response_ended") == 2)
        .await;
    assert_eq!(rig.conversation()[1], "assistant:A full answer.");
    assert_eq!(rig.stats.snapshot().cuts_kept, 1);
}

#[tokio::test(start_paused = true)]
async fn stop_applies_the_cut_before_it_is_forwarded() {
    let source = Scripted::new(|_| Cut::Heard(heard("Part")));
    let rig = Rig::new(EngineConfig::DEFAULT, {
        let source = Arc::clone(&source);
        move |b, _| b.with_truncation(source)
    })
    .await;
    rig.llm.push(LlmScript::text(["Part and more."]));
    rig.turn(1, "a").await;
    rig.until_label("llm_response_ended").await;
    rig.running
        .input()
        .send(SystemFrame::Stop.into())
        .await
        .unwrap();
    rig.until_label("stop").await;
    assert_eq!(rig.conversation()[1], "assistant:Part");
    assert_eq!(source.seen().len(), 1);
}

// ---- large responses --------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_large_response_is_finished_on_the_offload_and_a_small_one_inline() {
    let (rig, probe) = tool_rig(
        EngineConfig::DEFAULT,
        ToolKind::ReadOnly,
        10 * MS,
        None,
        None,
    )
    .await;
    let big = format!(r#"{{"q":"{}"}}"#, "x".repeat(6_000));
    rig.llm
        .push(LlmScript::text(["Checking."]).with_tool_call("call_big", "lookup", &big));
    rig.llm.push(LlmScript::text(["Done."]));
    rig.turn(1, "big").await;
    rig.until_label("llm_response_ended").await;
    let stats = rig.stats.snapshot();
    assert_eq!((stats.finishes_offloaded, stats.finishes_inline), (1, 1));
    assert_eq!(probe.started(), 1, "the big call parsed and ran");
    rig.context_copy().validate().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_a_real_pool_a_large_response_is_finished_and_its_tool_runs() {
    use brisvox_core::SystemClock;
    use brisvox_inference::{InferencePool, PoolConfig};
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 1,
            queue_capacity: 8,
            name: "llm-finish-test".to_owned(),
        },
        SystemClock::shared(),
    )
    .unwrap();
    let probe = Arc::new(ToolProbe::default());
    let handle = pool.handle();
    let rig = Rig::new(EngineConfig::DEFAULT, {
        let probe = Arc::clone(&probe);
        move |b, clock| {
            let (registry, engine) = lookup_tools(clock, &probe, ToolKind::ReadOnly, 5 * MS, None);
            b.with_tools(registry, engine)
                .with_offload(Offload::Pool(handle))
        }
    })
    .await;
    let big = format!(r#"{{"q":"{}"}}"#, "y".repeat(8_000));
    rig.llm
        .push(LlmScript::text(["Checking."]).with_tool_call("call_big", "lookup", &big));
    rig.llm.push(LlmScript::text(["Done."]));
    rig.turn(1, "big").await;
    rig.until_label("llm_response_ended").await;
    let stats = rig.stats.snapshot();
    assert_eq!((stats.finishes_offloaded, stats.finish_failures), (1, 0));
    assert_eq!(probe.started(), 1);
    drop(rig);
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
}

// ---- the flow hook -----------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_flow_supplies_the_node_prompt_and_tools_and_a_tool_result_enters_the_next_node() {
    let restaurant = Restaurant::new(&[]);
    let flow = restaurant_flow(Language::En, &restaurant).unwrap();
    let registry = Arc::clone(flow.registry());
    let rig = Rig::new(EngineConfig::DEFAULT, move |b, clock| {
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
    assert_eq!(rig.flow.node().await.unwrap().as_str(), "greeting");
    assert!(rig.conversation()[0].starts_with("system:"));
    assert!(rig.conversation()[0].contains("Greet the caller"));

    rig.llm.push(LlmScript::text(["Checking."]).with_tool_call(
        "call_1",
        "check_availability",
        r#"{"party_size":2,"date":"friday","time":"19:00"}"#,
    ));
    rig.llm
        .push(LlmScript::text(["A table is free. Your name?"]));
    rig.turn(1, "a table for two on friday at 19:00").await;
    rig.until_label("llm_response_ended").await;

    // The first request offered the greeting node's tools (name-ordered), the second the
    // confirm node's, with the confirm prompt as message 0.
    let sent = requests(&rig);
    assert_eq!(sent.len(), 2);
    let names =
        |r: &LlmRequest| -> Vec<String> { r.tools.iter().map(|t| t.name.to_string()).collect() };
    assert_eq!(names(&sent[0]), ["check_availability", "end_call"]);
    assert_eq!(
        names(&sent[1]),
        ["check_availability", "end_call", "make_reservation"]
    );
    assert!(sent[0].messages[0].content.contains("Greet the caller"));
    assert!(sent[1].messages[0].content.contains("A table is free"));
    assert!(
        sent[1].messages[0]
            .content
            .contains("2 guests on friday at 19:00"),
        "{}",
        sent[1].messages[0].content
    );
    assert_eq!(rig.flow.node().await.unwrap().as_str(), "confirm");
    assert_eq!(rig.flow.transitions().await, 1);
    rig.context_copy().validate().unwrap();
    let conversation = rig.conversation();
    assert!(conversation.last().unwrap().contains("Your name?"));
    assert_eq!(rig.stats.snapshot().flow_transitions, 1);
    let _ = restaurant;
}

#[tokio::test(start_paused = true)]
async fn the_flow_refuses_a_tool_outside_the_current_node() {
    let restaurant = Restaurant::new(&[]);
    let flow = restaurant_flow(Language::En, &restaurant).unwrap();
    let registry = Arc::clone(flow.registry());
    let rig = Rig::new(EngineConfig::DEFAULT, move |b, clock| {
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
    // make_reservation is not offered by the greeting node.
    rig.llm.push(LlmScript::text(["Booking."]).with_tool_call(
        "call_1",
        "make_reservation",
        r#"{"name":"Ada","party_size":2,"date":"friday","time":"19:00"}"#,
    ));
    rig.llm.push(LlmScript::text(["Sorry."]));
    rig.turn(1, "book it").await;
    rig.until_label("llm_response_ended").await;
    assert!(
        restaurant.bookings().is_empty(),
        "the refused call never ran"
    );
    assert!(
        rig.conversation()
            .iter()
            .any(|m| m.starts_with("tool!:") && m.contains("tool_not_available")),
        "{:?}",
        rig.conversation()
    );
    rig.context_copy().validate().unwrap();
    assert_eq!(rig.flow.node().await.unwrap().as_str(), "greeting");
}

/// A summarizer that answers a fixed line.
struct Line(&'static str);

impl brisvox_agent::window::Summarizer for Line {
    fn summarize<'a>(
        &'a self,
        _previous: Option<&'a brisvox_agent::window::Summary>,
        _messages: &'a [Message],
        _max_tokens: usize,
    ) -> brisvox_agent::window::BoxFuture<'a, Result<String, brisvox_agent::window::WindowError>>
    {
        Box::pin(async move { Ok(self.0.to_owned()) })
    }
}

#[tokio::test(start_paused = true)]
async fn a_flow_transition_that_replaces_the_history_resets_the_rounds_and_the_summary_state() {
    let restaurant = Restaurant::new(&[]);
    let flow = restaurant_flow(Language::En, &restaurant).unwrap();
    let registry = Arc::clone(flow.registry());
    let book = PlayoutBook::shared(16);
    let rig = Rig::new(EngineConfig::DEFAULT, {
        let book = Arc::clone(&book);
        move |b, clock| {
            let shared: SharedClock = Arc::new(*clock);
            let engine = Arc::new(ToolEngine::new(
                Arc::clone(&registry),
                SessionId::new(7),
                shared.clone(),
                brisvox_agent::tools::EngineConfig::default(),
            ));
            b.with_tools(registry, engine)
                .with_book(book)
                .with_summarizer(Arc::new(Line("the caller wants a table for two")))
                .with_flow(FlowManager::new(Arc::new(flow), shared))
                .unwrap()
        }
    })
    .await;
    rig.llm.push(LlmScript::text(["Checking."]).with_tool_call(
        "call_1",
        "check_availability",
        r#"{"party_size":2,"date":"friday","time":"19:00"}"#,
    ));
    rig.llm
        .push(LlmScript::text(["A table is free. Your name?"]));
    rig.turn(1, "a table for two on friday at 19:00").await;
    rig.until_label("llm_response_ended").await;

    // The confirm node resets the history around a summary: the first round's messages (the
    // assistant message with its call, the tool result) are gone.
    let conversation = rig.conversation();
    assert!(
        conversation[0].contains("A table is free"),
        "{conversation:?}"
    );
    assert!(
        conversation
            .iter()
            .any(|m| m.contains("the caller wants a table for two")),
        "{conversation:?}"
    );
    assert!(
        conversation.iter().all(|m| !m.starts_with("tool")),
        "{conversation:?}"
    );
    assert!(conversation.last().unwrap().contains("Your name?"));
    rig.context_copy().validate().unwrap();
    assert_eq!(rig.flow.node().await.unwrap().as_str(), "confirm");
    assert_eq!(rig.stats.snapshot().flow_transitions, 1);

    // The first round's index meant a message that no longer exists: the book no longer lists it
    // for truncation, only the answer after the reset.
    let pending = book.pending_truncation(TurnEpoch::new(1));
    assert_eq!(pending, vec![RoundKey::new(TurnEpoch::new(1), 1)]);
    // The next turn goes on from the reset context.
    rig.llm.push(LlmScript::text(["Booked."]));
    rig.turn(2, "Ada, yes").await;
    rig.until("answer", |r| count(r, "llm_response_ended") == 2)
        .await;
    rig.context_copy().validate().unwrap();
    assert_eq!(rig.stats.snapshot().validate_failures, 0);
}

// ---- failure and safety paths -------------------------------------------------------------------

/// A truncation source that never answers (a playback position that never arrives).
struct Hang;

impl TruncationSource for Hang {
    fn cut<'a>(&'a self, _request: &'a CutRequest) -> BoxFuture<'a, Cut> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test(start_paused = true)]
async fn a_truncation_source_that_never_answers_is_bounded_and_the_message_is_kept() {
    let rig = Rig::new(EngineConfig::DEFAULT, |b, _| {
        b.with_truncation(Arc::new(Hang))
    })
    .await;
    rig.llm.push(LlmScript::text(["One."]));
    rig.llm.push(LlmScript::text(["Two."]));
    rig.turn(1, "a").await;
    rig.until_label("llm_response_ended").await;

    // The next turn asks the source about round one: no answer within `analysis_job` (2 s).
    let asked = rig.clock.now();
    rig.turn(2, "b").await;
    rig.until("the second answer", |r| count(r, "llm_response_ended") == 2)
        .await;
    let waited = rig.clock.now() - asked;
    assert!(
        waited >= EngineConfig::DEFAULT.timeouts.analysis_job,
        "the wait is the documented bound, not shorter: {waited:?}"
    );
    assert!(
        waited < 2 * EngineConfig::DEFAULT.timeouts.analysis_job,
        "and not longer: {waited:?}"
    );
    let stats = rig.stats.snapshot();
    assert_eq!((stats.cut_requests, stats.cut_timeouts), (1, 1));
    assert_eq!(
        rig.conversation(),
        ["user:a", "assistant:One.", "user:b", "assistant:Two."],
        "the message is kept as written"
    );

    // Stop is not held back forever either: it is forwarded after the bound.
    rig.running
        .input()
        .send(SystemFrame::Stop.into())
        .await
        .unwrap();
    rig.until_label("stop").await;
    assert_eq!(rig.stats.snapshot().cut_timeouts, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_task_that_does_not_stop_in_time_leaves_the_context_alone_and_is_joined_again() {
    let provider = ActLlm::default();
    provider.push(Act::Text("First answer.".into()));
    // Turn two: a tool round, then a request that blocks its thread for 1.5 s (abort cannot take
    // effect until it returns). Real time: the margins below are wide so a loaded machine does
    // not fail the test (the join bound is 400 ms, the stamp tolerance 250 ms).
    provider.push(Act::ToolCall(
        "Checking.".into(),
        "call_1".into(),
        "lookup".into(),
        r#"{"q":"x"}"#.into(),
    ));
    provider.push(Act::Block(1_500 * MS, "late words".into()));
    let source = Scripted::new(|_| Cut::Keep);
    let mut config = EngineConfig::DEFAULT;
    config.timeouts.join = 400 * MS;
    let probe = Arc::new(ToolProbe::default());
    let mut rig = Rig::with_provider(config, provider.clone(), {
        let source = Arc::clone(&source);
        let probe = Arc::clone(&probe);
        move |b, clock| {
            let (registry, engine) = lookup_tools(clock, &probe, ToolKind::ReadOnly, 5 * MS, None);
            b.with_tools(registry, engine).with_truncation(source)
        }
    })
    .await;
    rig.turn(1, "q1").await;
    rig.until_label("llm_response_ended").await;
    rig.turn(2, "q2").await;
    // Wait for the blocked request without the runtime's timers: while a worker is blocked
    // and the others sleep, nothing drives them.
    for _ in 0..5_000 {
        if provider.entered() == 3 {
            break;
        }
        std::thread::sleep(MS);
    }
    assert_eq!(provider.entered(), 3);
    let before = rig.conversation();
    assert_eq!(before.len(), 5, "{before:?}");

    let barged_in = rig.clock.now();
    rig.interrupt(3).await;
    rig.until("the first join to time out", |r| {
        r.stats.snapshot().join_timeouts == 1
    })
    .await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.mutations_skipped, 1);
    assert_eq!(
        rig.conversation(),
        before,
        "no partial message, no closed call: the runner did not touch the context"
    );

    // A turn that arrives while the old task is still stuck is dropped (the task is joined
    // again first, and still does not stop), and the caller's machine is told upstream.
    rig.turn(3, "q3").await;
    rig.until("the second join to time out", |r| {
        r.stats.snapshot().join_timeouts == 2
    })
    .await;
    let stats = rig.stats.snapshot();
    assert_eq!((stats.mutations_skipped, stats.turns), (2, 2));
    assert_eq!(provider.entered(), 3, "the dropped turn made no request");
    assert_eq!(rig.upstream_frames().len(), 1);

    // The stuck task wakes up after its block: it must not write its answer into the context.
    rig.settle(1_700 * MS).await;
    assert_eq!(
        rig.conversation(),
        before,
        "a halted task never mutates the context, even when it outlives the join"
    );

    // The next turn finds the task gone and goes on from a consistent context.
    provider.push(Act::Text("Fine.".into()));
    rig.turn(4, "q4").await;
    rig.until("the recovered answer", |r| {
        count(r, "llm_response_ended") == 2
    })
    .await;
    let after = rig.conversation();
    assert_eq!(after[..5], before[..]);
    assert_eq!(after[5..], ["user:q4", "assistant:Fine."]);
    rig.context_copy().validate().unwrap();
    // The barge-in instant was taken when the interrupt arrived, not after the bounded join.
    let asked = source.seen();
    assert_eq!(asked.len(), 2, "rounds of turns one and two: {asked:?}");
    let asked = &asked[1..];
    let stamp = asked[0].interrupted_at.expect("stamped at the barge-in");
    assert!(
        stamp.saturating_duration_since(barged_in) < 250 * MS,
        "stamped {:?} after the barge-in; the join alone takes 400 ms",
        stamp.saturating_duration_since(barged_in)
    );
}

#[tokio::test(start_paused = true)]
async fn a_finish_that_fails_on_the_offload_ends_the_round_with_its_text_only() {
    use brisvox_core::SystemClock;
    use brisvox_inference::{InferencePool, PoolConfig};
    // A pool that has been shut down: every job is refused.
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 1,
            queue_capacity: 2,
            name: "llm-finish-refused".to_owned(),
        },
        SystemClock::shared(),
    )
    .unwrap();
    let handle = pool.handle();
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
    let probe = Arc::new(ToolProbe::default());
    let rig = Rig::new(EngineConfig::DEFAULT, {
        let probe = Arc::clone(&probe);
        move |b, clock| {
            let (registry, engine) = lookup_tools(clock, &probe, ToolKind::ReadOnly, 5 * MS, None);
            b.with_tools(registry, engine)
                .with_offload(Offload::Pool(handle))
        }
    })
    .await;
    let big = format!(r#"{{"q":"{}"}}"#, "y".repeat(8_000));
    rig.llm
        .push(LlmScript::text(["Checking."]).with_tool_call("call_big", "lookup", &big));
    rig.llm.push(LlmScript::text(["Fine."]));
    rig.turn(1, "big").await;
    rig.until_label("llm_response_ended").await;
    let stats = rig.stats.snapshot();
    assert_eq!((stats.finishes_offloaded, stats.finish_failures), (1, 1));
    assert_eq!(probe.started(), 0, "the unparsed call did not run");
    assert_eq!(
        rig.conversation(),
        ["user:big", "assistant:Checking."],
        "the text that was spoken stays in the context"
    );
    rig.context_copy().validate().unwrap();
    // The conversation goes on (a small response is finished inline).
    rig.turn(2, "again").await;
    rig.until("the next answer", |r| count(r, "llm_response_ended") == 2)
        .await;
    assert_eq!(rig.conversation().last().unwrap(), "assistant:Fine.");
}

#[tokio::test(start_paused = true)]
async fn a_cut_that_is_not_a_prefix_of_the_message_is_refused_and_counted() {
    let source = Scripted::new(|_| Cut::Heard(heard("Something else entirely")));
    let rig = Rig::new(EngineConfig::DEFAULT, move |b, _| b.with_truncation(source)).await;
    rig.llm.push(LlmScript::text(["The real answer."]));
    rig.llm.push(LlmScript::text(["Next."]));
    rig.turn(1, "a").await;
    rig.until_label("llm_response_ended").await;
    rig.turn(2, "b").await;
    rig.until("the second answer", |r| count(r, "llm_response_ended") == 2)
        .await;
    let stats = rig.stats.snapshot();
    assert_eq!((stats.cut_errors, stats.cuts_heard), (1, 0));
    assert_eq!(rig.conversation()[1], "assistant:The real answer.");
}

#[tokio::test(start_paused = true)]
async fn the_round_records_are_bounded_and_dropping_one_is_counted() {
    let mut config = EngineConfig::DEFAULT;
    config.llm.max_tool_rounds = 200;
    config.llm.window = brisvox_agent::WindowPolicy::new(1_000_000, 8);
    let (rig, probe) = tool_rig(config, ToolKind::ReadOnly, MS, None, None).await;
    let rounds = 130usize;
    for round in 0..rounds {
        rig.llm
            .push(LlmScript::text([format!("r{round}.")]).with_tool_call(
                &format!("call_{round}"),
                "lookup",
                r#"{"q":"x"}"#,
            ));
    }
    rig.llm.push(LlmScript::text(["Done."]));
    rig.turn(1, "go").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(probe.started(), rounds);
    // 130 tool rounds and the answer: 131 assistant messages, 128 remembered.
    assert_eq!(rig.stats.snapshot().records_dropped, 3);
    rig.context_copy().validate().unwrap();
    // The next turn asks about the 128 that are left, newest first, and goes on.
    rig.llm.push(LlmScript::text(["Again."]));
    rig.turn(2, "more").await;
    rig.until("the next answer", |r| count(r, "llm_response_ended") == 2)
        .await;
    assert_eq!(rig.stats.snapshot().cut_requests, 128);
    assert_eq!(rig.stats.snapshot().validate_failures, 0);
}

#[tokio::test(start_paused = true)]
async fn a_panic_in_the_round_task_closes_the_turn_and_the_next_turn_recovers() {
    let provider = ActLlm::default();
    provider.push(Act::Panic);
    provider.push(Act::Text("Fine.".into()));
    let mut rig = Rig::with_provider(EngineConfig::DEFAULT, provider, |b, _| b).await;
    rig.turn(1, "a").await;
    rig.until_label("llm_response_ended").await;
    // The pipeline hears that the response is over (no TTS will say so) and the session lives.
    assert_eq!(
        rig.labels(),
        [
            "user_turn_ended",
            "llm_response_started",
            "llm_response_ended"
        ]
    );
    assert_eq!(rig.upstream_frames().len(), 1);
    assert_eq!(rig.stats.snapshot().round_panics, 1);
    rig.turn(2, "b").await;
    rig.until("the next answer", |r| count(r, "llm_response_ended") == 2)
        .await;
    assert_eq!(rig.conversation(), ["user:a", "user:b", "assistant:Fine."]);
    rig.context_copy().validate().unwrap();
    assert_eq!(rig.stats.snapshot().turns, 2);
}

#[tokio::test(start_paused = true)]
async fn a_request_that_cannot_be_built_ends_the_turn_quietly_and_says_so() {
    let mut rig = Rig::default().await;
    // A tool result nobody asked for: the context is invalid, whoever put it there.
    rig.context.lock().unwrap().push(Message::tool_result(
        brisvox_agent::context::ToolCallId::new(TurnEpoch::new(9), 0),
        "orphan",
    ));
    rig.llm.push(LlmScript::text(["never sent"]));
    rig.turn(1, "hello").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(requests(&rig).len(), 0, "no request went out");
    let stats = rig.stats.snapshot();
    assert_eq!((stats.request_build_errors, stats.silent_turns), (1, 1));
    assert_eq!(rig.request_stats.snapshot().window_errors, 1);
    // The pipeline is told, so the caller is not left waiting for a response.
    assert_eq!(rig.upstream_frames().len(), 1);
}

/// A SideEffect tool that waits for its cancel token and records that it fired.
struct CancelWatcher {
    started: Arc<AtomicUsize>,
    saw_cancel: Arc<AtomicUsize>,
}

impl brisvox_agent::tools::Tool for CancelWatcher {
    fn call<'a>(
        &'a self,
        _args: serde_json::Value,
        ctx: brisvox_agent::tools::ToolCallCtx,
    ) -> brisvox_agent::tools::ToolFuture<'a> {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::SeqCst);
            ctx.cancel.cancelled().await;
            self.saw_cancel.fetch_add(1, Ordering::SeqCst);
            Ok(brisvox_agent::tools::ToolOutput::Json(
                serde_json::json!({"cancelled": true}),
            ))
        })
    }
}

#[tokio::test(start_paused = true)]
async fn cancel_cancels_the_running_tool_and_drops_the_round() {
    let started = Arc::new(AtomicUsize::new(0));
    let saw_cancel = Arc::new(AtomicUsize::new(0));
    let rig = Rig::new(EngineConfig::DEFAULT, {
        let (started, saw_cancel) = (Arc::clone(&started), Arc::clone(&saw_cancel));
        move |b, clock| {
            let mut registry = brisvox_agent::tools::ToolRegistry::new();
            registry
                .register(
                    brisvox_agent::tools::ToolSpec::new(
                        "lookup",
                        "Looks something up.",
                        serde_json::json!({"type": "object"}),
                        ToolKind::SideEffect,
                    )
                    .with_timeout(Duration::from_secs(60)),
                    CancelWatcher {
                        started,
                        saw_cancel,
                    },
                )
                .unwrap();
            let registry = Arc::new(registry);
            let shared: SharedClock = Arc::new(*clock);
            let engine = Arc::new(ToolEngine::new(
                Arc::clone(&registry),
                SessionId::new(1),
                shared,
                brisvox_agent::tools::EngineConfig::default(),
            ));
            b.with_tools(registry, engine)
        }
    })
    .await;
    rig.llm.push(tool_script("Checking."));
    rig.llm.push(LlmScript::text(["never"]));
    rig.turn(1, "q").await;
    rig.until("the tool to start", |_| started.load(Ordering::SeqCst) == 1)
        .await;
    rig.running
        .input()
        .send(SystemFrame::Cancel.into())
        .await
        .unwrap();
    rig.until_label("cancel").await;
    rig.settle(5 * SECOND).await;
    assert_eq!(
        saw_cancel.load(Ordering::SeqCst),
        1,
        "the tool was told the call is over"
    );
    assert_eq!(requests(&rig).len(), 1, "no second round after the cancel");
    assert!(!rig.labels().contains(&"llm_response_ended".to_owned()));
}

#[tokio::test(start_paused = true)]
async fn a_back_pressured_pipeline_does_not_stall_the_running_tool() {
    // The tool takes 3 s and a filler is due after 1 s. The filler's frames back up behind a gate
    // that holds until released: the tool must still finish at 3 s, not when the gate opens.
    let gate = Gate::default();
    gate.hold.store(true, Ordering::SeqCst);
    let probe = Arc::new(ToolProbe::default());
    let llm = MockLlm::new(Arc::new(SimClock::new()));
    let rig = Rig::gated(EngineConfig::DEFAULT, llm.clone(), &gate, {
        let probe = Arc::clone(&probe);
        move |b, clock| {
            let (registry, engine) =
                lookup_tools(clock, &probe, ToolKind::ReadOnly, 3 * SECOND, Some(SECOND));
            b.with_tools(registry, engine)
        }
    })
    .await;
    llm.push(tool_script("Checking."));
    llm.push(LlmScript::text(["It is sunny."]));
    rig.turn(1, "weather?").await;
    rig.until("the filler", |r| r.stats.snapshot().fillers == 1)
        .await;
    rig.settle(3 * SECOND).await;
    assert_eq!(
        probe.finished(),
        1,
        "the tool finished while the downstream was blocked"
    );
    // Release the gate: the rest of the turn follows.
    gate.hold.store(false, Ordering::SeqCst);
    gate.release.notify_one();
    rig.until_label("llm_response_ended").await;
    assert!(rig.conversation().last().unwrap().contains("sunny"));
}

// ---- provider data ------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn provider_data_is_kept_replayed_and_counted_in_the_window() {
    let provider = ActLlm::default();
    let data = format!(r#"{{"signature":"{}"}}"#, "s".repeat(6_000));
    provider.push(Act::TextWithData("Hello.".into(), data.clone()));
    provider.push(Act::Text("Again.".into()));
    let mut config = EngineConfig::DEFAULT;
    config.llm.window = brisvox_agent::WindowPolicy::new(40_000, 8);
    let rig = Rig::with_provider(config, provider.clone(), |b, _| b).await;
    rig.turn(1, "a").await;
    rig.until_label("llm_response_ended").await;
    rig.turn(2, "b").await;
    rig.until("the second answer", |r| count(r, "llm_response_ended") == 2)
        .await;
    // A 6 KB blob is parsed on the offload (not inline) and echoed verbatim in the next request.
    assert_eq!(rig.stats.snapshot().provider_data_offloaded, 1);
    let second = &provider.requests()[1];
    assert_eq!(
        second.messages[1].provider_data.as_deref(),
        Some(data.as_str())
    );
    // And it counts against the budget: about 1500 tokens, far more than the two texts.
    assert!(rig.request_stats.snapshot().max_estimated_tokens > 1_400);
}

#[tokio::test(start_paused = true)]
async fn oversized_provider_data_is_dropped_and_counted() {
    let provider = ActLlm::default();
    let huge = format!(r#"{{"signature":"{}"}}"#, "s".repeat(40_000));
    provider.push(Act::TextWithData("Hello.".into(), huge));
    provider.push(Act::Text("Again.".into()));
    // A window that could hold it: the fixed cap applies.
    let mut config = EngineConfig::DEFAULT;
    config.llm.window = brisvox_agent::WindowPolicy::new(400_000, 8);
    let rig = Rig::with_provider(config, provider.clone(), |b, _| b).await;
    rig.turn(1, "a").await;
    rig.until_label("llm_response_ended").await;
    rig.turn(2, "b").await;
    rig.until("the second answer", |r| count(r, "llm_response_ended") == 2)
        .await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.provider_data_oversized, stats.provider_data_offloaded),
        (1, 0)
    );
    let second = &provider.requests()[1];
    assert!(second.messages[1].provider_data.is_none());
    assert_eq!(&*second.messages[1].content, "Hello.", "the text is kept");
}

#[tokio::test(start_paused = true)]
async fn provider_data_that_does_not_fit_the_window_is_dropped_so_the_request_stays_valid() {
    // The default window (4000 tokens) allows a message 1000 tokens: 6 KB of provider data would
    // leave no room for the text. It is dropped, and the next request is built and valid.
    let provider = ActLlm::default();
    let data = format!(r#"{{"signature":"{}"}}"#, "s".repeat(6_000));
    provider.push(Act::TextWithData("Hello.".into(), data));
    provider.push(Act::Text("Again.".into()));
    let rig = Rig::with_provider(EngineConfig::DEFAULT, provider.clone(), |b, _| b).await;
    rig.turn(1, "a").await;
    rig.until_label("llm_response_ended").await;
    rig.turn(2, "b").await;
    rig.until("the second answer", |r| count(r, "llm_response_ended") == 2)
        .await;
    assert_eq!(rig.stats.snapshot().provider_data_oversized, 1);
    assert_eq!(rig.stats.snapshot().request_build_errors, 0);
    assert_eq!(provider.requests().len(), 2);
}

// ---- several tool calls in one response -----------------------------------------------------------

/// Two read-only tools that both take `delay`: `late` asks for a filler after 5 s, `early` after
/// 500 ms.
fn two_tools(
    clock: &SimClock,
    probe: &Arc<ToolProbe>,
    delay: Duration,
) -> (Arc<brisvox_agent::tools::ToolRegistry>, Arc<ToolEngine>) {
    let shared: SharedClock = Arc::new(*clock);
    let mut registry = brisvox_agent::tools::ToolRegistry::new();
    for (name, filler) in [("late", 5 * SECOND), ("early", 500 * MS)] {
        registry
            .register(
                brisvox_agent::tools::ToolSpec::new(
                    name,
                    "Looks something up.",
                    serde_json::json!({"type": "object"}),
                    ToolKind::ReadOnly,
                )
                .with_timeout(Duration::from_secs(30))
                .with_filler_after(Some(filler)),
                common::llm::SlowTool {
                    probe: Arc::clone(probe),
                    clock: shared.clone(),
                    delay,
                    result: serde_json::json!({"answer": name}),
                },
            )
            .unwrap();
    }
    let registry = Arc::new(registry);
    let engine = Arc::new(ToolEngine::new(
        Arc::clone(&registry),
        SessionId::new(1),
        shared,
        brisvox_agent::tools::EngineConfig::default(),
    ));
    (registry, engine)
}

async fn two_tool_rig(delay: Duration) -> (Rig, Arc<ToolProbe>) {
    let probe = Arc::new(ToolProbe::default());
    // Each tool's own filler delay (the smallest of the round wins), not the fixed default.
    let mut config = EngineConfig::DEFAULT;
    config.llm.filler = brisvox_agent::tools::FillerConfig::PER_TOOL;
    let rig = Rig::new(config, {
        let probe = Arc::clone(&probe);
        move |b, clock| {
            let (registry, engine) = two_tools(clock, &probe, delay);
            b.with_tools(registry, engine)
        }
    })
    .await;
    (rig, probe)
}

fn two_calls(text: &str) -> LlmScript {
    LlmScript::text([text])
        .with_tool_call("c1", "late", "{}")
        .with_tool_call("c2", "early", "{}")
}

/// Ids of the tool results of the context, in order.
fn result_ids(rig: &Rig) -> Vec<String> {
    rig.context_copy()
        .messages()
        .iter()
        .filter_map(|m| match m {
            Message::Tool { call_id, .. } => Some(call_id.as_str().to_owned()),
            _ => None,
        })
        .collect()
}

async fn two_calls_in_one_response(interleaved: bool) {
    let (rig, probe) = two_tool_rig(1_500 * MS).await;
    let first = if interleaved {
        two_calls("Checking.").with_fault(LlmFault::InterleavedToolDeltas)
    } else {
        two_calls("Checking.")
    };
    rig.llm.push(first);
    rig.llm.push(LlmScript::text(["Done."]));
    rig.turn(1, "go").await;
    rig.until("both tools to start", |_| probe.started() == 2)
        .await;
    // The filler timer is the smallest `filler_after` of the called tools (500 ms), not the
    // first call's (5 s) and not the largest.
    rig.settle(400 * MS).await;
    assert_eq!(rig.stats.snapshot().fillers, 0, "not before 500 ms");
    rig.settle(200 * MS).await;
    assert_eq!(rig.stats.snapshot().fillers, 1, "at about 500 ms");
    rig.until_label("llm_response_ended").await;

    assert_eq!(probe.finished(), 2);
    let conversation = rig.conversation();
    assert_eq!(conversation.len(), 6, "{conversation:?}");
    assert_eq!(conversation[1], "assistant+calls[late,early]:Checking.");
    // Both results, in call order, under the ids the assembler minted in order.
    assert!(conversation[2].contains("late"), "{conversation:?}");
    assert!(conversation[3].contains("early"), "{conversation:?}");
    assert_eq!(result_ids(&rig), ["t1.0", "t1.1"]);
    // The spoken filler after the results, before the answer.
    assert_eq!(conversation[4], "assistant:One moment, please.");
    assert_eq!(conversation[5], "assistant:Done.");
    rig.context_copy().validate().unwrap();
    // The second request replays both calls and both results under the provider's ids; it ends
    // on the results (the filler is placed after it was sent).
    let second = &requests(&rig)[1];
    assert_eq!(second.messages.len(), 4);
    assert_eq!(second.messages[1].tool_calls.len(), 2);
    assert_eq!(second.messages[2].tool_call_id.as_deref(), Some("c1"));
    assert_eq!(second.messages[3].tool_call_id.as_deref(), Some("c2"));
    let stats = rig.stats.snapshot();
    assert_eq!((stats.tool_rounds, stats.tool_calls), (1, 2));
}

#[tokio::test(start_paused = true)]
async fn two_calls_in_one_response_run_with_the_smallest_filler_and_both_results_in_order() {
    two_calls_in_one_response(false).await;
}

#[tokio::test(start_paused = true)]
async fn two_interleaved_calls_run_the_same_way() {
    two_calls_in_one_response(true).await;
}

#[tokio::test(start_paused = true)]
async fn a_barge_in_during_two_calls_closes_both() {
    let (rig, probe) = two_tool_rig(5 * SECOND).await;
    rig.llm.push(two_calls("Checking."));
    rig.turn(1, "go").await;
    rig.until("both tools to start", |_| probe.started() == 2)
        .await;
    rig.settle(100 * MS).await;
    rig.interrupt(2).await;
    rig.until_label("interrupt").await;
    let stats = rig.stats.snapshot();
    assert_eq!((stats.halts, stats.interrupted_tool_calls), (1, 2));
    let conversation = rig.conversation();
    assert_eq!(conversation.len(), 4, "{conversation:?}");
    assert!(conversation[2].starts_with("tool!:interrupted"));
    assert!(conversation[3].starts_with("tool!:interrupted"));
    assert_eq!(result_ids(&rig), ["t1.0", "t1.1"]);
    rig.context_copy().validate().unwrap();
}

// ---- a new turn or a stop while a round task is alive ----------------------------------------------

fn stalled_after_two_deltas() -> LlmScript {
    LlmScript::text(["Hello ", "wor", "ld"]).with_fault(LlmFault::Stall {
        before_event: 2,
        duration: Duration::from_secs(30),
    })
}

#[tokio::test(start_paused = true)]
async fn a_new_turn_while_a_round_is_streaming_halts_it_without_a_barge_in_stamp() {
    let source = Scripted::new(|_| Cut::Keep);
    let rig = Rig::new(EngineConfig::DEFAULT, {
        let source = Arc::clone(&source);
        move |b, _| b.with_truncation(source)
    })
    .await;
    rig.llm.push(stalled_after_two_deltas());
    rig.llm.push(LlmScript::text(["Fine."]));
    rig.turn(1, "hi").await;
    rig.until_label("text:wor").await;
    // No Interrupt first: the next turn ends while the first round is still streaming.
    rig.turn(2, "again").await;
    rig.until("the second answer", |r| count(r, "llm_response_ended") == 1)
        .await;
    rig.settle(60 * SECOND).await;

    assert_eq!(
        rig.conversation(),
        [
            "user:hi",
            "assistant:Hello wor",
            "user:again",
            "assistant:Fine."
        ],
        "the first task is gone: its stalled tail never reaches the context"
    );
    rig.context_copy().validate().unwrap();
    assert_eq!(requests(&rig).len(), 2, "exactly one request for turn two");
    let sent = requests(&rig);
    assert_eq!(roles(&sent[1]), [Role::User, Role::Assistant, Role::User]);
    assert_eq!(&*sent[1].messages[1].content, "Hello wor");
    let seen = source.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].interrupted_at, None,
        "no barge-in happened, so no stamp"
    );
    assert!(!rig.labels().contains(&"text:ld".to_owned()));
    let stats = rig.stats.snapshot();
    assert_eq!((stats.halts, stats.partial_messages), (1, 1));
}

#[tokio::test(start_paused = true)]
async fn a_stop_while_a_round_is_streaming_settles_the_partial_text_and_applies_the_cut() {
    let source = Scripted::new(|_| Cut::Heard(heard("Hello")));
    let rig = Rig::new(EngineConfig::DEFAULT, {
        let source = Arc::clone(&source);
        move |b, _| b.with_truncation(source)
    })
    .await;
    rig.llm.push(stalled_after_two_deltas());
    rig.turn(1, "hi").await;
    rig.until_label("text:wor").await;
    rig.running
        .input()
        .send(SystemFrame::Stop.into())
        .await
        .unwrap();
    rig.until_label("stop").await;
    rig.settle(60 * SECOND).await;
    assert_eq!(
        rig.conversation(),
        ["user:hi", "assistant:Hello"],
        "the partial text was pushed, then cut to what was heard"
    );
    let seen = source.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(&*seen[0].sent, "Hello wor");
    assert_eq!(seen[0].interrupted_at, None);
    assert_eq!(count(&rig, "llm_response_ended"), 0);
    assert_eq!(rig.stats.snapshot().halts, 1);
    rig.context_copy().validate().unwrap();
}

// ---- the cut happens before the Stop is forwarded and before the user message is pushed ---------

/// A truncation source that records, when it is asked, which frames the runner has sent
/// downstream so far and what the context holds.
#[derive(Default)]
struct Spy {
    rec: Mutex<Option<common::Recorded>>,
    context: Mutex<Option<Arc<Mutex<brisvox_agent::context::Context>>>>,
    asked: Mutex<Vec<(Vec<String>, Vec<String>)>>,
}

impl Spy {
    fn attach(&self, rig: &Rig) {
        *self.rec.lock().unwrap() = Some(rig.rec.clone());
        *self.context.lock().unwrap() = Some(Arc::clone(&rig.context));
    }

    fn asked(&self) -> Vec<(Vec<String>, Vec<String>)> {
        self.asked.lock().unwrap().clone()
    }
}

impl TruncationSource for Spy {
    fn cut<'a>(&'a self, _request: &'a CutRequest) -> BoxFuture<'a, Cut> {
        Box::pin(async move {
            // Answering takes a moment (like a settle wait), so a runner that forwarded the
            // frame before asking has had it recorded by the time this looks.
            tokio::time::sleep(50 * MS).await;
            let frames: Vec<String> = self
                .rec
                .lock()
                .unwrap()
                .as_ref()
                .map(|rec| {
                    rec.all()
                        .into_iter()
                        .filter(|(_, dir)| *dir == brisvox_core::Direction::Downstream)
                        .map(|(frame, _)| common::llm::label(&frame))
                        .collect()
                })
                .unwrap_or_default();
            let conversation = self
                .context
                .lock()
                .unwrap()
                .as_ref()
                .map(|ctx| {
                    common::llm::summarize(&ctx.lock().unwrap_or_else(PoisonError::into_inner))
                })
                .unwrap_or_default();
            self.asked.lock().unwrap().push((frames, conversation));
            Cut::Heard(heard("Part"))
        })
    }
}

#[tokio::test(start_paused = true)]
async fn the_cut_is_asked_before_the_stop_is_forwarded() {
    let spy = Arc::new(Spy::default());
    let rig = Rig::new(EngineConfig::DEFAULT, {
        let spy = Arc::clone(&spy);
        move |b, _| b.with_truncation(spy)
    })
    .await;
    spy.attach(&rig);
    rig.llm.push(LlmScript::text(["Part and more."]));
    rig.turn(1, "a").await;
    rig.until_label("llm_response_ended").await;
    rig.running
        .input()
        .send(SystemFrame::Stop.into())
        .await
        .unwrap();
    rig.until_label("stop").await;
    let asked = spy.asked();
    assert_eq!(asked.len(), 1);
    assert!(
        !asked[0].0.contains(&"stop".to_owned()),
        "the Stop was already forwarded when the cut was asked: {:?}",
        asked[0].0
    );
    assert_eq!(asked[0].1, ["user:a", "assistant:Part and more."]);
    assert_eq!(rig.conversation()[1], "assistant:Part");
}

#[tokio::test(start_paused = true)]
async fn the_cut_is_asked_before_the_next_user_message_is_pushed() {
    let spy = Arc::new(Spy::default());
    let rig = Rig::new(EngineConfig::DEFAULT, {
        let spy = Arc::clone(&spy);
        move |b, _| b.with_truncation(spy)
    })
    .await;
    spy.attach(&rig);
    rig.llm.push(LlmScript::text(["Part and more."]));
    rig.llm.push(LlmScript::text(["Next."]));
    rig.turn(1, "a").await;
    rig.until_label("llm_response_ended").await;
    rig.turn(2, "b").await;
    rig.until("the second answer", |r| count(r, "llm_response_ended") == 2)
        .await;
    let asked = spy.asked();
    assert_eq!(asked.len(), 1);
    assert_eq!(
        asked[0].1,
        ["user:a", "assistant:Part and more."],
        "the user message of turn two must not be in the context yet"
    );
    assert_eq!(rig.conversation()[1], "assistant:Part");
}

// ---- the truncation deadline is one for the whole pass -----------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_truncation_deadline_is_one_for_all_rounds_of_a_pass() {
    let probe = Arc::new(ToolProbe::default());
    let mut config = EngineConfig::DEFAULT;
    config.llm.max_tool_rounds = 6;
    let rig = Rig::new(config, {
        let probe = Arc::clone(&probe);
        move |b, clock| {
            let (registry, engine) = lookup_tools(clock, &probe, ToolKind::ReadOnly, 10 * MS, None);
            b.with_tools(registry, engine)
                .with_truncation(Arc::new(Hang))
        }
    })
    .await;
    // Three model rounds in one turn: two with a tool, then the answer.
    rig.llm.push(tool_script("One."));
    rig.llm.push(tool_script("Two."));
    rig.llm.push(LlmScript::text(["Three."]));
    rig.llm.push(LlmScript::text(["Next."]));
    rig.turn(1, "go").await;
    rig.until_label("llm_response_ended").await;
    let analysis_job = EngineConfig::DEFAULT.timeouts.analysis_job;

    let asked = rig.clock.now();
    rig.turn(2, "again").await;
    rig.until("the next answer", |r| count(r, "llm_response_ended") == 2)
        .await;
    let waited = rig.clock.now() - asked;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.cut_requests, stats.cut_timeouts),
        (3, 3),
        "all three rounds were asked and none answered"
    );
    assert!(
        waited >= analysis_job && waited < 2 * analysis_job,
        "one deadline for the pass, not one per round: waited {waited:?}"
    );
}

// ---- the flow reset forgets the window summary and keeps tool call ids unique --------------------------

#[tokio::test(start_paused = true)]
async fn a_context_replacing_transition_forgets_the_summary_and_never_reuses_a_call_id() {
    use brisvox_agent::window::SUMMARY_PREFIX;
    // 20:00 is always full.
    let restaurant = Restaurant::new(&["20:00"]);
    let flow = restaurant_flow(Language::En, &restaurant).unwrap();
    let registry = Arc::clone(flow.registry());
    // A small window, so earlier turns are dropped and a window summary exists before the
    // transition.
    let mut config = EngineConfig::DEFAULT;
    config.llm.window = brisvox_agent::WindowPolicy::new(300, 2);
    let rig = Rig::new(config, move |b, clock| {
        let shared: SharedClock = Arc::new(*clock);
        let engine = Arc::new(ToolEngine::new(
            Arc::clone(&registry),
            SessionId::new(7),
            shared.clone(),
            brisvox_agent::tools::EngineConfig::default(),
        ));
        b.with_tools(registry, engine)
            .with_summarizer(Arc::new(Line("the caller is chatting")))
            .with_flow(FlowManager::new(Arc::new(flow), shared))
            .unwrap()
    })
    .await;
    let filler = "word ".repeat(30);
    for turn in 1..=6u32 {
        rig.llm
            .push(LlmScript::text([format!("Answer {turn}. {filler}")]));
        rig.turn(turn, &format!("question {turn} {filler}")).await;
        rig.until("the answer", |r| {
            count(r, "llm_response_ended") == turn as usize
        })
        .await;
        rig.settle(5 * SECOND).await;
    }
    let before = rig.request_stats.snapshot();
    assert!(
        before.summary_covers_until > 0,
        "a summary exists: {before:?}"
    );

    // Turn 7: check availability (the confirm node resets the history), try to book the full
    // 20:00 in the new context (unavailable: the alternatives node), check again (the confirm
    // node resets once more) and book 19:00 (the done node resets it again), then answer. The
    // two SideEffect calls are deduplicated by (session, epoch, call id): the second must not
    // get the id of the first, which a reset removed from the context.
    let check = r#"{"party_size":2,"date":"friday","time":"19:00"}"#;
    rig.llm.push(LlmScript::text(["Checking."]).with_tool_call(
        "call_1",
        "check_availability",
        check,
    ));
    rig.llm.push(LlmScript::text(["Booking."]).with_tool_call(
        "call_2",
        "make_reservation",
        r#"{"name":"Ada","party_size":2,"date":"friday","time":"20:00"}"#,
    ));
    rig.llm.push(LlmScript::text(["Checking."]).with_tool_call(
        "call_3",
        "check_availability",
        check,
    ));
    rig.llm.push(LlmScript::text(["Booking."]).with_tool_call(
        "call_4",
        "make_reservation",
        r#"{"name":"Ada","party_size":2,"date":"friday","time":"19:00"}"#,
    ));
    rig.llm.push(LlmScript::text(["Your table is booked."]));
    let sent_before = requests(&rig).len();
    rig.turn(7, "a table for two on friday at 19:00").await;
    rig.until("the booking turn", |r| count(r, "llm_response_ended") == 7)
        .await;

    assert_eq!(rig.flow.transitions().await, 4);
    assert_eq!(
        restaurant.bookings().len(),
        1,
        "the second SideEffect call of the epoch got a fresh id, so the engine ran it"
    );
    // The summary of the old history is gone, and no request after the reset carries it.
    let after = rig.request_stats.snapshot();
    assert_eq!(after.summary_covers_until, 0, "{after:?}");
    assert_eq!((after.invalid_summaries, after.window_errors), (0, 0));
    let sent = requests(&rig);
    assert!(
        sent[sent_before - 1]
            .messages
            .iter()
            .any(|m| m.content.starts_with(SUMMARY_PREFIX)),
        "the request before the reset carried the window summary"
    );
    for request in &sent[sent_before + 1..] {
        assert!(
            request
                .messages
                .iter()
                .all(|m| !m.content.starts_with(SUMMARY_PREFIX)),
            "a request after the reset carries the old summary: {request:?}"
        );
    }
    rig.context_copy().validate().unwrap();
    assert_eq!(rig.stats.snapshot().validate_failures, 0);
}

// ---- request-phase stalls, a stream that closes without Done, quiet gaps ----------------------------------

#[tokio::test(start_paused = true)]
async fn a_request_that_stalls_before_the_stream_opens_times_out_and_is_retried() {
    let mut config = EngineConfig::DEFAULT;
    config.timeouts.llm_first_token = 200 * MS;
    let rig = Rig::new(config, |b, _| b).await;
    // The connect itself takes 30 s: the first-token deadline covers the request, not just the
    // first event.
    rig.llm.push(
        LlmScript::text(["late"]).with_fault(LlmFault::ConnectStall {
            duration: Duration::from_secs(30),
        }),
    );
    rig.llm.push(LlmScript::text(["Back."]));
    let started = rig.clock.now();
    rig.turn(1, "hi").await;
    rig.until_label("llm_response_ended").await;
    assert!(
        rig.clock.now() - started < SECOND,
        "the stalled connect did not hold the turn for its 30 s: {:?}",
        rig.clock.now() - started
    );
    let stats = rig.stats.snapshot();
    assert_eq!((stats.first_token_timeouts, stats.retries), (1, 1));
    assert_eq!(rig.conversation(), ["user:hi", "assistant:Back."]);
}

/// A provider whose streams end without `Done`: the first attempt without any event, the second
/// after one text delta.
#[derive(Clone, Default)]
struct Truncating {
    calls: Arc<AtomicUsize>,
}

impl LlmProvider for Truncating {
    fn name(&self) -> &'static str {
        "truncating"
    }

    async fn stream(&self, _request: LlmRequest) -> brisvox_providers::Result<LlmStream> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        if call >= 1 {
            tx.try_send(brisvox_providers::LlmEvent::TextDelta(Arc::from("Hel")))
                .unwrap();
        }
        drop(tx);
        Ok(LlmStream::new(rx))
    }
}

#[tokio::test(start_paused = true)]
async fn a_stream_that_closes_without_done_is_retried_before_output_and_kept_after_it() {
    let provider = Truncating::default();
    let rig = Rig::with_provider(EngineConfig::DEFAULT, provider.clone(), |b, _| b).await;
    rig.turn(1, "hi").await;
    rig.until_label("llm_response_ended").await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.streams_cut_off, stats.retries, stats.failed_rounds),
        (2, 1, 1),
        "closed empty: retried; closed after text: not retried, the round fails with its text"
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(rig.conversation(), ["user:hi", "assistant:Hel"]);
}

#[tokio::test(start_paused = true)]
async fn a_long_answer_with_quiet_gaps_below_the_idle_timeout_completes() {
    let mut config = EngineConfig::DEFAULT;
    config.timeouts.llm_first_token = 4 * SECOND;
    config.timeouts.llm_idle = 5 * SECOND;
    let rig = Rig::new(config, |b, _| b).await;
    // 14 deltas with 3 s between them: 39 s in all, far over both deadlines, but no single gap
    // is. A deadline that is not refreshed per event would kill this answer.
    let deltas: Vec<String> = (0..14).map(|i| format!("w{i} ")).collect();
    let mut script = LlmScript::text(deltas);
    for event in 1..14 {
        script = script.with_fault(LlmFault::Stall {
            before_event: event,
            duration: 3 * SECOND,
        });
    }
    rig.llm.push(script);
    rig.turn(1, "hi").await;
    rig.settle(45 * SECOND).await;
    rig.until_label("llm_response_ended").await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (
            stats.idle_timeouts,
            stats.first_token_timeouts,
            stats.failed_rounds
        ),
        (0, 0, 0)
    );
    let answer = rig.conversation().pop().unwrap();
    assert!(
        answer.starts_with("assistant:w0 w1 ") && answer.contains("w13"),
        "{answer}"
    );
}

// ---- a task that is stuck inside a tool does not touch the context after the join gave up ----------------

/// A tool that blocks its thread (the abort cannot take effect until it returns).
struct BlockingTool {
    entered: Arc<AtomicUsize>,
    block: Duration,
}

impl brisvox_agent::tools::Tool for BlockingTool {
    fn call<'a>(
        &'a self,
        _args: serde_json::Value,
        _ctx: brisvox_agent::tools::ToolCallCtx,
    ) -> brisvox_agent::tools::ToolFuture<'a> {
        Box::pin(async move {
            self.entered.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.block);
            Ok(brisvox_agent::tools::ToolOutput::Json(
                serde_json::json!({"answer": "late"}),
            ))
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_task_stuck_in_a_tool_does_not_append_its_result_after_the_join_gave_up() {
    let entered = Arc::new(AtomicUsize::new(0));
    let mut config = EngineConfig::DEFAULT;
    config.timeouts.join = 300 * MS;
    let rig = Rig::new(config, {
        let entered = Arc::clone(&entered);
        move |b, clock| {
            let mut registry = brisvox_agent::tools::ToolRegistry::new();
            registry
                .register(
                    brisvox_agent::tools::ToolSpec::new(
                        "lookup",
                        "Looks something up.",
                        serde_json::json!({"type": "object"}),
                        ToolKind::ReadOnly,
                    )
                    .with_timeout(Duration::from_secs(60)),
                    BlockingTool {
                        entered,
                        block: 1_200 * MS,
                    },
                )
                .unwrap();
            let registry = Arc::new(registry);
            let shared: SharedClock = Arc::new(*clock);
            let engine = Arc::new(ToolEngine::new(
                Arc::clone(&registry),
                SessionId::new(1),
                shared,
                brisvox_agent::tools::EngineConfig::default(),
            ));
            b.with_tools(registry, engine)
        }
    })
    .await;
    rig.llm.push(tool_script("Checking."));
    rig.llm.push(LlmScript::text(["never spoken"]));
    rig.turn(1, "q").await;
    for _ in 0..5_000 {
        if entered.load(Ordering::SeqCst) == 1 {
            break;
        }
        std::thread::sleep(MS);
    }
    assert_eq!(entered.load(Ordering::SeqCst), 1);
    let before = rig.conversation();
    assert_eq!(before.len(), 2, "{before:?}");

    rig.interrupt(2).await;
    rig.until("the join to time out", |r| {
        r.stats.snapshot().join_timeouts == 1
    })
    .await;
    // The tool returns after the join gave up. The task must see its cancelled token before it
    // appends the result (or a late note, or the next round's message).
    rig.settle(1_500 * MS).await;
    assert_eq!(
        rig.conversation(),
        before,
        "a halted task never mutates the context, even when the tool outlives the join"
    );
    assert_eq!(
        requests(&rig).len(),
        1,
        "and it starts no second round after the cancel"
    );
}
