//! A rig for the LLM runner: `LlmRunner` -> recorder on a paused `SimClock`, over the
//! simulator's `MockLlm`.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::disallowed_methods
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_agent::context::{Context, Message};
use brisvox_agent::played::TruncationStats;
use brisvox_agent::tools::{
    EngineConfig as ToolEngineConfig, SessionId, Tool, ToolCallCtx, ToolEngine, ToolFuture,
    ToolKind, ToolOutput, ToolRegistry, ToolSpec,
};
use brisvox_core::frame::{
    ControlFrame, ControlKind, DataFrame, Frame, SystemFrame, TranscriptFrame,
};
use brisvox_core::{
    Clock, Direction, Inbox, Links, Pipeline, QueueConfig, RunningPipeline, SharedClock, SimClock,
    TimelineRecorder, TurnEpoch,
};
use brisvox_engine::{
    EngineConfig, FlowProbe, LlmRunner, LlmRunnerBuilder, LlmStats, RequestStats, SpeculationStats,
};
use brisvox_providers::{FinishReason, LlmEvent, LlmProvider, LlmRequest, LlmStream};
use brisvox_sim::mock_llm::MockLlm;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{MS, Recorded, Recorder};

/// The runner under test and everything around it. `P` is the provider; [`MockLlm`] unless the
/// test brings its own ([`Rig::with_provider`]; `llm` is then an unused mock).
pub struct Rig<P: LlmProvider = MockLlm> {
    pub running: RunningPipeline,
    pub rec: Recorded,
    pub upstream: Inbox,
    pub clock: SimClock,
    pub llm: MockLlm,
    pub timeline: TimelineRecorder,
    pub stats: LlmStats,
    pub request_stats: RequestStats,
    pub spec_stats: SpeculationStats,
    pub context: Arc<Mutex<Context>>,
    pub truncations: Arc<Mutex<TruncationStats>>,
    pub flow: FlowProbe<P>,
}

impl Rig<MockLlm> {
    pub async fn new(
        config: EngineConfig,
        configure: impl FnOnce(LlmRunnerBuilder<MockLlm>, &SimClock) -> LlmRunnerBuilder<MockLlm>,
    ) -> Self {
        let clock = SimClock::new();
        let shared: SharedClock = Arc::new(clock);
        let llm = MockLlm::new(shared);
        let mut rig = Rig::with_provider(config, llm.clone(), configure).await;
        rig.llm = llm;
        rig
    }

    pub async fn default() -> Self {
        Self::new(EngineConfig::DEFAULT, |b, _| b).await
    }
}

impl<P: LlmProvider> Rig<P> {
    /// A rig over `provider` (the paused `SimClock` is created here).
    pub async fn with_provider(
        config: EngineConfig,
        provider: P,
        configure: impl FnOnce(LlmRunnerBuilder<P>, &SimClock) -> LlmRunnerBuilder<P>,
    ) -> Self {
        Self::build(config, provider, configure, None).await
    }

    /// A rig whose last stage is a [`Gate`] with a one-frame inbox: while `gate.hold` is set it
    /// stops consuming at the first `LlmText`, so the runner's sends back up.
    pub async fn gated(
        config: EngineConfig,
        provider: P,
        gate: &Gate,
        configure: impl FnOnce(LlmRunnerBuilder<P>, &SimClock) -> LlmRunnerBuilder<P>,
    ) -> Self {
        Self::build(config, provider, configure, Some(gate.clone())).await
    }

    async fn build(
        config: EngineConfig,
        provider: P,
        configure: impl FnOnce(LlmRunnerBuilder<P>, &SimClock) -> LlmRunnerBuilder<P>,
        gate: Option<Gate>,
    ) -> Self {
        let clock = SimClock::new();
        let shared: SharedClock = Arc::new(clock);
        let llm = MockLlm::new(shared.clone());
        let timeline = TimelineRecorder::new();
        let builder = LlmRunner::builder(
            Arc::new(provider),
            &config,
            shared.clone(),
            timeline.clone(),
        );
        let runner = configure(builder, &clock).build();
        let stats = runner.stats();
        let request_stats = runner.request_stats();
        let spec_stats = runner.speculation_stats();
        let context = runner.context();
        let truncations = runner.truncations();
        let flow = runner.flow_probe();
        let rec = Recorded::default();
        let upstream = Inbox::new(QueueConfig::DEFAULT);
        let pipeline = Pipeline::new("llm").then_with_queue(runner, config.queues.bot);
        let pipeline = match gate {
            Some(mut gate) => {
                gate.rec = rec.clone();
                pipeline.then_with_queue(
                    gate,
                    QueueConfig {
                        data_capacity: 1,
                        ..QueueConfig::DEFAULT
                    },
                )
            }
            None => pipeline.then(Recorder(rec.clone())),
        };
        let running = pipeline
            .start(
                Links {
                    downstream: None,
                    upstream: Some(upstream.sender(Direction::Upstream)),
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        Self {
            running,
            rec,
            upstream,
            clock,
            llm,
            timeline,
            stats,
            request_stats,
            spec_stats,
            context,
            truncations,
            flow,
        }
    }

    /// The caller's final transcript and the end of their turn, as the turn driver sends them.
    pub async fn turn(&self, epoch: u32, text: &str) {
        let epoch = TurnEpoch::new(epoch);
        self.running
            .input()
            .send(
                DataFrame::Transcript(TranscriptFrame {
                    epoch,
                    text: Arc::from(text),
                    is_final: true,
                    received_at: self.clock.now(),
                })
                .into(),
            )
            .await
            .unwrap();
        self.running
            .input()
            .send(
                ControlFrame {
                    epoch,
                    kind: ControlKind::UserTurnEnded,
                }
                .into(),
            )
            .await
            .unwrap();
    }

    /// A barge-in: the turn driver's `Interrupt` into the runner.
    pub async fn interrupt(&self, new_epoch: u32) {
        self.running
            .input()
            .send(
                SystemFrame::Interrupt {
                    new_epoch: TurnEpoch::new(new_epoch),
                }
                .into(),
            )
            .await
            .unwrap();
    }

    /// Frames the runner sent downstream, as short labels.
    pub fn labels(&self) -> Vec<String> {
        self.rec
            .all()
            .into_iter()
            .filter(|(frame, dir)| {
                *dir == Direction::Downstream && !matches!(frame, Frame::System(SystemFrame::Start))
            })
            .map(|(frame, _)| label(&frame))
            .collect()
    }

    /// Waits (in simulated time) until `cond` holds.
    pub async fn until(&self, what: &str, cond: impl Fn(&Self) -> bool) {
        for _ in 0..3_000 {
            if cond(self) {
                return;
            }
            self.clock.sleep(10 * MS).await;
        }
        panic!(
            "timed out waiting for {what}; frames so far: {:?}; stats {:?}",
            self.labels(),
            self.stats.snapshot()
        );
    }

    pub async fn until_label(&self, wanted: &str) {
        let wanted = wanted.to_owned();
        self.until(&wanted.clone(), move |rig| rig.labels().contains(&wanted))
            .await;
    }

    /// Lets everything that can run, run (in simulated time).
    pub async fn settle(&self, d: Duration) {
        self.clock.sleep(d).await;
    }

    /// The upstream frames the runner sent, drained.
    pub fn upstream_frames(&mut self) -> Vec<Frame> {
        let mut out = Vec::new();
        while let Ok((frame, _)) = self.upstream.try_recv() {
            out.push(frame);
        }
        out
    }

    pub fn conversation(&self) -> Vec<String> {
        summarize(&self.context.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn context_copy(&self) -> Context {
        self.context
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

pub fn label(frame: &Frame) -> String {
    match frame {
        Frame::Data(DataFrame::LlmText(t)) => format!("text:{}", t.text),
        Frame::Control(c) => c.kind.name().to_owned(),
        other => other.name().to_owned(),
    }
}

/// The context as short strings: `user:..`, `assistant:..`, `assistant+calls:..`, `tool:..`.
pub fn summarize(context: &Context) -> Vec<String> {
    context
        .messages()
        .iter()
        .map(|m| match m {
            Message::System { text } => format!("system:{text}"),
            Message::User { text } => format!("user:{text}"),
            Message::Assistant {
                text, tool_calls, ..
            } if tool_calls.is_empty() => format!("assistant:{text}"),
            Message::Assistant {
                text, tool_calls, ..
            } => format!(
                "assistant+calls[{}]:{text}",
                tool_calls
                    .iter()
                    .map(|c| c.name.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Message::Tool {
                content, is_error, ..
            } => format!("tool{}:{content}", if *is_error { "!" } else { "" }),
        })
        .collect()
}

/// What a test tool did.
#[derive(Default)]
pub struct ToolProbe {
    pub started: AtomicUsize,
    pub finished: AtomicUsize,
}

impl ToolProbe {
    pub fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }
    pub fn finished(&self) -> usize {
        self.finished.load(Ordering::SeqCst)
    }
}

/// A tool that takes `delay` of simulated time and answers `result`.
pub struct SlowTool {
    pub probe: Arc<ToolProbe>,
    pub clock: SharedClock,
    pub delay: Duration,
    pub result: Value,
}

impl Tool for SlowTool {
    fn call<'a>(&'a self, _args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async move {
            self.probe.started.fetch_add(1, Ordering::SeqCst);
            self.clock.sleep(self.delay).await;
            self.probe.finished.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput::Json(self.result.clone()))
        })
    }
}

/// A registry with one tool, `lookup`, and an engine over it.
pub fn lookup_tools(
    rig_clock: &SimClock,
    probe: &Arc<ToolProbe>,
    kind: ToolKind,
    delay: Duration,
    filler_after: Option<Duration>,
) -> (Arc<ToolRegistry>, Arc<ToolEngine>) {
    lookup_tools_returning(
        rig_clock,
        probe,
        kind,
        delay,
        filler_after,
        json!({"answer": "sunny"}),
    )
}

/// [`lookup_tools`] with a chosen result.
pub fn lookup_tools_returning(
    rig_clock: &SimClock,
    probe: &Arc<ToolProbe>,
    kind: ToolKind,
    delay: Duration,
    filler_after: Option<Duration>,
    result: Value,
) -> (Arc<ToolRegistry>, Arc<ToolEngine>) {
    let clock: SharedClock = Arc::new(*rig_clock);
    let mut registry = ToolRegistry::new();
    registry
        .register(
            ToolSpec::new(
                "lookup",
                "Looks something up.",
                json!({"type": "object", "properties": {"q": {"type": "string"}}}),
                kind,
            )
            .with_timeout(Duration::from_secs(30))
            .with_filler_after(filler_after),
            SlowTool {
                probe: Arc::clone(probe),
                clock: clock.clone(),
                delay,
                result,
            },
        )
        .unwrap();
    let registry = Arc::new(registry);
    let engine = Arc::new(ToolEngine::new(
        Arc::clone(&registry),
        SessionId::new(1),
        clock,
        ToolEngineConfig::default(),
    ));
    (registry, engine)
}

// ---- a provider that does what the test says ----------------------------------------------------

/// What [`ActLlm`] does on its next request.
pub enum Act {
    /// Answer with this text, then `Done`.
    Text(String),
    /// Answer with this text, then `Done` carrying this `provider_data` JSON text.
    TextWithData(String, String),
    /// Answer with this text and one tool call (`id`, `name`, JSON `arguments`).
    ToolCall(String, String, String, String),
    /// Panic inside `stream`.
    Panic,
    /// Block the calling thread for this long (real time, ignores an abort until it returns),
    /// then answer with the text.
    Block(Duration, String),
}

/// A provider with a queue of [`Act`]s (the empty answer once the queue is empty).
#[derive(Clone, Default)]
pub struct ActLlm {
    acts: Arc<Mutex<std::collections::VecDeque<Act>>>,
    entered: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<LlmRequest>>>,
}

impl ActLlm {
    pub fn push(&self, act: Act) {
        self.acts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(act);
    }

    /// Requests that reached `stream`.
    pub fn entered(&self) -> usize {
        self.entered.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> Vec<LlmRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl LlmProvider for ActLlm {
    fn name(&self) -> &'static str {
        "act"
    }

    async fn stream(&self, request: LlmRequest) -> brisvox_providers::Result<LlmStream> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request);
        self.entered.fetch_add(1, Ordering::SeqCst);
        let act = self
            .acts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        let mut call = None;
        let (text, data) = match act {
            None => (String::new(), None),
            Some(Act::Text(text)) => (text, None),
            Some(Act::TextWithData(text, data)) => (text, Some(data)),
            Some(Act::ToolCall(text, id, name, arguments)) => {
                call = Some((id, name, arguments));
                (text, None)
            }
            Some(Act::Panic) => panic!("scripted provider panic"),
            Some(Act::Block(duration, text)) => {
                std::thread::sleep(duration);
                (text, None)
            }
        };
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        if !text.is_empty() {
            tx.try_send(LlmEvent::TextDelta(Arc::from(text))).unwrap();
        }
        let finish = if let Some((id, name, arguments)) = call {
            tx.try_send(LlmEvent::ToolCallDelta {
                index: 0,
                id: Some(Arc::from(id)),
                name: Some(Arc::from(name)),
                arguments: Arc::from(arguments),
            })
            .unwrap();
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        };
        tx.try_send(LlmEvent::Done {
            finish,
            usage: None,
            provider_data: data.map(Arc::from),
        })
        .unwrap();
        Ok(LlmStream::new(rx))
    }
}

/// A tail stage that records every frame like [`Recorder`] and, while `hold` is set, stops at the
/// first `LlmText` until `release` is notified (a back-pressured pipeline).
#[derive(Clone, Default)]
pub struct Gate {
    pub hold: Arc<std::sync::atomic::AtomicBool>,
    pub release: Arc<tokio::sync::Notify>,
    rec: Recorded,
}

impl brisvox_core::Processor for Gate {
    fn name(&self) -> &'static str {
        "gate"
    }

    fn epoch_policy(&self) -> brisvox_core::EpochPolicy {
        brisvox_core::EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        frame: Frame,
        direction: Direction,
        _ctx: &mut brisvox_core::ProcessorContext,
    ) -> brisvox_core::Result<()> {
        let hold =
            matches!(frame, Frame::Data(DataFrame::LlmText(_))) && self.hold.load(Ordering::SeqCst);
        self.rec.lock().push((frame, direction));
        if hold {
            self.release.notified().await;
        }
        Ok(())
    }
}
