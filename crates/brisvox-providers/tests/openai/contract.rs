//! The provider contract suite run against the OpenAI adapter, on top of the loopback mock.

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{Secret, SharedClock, SystemClock};
use brisvox_providers::openai::{OpenAiConfig, OpenAiLlm};
use brisvox_sim::contract::{Fault, LlmHarness, Response, llm_contract};
use serde_json::json;

use crate::mock::{Mock, Script, Step};

/// The deadline the adapter enforces in this test. The suite's own constants (2 s quiet windows
/// for the leak checks) dominate the run time; a shorter deadline keeps the timeout checks quick.
const DEADLINE: Duration = Duration::from_millis(400);

/// Time between two text deltas: slow enough that a 200-delta response is still streaming when
/// the cancel check hangs up.
const PACE: Duration = Duration::from_millis(15);

pub struct OpenAiHarness {
    mock: Mock,
    provider: Arc<OpenAiLlm>,
    clock: SharedClock,
}

impl OpenAiHarness {
    pub async fn new() -> Self {
        Self::with_adapter_deadline(DEADLINE).await
    }

    /// A harness whose adapter enforces `deadline` while the suite expects [`DEADLINE`].
    pub async fn with_adapter_deadline(deadline: Duration) -> Self {
        // Connection: close per response, so the runtime's task count returns to its baseline
        // after each request (a pooled idle connection keeps a task alive on purpose).
        let mock = Mock::start(false).await;
        let clock = SystemClock::shared();
        let config = OpenAiConfig::new(Secret::new("sk-test-key".to_owned()))
            .with_base_url(mock.base_url())
            .with_timeout(deadline);
        let provider = Arc::new(OpenAiLlm::new(config, clock.clone()).expect("provider"));
        Self {
            mock,
            provider,
            clock,
        }
    }
}

/// Splits `text` into at most two pieces to exercise fragment joining.
fn halves(text: &str) -> Vec<String> {
    let mid = text.len() / 2;
    if mid == 0 || !text.is_char_boundary(mid) {
        return vec![text.to_owned()];
    }
    let (a, b) = text.split_at(mid);
    vec![a.to_owned(), b.to_owned()]
}

/// The wire script for a contract response and its faults.
fn script_for(response: &Response) -> Script {
    let mut reject = None;
    let mut pre_delay = Duration::ZERO;
    for fault in &response.faults {
        match fault {
            Fault::Reject { status } => reject = Some(*status),
            Fault::ConnectStall { duration } => pre_delay = *duration,
            _ => {}
        }
    }
    if let Some(status) = reject {
        let body = json!({"error": {"message": "Incorrect API key provided: sk-abc***",
            "type": "invalid_request_error", "code": "invalid_api_key"}})
        .to_string();
        let mut script = Script::status(status, &body);
        if status == 429 {
            script = script.header("Retry-After", "1");
        }
        script.pre_delay = pre_delay;
        return script;
    }

    // The counted events of the response (what the contract's `events_sent` counts): text
    // deltas, argument deltas, and the terminal event.
    let mut events: Vec<String> = Vec::new();
    let mut steps = vec![Step::event(
        json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress"}})
            .to_string(),
    )];
    let mut next_output = 0;
    if !response.text.is_empty() {
        steps.push(Step::event(
            json!({"type": "response.output_item.added", "output_index": 0,
                   "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}})
            .to_string(),
        ));
        next_output = 1;
    }
    for delta in &response.text {
        events.push(
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0,
                   "content_index": 0, "delta": delta})
            .to_string(),
        );
    }
    let text_events = events.len();
    // Announcements of the tool calls (not counted), emitted right after the text.
    let mut announcements = Vec::new();
    for (n, call) in response.tool_calls.iter().enumerate() {
        let output_index = next_output + n;
        let item_id = format!("fc_{n}");
        announcements.push(
            json!({"type": "response.output_item.added", "output_index": output_index,
                   "item": {"type": "function_call", "id": item_id, "call_id": call.id,
                            "name": call.name, "arguments": ""}})
            .to_string(),
        );
        for fragment in halves(&call.arguments) {
            events.push(
                json!({"type": "response.function_call_arguments.delta", "item_id": item_id,
                       "output_index": output_index, "delta": fragment})
                .to_string(),
            );
        }
    }
    events.push(
        json!({"type": "response.completed", "response": {
            "id": "resp_1", "status": "completed",
            "output": if response.tool_calls.is_empty() { json!([{"type": "message"}]) }
                      else { json!([{"type": "function_call"}]) },
            "usage": {"input_tokens": 12, "input_tokens_details": {"cached_tokens": 8},
                      "output_tokens": 5, "output_tokens_details": {"reasoning_tokens": 0},
                      "total_tokens": 17}}})
        .to_string(),
    );

    for (i, event) in events.iter().enumerate() {
        for fault in &response.faults {
            match fault {
                Fault::Stall {
                    before_event,
                    duration,
                } if *before_event == i => steps.push(Step::Sleep(*duration)),
                Fault::Disconnect { after_events } if *after_events == i => steps.push(Step::Close),
                Fault::Protocol { after_events } if *after_events == i => {
                    steps.push(Step::raw(b"data: {this is not json\n\n".to_vec()));
                }
                _ => {}
            }
        }
        if i == text_events {
            steps.extend(announcements.drain(..).map(Step::event));
        }
        steps.push(Step::counted(event.clone()));
        if i < text_events {
            steps.push(Step::Sleep(PACE));
        }
    }
    let mut script = Script::stream(steps);
    script.pre_delay = pre_delay;
    script
}

impl LlmHarness for OpenAiHarness {
    type P = OpenAiLlm;

    fn provider(&self) -> Arc<OpenAiLlm> {
        Arc::clone(&self.provider)
    }

    fn clock(&self) -> SharedClock {
        self.clock.clone()
    }

    fn io_timeout(&self) -> Duration {
        DEADLINE
    }

    fn reset(&self) {
        self.mock.clear_scripts();
    }

    fn script(&self, response: Response) {
        self.mock.script(script_for(&response));
    }

    fn events_sent(&self) -> usize {
        self.mock.events_sent()
    }
}

/// The whole suite: streaming, cancel, reconnect, error mapping, timeouts, warm, task leaks.
/// Takes about ten seconds because the suite itself waits out 2 s quiet windows for its leak
/// checks.
#[tokio::test]
async fn the_openai_adapter_passes_the_llm_contract_suite() {
    let harness = OpenAiHarness::new().await;
    let report = llm_contract(&harness).await;
    let failures: Vec<_> = report.failures().collect();
    assert!(
        report.passed(),
        "contract failures: {failures:#?}\nall checks: {:#?}",
        report.checks
    );
    assert_eq!(report.checks.len(), 7);
}

/// The suite must bite: an adapter whose deadlines are three times the configured ones fails the
/// timeout check (and only the checks that depend on the deadline), so a pass above means
/// something.
#[tokio::test]
async fn an_adapter_with_the_wrong_deadline_fails_the_timeout_check() {
    let harness = OpenAiHarness::with_adapter_deadline(DEADLINE * 3).await;
    let report = llm_contract(&harness).await;
    assert!(
        report.check("timeout").is_some_and(|c| !c.passed()),
        "{report:#?}"
    );
    assert!(
        report.check("streaming").is_some_and(|c| c.passed()),
        "{report:#?}"
    );
}
