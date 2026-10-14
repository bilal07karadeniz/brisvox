//! The provider contract suite run against the Anthropic adapter, on top of the loopback
//! mock.

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{Secret, SharedClock, SystemClock};
use brisvox_providers::anthropic::{AnthropicConfig, AnthropicLlm};
use brisvox_sim::contract::{Fault, LlmHarness, Response, llm_contract};
use serde_json::json;

use crate::mock::{Mock, Script, Step};
use crate::wire;

/// The deadline the adapter enforces in this test. The suite's own constants (2 s quiet windows
/// for the leak checks) dominate the run time; a shorter deadline keeps the timeout checks quick.
const DEADLINE: Duration = Duration::from_millis(400);

/// Time between two text deltas: slow enough that a 200-delta response is still streaming when
/// the cancel check hangs up.
const PACE: Duration = Duration::from_millis(15);

pub struct AnthropicHarness {
    mock: Mock,
    provider: Arc<AnthropicLlm>,
    clock: SharedClock,
}

impl AnthropicHarness {
    pub async fn new() -> Self {
        Self::with_adapter_deadline(DEADLINE).await
    }

    /// A harness whose adapter enforces `deadline` while the suite expects [`DEADLINE`].
    pub async fn with_adapter_deadline(deadline: Duration) -> Self {
        // Connection: close per response, so the runtime's task count returns to its baseline
        // after each request (a pooled idle connection keeps a task alive on purpose).
        let mock = Mock::start(false).await;
        let clock = SystemClock::shared();
        let config = AnthropicConfig::new(Secret::new("sk-ant-test-key".to_owned()))
            .with_base_url(mock.base_url())
            .with_timeout(deadline);
        let provider = Arc::new(AnthropicLlm::new(config, clock.clone()).expect("provider"));
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
        let kind = match status {
            401 => "authentication_error",
            403 => "permission_error",
            429 => "rate_limit_error",
            529 => "overloaded_error",
            500..=599 => "api_error",
            _ => "invalid_request_error",
        };
        let body = json!({"type": "error", "request_id": "req_1",
            "error": {"type": kind, "message": "invalid x-api-key sk-ant-abc***"}})
        .to_string();
        let mut script = Script::status(status, &body);
        if status == 429 {
            script = script.header("retry-after", "1");
        }
        script.pre_delay = pre_delay;
        return script;
    }

    // The counted events of the response (what the contract's `events_sent` counts): text
    // deltas, argument deltas, and the terminal event.
    let mut events: Vec<String> = Vec::new();
    let mut steps = vec![Step::event(wire::message_start(12))];
    let mut next_block = 0;
    if !response.text.is_empty() {
        steps.push(Step::event(wire::text_start(0)));
        next_block = 1;
    }
    for delta in &response.text {
        events.push(wire::text_delta(0, delta));
    }
    let text_events = events.len();
    // The tool blocks, opened right after the text (not counted).
    let mut openings = Vec::new();
    for (n, call) in response.tool_calls.iter().enumerate() {
        let index = next_block + n as u32;
        openings.push(wire::tool_start(index, &call.id, &call.name));
        for fragment in halves(&call.arguments) {
            events.push(wire::json_delta(index, &fragment));
        }
    }
    let stop_reason = if response.tool_calls.is_empty() {
        "end_turn"
    } else {
        "tool_use"
    };
    let last = events.len();
    events.push(wire::message_stop());

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
            if text_events > 0 {
                steps.push(Step::event(wire::block_stop(0)));
            }
            steps.extend(openings.drain(..).map(Step::event));
        }
        if i == last {
            // Every block is closed before the message ends, as the real API does.
            let calls = response.tool_calls.len() as u32;
            steps
                .extend((next_block..next_block + calls).map(|n| Step::event(wire::block_stop(n))));
            steps.push(Step::event(wire::message_delta(stop_reason, 5)));
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

impl LlmHarness for AnthropicHarness {
    type P = AnthropicLlm;

    fn provider(&self) -> Arc<AnthropicLlm> {
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
async fn the_anthropic_adapter_passes_the_llm_contract_suite() {
    let harness = AnthropicHarness::new().await;
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
    let harness = AnthropicHarness::with_adapter_deadline(DEADLINE * 3).await;
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
