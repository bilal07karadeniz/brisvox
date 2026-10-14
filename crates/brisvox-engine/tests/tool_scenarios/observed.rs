//! The provider under test, behind one type, and a wrapper that records every round.
//!
//! [`Observed`] sits between the runner and the provider: it keeps each request (what the engine
//! sent), when it was sent, when the first event came back (the time to first token) and when the
//! response ended, and how many tool calls it carried. For live runs it also spaces requests
//! (the free Gemini tier allows about five a minute).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_core::{Clock, SimClock};
use brisvox_providers::anthropic::AnthropicLlm;
use brisvox_providers::gemini::GeminiLlm;
use brisvox_providers::openai_compatible::OpenAiCompatibleLlm;
use brisvox_providers::{
    Error, FinishReason, LlmEvent, LlmProvider, LlmRequest, LlmStream, Result,
};
use brisvox_sim::mock_llm::MockLlm;
use tokio::sync::mpsc;

/// Every provider the scenarios run against.
pub enum AnyLlm {
    Mock(MockLlm),
    OpenAi(OpenAiCompatibleLlm),
    Anthropic(AnthropicLlm),
    Gemini(GeminiLlm),
}

impl LlmProvider for AnyLlm {
    fn name(&self) -> &'static str {
        match self {
            Self::Mock(p) => p.name(),
            Self::OpenAi(p) => p.name(),
            Self::Anthropic(p) => p.name(),
            Self::Gemini(p) => p.name(),
        }
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmStream> {
        match self {
            Self::Mock(p) => p.stream(request).await,
            Self::OpenAi(p) => p.stream(request).await,
            Self::Anthropic(p) => p.stream(request).await,
            Self::Gemini(p) => p.stream(request).await,
        }
    }

    async fn warm(&self) -> Result<()> {
        match self {
            Self::Mock(p) => p.warm().await,
            Self::OpenAi(p) => p.warm().await,
            Self::Anthropic(p) => p.warm().await,
            Self::Gemini(p) => p.warm().await,
        }
    }
}

/// One request to the provider and its response.
#[derive(Debug, Clone)]
pub struct Round {
    pub request: LlmRequest,
    /// Since the clock's origin.
    pub sent: Duration,
    /// The request was refused or failed before streaming.
    pub error: Option<String>,
    /// Sent to the first event of the response.
    pub first_event: Option<Duration>,
    /// Sent to the first text delta.
    pub first_text: Option<Duration>,
    /// Sent to the end of the response.
    pub total: Option<Duration>,
    pub tool_calls: usize,
    pub text_bytes: usize,
    pub finish: Option<FinishReason>,
}

impl Round {
    /// The rate limit of the provider (HTTP 429) refused this round.
    pub fn rate_limited(&self) -> bool {
        self.error.as_deref().is_some_and(|e| e.contains("429"))
    }
}

/// Records the rounds of `inner`; see the module docs.
pub struct Observed {
    inner: Arc<AnyLlm>,
    clock: SimClock,
    rounds: Arc<Mutex<Vec<Round>>>,
    /// Least time between two requests (zero: none).
    gap: Duration,
    last: Arc<Mutex<Option<Duration>>>,
    limited: Arc<AtomicBool>,
}

/// A handle on the rounds, kept by the test while the runner owns the provider.
#[derive(Clone)]
pub struct Rounds {
    rounds: Arc<Mutex<Vec<Round>>>,
    limited: Arc<AtomicBool>,
}

impl Rounds {
    pub fn all(&self) -> Vec<Round> {
        self.rounds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// A round was refused with HTTP 429.
    pub fn rate_limited(&self) -> bool {
        self.limited.load(Ordering::SeqCst)
    }
}

impl Observed {
    pub fn new(inner: AnyLlm, clock: SimClock, gap: Duration) -> (Self, Rounds) {
        let rounds: Arc<Mutex<Vec<Round>>> = Arc::default();
        let limited = Arc::new(AtomicBool::new(false));
        let handle = Rounds {
            rounds: Arc::clone(&rounds),
            limited: Arc::clone(&limited),
        };
        (
            Self {
                inner: Arc::new(inner),
                clock,
                rounds,
                gap,
                last: Arc::default(),
                limited,
            },
            handle,
        )
    }
}

fn update(rounds: &Mutex<Vec<Round>>, index: usize, change: impl FnOnce(&mut Round)) {
    let mut rounds = rounds.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(round) = rounds.get_mut(index) {
        change(round);
    }
}

impl LlmProvider for Observed {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    async fn warm(&self) -> Result<()> {
        self.inner.warm().await
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmStream> {
        if !self.gap.is_zero() {
            let wait = {
                let last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
                last.and_then(|at| (at + self.gap).checked_sub(self.clock.elapsed()))
            };
            if let Some(wait) = wait {
                self.clock.sleep(wait).await;
            }
        }
        let sent = self.clock.elapsed();
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(sent);
        let index = {
            let mut rounds = self.rounds.lock().unwrap_or_else(PoisonError::into_inner);
            rounds.push(Round {
                request: request.clone(),
                sent,
                error: None,
                first_event: None,
                first_text: None,
                total: None,
                tool_calls: 0,
                text_bytes: 0,
                finish: None,
            });
            rounds.len() - 1
        };
        let mut inner = match self.inner.stream(request).await {
            Ok(stream) => stream,
            Err(error) => {
                if matches!(error, Error::Rejected { status: 429, .. }) {
                    self.limited.store(true, Ordering::SeqCst);
                }
                let shown = error.to_string();
                update(&self.rounds, index, |r| r.error = Some(shown));
                return Err(error);
            }
        };
        let (tx, rx) = mpsc::channel(brisvox_providers::llm::LLM_EVENT_CAPACITY);
        let rounds = Arc::clone(&self.rounds);
        let limited = Arc::clone(&self.limited);
        let clock = self.clock;
        tokio::spawn(async move {
            let mut seen = std::collections::HashSet::new();
            while let Some(event) = inner.next().await {
                let at = clock.elapsed().saturating_sub(sent);
                match &event {
                    LlmEvent::TextDelta(text) => {
                        let bytes = text.len();
                        update(&rounds, index, |r| {
                            r.first_event.get_or_insert(at);
                            r.first_text.get_or_insert(at);
                            r.text_bytes += bytes;
                        });
                    }
                    LlmEvent::ToolCallDelta { index: slot, .. } => {
                        let opens = seen.insert(*slot);
                        update(&rounds, index, |r| {
                            r.first_event.get_or_insert(at);
                            r.tool_calls += usize::from(opens);
                        });
                    }
                    LlmEvent::Done { finish, .. } => {
                        let finish = *finish;
                        update(&rounds, index, |r| {
                            r.first_event.get_or_insert(at);
                            r.total = Some(at);
                            r.finish = Some(finish);
                        });
                    }
                    LlmEvent::Error(error) => {
                        if error.to_string().contains("429") {
                            limited.store(true, Ordering::SeqCst);
                        }
                        let shown = error.to_string();
                        update(&rounds, index, |r| r.error = Some(shown));
                    }
                }
                if tx.send(event).await.is_err() {
                    return;
                }
            }
        });
        Ok(LlmStream::new(rx))
    }
}
