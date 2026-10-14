//! The provider contract suite run against the Gemini adapter, on top of the loopback mock.

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{Secret, SharedClock, SystemClock};
use brisvox_providers::gemini::{GeminiConfig, GeminiLlm};
use brisvox_sim::contract::{Fault, LlmHarness, Response, llm_contract};
use serde_json::{Value, json};

use crate::mock::{Mock, Script, Step};

/// The deadline the adapter enforces in this test. The suite's own constants (2 s quiet windows
/// for the leak checks) dominate the run time; a shorter deadline keeps the timeout checks quick.
const DEADLINE: Duration = Duration::from_millis(400);

/// Time between two text deltas: slow enough that a 200-delta response is still streaming when
/// the cancel check hangs up.
const PACE: Duration = Duration::from_millis(15);

pub struct GeminiHarness {
    mock: Mock,
    provider: Arc<GeminiLlm>,
    clock: SharedClock,
}

impl GeminiHarness {
    pub async fn new() -> Self {
        Self::with_adapter_deadline(DEADLINE).await
    }

    /// A harness whose adapter enforces `deadline` while the suite expects [`DEADLINE`].
    pub async fn with_adapter_deadline(deadline: Duration) -> Self {
        // Connection: close per response, so the runtime's task count returns to its baseline
        // after each request (a pooled idle connection keeps a task alive on purpose).
        let mock = Mock::start(false).await;
        let clock = SystemClock::shared();
        let config = GeminiConfig::new(Secret::new("AIza-test-key".to_owned()))
            .with_base_url(mock.base_url())
            .with_timeout(deadline);
        let provider = Arc::new(GeminiLlm::new(config, clock.clone()).expect("provider"));
        Self {
            mock,
            provider,
            clock,
        }
    }
}

fn chunk(parts: Value, finish: Option<&str>, usage: bool) -> String {
    let mut candidate = json!({"content": {"role": "model", "parts": parts}, "index": 0});
    if let Some(finish) = finish {
        candidate["finishReason"] = json!(finish);
    }
    let mut chunk =
        json!({"candidates": [candidate], "modelVersion": "gemini-test", "responseId": "r1"});
    if usage {
        chunk["usageMetadata"] = json!({"promptTokenCount": 12, "cachedContentTokenCount": 8,
            "candidatesTokenCount": 5, "thoughtsTokenCount": 0, "totalTokenCount": 17});
    }
    chunk.to_string()
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
        let body = json!({"error": {"code": status, "message": "API key not valid: AIza-test***",
            "status": "INVALID_ARGUMENT",
            "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo",
                         "reason": "API_KEY_INVALID", "domain": "googleapis.com"}]}})
        .to_string();
        let mut script = Script::status(status, &body);
        if status == 429 {
            script = script.header("Retry-After", "1");
        }
        script.pre_delay = pre_delay;
        return script;
    }

    // The counted events of the response (what the contract's `events_sent` counts): one chunk
    // per text delta, one per (whole) tool call, and the final chunk.
    let mut events: Vec<String> = Vec::new();
    for delta in &response.text {
        events.push(chunk(json!([{"text": delta}]), None, false));
    }
    let text_events = events.len();
    for call in &response.tool_calls {
        let args: Value = serde_json::from_str(&call.arguments).expect("object arguments");
        events.push(chunk(
            json!([{"functionCall": {"id": call.id, "name": call.name, "args": args}}]),
            None,
            false,
        ));
    }
    events.push(chunk(json!([]), Some("STOP"), true));

    let mut steps = Vec::new();
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
        steps.push(Step::counted(event.clone()));
        if i < text_events {
            steps.push(Step::Sleep(PACE));
        }
    }
    let mut script = Script::stream(steps);
    script.pre_delay = pre_delay;
    script
}

impl LlmHarness for GeminiHarness {
    type P = GeminiLlm;

    fn provider(&self) -> Arc<GeminiLlm> {
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
async fn the_gemini_adapter_passes_the_llm_contract_suite() {
    let harness = GeminiHarness::new().await;
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
    let harness = GeminiHarness::with_adapter_deadline(DEADLINE * 3).await;
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
