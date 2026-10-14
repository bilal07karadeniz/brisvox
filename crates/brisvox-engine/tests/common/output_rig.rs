//! The all-production pipeline over the loopback transport as a `SessionFactory` (shared by
//! `hybrid_output.rs` and `playout_truncation.rs`, included with `#[path]`): the engine's input
//! chain (pump, `AudioIn`, VAD worker), `SttStage`, `TurnDriver`, `LlmRunner` with the
//! `PlayoutBook`-backed truncation source, and the output chain (`SegmenterStage`, `TtsStage`,
//! `TransportOut`) writing to a real `TransportSession` over the loopback transport.
#![allow(dead_code, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_agent::tools::{
    EngineConfig as ToolEngineConfig, SessionId, Tool, ToolCallCtx, ToolEngine, ToolFuture,
    ToolKind, ToolOutput, ToolRegistry, ToolSpec,
};
use brisvox_agent::{Language, TurnPolicy};
use brisvox_core::frame::{ControlFrame, ControlKind, DataFrame, Frame};
use brisvox_core::{Direction, EpochPolicy, Pipeline, Processor, ProcessorContext, SharedClock};
use brisvox_engine::{
    AudioIn, CallRatesCell, EnergyModel, EngineConfig, InterruptionConfig, LlmRunner, LlmStats,
    Offload, PlayoutTruncation, PlayoutTruncationConfig, SegmenterStage, SegmenterStageConfig,
    SegmenterStats, SttStage, SttStageConfig, SttStats, TransportOut, TransportOutConfig,
    TransportOutStats, TruncationSource, TtsStage, TtsStageConfig, TtsStats, TurnDriver,
    TurnDriverConfig, TurnDriverStats, TurnProbe, VadSetup, VadWorkerConfig, clear_hook,
};
use brisvox_inference::VadParams;
use brisvox_inference::smart_turn::TurnModel;
use brisvox_sim::factory::{BuiltSession, SessionFactory, SessionParts};
use brisvox_sim::input::{InputSink, InputTarget};
use brisvox_sim::output::{OutputLog, WireKind};
use brisvox_sim::reference::{PlayLog, SessionProbes, WordLog};
use brisvox_transport::ClientEvent;

use crate::common::MS;
use crate::common::loopback::{self, LoopbackInput, LoopbackOptions};

/// A Smart Turn model with a fixed answer.
pub struct FixedModel(pub f32);

impl TurnModel for FixedModel {
    fn predict(&self, _features: &[f32]) -> brisvox_inference::Result<f32> {
        Ok(self.0)
    }
}

/// How the turn driver decides.
#[derive(Clone, Copy, Debug)]
pub enum Policy {
    /// Fixed silence of 450 ms after the (backdated) VAD stop.
    Fixed,
    /// Fixed silence of 300 ms: for tests whose barge-ins come 0.4 s after the first utterance
    /// (with 450 ms the caller would "resume" the first turn).
    Quick,
    /// Adaptive with a model that always answers `probability`.
    Smart(f32),
}

/// One built call's handles, for tests that look inside.
#[derive(Clone)]
pub struct CallHandles {
    pub input: Arc<LoopbackInput>,
    pub driver: TurnDriverStats,
    pub stt: SttStats,
    pub llm: LlmStats,
    pub segmenter: SegmenterStats,
    pub tts: TtsStats,
    pub out: TransportOutStats,
    /// `TtsEnded` frames that travelled up to the turn driver (counted just below it).
    pub ends_at_driver: Arc<AtomicU64>,
}

/// A recording tap in front of the transport stage: the suite's output records (the interrupts
/// are seen here, not at the client) and the word log of the probes.
struct Tap {
    output: OutputLog,
    words: WordLog,
    clock: SharedClock,
}

impl Processor for Tap {
    fn name(&self) -> &'static str {
        "output_tap"
    }

    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::DropStale
    }

    async fn process(
        &mut self,
        frame: Frame,
        direction: Direction,
        ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        if direction == Direction::Upstream {
            if let Some(up) = ctx.sender(Direction::Upstream) {
                let _ = up.try_send(frame);
            }
            return Ok(());
        }
        self.output.record_frame(&frame, self.clock.now());
        if let Frame::Data(DataFrame::Word(word)) = &frame {
            self.words
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((word.epoch, Arc::clone(&word.word), word.start));
        }
        ctx.forward(frame, direction).await
    }
}

/// A pass-through right below the turn driver that counts the `TtsEnded` frames travelling up to
/// it, whoever sent them (the runner for a turn without text, the transport stage for a response
/// without audio or on the mark echo).
struct UpCount(Arc<AtomicU64>);

impl Processor for UpCount {
    fn name(&self) -> &'static str {
        "up_count"
    }

    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        frame: Frame,
        direction: Direction,
        ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        if direction == Direction::Upstream {
            if matches!(
                &frame,
                Frame::Control(ControlFrame {
                    kind: ControlKind::TtsEnded,
                    ..
                })
            ) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            if let Some(up) = ctx.sender(Direction::Upstream) {
                let _ = up.try_send(frame);
            }
            return Ok(());
        }
        ctx.forward(frame, direction).await
    }
}

/// A ReadOnly tool that answers at once (the suite's s08 calls `time_in`).
struct TimeIn;

impl Tool for TimeIn {
    fn call<'a>(&'a self, _args: serde_json::Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async { Ok(ToolOutput::Json(serde_json::json!({"time": "09:00"}))) })
    }
}

pub fn time_in_tools(clock: &SharedClock) -> (Arc<ToolRegistry>, Arc<ToolEngine>) {
    let mut registry = ToolRegistry::new();
    registry
        .register(
            ToolSpec::new(
                "time_in",
                "The local time in a city.",
                serde_json::json!({
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"],
                }),
                ToolKind::ReadOnly,
            ),
            TimeIn,
        )
        .unwrap();
    let registry = Arc::new(registry);
    let engine = Arc::new(ToolEngine::new(
        Arc::clone(&registry),
        SessionId::new(1),
        clock.clone(),
        ToolEngineConfig::default(),
    ));
    (registry, engine)
}

/// Builds the tools of a call from its clock (see [`HybridOutput::with_tool_factory`]).
pub type ToolFactory =
    Arc<dyn Fn(&SharedClock) -> (Arc<ToolRegistry>, Arc<ToolEngine>) + Send + Sync>;

/// Wraps the production truncation source (see [`HybridOutput::with_source_wrap`]).
pub type SourceWrap =
    Arc<dyn Fn(Arc<dyn TruncationSource>) -> Arc<dyn TruncationSource> + Send + Sync>;

/// The all-production pipeline over the loopback transport, as a [`SessionFactory`]: the engine's
/// input chain, STT stage, turn driver, LLM runner with the `PlayoutBook`-backed
/// [`PlayoutTruncation`](brisvox_engine::PlayoutTruncation), and the output chain.
#[derive(Clone)]
pub struct HybridOutput {
    pub config: EngineConfig,
    pub policy: Policy,
    tools: Option<ToolFactory>,
    wrap: Option<SourceWrap>,
    calls: Arc<Mutex<Vec<CallHandles>>>,
}

impl HybridOutput {
    pub fn new(policy: Policy, interruption: InterruptionConfig) -> Self {
        let mut config = EngineConfig::DEFAULT;
        config.interruption = interruption;
        // The simulator's energy VAD reports a stop 300 ms after the speech.
        config.interruption.vad_hangover = if interruption == InterruptionConfig::OFF {
            Duration::ZERO
        } else {
            300 * MS
        };
        config.turn = match policy {
            Policy::Fixed => TurnPolicy::fixed(450 * MS),
            Policy::Quick => TurnPolicy::fixed(300 * MS),
            Policy::Smart(_) => TurnPolicy::Adaptive {
                fallback_silence: Duration::from_secs(3),
                require_final: true,
                final_grace: 800 * MS,
            },
        };
        // The loopback client's playback is the wire arrival: nothing is buffered beyond it.
        config.lead_margin = Duration::ZERO;
        // The suite's s06 scripts a failing response followed by the NEXT turn's answer: a retry
        // would consume that answer (see hybrid_llm.rs).
        config.llm.retries = 0;
        Self {
            config,
            policy,
            tools: None,
            wrap: None,
            calls: Arc::default(),
        }
    }

    /// The production default for LLM retries (one) instead of the suite accommodation (zero).
    pub fn with_default_retries(mut self) -> Self {
        self.config.llm.retries = EngineConfig::DEFAULT.llm.retries;
        self
    }

    /// The runner has the `time_in` tool (the suite's s08 calls it).
    pub fn with_tools(self) -> Self {
        self.with_tool_factory(Arc::new(time_in_tools))
    }

    /// The runner has the tools `factory` builds for each call.
    pub fn with_tool_factory(mut self, factory: ToolFactory) -> Self {
        self.tools = Some(factory);
        self
    }

    /// Passes the production truncation source through `wrap` (a test double around it).
    pub fn with_source_wrap(mut self, wrap: SourceWrap) -> Self {
        self.wrap = Some(wrap);
        self
    }

    pub fn handles(&self) -> Vec<CallHandles> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl SessionFactory for HybridOutput {
    async fn build(&self, parts: SessionParts) -> brisvox_sim::Result<BuiltSession> {
        let SessionParts {
            clock,
            stt,
            llm: mock_llm,
            tts: mock_tts,
            vad,
            transport_delay: _,
            timeline,
            origin,
            input_rate: _,
            output_rate: _,
        } = parts;
        let rates = CallRatesCell::new(self.config.rates.fallback_call);
        let (input, commands) = loopback::open_with(
            &clock,
            &self.config,
            LoopbackOptions {
                client_queue: Some(65_536),
                output: Some(rates.clone()),
            },
        )
        .await
        .map_err(|e| brisvox_sim::Error::Assertion(format!("hybrid setup: {e}")))?;
        let book = Arc::clone(&input.book);

        let vad_setup = VadSetup::new(
            Offload::Inline(clock.clone()),
            VadWorkerConfig {
                params: VadParams {
                    threshold: 0.5,
                    neg_threshold: 0.35,
                    min_speech: vad.start_after,
                    min_silence: vad.stop_after,
                    speech_pad: Duration::ZERO,
                },
                ..VadWorkerConfig::from_engine(&self.config)
            },
            move || {
                #[allow(clippy::cast_possible_truncation)] // a threshold of a few hundred
                Ok(EnergyModel::from_pcm_rms(vad.threshold as f32))
            },
        );
        let audio_in = AudioIn::new(&self.config).with_vad(vad_setup);

        let stt_stage = SttStage::new(
            Arc::new(stt),
            SttStageConfig::from_engine(&self.config),
            clock.clone(),
        );
        let mut driver = TurnDriver::new(
            TurnDriverConfig::from_engine(&self.config),
            clock.clone(),
            timeline.clone(),
        )
        .with_clear_hook(clear_hook(commands.clone(), Arc::clone(&book)))
        .with_probe(TurnProbe::new(256));
        if let Policy::Smart(probability) = self.policy {
            driver = driver.with_smart_turn(
                Arc::new(FixedModel(probability)),
                Offload::Inline(clock.clone()),
            );
        }

        let mut builder = LlmRunner::builder(
            Arc::new(mock_llm),
            &self.config,
            clock.clone(),
            timeline.clone(),
        )
        .with_playout(Arc::clone(&book));
        if let Some(wrap) = &self.wrap {
            let source = PlayoutTruncation::new(
                Arc::clone(&book),
                clock.clone(),
                PlayoutTruncationConfig::from_engine(&self.config),
            );
            builder = builder.with_truncation(wrap(Arc::new(source)));
        }
        if let Some(tools) = &self.tools {
            let (registry, engine) = tools(&clock);
            builder = builder.with_tools(registry, engine);
        }
        let llm_stage = builder.build();

        // The output chain under test.
        let segmenter = SegmenterStage::new(SegmenterStageConfig::new(Language::En));
        let tts_stage = TtsStage::new(
            Arc::new(mock_tts),
            TtsStageConfig::from_engine(&self.config),
            clock.clone(),
            timeline.clone(),
        )
        .with_book(Arc::clone(&book))
        .with_rates(rates)
        .with_offload(Offload::Inline(clock.clone()));
        let transport_out = TransportOut::new(
            commands,
            Arc::clone(&book),
            TransportOutConfig::from_engine(&self.config),
            clock.clone(),
            timeline.clone(),
        );

        // The probes and the log: the tap sees the pipeline's output, the drain hook below turns
        // what the client received into wire records and the playout log.
        let words = WordLog::default();
        let played = PlayLog::default();
        // The transport stage marks FirstAudioOut and AudioOutStopped itself, at the real
        // handoff: the log must not mark them at the tap.
        let output = OutputLog::new(origin, None);
        let tap = Tap {
            output: output.clone(),
            words: Arc::clone(&words),
            clock: clock.clone(),
        };
        let ends_at_driver = Arc::new(AtomicU64::new(0));
        let handles = CallHandles {
            ends_at_driver: Arc::clone(&ends_at_driver),
            input: Arc::clone(&input),
            driver: driver.stats(),
            stt: stt_stage.stats(),
            llm: llm_stage.stats(),
            segmenter: segmenter.stats(),
            tts: tts_stage.stats(),
            out: transport_out.stats(),
        };
        let probes = SessionProbes::new(
            llm_stage.context(),
            Arc::clone(&words),
            Arc::clone(&played),
            llm_stage.truncations(),
        );

        let queues = self.config.queues;
        let pipeline = Pipeline::new("hybrid-output")
            .then_with_queue(audio_in, queues.input)
            .then_with_queue(stt_stage, queues.user)
            .then_with_queue(driver, queues.user)
            .then_with_queue(UpCount(ends_at_driver), queues.user)
            .then_with_queue(llm_stage, queues.bot)
            .then_with_queue(segmenter, queues.bot)
            .then_with_queue(tts_stage, queues.bot)
            .then_with_queue(tap, queues.sink)
            .then_with_queue(transport_out, queues.sink);
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(handles);

        let drain_input = Arc::clone(&input);
        let drain_output = output.clone();
        let drain_played = Arc::clone(&played);
        Ok(BuiltSession::new(
            pipeline,
            probes,
            InputSink::target(Arc::clone(&input) as Arc<dyn InputTarget>),
            output,
        )
        .with_drain(move || async move {
            // The client's events carry their arrival times: record them all.
            while let Some(event) = drain_input.recv_client(MS).await {
                record_wire(&drain_output, &drain_played, &event);
            }
        })
        .with_guard(input))
    }
}

/// What a client on the wire received, into the suite's wire records and the playout log.
pub fn record_wire(output: &OutputLog, played: &PlayLog, event: &ClientEvent) {
    match event {
        ClientEvent::Audio { frame, received_at } => {
            output.record_wire(
                WireKind::Audio,
                *received_at,
                Some(frame.epoch()),
                Some(frame.duration()),
            );
            played
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(frame.epoch())
                .or_default()
                .push(*received_at, frame.duration());
        }
        ClientEvent::Mark { received_at, .. } => {
            output.record_wire(WireKind::Mark, *received_at, None, None);
        }
        ClientEvent::Clear { epoch, at } => {
            output.record_wire(WireKind::Clear, *at, Some(*epoch), None);
        }
        ClientEvent::Hangup { at } => output.record_wire(WireKind::Hangup, *at, None, None),
    }
}
