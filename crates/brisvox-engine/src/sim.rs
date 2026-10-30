//! The production pipeline as a simulator [`SessionFactory`] (feature `sim`).
//!
//! [`EngineFactory`] builds the all-production chain of [`build_session_pipeline`] over a
//! [`LoopbackTransport`]: the scripted caller's audio goes through a [`LoopbackClient`], a real
//! [`TransportSession`] and the [`Pump`](crate::pump::Pump); the bot's audio goes out through `TransportOut`, the
//! session's pacer and back to the loopback client. The whole shared suite of `brisvox-sim`
//! ([`brisvox_sim::suite`]) runs on it under the Budget profile, on a paused clock.
//!
//! What is real: every stage of the engine, the transport session and its output pacer, the mark
//! echo of the client (auto-ack with no delay), the VAD worker (an [`EnergyModel`] under
//! [`Offload::Inline`], the production batching and gap handling), the turn driver with the real
//! interruption policy and, for the adaptive policy, a fixed-answer Smart Turn model.
//!
//! # What the output log holds
//! The suite's oracles read the log of what the bot sent:
//! - A recording tap in front of `TransportOut` records every frame but audio at the time it
//!   passes (so `Interrupt` frames are counted as the pipeline sent them, not as the client was
//!   told to clear: a client clears on every epoch bump).
//! - The audio is recorded when the loopback client RECEIVED it (the arrival time of the wire
//!   record). A tap in front of the transport would see it up to a second early: the transport
//!   queues and its pacer releases a frame every 20 ms. The played-text oracle
//!   ([`brisvox_sim::suite::played_text`]) compares the heard words with the audio played at the
//!   barge-in instant, so it needs playout time; the client's events also fill the probes' playout
//!   log and the wire records.
//! - The client's events are read after the run (the loopback client's queue is large enough for
//!   a whole call), by the drain hook of the [`BuiltSession`].
//!
//! # Differences from the reference chain
//! They are listed in `docs/TESTING.md` ("Divergences of the production pipeline") and in
//! `docs/PIPELINE.md`; the ones that shape this factory:
//! - Output flows through a transport: its clock-driven pacer, a `Clear` that drops queued audio,
//!   end marks echoed by the client. `SessionParts::transport_delay` (a slow consumer) is a slow
//!   reader of the loopback client: the client's queue holds one event and a task reads
//!   the line, waiting `transport_delay` after every audio frame, so the adapter waits for it and
//!   the engine's output backs up behind the transport's bounded queues, as behind a slow
//!   socket. Its records are taken as it reads them (arrival times spaced by the delay).
//! - `SessionParts::input_rate` and `output_rate` are not used: the loopback call runs at the
//!   engine's fallback call rates (16 kHz both ways), the TTS delivers 24 kHz and the stage
//!   resamples.
//! - The suite's s06 scripts a failing LLM response followed by the next turn's answer, so a
//!   retry would consume that answer: the factory sets `llm.retries = 0` (the engine default is
//!   1; [`EngineFactory::with_default_retries`] restores it and the tests then run every other
//!   scenario).
//! - The simulator's energy VAD reports a stop `stop_after` after the speech, which the barge-in
//!   guards must know ([`InterruptionConfig::vad_hangover`]): the factory sets it from
//!   `SessionParts::vad`.
//! - `lead_margin` is one frame ([`LEAD_MARGIN`]): the oracle is wire arrival, played continuously,
//!   while the book counts a released frame whole.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_agent::Language;
use brisvox_agent::TurnPolicy;
use brisvox_agent::tools::{
    EngineConfig as ToolEngineConfig, FillerPolicy, SessionId, Tool, ToolCallCtx, ToolEngine,
    ToolFuture, ToolKind, ToolOutput, ToolRegistry, ToolSpec,
};
use brisvox_core::frame::{AudioFrame, DataFrame, Frame};
use brisvox_core::{FrameSender, SharedClock, TimelineRecorder, clock};
use brisvox_inference::VadParams;
use brisvox_inference::smart_turn::TurnModel;
use brisvox_providers::resilience::{ResilientLlm, ResilientStt, ResilientTts};
use brisvox_providers::{LlmProvider, SttProvider, TtsProvider};
use brisvox_sim::factory::{BuiltSession, SessionFactory, SessionParts};
use brisvox_sim::input::{InputSink, InputTarget, SendFuture};
use brisvox_sim::mock_llm::MockLlm;
use brisvox_sim::mock_stt::MockStt;
use brisvox_sim::mock_tts::MockTts;
use brisvox_sim::outage::Outage;
use brisvox_sim::output::{OutputLog, WireKind};
use brisvox_sim::reference::{PlayLog, SessionProbes, WordLog};
use brisvox_sim::vad::VadConfig;
use brisvox_transport::{
    ClientEvent, CommandSender, LoopbackCaller, LoopbackClient, LoopbackConfig, LoopbackListener,
    LoopbackTransport, MarkAcks, SessionConfig, Transport, TransportSession,
};
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::config::{EngineConfig, InterruptionConfig};
use crate::filler_clip::FillerClipStore;
use crate::offload::Offload;
use crate::pump::PumpHandle;
use crate::session::{
    FrameTap, PumpWiring, SessionBuilder, SessionHandles, SessionPipeline, SourceWrap,
    build_session_pipeline,
};
use crate::vad::{EnergyModel, VadSetup, VadWorkerConfig};

const MS: Duration = Duration::from_millis(1);

/// The lead margin of the factory's calls: one 20 ms frame (see [`EngineFactory::new`]).
pub const LEAD_MARGIN: Duration = Duration::from_millis(20);

/// A Smart Turn model with a fixed answer (the adaptive policy under test, deterministic).
#[derive(Debug, Clone, Copy)]
pub struct FixedTurnModel(pub f32);

impl TurnModel for FixedTurnModel {
    fn predict(&self, _features: &[f32]) -> brisvox_inference::Result<f32> {
        Ok(self.0)
    }
}

/// How the factory decides the end of the user's turn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Policy {
    /// Fixed silence after the (backdated) VAD stop.
    Fixed(Duration),
    /// The adaptive policy with a model that always answers this probability.
    Smart(f32),
}

/// Builds the tools of a call from its clock (see [`EngineFactory::with_tool_factory`]).
pub type ToolFactory =
    Arc<dyn Fn(&SharedClock) -> (Arc<ToolRegistry>, Arc<ToolEngine>) + Send + Sync>;

/// What [`time_in_tools`]' tool answers, as the tool result message of the context holds it.
pub const TIME_IN_RESULT: &str = r#"{"time":"09:00"}"#;

/// A ReadOnly tool that answers at once (the suite's s08 calls `time_in`).
struct TimeIn;

impl Tool for TimeIn {
    fn call<'a>(&'a self, _args: serde_json::Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async { Ok(ToolOutput::Json(serde_json::json!({"time": "09:00"}))) })
    }
}

/// The registry and engine of the suite's s08 tool: `time_in(city)`, ReadOnly, answering at once.
///
/// The registration of this one well-formed tool cannot fail; if it ever did, the error is logged
/// and the registry stays empty, which makes s08 fail visibly (an unknown tool).
pub fn time_in_tools(clock: &SharedClock) -> (Arc<ToolRegistry>, Arc<ToolEngine>) {
    let mut registry = ToolRegistry::new();
    let spec = ToolSpec::new(
        "time_in",
        "The local time in a city.",
        serde_json::json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"],
        }),
        ToolKind::ReadOnly,
    );
    if let Err(error) = registry.register(spec, TimeIn) {
        tracing::error!(target: "brisvox::engine", %error, "the time_in tool did not register");
    }
    let registry = Arc::new(registry);
    let engine = Arc::new(ToolEngine::new(
        Arc::clone(&registry),
        SessionId::new(1),
        clock.clone(),
        ToolEngineConfig::default(),
    ));
    (registry, engine)
}

/// The production pipeline over the loopback transport; see the [module docs](self).
///
/// Cloning shares the list of built calls ([`Self::handles`]).
#[derive(Clone)]
pub struct EngineFactory {
    config: EngineConfig,
    policy: Policy,
    language: Language,
    first_chunk_words: Option<usize>,
    tools: Option<ToolFactory>,
    wrap: Option<SourceWrap>,
    calls: Arc<Mutex<Vec<SessionHandles>>>,
    /// Whether the handles of every call are kept for [`Self::handles`].
    keep_handles: bool,
    /// Puts every call's providers behind the resilience layer.
    resilience: Option<ResilienceWrap>,
    /// The runner's filler policy (each call gets a clone, so its own phrase rotation).
    filler_policy: Option<FillerPolicy>,
    /// Pre-rendered filler clips shared by every call.
    filler_clips: Option<Arc<FillerClipStore>>,
    /// The languages of every call (`SessionBuilder::with_languages`).
    languages: Option<Arc<crate::language::LanguageSettings>>,
}

impl Default for EngineFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineFactory {
    /// Fixed silence of 450 ms, the recommended barge-in guards, English, no tools.
    pub fn new() -> Self {
        let mut config = EngineConfig::DEFAULT;
        // The loopback client's playback is the wire arrival: nothing is buffered beyond it. The
        // book counts a frame as played when the transport RELEASES it (whole), the suite's
        // oracle plays it continuously from its arrival, so the position runs up to one frame
        // ahead of the oracle: one frame of lead takes that out (the shared suite has no word of
        // slack for a word that starts in the chunk in flight).
        config.lead_margin = LEAD_MARGIN;
        // The suite's s06 scripts a failing response followed by the NEXT turn's answer: a retry
        // would consume that answer (see the module docs).
        config.llm.retries = 0;
        Self {
            config,
            policy: Policy::Fixed(450 * MS),
            language: Language::En,
            first_chunk_words: None,
            tools: None,
            wrap: None,
            calls: Arc::default(),
            keep_handles: true,
            resilience: None,
            filler_policy: None,
            filler_clips: None,
            languages: None,
        }
    }

    /// Lets every call follow the caller between the languages of `settings`
    /// ([`SessionBuilder::with_languages`]).
    #[must_use]
    pub fn with_languages(mut self, settings: Arc<crate::language::LanguageSettings>) -> Self {
        self.languages = Some(settings);
        self
    }

    /// Ends turns after `silence` of quiet. The played-text suite barges in 0.4 s after the first
    /// utterance and needs 300 ms (with 450 ms the caller would "resume" the first turn).
    #[must_use]
    pub fn with_fixed_silence(mut self, silence: Duration) -> Self {
        self.policy = Policy::Fixed(silence);
        self
    }

    /// The adaptive policy with a Smart Turn model that always answers `probability`.
    #[must_use]
    pub fn with_smart_turn(mut self, probability: f32) -> Self {
        self.policy = Policy::Smart(probability);
        self
    }

    /// The barge-in guards (default [`InterruptionConfig::RECOMMENDED`]); the VAD hangover is set
    /// from the simulator's VAD when the call is built.
    #[must_use]
    pub fn with_interruption(mut self, interruption: InterruptionConfig) -> Self {
        self.config.interruption = interruption;
        self
    }

    /// Every barge-in guard off (each candidate interrupts at once).
    #[must_use]
    pub fn with_guards_off(self) -> Self {
        self.with_interruption(InterruptionConfig::OFF)
    }

    /// The engine default for LLM retries (one) instead of the suite accommodation (zero).
    #[must_use]
    pub fn with_default_retries(mut self) -> Self {
        self.config.llm.retries = EngineConfig::DEFAULT.llm.retries;
        self
    }

    /// Changes the engine configuration (for example `lead_margin` or speculation).
    #[must_use]
    pub fn with_config(mut self, change: impl FnOnce(&mut EngineConfig)) -> Self {
        change(&mut self.config);
        self
    }

    /// The language of the segmenter and the speech normalizer.
    #[must_use]
    pub const fn with_language(mut self, language: Language) -> Self {
        self.language = language;
        self
    }

    /// Words the first TTS chunk of a response must hold
    /// ([`SessionBuilder::with_first_chunk_words`]; default: the language's).
    #[must_use]
    pub const fn with_first_chunk_words(mut self, words: usize) -> Self {
        self.first_chunk_words = Some(words);
        self
    }

    /// The runner has the `time_in` tool (the suite's s08 calls it).
    #[must_use]
    pub fn with_time_tool(self) -> Self {
        self.with_tool_factory(Arc::new(time_in_tools))
    }

    /// The runner has the tools `factory` builds for each call.
    #[must_use]
    pub fn with_tool_factory(mut self, factory: ToolFactory) -> Self {
        self.tools = Some(factory);
        self
    }

    /// Passes the production truncation source through `wrap` (a test double around it).
    #[must_use]
    pub fn with_source_wrap(mut self, wrap: SourceWrap) -> Self {
        self.wrap = Some(wrap);
        self
    }

    /// Keeps no call's handles ([`Self::handles`] stays empty): the heap check of the chaos suite
    /// needs a factory that holds nothing once its calls are over (each call's handles hold its
    /// book, context and counters, about 12 KB).
    #[must_use]
    pub const fn without_handle_log(mut self) -> Self {
        self.keep_handles = false;
        self
    }

    /// Builds every call over the providers `wrap` makes from the call's mocks: the resilience
    /// layer of `brisvox_providers::resilience` with each stage's primary and secondary
    /// behind an [`Outage`] switch. Without it the mocks are used directly.
    #[must_use]
    pub fn with_resilience(mut self, wrap: ResilienceWrap) -> Self {
        self.resilience = Some(wrap);
        self
    }

    /// The runner's filler policy for every call
    /// ([`SessionBuilder::with_filler_policy`]).
    #[must_use]
    pub fn with_filler_policy(mut self, policy: FillerPolicy) -> Self {
        self.filler_policy = Some(policy);
        self
    }

    /// Pre-rendered filler clips for every call ([`SessionBuilder::with_filler_clips`]).
    #[must_use]
    pub fn with_filler_clips(mut self, store: Arc<FillerClipStore>) -> Self {
        self.filler_clips = Some(store);
        self
    }

    /// The engine configuration the calls are built with (before the per-call adjustments of the
    /// module docs).
    pub const fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// The handles of every call built so far, in build order.
    pub fn handles(&self) -> Vec<SessionHandles> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The recording tap: everything but audio is recorded as it passes (audio is recorded when the
/// client receives it), and TTS words fill the probes' word log.
struct Tap {
    output: OutputLog,
    words: WordLog,
    clock: SharedClock,
}

impl FrameTap for Tap {
    fn observe(&mut self, frame: &Frame) {
        match frame {
            // Recorded at the wire, see the module docs.
            Frame::Data(DataFrame::Audio(_)) => {}
            Frame::Data(DataFrame::Word(word)) => {
                self.words
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push((word.epoch, Arc::clone(&word.word), word.start));
                self.output.record_frame(frame, self.clock.now());
            }
            _ => self.output.record_frame(frame, self.clock.now()),
        }
    }
}

/// Audio frames in through a loopback client, the transport session and the pump. The pump
/// starts when the runner binds the sink to the started pipeline.
pub struct LoopbackInput {
    caller: tokio::sync::Mutex<LoopbackCaller>,
    listener: Arc<tokio::sync::Mutex<LoopbackListener>>,
    /// The slow reader of the line (a slow consumer) and the token that stops it.
    consumer: Mutex<Option<(CancellationToken, tokio::task::JoinHandle<()>)>>,
    session: Mutex<Option<TransportSession>>,
    /// The running pump (after the sink was bound).
    pump: Mutex<Option<PumpHandle>>,
    wiring: PumpWiring,
    cancel: CancellationToken,
    input_sender: Mutex<Option<FrameSender>>,
    _stop_on_drop: DropGuard,
}

impl LoopbackInput {
    /// The next thing the transport sent to the client (audio, marks, clears), waiting at most
    /// `timeout` on `clock`.
    pub async fn recv_client(&self, clock: &SharedClock, timeout: Duration) -> Option<ClientEvent> {
        let mut listener = self.listener.lock().await;
        tokio::select! {
            event = listener.recv() => event,
            () = clock::wait_for(&**clock, timeout) => None,
        }
    }

    /// Stops the slow reader, if there is one, and waits for it (it ends at its next step).
    pub async fn stop_consumer(&self) {
        let consumer = self
            .consumer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some((stop, task)) = consumer {
            stop.cancel();
            let _ = task.await;
        }
    }

    /// Whether the pump has been started (the sink was bound to a running pipeline).
    pub fn pump_running(&self) -> bool {
        self.pump
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }
}

impl InputTarget for LoopbackInput {
    fn send(&self, frame: AudioFrame) -> SendFuture<'_> {
        Box::pin(async move {
            let mut caller = self.caller.lock().await;
            caller
                .send_audio(frame.data().clone(), frame.sample_rate())
                .await
                .is_ok()
        })
    }

    fn bind(&self, input: &FrameSender) {
        // The pump needs the upstream input as well: it starts in `bind_upstream`.
        *self
            .input_sender
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(input.clone());
    }

    fn bind_upstream(&self, upstream: &FrameSender) {
        let input = self
            .input_sender
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let session = self
            .session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let (Some(input), Some(session)) = (input, session) else {
            return;
        };
        let handle = self.wiring.spawn(session, &input, upstream, &self.cancel);
        *self.pump.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
    }
}

impl SessionFactory for EngineFactory {
    async fn build(&self, parts: SessionParts) -> brisvox_sim::Result<BuiltSession> {
        let SessionParts {
            clock,
            stt,
            llm,
            tts,
            vad,
            transport_delay,
            timeline,
            origin,
            input_rate: _,
            output_rate: _,
        } = parts;
        let setup_error = |what: &str, error: &dyn std::fmt::Display| {
            brisvox_sim::Error::Assertion(format!("engine factory setup: {what}: {error}"))
        };

        let mut config = self.config;
        // The simulator's energy VAD reports a stop `stop_after` after the speech.
        config.interruption.vad_hangover = if config.interruption == InterruptionConfig::OFF {
            Duration::ZERO
        } else {
            vad.stop_after
        };
        if let Policy::Fixed(silence) = self.policy {
            config.turn = TurnPolicy::fixed(silence);
        } else {
            config.turn = TurnPolicy::Adaptive {
                fallback_silence: Duration::from_secs(3),
                require_final: true,
                final_grace: 800 * MS,
            };
        }

        // A slow consumer reads a line that holds one event (see the module docs).
        let slow = !transport_delay.is_zero();
        let LoopbackCall {
            session,
            client,
            cancel,
        } = open_loopback(&clock, if slow { 1 } else { CLIENT_QUEUE })
            .await
            .map_err(|e| setup_error("loopback", &e))?;
        let commands = session.commands();

        let offload = Offload::Inline(clock.clone());
        let words = WordLog::default();
        let played = PlayLog::default();
        // The transport stage marks FirstAudioOut and AudioOutStopped at the real handoff: the
        // log must not mark them.
        let output = OutputLog::new(origin, None);

        let assembly = Assembly {
            config,
            clock: clock.clone(),
            timeline,
            tap: Tap {
                output: output.clone(),
                words: Arc::clone(&words),
                clock: clock.clone(),
            },
            vad: energy_vad_setup(offload.clone(), &vad, &config),
            offload,
            commands,
        };
        let built = match &self.resilience {
            None => self.assemble(assembly, Arc::new(stt), Arc::new(llm), Arc::new(tts)),
            Some(wrap) => {
                let providers = wrap(SimProviders {
                    clock: clock.clone(),
                    stt,
                    llm,
                    tts,
                });
                self.assemble(
                    assembly,
                    Arc::new(providers.stt),
                    Arc::new(providers.llm),
                    Arc::new(providers.tts),
                )
            }
        }?;
        let handles = built.handles.clone();
        let probes = SessionProbes::new(
            handles.context.clone(),
            Arc::clone(&words),
            Arc::clone(&played),
            handles.truncations.clone(),
        );

        let (caller, listener) = client.split();
        let listener = Arc::new(tokio::sync::Mutex::new(listener));
        let consumer = slow.then(|| {
            let stop = CancellationToken::new();
            let task = tokio::spawn(slow_consumer(
                Arc::clone(&listener),
                SlowReader {
                    output: output.clone(),
                    played: Arc::clone(&played),
                    clock: clock.clone(),
                    delay: transport_delay,
                },
                stop.clone(),
                cancel.clone(),
            ));
            (stop, task)
        });
        let input = Arc::new(LoopbackInput {
            caller: tokio::sync::Mutex::new(caller),
            listener,
            consumer: Mutex::new(consumer),
            session: Mutex::new(Some(session)),
            pump: Mutex::new(None),
            wiring: built.pump,
            cancel: cancel.clone(),
            input_sender: Mutex::new(None),
            _stop_on_drop: cancel.drop_guard(),
        });
        if self.keep_handles {
            self.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(handles);
        }

        let drain_input = Arc::clone(&input);
        let drain_output = output.clone();
        let drain_played = Arc::clone(&played);
        let drain_clock = clock;
        Ok(BuiltSession::new(
            built.pipeline,
            probes,
            InputSink::target(Arc::clone(&input) as Arc<dyn InputTarget>),
            output,
        )
        .with_drain(move || async move {
            // A slow reader recorded what it read; whatever is left is read at once.
            drain_input.stop_consumer().await;
            // The client's events carry their arrival times: record them all.
            while let Some(event) = drain_input.recv_client(&drain_clock, MS).await {
                record_wire(&drain_output, &drain_played, &event);
            }
        })
        .with_guard(input))
    }
}

/// One call's mocks, handed to a [`ResilienceWrap`] (the scenario already scripted them).
#[derive(Debug)]
pub struct SimProviders {
    /// The call's clock.
    pub clock: SharedClock,
    /// The call's STT mock.
    pub stt: MockStt,
    /// The call's LLM mock.
    pub llm: MockLlm,
    /// The call's TTS mock.
    pub tts: MockTts,
}

/// One call's providers behind the resilience layer: every leg is a mock behind an [`Outage`]
/// switch (secondaries are usually clones of the call's mocks, so they share its script).
#[derive(Debug)]
pub struct SimResilient {
    /// The STT stage.
    pub stt: ResilientStt<Outage<MockStt>>,
    /// The LLM stage.
    pub llm: ResilientLlm<Outage<MockLlm>>,
    /// The TTS stage.
    pub tts: ResilientTts<Outage<MockTts>>,
}

/// Builds a call's [`SimResilient`] from its [`SimProviders`] ([`EngineFactory::with_resilience`]).
pub type ResilienceWrap = Arc<dyn Fn(SimProviders) -> SimResilient + Send + Sync>;

/// The provider-independent parts of one call's pipeline (see [`EngineFactory::assemble`]).
struct Assembly {
    config: EngineConfig,
    clock: SharedClock,
    timeline: TimelineRecorder,
    offload: Offload,
    tap: Tap,
    vad: VadSetup<EnergyModel>,
    commands: CommandSender,
}

impl EngineFactory {
    /// The session pipeline of one call over the given providers.
    fn assemble<S, L, T>(
        &self,
        assembly: Assembly,
        stt: Arc<S>,
        llm: Arc<L>,
        tts: Arc<T>,
    ) -> brisvox_sim::Result<SessionPipeline>
    where
        S: SttProvider,
        L: LlmProvider,
        T: TtsProvider,
    {
        let setup_error = |what: &str, error: &dyn std::fmt::Display| {
            brisvox_sim::Error::Assertion(format!("engine factory setup: {what}: {error}"))
        };
        let Assembly {
            config,
            clock,
            timeline,
            offload,
            tap,
            vad,
            commands,
        } = assembly;
        let mut builder =
            SessionBuilder::new(config, clock.clone(), timeline, offload, stt, llm, tts)
                .map_err(|e| setup_error("config", &e))?
                .with_language(self.language)
                .with_turn_probe(256)
                .with_tap(tap)
                .with_vad(vad);
        if let Policy::Smart(probability) = self.policy {
            builder = builder.with_smart_turn(Arc::new(FixedTurnModel(probability)));
        }
        if let Some(words) = self.first_chunk_words {
            builder = builder.with_first_chunk_words(words);
        }
        if let Some(wrap) = &self.wrap {
            builder = builder.with_source_wrap(Arc::clone(wrap));
        }
        if let Some(tools) = &self.tools {
            let (registry, engine) = tools(&clock);
            builder = builder.with_tools(registry, engine);
        }
        if let Some(policy) = &self.filler_policy {
            builder = builder.with_filler_policy(policy.clone());
        }
        if let Some(store) = &self.filler_clips {
            builder = builder.with_filler_clips(Arc::clone(store));
        }
        if let Some(settings) = &self.languages {
            builder = builder.with_languages(Arc::clone(settings));
        }
        build_session_pipeline(builder, commands).map_err(|e| setup_error("build", &e))
    }
}

/// Where a slow reader records what it reads, and how slowly it reads.
struct SlowReader {
    output: OutputLog,
    played: PlayLog,
    clock: SharedClock,
    delay: Duration,
}

/// Reads the line until `stop` or `cancel`, or until the transport goes: records every event
/// when it reads it (audio and marks stamped with the read time: when the slow client took them)
/// and waits `reader.delay` after each audio frame (a client that takes longer than a frame's
/// duration to play a frame). Owns the listener's lock for its whole life.
async fn slow_consumer(
    listener: Arc<tokio::sync::Mutex<LoopbackListener>>,
    reader: SlowReader,
    stop: CancellationToken,
    cancel: CancellationToken,
) {
    let mut listener = listener.lock().await;
    loop {
        let event = tokio::select! {
            biased;
            () = stop.cancelled() => return,
            () = cancel.cancelled() => return,
            event = listener.recv() => event,
        };
        let Some(mut event) = event else {
            return;
        };
        // The slow client takes (plays) the frame when it reads it, not when the adapter put
        // it in its one-slot queue.
        let now = reader.clock.now();
        match &mut event {
            ClientEvent::Audio { received_at, .. } | ClientEvent::Mark { received_at, .. } => {
                *received_at = now;
            }
            ClientEvent::Clear { .. } | ClientEvent::Hangup { .. } => {}
        }
        record_wire(&reader.output, &reader.played, &event);
        if matches!(event, ClientEvent::Audio { .. }) {
            tokio::select! {
                biased;
                () = stop.cancelled() => return,
                () = cancel.cancelled() => return,
                () = reader.clock.sleep(reader.delay) => {}
            }
        }
    }
}

/// The loopback client's queue capacity: a whole call fits.
const CLIENT_QUEUE: usize = 65_536;

/// A loopback call: the engine's side (a transport session) and the caller's side (a client).
pub struct LoopbackCall {
    /// The engine's side of the call; hand it to [`run_call`](crate::run_call) or a pump.
    pub session: TransportSession,
    /// The caller's side: sends audio, receives what the engine sent, with arrival times.
    pub client: LoopbackClient,
    /// Cancelling it ends the session (the call's lifetime token).
    pub cancel: CancellationToken,
}

/// Opens a loopback call as the simulator's production factory does: latency 0, end marks
/// echoed at once, a position report per 20 ms frame, and a client queue of `client_queue`
/// events (the factory reads the client's events after the run, so a whole call must fit).
///
/// # Errors
/// A description of the transport failure (never in practice for a loopback).
pub async fn open_loopback(
    clock: &SharedClock,
    client_queue: usize,
) -> Result<LoopbackCall, String> {
    let defaults = LoopbackConfig::default();
    let (mut transport, client) = LoopbackTransport::new(
        clock.clone(),
        LoopbackConfig {
            latency: Duration::ZERO,
            mark_acks: MarkAcks::Auto {
                delay: Duration::ZERO,
            },
            client_queue,
            max_buffered: client_queue,
            ..defaults
        },
    );
    let cancel = CancellationToken::new();
    let link = transport
        .accept(&cancel)
        .await
        .map_err(|e| format!("accept: {e}"))?
        .ok_or_else(|| "accept: no call".to_owned())?;
    let session = TransportSession::open(
        clock.clone(),
        link,
        SessionConfig {
            position_interval: 20 * MS,
            ..SessionConfig::default()
        },
        &cancel,
    )
    .await
    .map_err(|e| format!("open: {e}"))?;
    Ok(LoopbackCall {
        session,
        client,
        cancel,
    })
}

/// The VAD of the simulator's energy detector as a production [`VadSetup`]: the real worker with
/// batching and gap handling over an [`EnergyModel`] with the threshold, minimum speech and
/// minimum silence of `vad`.
pub fn energy_vad_setup(
    offload: Offload,
    vad: &VadConfig,
    config: &EngineConfig,
) -> VadSetup<EnergyModel> {
    let threshold = vad.threshold;
    VadSetup::new(
        offload,
        VadWorkerConfig {
            params: VadParams {
                threshold: 0.5,
                neg_threshold: 0.35,
                min_speech: vad.start_after,
                min_silence: vad.stop_after,
                speech_pad: Duration::ZERO,
            },
            ..VadWorkerConfig::from_engine(config)
        },
        move || {
            #[allow(clippy::cast_possible_truncation)] // a threshold of a few hundred
            Ok(EnergyModel::from_pcm_rms(threshold as f32))
        },
    )
}

/// What a client on the wire received, into the suite's wire records, the output log's audio
/// records and the playout log.
pub fn record_wire(output: &OutputLog, played: &PlayLog, event: &ClientEvent) {
    match event {
        ClientEvent::Audio { frame, received_at } => {
            output.record_wire(
                WireKind::Audio,
                *received_at,
                Some(frame.epoch()),
                Some(frame.duration()),
            );
            output.record_audio(frame.epoch(), *received_at, frame.duration());
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
