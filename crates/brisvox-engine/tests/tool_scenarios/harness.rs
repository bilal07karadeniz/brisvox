//! The harness: one call of the real engine path (`LlmRunner` + `ToolEngine` in the runner's
//! pipeline, the rig of `tests/common/llm.rs`) over a scripted or a live provider.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_agent::context::{Context, Message};
use brisvox_agent::tools::{FillerPolicy, ToolEngine, ToolStatsSnapshot};
use brisvox_core::{Clock, SimClock};
use brisvox_engine::EngineConfig;
use brisvox_sim::mock_llm::{LlmFault, LlmScript, MockLlm};

use crate::common::llm::Rig;
use crate::observed::{AnyLlm, Observed, Round, Rounds};
use crate::wire::{Dialect, Turn, WireServer};
use crate::world::{Delays, ToolEvent, Which, World, tools};

pub const MS: Duration = Duration::from_millis(1);

/// How the scripted provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// `MockLlm`: the calls of a response arrive one after the other, argument fragments whole.
    Plain,
    /// `MockLlm`: the argument fragments of the calls interleave.
    Interleaved,
    /// `OpenAiCompatibleLlm` over a loopback server (interleaved `index` deltas).
    OpenAi,
    /// `AnthropicLlm` over a loopback server (one content block per call).
    Anthropic,
    /// `GeminiLlm` over a loopback server (whole `functionCall` parts in one chunk).
    Gemini,
}

impl Shape {
    pub const ALL: [Shape; 5] = [
        Shape::Plain,
        Shape::Interleaved,
        Shape::OpenAi,
        Shape::Anthropic,
        Shape::Gemini,
    ];

    /// The runtime is paused (simulated time); otherwise sockets need real time.
    pub fn paused(self) -> bool {
        matches!(self, Shape::Plain | Shape::Interleaved)
    }

    pub fn dialect(self) -> Option<Dialect> {
        match self {
            Shape::OpenAi => Some(Dialect::OpenAi),
            Shape::Anthropic => Some(Dialect::Anthropic),
            Shape::Gemini => Some(Dialect::Gemini),
            Shape::Plain | Shape::Interleaved => None,
        }
    }
}

/// The provider of a run, with the server behind it when it is one.
pub struct Provider {
    pub llm: AnyLlm,
    pub server: Option<WireServer>,
    pub dialect: Option<Dialect>,
    pub shape: Option<Shape>,
    /// Least time between two requests (live).
    pub gap: Duration,
    /// Rounds the script has (the engine must make exactly that many requests).
    pub scripted: Option<usize>,
    pub live: bool,
}

impl Provider {
    /// A scripted provider of `shape` that answers `turns` in order.
    pub async fn scripted(shape: Shape, turns: &[Turn]) -> Self {
        let clock = SimClock::new();
        let (llm, server) = match shape {
            Shape::Plain | Shape::Interleaved => {
                let mock = MockLlm::new(Arc::new(clock));
                for turn in turns {
                    let mut script = LlmScript::text(turn.text.clone());
                    for call in &turn.calls {
                        script = script.with_tool_call(&call.id, &call.name, &call.args);
                    }
                    if shape == Shape::Interleaved && turn.calls.len() > 1 {
                        script = script.with_fault(LlmFault::InterleavedToolDeltas);
                    }
                    mock.push(script);
                }
                (AnyLlm::Mock(mock), None)
            }
            Shape::OpenAi => {
                let server = WireServer::start(Dialect::OpenAi, turns).await;
                let mut config = brisvox_providers::openai_compatible::OpenAiCompatibleConfig::new(
                    format!("{}/v1", server.base_url()),
                    "scripted",
                );
                config.allow_insecure_http = true;
                let llm = brisvox_providers::openai_compatible::OpenAiCompatibleLlm::new(
                    config,
                    Arc::new(clock),
                )
                .unwrap();
                (AnyLlm::OpenAi(llm), Some(server))
            }
            Shape::Anthropic => {
                let server = WireServer::start(Dialect::Anthropic, turns).await;
                let config = brisvox_providers::anthropic::AnthropicConfig::new(
                    brisvox_core::Secret::new("sk-ant-scripted".to_owned()),
                )
                .with_base_url(server.base_url());
                let llm = brisvox_providers::anthropic::AnthropicLlm::new(config, Arc::new(clock))
                    .unwrap();
                (AnyLlm::Anthropic(llm), Some(server))
            }
            Shape::Gemini => {
                let server = WireServer::start(Dialect::Gemini, turns).await;
                let config = brisvox_providers::gemini::GeminiConfig::new(
                    brisvox_core::Secret::new("AIza-scripted".to_owned()),
                )
                .with_base_url(server.base_url());
                let llm =
                    brisvox_providers::gemini::GeminiLlm::new(config, Arc::new(clock)).unwrap();
                (AnyLlm::Gemini(llm), Some(server))
            }
        };
        Self {
            llm,
            server,
            dialect: shape.dialect(),
            shape: Some(shape),
            gap: Duration::ZERO,
            scripted: Some(turns.len()),
            live: false,
        }
    }

    /// The same provider judged like a live one: loose assertions only (to run the live
    /// checks of the scenarios against a script, which a real model's variety cannot).
    pub fn loose(mut self) -> Self {
        self.live = true;
        self.scripted = None;
        self
    }

    /// A real provider.
    pub fn live(llm: AnyLlm, gap: Duration) -> Self {
        Self {
            llm,
            server: None,
            dialect: None,
            shape: None,
            gap,
            scripted: None,
            live: true,
        }
    }

    pub fn name(&self) -> String {
        use brisvox_providers::LlmProvider;
        match self.shape {
            Some(shape) => format!("{shape:?}"),
            None => self.llm.name().to_owned(),
        }
    }
}

/// How the filler is configured.
#[derive(Debug, Clone, Copy)]
pub enum FillerSetup {
    /// The runner's default (fixed 1.2 s).
    Default,
    Fixed(Duration),
    /// Each tool's own `filler_after`.
    PerTool,
}

/// What a scenario needs around the provider.
#[derive(Clone)]
pub struct Setup {
    pub system: &'static str,
    pub which: Which,
    pub delays: Delays,
    pub filler: FillerSetup,
    /// Replaces every tool's timeout (30 s) when set.
    pub tool_timeout: Option<Duration>,
}

/// A run: the rig, the world and the records.
pub struct Env {
    pub rig: Rig<Observed>,
    pub world: Arc<World>,
    pub engine: Arc<ToolEngine>,
    pub rounds: Rounds,
    pub clock: SimClock,
    pub provider_name: String,
    pub dialect: Option<Dialect>,
    pub server: Option<WireServer>,
    pub scripted: Option<usize>,
    pub live: bool,
    /// Simulated time (a paused runtime); otherwise real time.
    pub paused: bool,
    /// When each frame label was first seen (index-aligned with `rig.labels()`).
    marks: Mutex<Vec<Duration>>,
}

impl Env {
    pub async fn start(provider: Provider, setup: &Setup) -> Self {
        let provider_name = provider.name();
        let clock = SimClock::new();
        let world = World::new(clock, setup.delays.clone());
        let (registry, engine) = tools(&world, setup.which, setup.tool_timeout);
        let (observed, rounds) = Observed::new(provider.llm, clock, provider.gap);
        let mut config = EngineConfig::DEFAULT;
        if provider.live {
            config.timeouts.llm_first_token = Duration::from_secs(45);
            config.timeouts.llm_idle = Duration::from_secs(45);
        }
        let policy = match setup.filler {
            FillerSetup::Default => FillerPolicy::new(),
            FillerSetup::Fixed(d) => FillerPolicy::new().with_delay(d),
            FillerSetup::PerTool => FillerPolicy::per_tool(),
        };
        let rig = Rig::with_provider(config, observed, {
            let engine = Arc::clone(&engine);
            let system = setup.system;
            move |builder, _| {
                builder
                    .with_system_prompt(system)
                    .with_tools(registry, engine)
                    .with_filler_policy(policy)
            }
        })
        .await;
        Self {
            rig,
            world,
            engine,
            rounds,
            clock,
            provider_name,
            dialect: provider.dialect,
            server: provider.server,
            scripted: provider.scripted,
            live: provider.live,
            paused: provider.shape.is_some_and(Shape::paused),
            marks: Mutex::default(),
        }
    }

    pub fn now(&self) -> Duration {
        self.clock.elapsed()
    }

    /// The provider refused with HTTP 429 (a free tier ran out): the run proves nothing, say so.
    pub fn limited(&self) -> bool {
        let limited = self.rounds.rate_limited();
        if limited {
            eprintln!(
                "SKIPPED: {} answered HTTP 429 (rate limit or daily quota); rerun later",
                self.provider_name
            );
        }
        limited
    }

    /// Longest wait for one step.
    pub fn limit(&self) -> Duration {
        if self.live {
            Duration::from_secs(120)
        } else {
            Duration::from_secs(30)
        }
    }

    fn mark(&self) {
        let labels = self.rig.labels().len();
        let mut marks = self.marks.lock().unwrap_or_else(PoisonError::into_inner);
        let now = self.clock.elapsed();
        while marks.len() < labels {
            marks.push(now);
        }
    }

    /// `llm_response_ended` frames so far.
    pub fn ended(&self) -> usize {
        self.rig
            .labels()
            .iter()
            .filter(|l| *l == "llm_response_ended")
            .count()
    }

    /// Waits until `cond` holds, polling every 5 ms of the clock, for at most `limit`.
    pub async fn wait(&self, what: &str, limit: Duration, cond: impl Fn(&Self) -> bool) {
        let deadline = self.clock.elapsed() + limit;
        loop {
            self.mark();
            if cond(self) || self.rounds.rate_limited() {
                return;
            }
            assert!(
                self.clock.elapsed() < deadline,
                "timed out ({limit:?}) waiting for {what}; frames: {:?}; context: {:?}; rounds: {}; tools: {:?}",
                self.rig.labels(),
                self.rig.conversation(),
                self.rounds.all().len(),
                self.world.events(),
            );
            self.clock.sleep(5 * MS).await;
        }
    }

    /// Lets `d` pass, tracking frames.
    pub async fn pass(&self, d: Duration) {
        let deadline = self.clock.elapsed() + d;
        while self.clock.elapsed() < deadline {
            self.mark();
            self.clock.sleep(5 * MS).await;
        }
        self.mark();
    }

    /// The caller says `text` as turn `epoch`.
    pub async fn say(&self, epoch: u32, text: &str) {
        self.rig.turn(epoch, text).await;
    }

    /// The caller says `text`; waits for the `n`th end of a response.
    pub async fn ask(&self, epoch: u32, text: &str, n: usize, limit: Duration) {
        self.say(epoch, text).await;
        self.wait("the end of the response", limit, |e| e.ended() >= n)
            .await;
        self.pass(20 * MS).await;
    }

    /// The text frames the runner sent downstream, in order.
    pub fn spoken(&self) -> Vec<String> {
        self.rig
            .labels()
            .into_iter()
            .filter_map(|l| l.strip_prefix("text:").map(str::to_owned))
            .collect()
    }

    /// Everything spoken, joined.
    pub fn spoken_text(&self) -> String {
        self.spoken().join("")
    }

    /// When the first frame with this label was seen, from the start of the clock.
    pub fn first_seen(&self, label: &str) -> Option<Duration> {
        let labels = self.rig.labels();
        let marks = self.marks.lock().unwrap_or_else(PoisonError::into_inner);
        labels
            .iter()
            .position(|l| l == label)
            .and_then(|i| marks.get(i).copied())
    }

    pub fn context(&self) -> Context {
        self.rig.context_copy()
    }

    pub fn tool_stats(&self) -> ToolStatsSnapshot {
        self.engine.stats()
    }

    pub fn requests(&self) -> Vec<Round> {
        self.rounds.all()
    }

    /// The tool results of the context, each as `(is_error, content)`, in order.
    pub fn tool_messages(&self) -> Vec<(bool, String)> {
        self.context()
            .messages()
            .iter()
            .filter_map(|m| match m {
                Message::Tool {
                    content, is_error, ..
                } => Some((*is_error, content.to_string())),
                _ => None,
            })
            .collect()
    }

    /// The assistant messages that called tools: the names of each message's calls.
    pub fn call_groups(&self) -> Vec<Vec<String>> {
        self.context()
            .messages()
            .iter()
            .filter_map(|m| match m {
                Message::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                    Some(tool_calls.iter().map(|c| c.name.to_string()).collect())
                }
                _ => None,
            })
            .collect()
    }

    /// The run's evidence as lines for a failure message and the live log.
    pub fn report(&self, scenario: &str) {
        eprintln!("--- {scenario} [{}] ---", self.provider_name);
        for (i, r) in self.requests().iter().enumerate() {
            eprintln!(
                "  round {i}: sent @{:>5} ms, first event {:>5} ms, first text {:>5} ms, total {:>5} ms, {} call(s), {} text bytes{}",
                r.sent.as_millis(),
                r.first_event.map_or(-1, |d| d.as_millis() as i64),
                r.first_text.map_or(-1, |d| d.as_millis() as i64),
                r.total.map_or(-1, |d| d.as_millis() as i64),
                r.tool_calls,
                r.text_bytes,
                r.error
                    .as_deref()
                    .map_or(String::new(), |e| format!(", error: {e}")),
            );
        }
        for e in self.world.events() {
            eprintln!(
                "  tool {} {} @{} ms .. {}{}",
                e.tool,
                e.args,
                e.started.as_millis(),
                e.finished
                    .map_or("-".to_owned(), |d| format!("{} ms", d.as_millis())),
                if e.dropped { " (dropped)" } else { "" },
            );
        }
        let stats = self.rig.stats.snapshot();
        eprintln!(
            "  runner: tool rounds {}, tool calls {}, fillers {}, late notes {}, fallbacks {}",
            stats.tool_rounds, stats.tool_calls, stats.fillers, stats.late_notes, stats.fallbacks
        );
        eprintln!("  spoken: {:?}", self.spoken_text());
    }
}

// ---- invariants every scenario must keep ---------------------------------------------------------

/// The context is valid and every tool-calling assistant message is followed by exactly its
/// results, in the order the model issued the calls.
pub fn check_context(env: &Env) {
    let context = env.context();
    context.validate().unwrap();
    let messages = context.messages();
    for (at, message) in messages.iter().enumerate() {
        let Message::Assistant { tool_calls, .. } = message else {
            continue;
        };
        for (k, call) in tool_calls.iter().enumerate() {
            let Some(Message::Tool { call_id, .. }) = messages.get(at + 1 + k) else {
                panic!(
                    "call {k} of message {at} has no result next to it: {:?}",
                    env.rig.conversation()
                );
            };
            assert_eq!(
                call_id,
                &call.id,
                "results are not in issue order: {:?}",
                env.rig.conversation()
            );
        }
    }
}

/// The wall time between the first start and the last finish of `events`.
pub fn span(events: &[ToolEvent]) -> Duration {
    let first = events.iter().map(|e| e.started).min().unwrap_or_default();
    let last = events
        .iter()
        .filter_map(|e| e.finished)
        .max()
        .unwrap_or_default();
    last.saturating_sub(first)
}
