//! The provider contract suite run against the adapter, once per preset, on top of the
//! loopback mock speaking that preset's dialect.

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{SharedClock, SystemClock};
use brisvox_providers::openai_compatible::{OpenAiCompatibleConfig, OpenAiCompatibleLlm, Preset};
use brisvox_sim::contract::{LlmHarness, Response, llm_contract};

use crate::mock::Mock;
use crate::wire::{Dialect, script_for};

/// The deadline the adapter enforces in this test. The suite's own constants (2 s quiet windows
/// for the leak checks) dominate the run time; a shorter deadline keeps the timeout checks quick.
const DEADLINE: Duration = Duration::from_millis(400);

pub struct CompatHarness {
    mock: Mock,
    provider: Arc<OpenAiCompatibleLlm>,
    clock: SharedClock,
    dialect: Dialect,
}

impl CompatHarness {
    pub async fn new(preset: Preset) -> Self {
        Self::with_adapter_deadline(preset, DEADLINE).await
    }

    /// A harness whose adapter enforces `deadline` while the suite expects [`DEADLINE`].
    pub async fn with_adapter_deadline(preset: Preset, deadline: Duration) -> Self {
        // Connection: close per response, so the runtime's task count returns to its baseline
        // after each request (a pooled idle connection keeps a task alive on purpose).
        let mock = Mock::start(false).await;
        let clock = SystemClock::shared();
        let config = OpenAiCompatibleConfig::preset(preset)
            .with_api_key("test-key-0123456789")
            .with_base_url(format!("{}/v1", mock.base_url()))
            .with_timeout(deadline);
        let provider = Arc::new(OpenAiCompatibleLlm::new(config, clock.clone()).expect("provider"));
        Self {
            mock,
            provider,
            clock,
            dialect: Dialect::of(preset),
        }
    }
}

impl LlmHarness for CompatHarness {
    type P = OpenAiCompatibleLlm;

    fn provider(&self) -> Arc<OpenAiCompatibleLlm> {
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
        self.mock.script(script_for(&self.dialect, &response));
    }

    fn events_sent(&self) -> usize {
        self.mock.events_sent()
    }
}

/// The whole suite against one preset: streaming, cancel, reconnect, error mapping, timeouts,
/// warm, task leaks. Takes about ten seconds because the suite itself waits out 2 s quiet
/// windows for its leak checks.
async fn passes_the_suite(preset: Preset) {
    let harness = CompatHarness::new(preset).await;
    let report = llm_contract(&harness).await;
    let failures: Vec<_> = report.failures().collect();
    assert!(
        report.passed(),
        "{preset:?}: contract failures: {failures:#?}\nall checks: {:#?}",
        report.checks
    );
    assert_eq!(report.checks.len(), 7);
}

#[tokio::test]
async fn the_groq_preset_passes_the_llm_contract_suite() {
    passes_the_suite(Preset::Groq).await;
}

#[tokio::test]
async fn the_together_preset_passes_the_llm_contract_suite() {
    passes_the_suite(Preset::Together).await;
}

#[tokio::test]
async fn the_fireworks_preset_passes_the_llm_contract_suite() {
    passes_the_suite(Preset::Fireworks).await;
}

#[tokio::test]
async fn the_vllm_preset_passes_the_llm_contract_suite() {
    passes_the_suite(Preset::Vllm).await;
}

#[tokio::test]
async fn the_ollama_preset_passes_the_llm_contract_suite() {
    passes_the_suite(Preset::Ollama).await;
}

#[tokio::test]
async fn the_openrouter_preset_passes_the_llm_contract_suite() {
    passes_the_suite(Preset::OpenRouter).await;
}

/// The suite must bite: an adapter whose deadlines are three times the configured ones fails the
/// timeout check (and only the checks that depend on the deadline), so a pass above means
/// something.
#[tokio::test]
async fn an_adapter_with_the_wrong_deadline_fails_the_timeout_check() {
    let harness = CompatHarness::with_adapter_deadline(Preset::Groq, DEADLINE * 3).await;
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
