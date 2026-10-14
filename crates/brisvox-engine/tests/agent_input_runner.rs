//! The runner side of the application inputs (`brisvox_engine::agent_input`): a bot line
//! (an `LlmText` from the turn driver) is spoken verbatim without an LLM request and enters the
//! context; conversation edits are applied before the next response, never during one. The LLM
//! is the simulator's `MockLlm`; time is a paused `SimClock`.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods
)]

mod common;

use std::sync::Arc;

use brisvox_agent::context::Message;
use brisvox_agent::tools::ToolKind;
use brisvox_core::Clock;
use brisvox_core::TurnEpoch;
use brisvox_core::frame::{DataFrame, TextFrame};
use brisvox_engine::{
    ConversationEdit, EditSender, EngineConfig, SpeculationConfig, candidate_frame,
    conversation_edits,
};
use brisvox_sim::mock_llm::LlmScript;
use common::MS;
use common::llm::{Rig, ToolProbe, lookup_tools};

/// The turn driver's bot line of `epoch` into the runner.
async fn line(rig: &Rig, epoch: u32, text: &str) {
    rig.running
        .input()
        .send(
            DataFrame::LlmText(TextFrame {
                epoch: TurnEpoch::new(epoch),
                text: Arc::from(text),
            })
            .into(),
        )
        .await
        .unwrap();
}

async fn rig_with_edits() -> (Rig, EditSender) {
    let (edits, input) = conversation_edits(16);
    let rig = Rig::new(EngineConfig::DEFAULT, move |builder, _| {
        builder
            .with_system_prompt("You are a voice agent.")
            .with_conversation_edits(input)
    })
    .await;
    (rig, edits)
}

#[tokio::test(start_paused = true)]
async fn a_bot_line_is_spoken_verbatim_without_a_request_and_enters_the_context() {
    let (rig, _edits) = rig_with_edits().await;
    line(&rig, 0, "Hello, how can I help?").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(
        rig.labels(),
        [
            "llm_response_started",
            "text:Hello, how can I help?",
            "flush",
            "llm_response_ended",
        ]
    );
    assert!(rig.llm.requests().is_empty(), "a line asks no LLM");
    assert_eq!(
        rig.conversation(),
        [
            "system:You are a voice agent.",
            "assistant:Hello, how can I help?"
        ]
    );
    // The next user turn's request carries the line before the user's words.
    rig.llm.push(LlmScript::text(["Sure."]));
    rig.turn(1, "book a table").await;
    rig.until("one request", |r| r.llm.requests().len() == 1)
        .await;
    let sent = rig.llm.requests();
    let texts: Vec<&str> = sent[0].messages.iter().map(|m| &*m.content).collect();
    assert_eq!(
        texts,
        [
            "You are a voice agent.",
            "Hello, how can I help?",
            "book a table"
        ]
    );
    let stats = rig.stats.snapshot();
    assert_eq!((stats.bot_lines, stats.turns), (1, 1), "{stats:?}");
    rig.context_copy().validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn edits_wait_for_the_next_response_and_shape_its_request() {
    let (rig, edits) = rig_with_edits().await;
    rig.llm.push(LlmScript::text(["First answer."]));
    rig.turn(0, "hi").await;
    rig.until_label("llm_response_ended").await;
    // Made after the response: nothing changes until the next one starts.
    edits
        .try_send(ConversationEdit::SetSystemPrompt(Arc::from("Be brief.")))
        .unwrap();
    edits
        .try_send(ConversationEdit::Append(Message::system(
            "The caller is a VIP.",
        )))
        .unwrap();
    rig.settle(100 * MS).await;
    assert_eq!(
        rig.conversation(),
        [
            "system:You are a voice agent.",
            "user:hi",
            "assistant:First answer."
        ],
        "edits are not applied between responses"
    );
    rig.llm.push(LlmScript::text(["Second."]));
    rig.turn(1, "and now?").await;
    rig.until("two requests", |r| r.llm.requests().len() == 2)
        .await;
    let sent = rig.llm.requests();
    let texts: Vec<&str> = sent[1].messages.iter().map(|m| &*m.content).collect();
    assert_eq!(
        texts,
        [
            "Be brief.",
            "hi",
            "First answer.",
            "The caller is a VIP.",
            "and now?"
        ]
    );
    assert_eq!(rig.stats.snapshot().edits_applied, 2);
    rig.context_copy().validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn hidden_tools_are_not_offered_and_a_call_to_one_does_not_run() {
    let probe = Arc::new(ToolProbe::default());
    let (edits, input) = conversation_edits(16);
    let rig = Rig::new(EngineConfig::DEFAULT, {
        let probe = Arc::clone(&probe);
        move |builder, clock| {
            let (registry, engine) = lookup_tools(clock, &probe, ToolKind::ReadOnly, 10 * MS, None);
            builder
                .with_tools(registry, engine)
                .with_conversation_edits(input)
        }
    })
    .await;
    edits
        .try_send(ConversationEdit::SetTools(Some(Arc::from([]))))
        .unwrap();
    // The model calls the hidden tool anyway.
    rig.llm
        .push(LlmScript::text(["Checking."]).with_tool_call("call_1", "lookup", r#"{"q":"x"}"#));
    rig.llm.push(LlmScript::text(["I cannot look that up."]));
    rig.turn(0, "weather?").await;
    rig.until("two requests", |r| r.llm.requests().len() == 2)
        .await;
    rig.until_label("llm_response_ended").await;
    let sent = rig.llm.requests();
    assert!(sent[0].tools.is_empty(), "the hidden tool is not offered");
    assert_eq!(probe.started(), 0, "the hidden tool did not run");
    let conversation = rig.conversation();
    assert!(
        conversation[2].starts_with("tool!:") && conversation[2].contains("unknown_tool"),
        "{conversation:?}"
    );
    assert_eq!(rig.stats.snapshot().hidden_tool_calls, 1);

    // Offered again: the next request lists it.
    edits.try_send(ConversationEdit::SetTools(None)).unwrap();
    rig.llm.push(LlmScript::text(["Fine."]));
    rig.turn(1, "try again").await;
    rig.until("three requests", |r| r.llm.requests().len() == 3)
        .await;
    assert_eq!(rig.llm.requests()[2].tools.len(), 1);
    rig.context_copy().validate().unwrap();
}

/// The speculation fingerprint sees only the length and the end of each message, and not the
/// offered tools: an edit applied at the turn end must discard the speculation anyway, or the
/// answer built with the old prompt (or tool list) would be committed.
#[tokio::test(start_paused = true)]
async fn an_edit_at_the_turn_end_discards_the_speculation() {
    let mut config = EngineConfig::DEFAULT;
    config.speculation = SpeculationConfig::ENABLED;
    let (edits, input) = conversation_edits(16);
    let rig = Rig::new(config, move |builder, _| {
        builder
            .with_system_prompt("customer: Alice. Be polite and brief.")
            .with_conversation_edits(input)
    })
    .await;
    rig.llm.push(LlmScript::text(["Hello Alice."]));
    rig.llm.push(LlmScript::text(["Hello Bobby."]));
    rig.running
        .input()
        .send(candidate_frame(TurnEpoch::new(1), "hi", rig.clock.now()))
        .await
        .unwrap();
    rig.settle(100 * MS).await;
    assert_eq!(rig.llm.requests().len(), 1, "the speculation started");
    // Same length, same last 16 bytes: invisible to the fingerprint.
    edits
        .try_send(ConversationEdit::SetSystemPrompt(Arc::from(
            "customer: Bobby. Be polite and brief.",
        )))
        .unwrap();
    rig.turn(1, "hi").await;
    rig.until_label("llm_response_ended").await;
    let spec = rig.spec_stats.snapshot();
    assert_eq!((spec.context_changes, spec.hits), (1, 0), "{spec:?}");
    assert_eq!(
        rig.conversation(),
        [
            "system:customer: Bobby. Be polite and brief.",
            "user:hi",
            "assistant:Hello Bobby."
        ]
    );
    let sent = rig.llm.requests();
    assert_eq!(sent.len(), 2, "the turn made its own request");
    assert_eq!(
        &*sent[1].messages[0].content,
        "customer: Bobby. Be polite and brief."
    );
}
