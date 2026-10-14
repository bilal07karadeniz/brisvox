//! Tests of the telephony tools: `transfer_call`, `send_dtmf` and
//! `end_call` through the real `ToolEngine` over a recording fake of `TelephonyOps`, the DTMF
//! note, and the leave-message flow driven by the voicemail driver. Paused time with `SimClock`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use brisvox_agent::Assembled;
use brisvox_agent::context::{Context, Message, ToolCall, ToolCallId};
use brisvox_agent::flows::{Action, FlowManager};
use brisvox_agent::segment::Language;
use brisvox_agent::telephony::dtmf::{DtmfCollector, render_dtmf_note};
use brisvox_agent::telephony::tools::DEFAULT_SETTLE_TIMEOUT;
use brisvox_agent::telephony::voicemail::{
    ACTION_BARGE_IN, NODE_FAREWELL, NODE_HUMAN, NODE_LEAVE_MESSAGE, NODE_WAIT_GREETING, Step,
    VoicemailConfig, VoicemailDriver, VoicemailNode, VoicemailPolicy, voicemail_flow,
};
use brisvox_agent::telephony::{OpsFuture, register_telephony_tools};
use brisvox_agent::tools::{
    EngineConfig, IdempotencyKey, SessionId, ToolEngine, ToolKind, ToolRegistry,
};
use brisvox_agent::{
    AnsweredBy, Briefing, DtmfString, TelephonyError, TelephonyErrorKind, TelephonyOps,
    TelephonyPolicy, TransferMode, TransferReport, TransferRequest,
};
use brisvox_core::frame::DtmfDigit;
use brisvox_core::{SharedClock, SimClock, TurnEpoch};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn clock() -> SharedClock {
    Arc::new(SimClock::new())
}

fn epoch(n: u32) -> TurnEpoch {
    TurnEpoch::new(n)
}

/// A recording fake of the application's binding.
#[derive(Default)]
struct Fake {
    log: Mutex<Vec<String>>,
    dtmf: Mutex<Vec<(String, u64)>>,
    transfers: Mutex<Vec<TransferRequest>>,
    ended: AtomicUsize,
    /// Results of successive `transfer` calls; an empty queue means success.
    transfer_results: Mutex<VecDeque<Result<TransferReport, TelephonyError>>>,
    dtmf_error: Mutex<Option<TelephonyError>>,
    hang_settle: bool,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn hanging_settle() -> Arc<Self> {
        Arc::new(Self {
            hang_settle: true,
            ..Self::default()
        })
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn queue_transfer(&self, result: Result<TransferReport, TelephonyError>) {
        self.transfer_results.lock().unwrap().push_back(result);
    }
}

impl TelephonyOps for Fake {
    fn settle_speech(&self) -> OpsFuture<'_, ()> {
        Box::pin(async move {
            self.log.lock().unwrap().push("settle".to_owned());
            if self.hang_settle {
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }

    fn send_dtmf<'a>(&'a self, digits: &'a DtmfString, idempotency: u64) -> OpsFuture<'a, ()> {
        Box::pin(async move {
            self.log.lock().unwrap().push("dtmf".to_owned());
            if let Some(error) = self.dtmf_error.lock().unwrap().clone() {
                return Err(error);
            }
            self.dtmf
                .lock()
                .unwrap()
                .push((digits.as_str().to_owned(), idempotency));
            Ok(())
        })
    }

    fn transfer<'a>(&'a self, request: &'a TransferRequest) -> OpsFuture<'a, TransferReport> {
        Box::pin(async move {
            self.log.lock().unwrap().push("transfer".to_owned());
            self.transfers.lock().unwrap().push(request.clone());
            self.transfer_results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(TransferReport {
                    mode: request.mode,
                    briefing: if request.mode == TransferMode::Warm && request.summary.is_some() {
                        Briefing::Whisper
                    } else {
                        Briefing::None
                    },
                }))
        })
    }

    fn end_call(&self) -> OpsFuture<'_, ()> {
        Box::pin(async move {
            self.ended.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

fn policy() -> TelephonyPolicy {
    TelephonyPolicy::new()
        .destination("sales", "+905551112233", "the sales team")
        .unwrap()
        .destination("manager", "+905559998877", "the duty manager")
        .unwrap()
        .with_warm(true)
}

fn registry(ops: &Arc<Fake>, policy: &TelephonyPolicy, clock: &SharedClock) -> ToolRegistry {
    let ops: Arc<dyn TelephonyOps> = ops.clone();
    let mut registry = ToolRegistry::new();
    register_telephony_tools(&mut registry, &ops, policy, clock).unwrap();
    registry
}

fn engine(registry: ToolRegistry, clock: &SharedClock) -> ToolEngine {
    ToolEngine::new(
        Arc::new(registry),
        SessionId::new(3),
        Arc::clone(clock),
        EngineConfig::default(),
    )
}

fn call(turn: u32, seq: u32, name: &str, arguments: Value) -> Assembled {
    Assembled::Complete(ToolCall {
        id: ToolCallId::new(epoch(turn), seq),
        provider_id: None,
        name: Arc::from(name),
        arguments,
        provider_data: None,
    })
}

/// Runs one call and returns (is_error, parsed content).
async fn run(engine: &ToolEngine, turn: u32, seq: u32, name: &str, args: Value) -> (bool, Value) {
    let execution = engine
        .execute(
            vec![call(turn, seq, name, args)],
            epoch(turn),
            &CancellationToken::new(),
        )
        .await;
    match execution.results.first() {
        Some(Message::Tool {
            content, is_error, ..
        }) => (
            *is_error,
            serde_json::from_str(content).unwrap_or_else(|_| json!(content.to_string())),
        ),
        other => panic!("no tool result: {other:?}"),
    }
}

fn error_kind(body: &Value) -> &str {
    body["error"].as_str().unwrap_or_default()
}

#[test]
fn the_tool_set_follows_the_policy() {
    let c = clock();
    let fake = Fake::new();

    let all = registry(&fake, &policy(), &c);
    let names: Vec<&str> = all.specs().map(|s| &*s.name).collect();
    assert_eq!(names, ["end_call", "send_dtmf", "transfer_call"]);
    assert!(all.specs().all(|s| s.kind == ToolKind::SideEffect));
    let transfer = all.spec("transfer_call").unwrap();
    assert_eq!(
        transfer.parameters["properties"]["destination"]["enum"],
        json!(["sales", "manager"])
    );
    assert_eq!(
        transfer.parameters["properties"]["mode"]["enum"],
        json!(["cold", "warm"])
    );
    assert!(transfer.description.contains("sales (the sales team)"));
    assert!(
        !transfer.description.contains("+90555"),
        "numbers stay out of the prompt"
    );

    let minimal = registry(&fake, &TelephonyPolicy::new().with_dtmf(false), &c);
    let names: Vec<&str> = minimal.specs().map(|s| &*s.name).collect();
    assert_eq!(names, ["end_call"], "no destination: no transfer_call");

    let cold_only = TelephonyPolicy::new()
        .destination("sales", "+905551112233", "")
        .unwrap();
    let registry = self::registry(&fake, &cold_only, &c);
    assert_eq!(
        registry.spec("transfer_call").unwrap().parameters["properties"]["mode"]["enum"],
        json!(["cold"])
    );
}

#[tokio::test(start_paused = true)]
async fn a_cold_transfer_settles_speech_first_and_carries_the_idempotency_key() {
    let c = clock();
    let fake = Fake::new();
    let engine = engine(registry(&fake, &policy(), &c), &c);

    let (is_error, body) = run(
        &engine,
        1,
        0,
        "transfer_call",
        json!({ "destination": "sales", "reason": "caller wants an invoice" }),
    )
    .await;
    assert!(!is_error, "{body}");
    assert_eq!(body["status"], "initiated");
    assert_eq!(body["mode"], "cold");
    assert_eq!(body["destination"], "sales");
    assert!(
        !body.to_string().contains("+90555"),
        "the result carries the name, not the number"
    );

    assert_eq!(fake.log(), ["settle", "transfer"]);
    let transfers = fake.transfers.lock().unwrap();
    let request = &transfers[0];
    assert_eq!(request.destination.as_str(), "+905551112233");
    assert_eq!(request.destination_name, "sales");
    assert_eq!(request.mode, TransferMode::Cold);
    assert_eq!(request.reason.as_deref(), Some("caller wants an invoice"));
    let expected = IdempotencyKey::new(SessionId::new(3), epoch(1), ToolCallId::new(epoch(1), 0));
    assert_eq!(request.idempotency, expected.digest());
}

#[tokio::test(start_paused = true)]
async fn a_warm_transfer_passes_a_sanitized_summary() {
    let c = clock();
    let fake = Fake::new();
    let engine = engine(registry(&fake, &policy(), &c), &c);

    let (is_error, body) = run(
        &engine,
        1,
        0,
        "transfer_call",
        json!({
            "destination": "manager",
            "mode": "warm",
            "summary": "  Ali  wants\na refund\u{0}  of order 12 ",
        }),
    )
    .await;
    assert!(!is_error, "{body}");
    assert_eq!(body["mode"], "warm");
    assert_eq!(body["summary_delivered"], true);
    let transfers = fake.transfers.lock().unwrap();
    assert_eq!(
        transfers[0].summary.as_deref(),
        Some("Ali wants a refund of order 12")
    );
    assert_eq!(transfers[0].mode, TransferMode::Warm);
}

#[tokio::test(start_paused = true)]
async fn destinations_outside_the_allow_list_never_reach_the_binding() {
    let c = clock();
    let fake = Fake::new();
    let engine = engine(registry(&fake, &policy(), &c), &c);

    for (seq, args) in [
        json!({ "destination": "+905559990000" }),
        json!({ "destination": "premium" }),
        json!({}),
        json!({ "destination": "sales", "number": "+9000000000" }),
        json!({ "destination": "sales", "mode": "teleport" }),
    ]
    .into_iter()
    .enumerate()
    {
        let turn = u32::try_from(seq).unwrap() + 1;
        let (is_error, body) = run(&engine, turn, 0, "transfer_call", args).await;
        assert!(is_error);
        assert_eq!(error_kind(&body), "invalid_arguments", "{body}");
    }
    assert!(fake.log().is_empty());

    // Warm is refused when the policy does not offer it.
    let cold_only = TelephonyPolicy::new()
        .destination("sales", "+905551112233", "")
        .unwrap();
    let engine = self::engine(registry(&fake, &cold_only, &c), &c);
    let (is_error, body) = run(
        &engine,
        9,
        0,
        "transfer_call",
        json!({ "destination": "sales", "mode": "warm" }),
    )
    .await;
    assert!(is_error);
    assert_eq!(error_kind(&body), "invalid_arguments");
    assert!(fake.log().is_empty());
}

#[tokio::test(start_paused = true)]
async fn only_one_transfer_per_call_unless_the_first_failed_for_certain() {
    let c = clock();

    // Accepted: a second one is refused.
    let fake = Fake::new();
    let engine = engine(registry(&fake, &policy(), &c), &c);
    let (is_error, _) = run(
        &engine,
        1,
        0,
        "transfer_call",
        json!({ "destination": "sales" }),
    )
    .await;
    assert!(!is_error);
    let (is_error, body) = run(
        &engine,
        1,
        1,
        "transfer_call",
        json!({ "destination": "manager" }),
    )
    .await;
    assert!(is_error);
    assert!(
        body["detail"].as_str().unwrap().contains("already started"),
        "{body}"
    );
    assert_eq!(fake.transfers.lock().unwrap().len(), 1);

    // Rejected for certain: the claim is released and the model may try another destination.
    let fake = Fake::new();
    fake.queue_transfer(Err(TelephonyError::new(
        TelephonyErrorKind::Rejected,
        "the provider refused the transfer",
    )));
    let engine = self::engine(registry(&fake, &policy(), &c), &c);
    let (is_error, body) = run(
        &engine,
        1,
        0,
        "transfer_call",
        json!({ "destination": "sales" }),
    )
    .await;
    assert!(is_error);
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("Nothing was done"),
        "{body}"
    );
    let (is_error, _) = run(
        &engine,
        1,
        1,
        "transfer_call",
        json!({ "destination": "manager" }),
    )
    .await;
    assert!(!is_error);
    assert_eq!(fake.transfers.lock().unwrap().len(), 2);

    // Outcome unknown: the claim stays, and the model is told not to repeat it.
    let fake = Fake::new();
    fake.queue_transfer(Err(TelephonyError::new(
        TelephonyErrorKind::OutcomeUnknown,
        "timed out",
    )));
    let engine = self::engine(registry(&fake, &policy(), &c), &c);
    let (is_error, body) = run(
        &engine,
        1,
        0,
        "transfer_call",
        json!({ "destination": "sales" }),
    )
    .await;
    assert!(is_error);
    assert!(
        body["detail"].as_str().unwrap().contains("do not repeat"),
        "{body}"
    );
    let (is_error, _) = run(
        &engine,
        1,
        1,
        "transfer_call",
        json!({ "destination": "manager" }),
    )
    .await;
    assert!(is_error);
    assert_eq!(fake.transfers.lock().unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_repeated_call_id_does_not_transfer_twice() {
    let c = clock();
    let fake = Fake::new();
    let engine = engine(registry(&fake, &policy(), &c), &c);
    let args = json!({ "destination": "sales" });
    let (first_error, first) = run(&engine, 1, 0, "transfer_call", args.clone()).await;
    let (second_error, second) = run(&engine, 1, 0, "transfer_call", args).await;
    assert!(!first_error && !second_error);
    assert_eq!(first, second, "the recorded result is replayed");
    assert_eq!(
        fake.transfers.lock().unwrap().len(),
        1,
        "the binding ran once"
    );
}

#[tokio::test(start_paused = true)]
async fn a_speech_that_never_settles_delays_the_transfer_by_the_timeout_only() {
    let c = clock();
    let fake = Fake::hanging_settle();
    let engine = engine(registry(&fake, &policy(), &c), &c);
    let start = c.now();
    let (is_error, body) = run(
        &engine,
        1,
        0,
        "transfer_call",
        json!({ "destination": "sales" }),
    )
    .await;
    assert!(!is_error, "{body}");
    assert_eq!(c.now() - start, DEFAULT_SETTLE_TIMEOUT);
    assert_eq!(fake.log(), ["settle", "transfer"]);
}

#[tokio::test(start_paused = true)]
async fn send_dtmf_normalizes_validates_and_reports_failures() {
    let c = clock();
    let fake = Fake::new();
    let engine = engine(registry(&fake, &policy(), &c), &c);

    let (is_error, body) = run(&engine, 1, 0, "send_dtmf", json!({ "digits": "1a#wW2" })).await;
    assert!(!is_error, "{body}");
    assert_eq!(body, json!({ "status": "sent", "keys": 6 }));
    {
        let sent = fake.dtmf.lock().unwrap();
        assert_eq!(sent[0].0, "1A#wW2");
        let expected =
            IdempotencyKey::new(SessionId::new(3), epoch(1), ToolCallId::new(epoch(1), 0));
        assert_eq!(sent[0].1, expected.digest());
    }

    for (seq, digits) in ["", "12x", "1 2", &"1".repeat(33), "ww"].iter().enumerate() {
        let (is_error, body) = run(
            &engine,
            u32::try_from(seq).unwrap() + 10,
            0,
            "send_dtmf",
            json!({ "digits": digits }),
        )
        .await;
        assert!(is_error, "{digits:?}");
        assert!(
            matches!(error_kind(&body), "invalid_arguments" | "tool_failed"),
            "{digits:?}: {body}"
        );
    }
    assert_eq!(fake.dtmf.lock().unwrap().len(), 1);

    *fake.dtmf_error.lock().unwrap() = Some(TelephonyError::unsupported(
        "DTMF is not supported with this configuration",
    ));
    let (is_error, body) = run(&engine, 99, 0, "send_dtmf", json!({ "digits": "5" })).await;
    assert!(is_error);
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("not available on this call"),
        "{body}"
    );
}

#[tokio::test(start_paused = true)]
async fn dtmf_length_follows_the_policy() {
    let c = clock();
    let fake = Fake::new();
    let mut short = policy();
    short.max_dtmf_digits = 4;
    let engine = engine(registry(&fake, &short, &c), &c);
    let (is_error, _) = run(&engine, 1, 0, "send_dtmf", json!({ "digits": "1234" })).await;
    assert!(!is_error);
    let (is_error, _) = run(&engine, 1, 1, "send_dtmf", json!({ "digits": "12345" })).await;
    assert!(is_error);
    assert_eq!(fake.dtmf.lock().unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn end_call_records_the_request_and_asks_for_a_goodbye() {
    let c = clock();
    let fake = Fake::new();
    let engine = engine(registry(&fake, &policy(), &c), &c);
    let (is_error, body) = run(&engine, 1, 0, "end_call", json!({})).await;
    assert!(!is_error);
    assert_eq!(body["status"], "ending");
    assert!(body["instruction"].as_str().unwrap().contains("goodbye"));
    assert_eq!(fake.ended.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------------------------
// DTMF in
// ---------------------------------------------------------------------------------------------

#[test]
fn caller_key_presses_become_a_user_note() {
    let keys = [DtmfDigit::D1, DtmfDigit::Star, DtmfDigit::Pound];
    assert_eq!(
        render_dtmf_note(&keys, Language::En).unwrap(),
        "[The caller pressed the keys: 1 * #]"
    );
    assert!(
        render_dtmf_note(&keys, Language::Tr)
            .unwrap()
            .contains("tuşlara")
    );
    let collector = DtmfCollector::default();
    assert!(!collector.is_pending());
}

// ---------------------------------------------------------------------------------------------
// Voicemail flow
// ---------------------------------------------------------------------------------------------

const MESSAGE: &str = "Hello, this is Bella Tavola about your booking. Please call us back.";

fn flow_registry(c: &SharedClock) -> Arc<ToolRegistry> {
    Arc::new(registry(
        &Fake::new(),
        &TelephonyPolicy::new().with_dtmf(false),
        c,
    ))
}

fn now() -> tokio::time::Instant {
    #[allow(clippy::disallowed_methods)] // tests need some Instant value
    tokio::time::Instant::now()
}

#[tokio::test(start_paused = true)]
async fn a_machine_gets_the_fixed_message_and_the_call_ends() {
    let c = clock();
    let config = VoicemailConfig::new(Language::En, "Confirm the booking for tonight.", MESSAGE);
    let flow = voicemail_flow(&config, flow_registry(&c)).unwrap();
    let mut manager = FlowManager::new(Arc::new(flow), Arc::clone(&c));
    let mut ctx = Context::new();
    let entered = manager.start(&mut ctx).unwrap();
    assert_eq!(entered.node.as_str(), "classify");
    assert!(
        !entered.respond_immediately,
        "the bot waits for the verdict"
    );
    assert!(manager.tool_specs().is_empty());

    let t0 = now();
    let mut driver = VoicemailDriver::new(VoicemailPolicy::default(), t0);

    // Twilio `machine_start`: wait for the end of the greeting. No speech, barge-in off.
    let Step::Enter(node) = driver.on_verdict(AnsweredBy::MachineStart, t0) else {
        panic!("expected a node");
    };
    assert_eq!(node, VoicemailNode::WaitGreeting);
    let entered = manager
        .transition_to(&mut ctx, &node.node_id(), None)
        .await
        .unwrap();
    assert_eq!(entered.node.as_str(), NODE_WAIT_GREETING);
    assert!(!entered.respond_immediately);
    assert!(
        !entered
            .pre_actions
            .iter()
            .any(|a| matches!(a, Action::Say(_)))
    );

    // `machine_end_beep`: speak the message, then hang up.
    let Step::Enter(node) = driver.on_verdict(AnsweredBy::MachineEndBeep, t0) else {
        panic!("expected a node");
    };
    let entered = manager
        .transition_to(&mut ctx, &node.node_id(), None)
        .await
        .unwrap();
    assert_eq!(entered.node.as_str(), NODE_LEAVE_MESSAGE);
    assert!(
        !entered.respond_immediately,
        "no model turn while the message plays"
    );
    assert!(entered.context_replaced);
    assert_eq!(
        entered.pre_actions,
        vec![
            Action::Custom {
                name: ACTION_BARGE_IN.into(),
                payload: json!({ "enabled": false }),
            },
            Action::Say(MESSAGE.into()),
            Action::EndCall,
        ]
    );
    assert!(
        manager.tool_specs().is_empty(),
        "the model has no tools while leaving the message"
    );
    driver.message_played();
    assert_eq!(
        driver.outcome(),
        Some(brisvox_agent::telephony::voicemail::CallOutcome::MachineMessageLeft)
    );
    ctx.validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_person_gets_the_conversation_and_end_call_leads_to_the_farewell() {
    let c = clock();
    let config = VoicemailConfig::new(Language::Tr, "Bu akşamki rezervasyonu doğrula.", "Merhaba.");
    let registry = flow_registry(&c);
    let flow = voicemail_flow(&config, Arc::clone(&registry)).unwrap();
    let mut manager = FlowManager::new(Arc::new(flow), Arc::clone(&c));
    let mut ctx = Context::new();
    manager.start(&mut ctx).unwrap();
    assert!(manager.system_prompt().contains("kimin açtığı"));

    let t0 = now();
    let mut driver = VoicemailDriver::new(VoicemailPolicy::default(), t0);
    let Step::Enter(node) = driver.on_verdict(AnsweredBy::Human, t0) else {
        panic!("expected a node");
    };
    assert_eq!(node, VoicemailNode::Human);
    let entered = manager
        .transition_to(&mut ctx, &node.node_id(), None)
        .await
        .unwrap();
    assert_eq!(entered.node.as_str(), NODE_HUMAN);
    assert!(
        entered.respond_immediately,
        "the bot greets a person at once"
    );
    let tools: Vec<String> = manager
        .tool_specs()
        .iter()
        .map(|s| s.name.to_string())
        .collect();
    assert_eq!(tools, ["end_call"]);

    // The model calls end_call: the farewell node follows and hangs up after its response.
    let engine = ToolEngine::new(
        registry,
        SessionId::new(1),
        Arc::clone(&c),
        EngineConfig::default(),
    );
    ctx.push(Message::user("Hayır, teşekkürler."));
    let ids = manager.next_tool_call_ids(&ctx, epoch(1), 1);
    let call = ToolCall {
        id: ids[0].clone(),
        provider_id: None,
        name: Arc::from("end_call"),
        arguments: json!({}),
        provider_data: None,
    };
    ctx.push(Message::assistant_with_calls("", vec![call.clone()]));
    let mut screened = manager.screen(vec![Assembled::Complete(call)]);
    let execution = engine
        .execute(
            screened.take_admitted(),
            epoch(1),
            &CancellationToken::new(),
        )
        .await;
    let applied = manager
        .apply(&mut ctx, screened, execution, None)
        .await
        .unwrap();
    let entered = applied.transition.expect("end_call transitions");
    assert_eq!(entered.node.as_str(), NODE_FAREWELL);
    assert_eq!(entered.post_actions, vec![Action::EndCall]);
}

#[tokio::test(start_paused = true)]
async fn the_flow_validates_its_inputs() {
    let c = clock();
    let blank = VoicemailConfig::new(Language::En, "Talk.", "   ");
    assert!(voicemail_flow(&blank, flow_registry(&c)).is_err());

    // Without an end_call tool in the registry there is no farewell node and no tool binding.
    let bare = Arc::new(ToolRegistry::new());
    let config = VoicemailConfig::new(Language::En, "Talk.", MESSAGE);
    let flow = voicemail_flow(&config, bare).unwrap();
    assert!(flow.node(&NODE_FAREWELL.into()).is_none());
    assert!(flow.node(&NODE_HUMAN.into()).is_some());
}

#[test]
fn the_driver_and_the_flow_agree_on_node_names() {
    for node in [
        VoicemailNode::Human,
        VoicemailNode::WaitGreeting,
        VoicemailNode::LeaveMessage,
    ] {
        let id = node.node_id();
        assert!(
            [NODE_HUMAN, NODE_WAIT_GREETING, NODE_LEAVE_MESSAGE].contains(&id.as_str()),
            "{id}"
        );
    }
}
