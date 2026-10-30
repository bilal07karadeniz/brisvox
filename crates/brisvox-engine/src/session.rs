//! Assembly of one call (`docs/PIPELINE.md` sections 1, 2 and 8): [`SessionBuilder`]
//! wires the stages in their design order, [`build_session_pipeline`] turns it into a
//! [`SessionPipeline`] (the not yet started pipeline plus everything that needs the started
//! pipeline), and [`run_call`] starts it over a [`TransportSession`] and returns the running
//! [`Call`].
//!
//! ```text
//! TransportSession --pump--> [AudioIn(+VAD)] -> [SttStage] -> [TurnDriver] -> [LlmRunner]
//!                                -> [SegmenterStage] -> [TtsStage] -> (tap) -> [TransportOut]
//!                                                                                  |
//!                                         TransportSession.commands() <------------+
//! ```
//!
//! # What the assembly decides
//! - **Queues.** The inbox shapes of [`EngineConfig::queues`]: `input` for `AudioIn` (drop-oldest
//!   audio), `user` for STT and the turn driver, `bot` for the runner, the segmenter and TTS,
//!   `sink` for the tap and `TransportOut`. All bounded, with the overflow policies documented on
//!   [`QueueConfigs`](crate::QueueConfigs).
//! - **One book.** One [`PlayoutBook`] is shared by the pump (marks, positions), the TTS stage
//!   (utterances, sent text), `TransportOut` (words, audio), the runner (round to message index,
//!   and the [`PlayoutTruncation`] source) and the turn driver's clear hook.
//! - **Clear hook.** The turn driver clears the transport with [`clear_hook`], which tells the
//!   book (`note_clear`) before it sends the `Clear` command, as the settle wait of the played
//!   text truncation requires. [`SessionHandles::unnoted_clear_cuts`] is the check that
//!   this stayed wired.
//! - **Offload.** One [`Offload`] (the inference pool in production, inline in paused-time
//!   simulations) runs the VAD, Smart Turn, the runner's heavy builds and finishes, and the TTS
//!   stage's normalization and resampler construction. It is a required argument of
//!   [`SessionBuilder::new`], so a production assembly cannot fall back to running models on a
//!   tokio worker by omission, and [`SessionBuilder::with_vad`] replaces the offload carried by
//!   the [`VadSetup`] with it. `Offload::Inline` is for paused-time simulations and tests only:
//!   it runs every job on the calling task and cannot time a job out.
//! - **Warm-up at `Start`.** Every stage that owns a connection or a model warms it when the
//!   pipeline's `Start` reaches it, in a helper task bounded by an explicit timeout: STT and TTS
//!   (provider `warm`, then connect), the runner (`LlmProvider::warm`), the VAD worker
//!   (`SpeechModel::warm` once per inference pool thread) and the turn driver
//!   (`TurnModel::warm` for one analysis session per pool thread). The two model warm-ups run
//!   once per pool and model for the whole process ([`Offload::claim_warm`]): the first call
//!   warms, later calls skip it, so a burst of call starts cannot fill the shared pool's queue.
//!   A failed warm-up is counted, released for the next call to retry, and never fails the
//!   call.
//! - **Early analysis.** With [`EngineConfig::early_analysis`] enabled, [`SessionBuilder::with_vad`]
//!   applies [`VadSetup::with_early_analysis`], the VAD half of the switch (the driver half is
//!   its configuration). Do not apply it yourself.
//! - **Ownership of the pump.** The pump reads the session, so it needs the started pipeline's
//!   two inputs: [`run_call`] starts it right after the pipeline. A harness that starts the
//!   pipeline itself (the simulator) uses [`SessionPipeline::pump`] with the inputs of its
//!   running pipeline.
//!
//! # A tap
//! [`SessionBuilder::with_tap`] puts a [`FrameTap`] right before `TransportOut`: a pass-through
//! stage that shows the tap every downstream frame `TransportOut` receives, stale ones included
//! (it does not filter: `TransportOut` drops and counts stale frames itself). The simulator's
//! `EngineFactory` uses it to count `Interrupt` frames and record words; production does not
//! install one, so the hot path has no extra stage. The tap stage has the `sink` inbox shape, so
//! the upstream lane `TransportOut` writes into (`TtsStarted`, the pump's `TtsEnded`) is 4 data
//! frames deep instead of the TTS stage's 32: upstream frames it cannot pass on at once are
//! dropped and counted ([`SessionHandles::tap_upstream_dropped`], which tests assert is zero).
//!
//! # A caller-audio tap
//! [`SessionBuilder::with_caller_audio_tap`] shows a [`CallerAudioTap`] every caller audio frame
//! as the first stage ([`AudioIn`]) receives it, before any conversion: at the call's own rate,
//! with the transport's sequence numbers. It adds no stage and no queue (the stage's task calls
//! it), so the hot path is unchanged without one. The server's recording sink uses it
//! (`docs/PRIVACY.md`); the bot's side of a recording uses [`SessionBuilder::with_tap`].
//!
//! # Events and typed messages
//! [`SessionBuilder::with_events`] publishes the call's conversation events (user and bot
//! speaking, transcripts, LLM and TTS text, barge-ins, turn latencies) to an [`EngineEvents`]
//! ring. They are observed on the inputs of the turn driver, the runner, the segmenter, the TTS
//! stage and `TransportOut` by the stages' own tasks, and the turn driver publishes the user
//! turns and barge-ins it decides: no extra stage, queue or hop, and a
//! subscriber that falls behind loses the oldest events instead of slowing the call (see
//! [`crate::events`]). [`SessionBuilder::with_text_input`] lets the application type user
//! messages into the call ([`crate::text_input`]); the turn driver turns them into user turns.
//! [`SessionBuilder::with_say_input`] and [`SessionBuilder::with_conversation_edits`] let an
//! application (an out-of-process agent) make the bot speak a line of its own and edit the
//! conversation ([`crate::agent_input`]).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_agent::context::Context;
use brisvox_agent::flows::FlowManager;
use brisvox_agent::played::TruncationStats;
use brisvox_agent::tools::{FillerClips, FillerPolicy, ToolEngine, ToolRegistry};
use brisvox_agent::window::Summarizer;
use brisvox_agent::{InterruptionPolicy, Language};
use brisvox_core::frame::Frame;
use brisvox_core::{
    Direction, EpochPolicy, FrameSender, Links, Pipeline, PipelineStats, Processor,
    ProcessorContext, RunningPipeline, SharedClock, TimelineRecorder,
};
use brisvox_inference::SpeechModel;
use brisvox_inference::smart_turn::TurnModel;
use brisvox_providers::s2s::S2sProvider;
use brisvox_providers::{LlmProvider, SttProvider, TtsProvider};
use brisvox_transport::{CommandSender, TransportSession};
use tokio_util::sync::CancellationToken;

use crate::agent_input::{EditReceiver, SayReceiver};
use crate::audio_in::{AudioIn, AudioInStats, AudioInStatsSnapshot, CallerAudioTap};
use crate::config::EngineConfig;
use crate::error::{Error, Result};
use crate::events::{EngineEvents, Observed, Observer, Site};
use crate::filler_clip::FillerClipStore;
use crate::language::{LanguageSettings, LanguageStatsSnapshot, LanguageTracker};
use crate::llm::{LlmStats, LlmStatsSnapshot};
use crate::llm_runner::LlmRunner;
use crate::offload::Offload;
use crate::playout::{BookStats, DEFAULT_BOOK_CAPACITY, PlayoutBook, clear_hook};
use crate::pump::{Pump, PumpConfig, PumpHandle, PumpLinks, PumpStats};
use crate::rates::CallRatesCell;
use crate::request::{RequestStats, RequestStatsSnapshot};
use crate::s2s::{S2sConnect, S2sOptions, S2sStage, S2sStageConfig, S2sStats, S2sWiring, Unused};
use crate::segmenter::{
    SegmenterStage, SegmenterStageConfig, SegmenterStats, SegmenterStatsSnapshot,
};
use crate::speculation::{SpeculationStats, SpeculationStatsSnapshot};
use crate::stt::{SttStage, SttStageConfig, SttStats, SttStatsSnapshot};
use crate::text_input::TextTurnReceiver;
use crate::transport_out::{
    TransportOut, TransportOutConfig, TransportOutStats, TransportOutStatsSnapshot,
};
use crate::truncation::{PlayoutTruncation, PlayoutTruncationConfig, TruncationSource};
use crate::tts::{TtsStage, TtsStageConfig, TtsStats, TtsStatsSnapshot};
use crate::turn_driver::{
    TurnDriver, TurnDriverConfig, TurnDriverStats, TurnDriverStatsSnapshot, TurnProbe,
};
use crate::vad::{VadSetup, VadStats, VadStatsSnapshot};

/// Sees every downstream frame that reaches the tap stage (see the [module docs](self), "A
/// tap"). It must be cheap and must not block: it runs on the pipeline's task.
pub trait FrameTap: Send + 'static {
    /// Called with every downstream frame, in order, before it is forwarded to `TransportOut`.
    fn observe(&mut self, frame: &Frame);
}

/// The pass-through stage that shows its [`FrameTap`] the frames.
struct TapStage {
    tap: Box<dyn FrameTap>,
    /// Upstream frames that could not be passed on (full or closed lane).
    upstream_dropped: Arc<AtomicU64>,
}

impl Processor for TapStage {
    fn name(&self) -> &'static str {
        "output_tap"
    }

    /// A pure observer: `TransportOut` filters (and counts) stale frames itself, so the tap
    /// must hand it exactly what it would get without the tap.
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
            // Not its business: pass the upstream traffic (`TtsStarted`, `TtsEnded`) along
            // without waiting (the rule of `Processor`: upstream pushes never await).
            let sent = ctx
                .sender(Direction::Upstream)
                .is_some_and(|up| up.try_send(frame).is_ok());
            if !sent {
                self.upstream_dropped.fetch_add(1, Ordering::Relaxed);
            }
            return Ok(());
        }
        self.tap.observe(&frame);
        ctx.forward(frame, direction).await
    }
}

/// Wraps the truncation source of a call (a test double around the production one).
pub type SourceWrap =
    Arc<dyn Fn(Arc<dyn TruncationSource>) -> Arc<dyn TruncationSource> + Send + Sync>;

/// `wrap` applied on top of `inner` (`inner` first, `wrap` around its result).
fn compose_wrap(inner: Option<SourceWrap>, wrap: SourceWrap) -> SourceWrap {
    match inner {
        Some(inner) => Arc::new(move |source| wrap(inner(source))),
        None => wrap,
    }
}

/// Everything configurable about the assembly of one call; see the [module docs](self).
///
/// The providers are shared handles (`Arc`): one provider value serves many calls.
pub struct SessionBuilder<S: SttProvider, L: LlmProvider, T: TtsProvider> {
    config: EngineConfig,
    clock: SharedClock,
    timeline: TimelineRecorder,
    stt: Arc<S>,
    llm: Arc<L>,
    tts: Arc<T>,
    offload: Offload,
    audio_in: AudioIn,
    smart_turn: Option<Arc<dyn TurnModel>>,
    /// Where the Smart Turn analyses run when not on `offload` ([`Self::with_smart_turn_offload`]).
    turn_offload: Option<Offload>,
    language: Language,
    first_chunk_words: Option<usize>,
    voice: Option<Arc<str>>,
    interruption: Option<InterruptionPolicy>,
    system_prompt: Option<Arc<str>>,
    tools: Option<(Arc<ToolRegistry>, Arc<ToolEngine>)>,
    flow: Option<FlowManager>,
    summarizer: Option<Arc<dyn Summarizer>>,
    phrases: Option<(Arc<str>, Arc<str>)>,
    filler_policy: Option<FillerPolicy>,
    filler_clips: Option<Arc<FillerClipStore>>,
    wrap: Option<SourceWrap>,
    tap: Option<Box<dyn FrameTap>>,
    caller_tap: Option<Box<dyn CallerAudioTap>>,
    probe: Option<TurnProbe>,
    book_capacity: usize,
    events: Option<EngineEvents>,
    text_input: Option<TextTurnReceiver>,
    say_input: Option<SayReceiver>,
    edits: Option<EditReceiver>,
    s2s: Option<(Arc<dyn S2sConnect>, S2sOptions)>,
    languages: Option<Arc<LanguageSettings>>,
}

impl SessionBuilder<Unused, Unused, Unused> {
    /// A builder for a call on a speech-to-speech model (`docs/S2S.md`): the pipeline is
    /// `AudioIn -> S2sStage -> TransportOut`, and the classic STT, LLM and TTS stages do not
    /// exist ([`Unused`] fills their type parameters). The other options apply as far as they
    /// make sense here: `with_vad` (turn detection in [`S2sTurnMode::Local`](crate::s2s::S2sTurnMode::Local)), `with_voice`,
    /// `with_system_prompt` (the agent's instructions), `with_tools`, `with_events`,
    /// `with_tap`. See [`Self::with_s2s`] for what is ignored.
    ///
    /// # Errors
    /// [`Error::Config`] when [`EngineConfig::validate`] rejects `config`.
    pub fn new_s2s<P: S2sProvider>(
        config: EngineConfig,
        clock: SharedClock,
        timeline: TimelineRecorder,
        offload: Offload,
        provider: Arc<P>,
        options: S2sOptions,
    ) -> Result<Self> {
        Ok(Self::new(
            config,
            clock,
            timeline,
            offload,
            Arc::new(Unused),
            Arc::new(Unused),
            Arc::new(Unused),
        )?
        .with_s2s(provider, options))
    }
}

impl<S: SttProvider, L: LlmProvider, T: TtsProvider> SessionBuilder<S, L, T> {
    /// A builder over the three providers, with heavy work on `offload`: [`Offload::Pool`] (the
    /// shared inference pool) in production, [`Offload::Inline`] only in paused-time simulations
    /// and tests (it runs the VAD, Smart Turn and the runner's heavy builds on the calling tokio
    /// task, against the engine's no-heavy-work-on-workers rule, and cannot time a job out).
    /// Defaults: no VAD and no Smart Turn model (an adaptive policy then ends every turn at its
    /// fallback deadline), English, no tools, no system prompt, no tap.
    ///
    /// # Errors
    /// [`Error::Config`] when [`EngineConfig::validate`] rejects `config`.
    pub fn new(
        config: EngineConfig,
        clock: SharedClock,
        timeline: TimelineRecorder,
        offload: Offload,
        stt: Arc<S>,
        llm: Arc<L>,
        tts: Arc<T>,
    ) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            audio_in: AudioIn::new(&config),
            offload,
            config,
            clock,
            timeline,
            stt,
            llm,
            tts,
            smart_turn: None,
            turn_offload: None,
            language: Language::En,
            first_chunk_words: None,
            voice: None,
            interruption: None,
            system_prompt: None,
            tools: None,
            flow: None,
            summarizer: None,
            phrases: None,
            filler_policy: None,
            filler_clips: None,
            wrap: None,
            tap: None,
            caller_tap: None,
            probe: None,
            book_capacity: DEFAULT_BOOK_CAPACITY,
            events: None,
            text_input: None,
            say_input: None,
            edits: None,
            s2s: None,
            languages: None,
        })
    }

    /// Lets the call follow the caller between several languages (`crate::language`): the
    /// STT stage reports each finished utterance to a [`LanguageTracker`] over `settings`, which
    /// decides when the agent switches; the next response then has the new language pinned in
    /// its LLM request, is cut and normalized for that language and spoken with its voice and
    /// TTS model. `with_voice` and `with_language` are replaced by the default language of
    /// `settings`. Classic pipeline only (a speech-to-speech call ignores it). A switch is
    /// published as [`EngineEvent::LanguageSwitched`](crate::EngineEvent::LanguageSwitched) when
    /// [`Self::with_events`] is set.
    #[must_use]
    pub fn with_languages(mut self, settings: Arc<LanguageSettings>) -> Self {
        self.languages = Some(settings);
        self
    }

    /// Runs the call on a speech-to-speech model (`docs/S2S.md`): the pipeline becomes
    /// `AudioIn -> S2sStage -> TransportOut` and the STT, LLM and TTS providers given to
    /// [`Self::new`] are never used ([`Self::new_s2s`] needs none). The system prompt becomes
    /// the model's instructions, the tools of [`Self::with_tools`] are offered to it and run
    /// through the call's `ToolEngine`, [`Self::with_voice`] picks its voice.
    ///
    /// Not supported with a speech-to-speech model (ignored): `with_smart_turn`,
    /// `with_interruption`, `with_flow`, `with_summarizer`, `with_phrases`,
    /// `with_filler_policy`, `with_filler_clips`, `with_source_wrap`, `with_text_input`,
    /// `with_say_input`, `with_conversation_edits` and speculation. Barge-in is decided by the
    /// VAD alone (the provider's, or the local one in [`S2sTurnMode::Local`](crate::s2s::S2sTurnMode::Local)).
    #[must_use]
    pub fn with_s2s<P: S2sProvider>(mut self, provider: Arc<P>, options: S2sOptions) -> Self {
        self.s2s = Some((provider as Arc<dyn S2sConnect>, options));
        self
    }

    /// Runs voice activity detection on the caller's audio. Applies the VAD half of
    /// [`EngineConfig::early_analysis`] ([`VadSetup::with_early_analysis`]) and the model warm-up
    /// at `Start` ([`VadSetup::with_warm_up`], bounded by `timeouts.connect`). The offload the
    /// setup was built with is replaced by the builder's (the one given to [`Self::new`]), so the
    /// VAD always runs where the rest of the call's heavy work runs.
    #[must_use]
    pub fn with_vad<M: SpeechModel + 'static>(mut self, setup: VadSetup<M>) -> Self {
        let setup = setup
            .with_offload(self.offload.clone())
            .with_early_analysis(&self.config)
            .with_warm_up(self.config.timeouts.connect)
            .with_timeline(self.timeline.clone(), self.clock.clone());
        self.audio_in = AudioIn::new(&self.config).with_vad(setup);
        self
    }

    /// Decides the end of the user's turn with `model` when the policy is adaptive (Smart Turn
    /// on the builder's offload).
    #[must_use]
    pub fn with_smart_turn(mut self, model: Arc<dyn TurnModel>) -> Self {
        self.smart_turn = Some(model);
        self
    }

    /// Runs the Smart Turn analyses (and the model's warm-up) on `offload` instead of the
    /// builder's own: a dedicated, bounded executor of their own, so a 20-25 ms analysis
    /// never holds a thread that another call's 32 ms VAD window is waiting for. An analysis the
    /// executor refuses (its queue is full) or does not start in time fails like any failed
    /// analysis and the turn ends at its fallback deadline. Without it the analyses share the
    /// builder's offload with the VAD windows.
    #[must_use]
    pub fn with_smart_turn_offload(mut self, offload: Offload) -> Self {
        self.turn_offload = Some(offload);
        self
    }

    /// The language of the segmenter (first-chunk word count) and of the speech normalizer.
    #[must_use]
    pub const fn with_language(mut self, language: Language) -> Self {
        self.language = language;
        self
    }

    /// Words the first TTS chunk of a response must hold before it may end at a clause
    /// boundary (`SegmenterConfig::first_min_words`; default: the language's,
    /// [`SegmenterStageConfig::first_min_words_for`]). Fewer words start the audio sooner when
    /// the LLM streams a long first sentence word by word (each word waits for the next token),
    /// at the price of a shorter first utterance for the TTS.
    #[must_use]
    pub const fn with_first_chunk_words(mut self, words: usize) -> Self {
        self.first_chunk_words = Some(words);
        self
    }

    /// The TTS provider voice.
    #[must_use]
    pub fn with_voice(mut self, voice: impl Into<Arc<str>>) -> Self {
        self.voice = Some(voice.into());
        self
    }

    /// Judges barge-ins with `policy` instead of the one [`EngineConfig::interruption`]
    /// describes (custom backchannel lists).
    #[must_use]
    pub fn with_interruption(mut self, policy: InterruptionPolicy) -> Self {
        self.interruption = Some(policy);
        self
    }

    /// The conversation starts with this system prompt.
    #[must_use]
    pub fn with_system_prompt(mut self, prompt: impl Into<Arc<str>>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Gives the LLM tools; `engine` must have been built over `registry`.
    #[must_use]
    pub fn with_tools(mut self, registry: Arc<ToolRegistry>, engine: Arc<ToolEngine>) -> Self {
        self.tools = Some((registry, engine));
        self
    }

    /// Drives the conversation with a flow (see
    /// [`LlmRunnerBuilder::with_flow`](crate::LlmRunnerBuilder::with_flow)); it starts when the
    /// pipeline is built.
    #[must_use]
    pub fn with_flow(mut self, flow: FlowManager) -> Self {
        self.flow = Some(flow);
        self
    }

    /// Keeps the summary of dropped turns up to date (see
    /// [`LlmRunnerBuilder::with_summarizer`](crate::LlmRunnerBuilder::with_summarizer)).
    #[must_use]
    pub fn with_summarizer(mut self, summarizer: Arc<dyn Summarizer>) -> Self {
        self.summarizer = Some(summarizer);
        self
    }

    /// The canned filler and fallback lines of the runner.
    #[must_use]
    pub fn with_phrases(
        mut self,
        filler: impl Into<Arc<str>>,
        fallback: impl Into<Arc<str>>,
    ) -> Self {
        self.phrases = Some((filler.into(), fallback.into()));
        self
    }

    /// The runner's filler policy (see
    /// [`LlmRunnerBuilder::with_filler_policy`](crate::LlmRunnerBuilder::with_filler_policy)):
    /// replaces the one built from [`LlmConfig::filler`](crate::LlmConfig::filler). Phrases set
    /// with [`Self::with_phrases`] replace its generic phrase.
    #[must_use]
    pub fn with_filler_policy(mut self, policy: FillerPolicy) -> Self {
        self.filler_policy = Some(policy);
        self
    }

    /// Pre-rendered filler clips: the filler policy's clip cache and the audio the TTS
    /// stage plays for a filler round whose phrase names a clip in `store` (see
    /// [`crate::filler_clip`]). Without it every filler is rendered by the TTS provider.
    #[must_use]
    pub fn with_filler_clips(mut self, store: Arc<FillerClipStore>) -> Self {
        self.filler_clips = Some(store);
        self
    }

    /// Passes the production truncation source through `wrap` (a test double around it, or an
    /// observer such as the recording's transcript). Wraps compose: a later call wraps the
    /// source the earlier ones produced (the earlier wrap is innermost).
    #[must_use]
    pub fn with_source_wrap(mut self, wrap: SourceWrap) -> Self {
        self.wrap = Some(compose_wrap(self.wrap.take(), wrap));
        self
    }

    /// Shows `tap` every caller audio frame as the first stage receives it (the recording
    /// sink; see [`CallerAudioTap`]). Unlike [`Self::with_tap`] this adds no stage.
    #[must_use]
    pub fn with_caller_audio_tap(mut self, tap: impl CallerAudioTap) -> Self {
        // Kept apart from `audio_in`: `with_vad` builds a new stage, which must not lose it.
        self.caller_tap = Some(Box::new(tap));
        self
    }

    /// Puts `tap` right before `TransportOut` (see the [module docs](self)).
    #[must_use]
    pub fn with_tap(mut self, tap: impl FrameTap) -> Self {
        self.tap = Some(Box::new(tap));
        self
    }

    /// Records the turn machine's actions on a [`TurnProbe`] of `capacity` actions (diagnostics
    /// and tests; [`SessionHandles::turn_probe`]).
    #[must_use]
    pub fn with_turn_probe(mut self, capacity: usize) -> Self {
        self.probe = Some(TurnProbe::new(capacity));
        self
    }

    /// Publishes the call's conversation events to `events` (see the [module docs](self) and
    /// [`crate::events`]); subscribe before the call starts to see them all.
    #[must_use]
    pub fn with_events(mut self, events: EngineEvents) -> Self {
        self.events = Some(events);
        self
    }

    /// The events the call will publish to, if [`Self::with_events`] set them (an RTVI bridge
    /// does): another observer subscribes to these instead of replacing them.
    pub const fn events(&self) -> Option<&EngineEvents> {
        self.events.as_ref()
    }

    /// Takes typed user messages from `input` (see [`crate::text_input`]).
    #[must_use]
    pub fn with_text_input(mut self, input: TextTurnReceiver) -> Self {
        self.text_input = Some(input);
        self
    }

    /// Takes lines the bot speaks from `input` (see [`crate::agent_input`]): the turn
    /// driver starts a bot turn for each one, the runner speaks it verbatim.
    #[must_use]
    pub fn with_say_input(mut self, input: SayReceiver) -> Self {
        self.say_input = Some(input);
        self
    }

    /// Applies the conversation edits of `edits` (see [`crate::agent_input`]) before each
    /// response starts.
    #[must_use]
    pub fn with_conversation_edits(mut self, edits: EditReceiver) -> Self {
        self.edits = Some(edits);
        self
    }

    /// The tools given to [`Self::with_tools`], if any: an application that adds tools of its
    /// own (the agent link's remote tools) builds a registry over them instead of dropping them.
    pub fn tools(&self) -> Option<(&Arc<ToolRegistry>, &Arc<ToolEngine>)> {
        self.tools
            .as_ref()
            .map(|(registry, engine)| (registry, engine))
    }

    /// The clock of the call.
    pub const fn clock(&self) -> &SharedClock {
        &self.clock
    }

    /// Rounds the shared [`PlayoutBook`] keeps (default [`DEFAULT_BOOK_CAPACITY`]).
    #[must_use]
    pub const fn with_book_capacity(mut self, capacity: usize) -> Self {
        self.book_capacity = capacity;
        self
    }
}

/// The counters and probes of one call, cheap to clone and valid after the stages moved into the
/// pipeline.
#[derive(Clone)]
pub struct SessionHandles {
    /// `AudioIn`: frames, gaps, resampling.
    pub audio_in: AudioInStats,
    /// The VAD worker (includes the model warm-up counters).
    pub vad: VadStats,
    /// The STT stage.
    pub stt: SttStats,
    /// The turn driver (includes the Smart Turn warm-up counters).
    pub driver: TurnDriverStats,
    /// The LLM runner.
    pub llm: LlmStats,
    /// Speculative generation (all zero unless enabled).
    pub speculation: SpeculationStats,
    /// The request builder (windows, summaries).
    pub requests: RequestStats,
    /// The segmenter stage.
    pub segmenter: SegmenterStats,
    /// The TTS stage.
    pub tts: TtsStats,
    /// The speech-to-speech stage (all zero in a classic call).
    pub s2s: S2sStats,
    /// The transport stage.
    pub out: TransportOutStats,
    /// The book every stage writes: what was sent and what was played.
    pub book: Arc<PlayoutBook>,
    /// The conversation (the sim's `SessionProbes` reads it).
    pub context: Arc<Mutex<Context>>,
    /// Played-text truncation counters.
    pub truncations: Arc<Mutex<TruncationStats>>,
    /// The turn machine's recent actions, when [`SessionBuilder::with_turn_probe`] was used.
    pub turn_probe: Option<TurnProbe>,
    /// The language tracker, when [`SessionBuilder::with_languages`] was used.
    pub language: Option<Arc<LanguageTracker>>,
    source: Arc<PlayoutTruncation>,
    tap_upstream_dropped: Arc<AtomicU64>,
}

impl SessionHandles {
    /// Interrupted responses the truncation source was asked about while the book had never been
    /// told of a clear: non-zero means the turn driver's clear hook is not installed. The
    /// assembly always installs it, so this stays zero (tests assert it).
    pub fn unnoted_clear_cuts(&self) -> u64 {
        self.source.unnoted_clear_cuts()
    }

    /// Upstream frames (`TtsStarted`, `TtsEnded`) the tap stage could not pass on to the TTS
    /// stage because its upstream lane was full or closed; always zero without a tap
    /// ([`SessionBuilder::with_tap`]). A lost `TtsEnded` would leave the turn machine waiting for
    /// the bot to finish, so tests assert this stays zero.
    pub fn tap_upstream_dropped(&self) -> u64 {
        self.tap_upstream_dropped.load(Ordering::Relaxed)
    }
}

impl std::fmt::Debug for SessionHandles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHandles").finish_non_exhaustive()
    }
}

/// What the pump needs once the pipeline is running, see [`SessionPipeline::pump`].
#[derive(Debug, Clone)]
pub struct PumpWiring {
    book: Arc<PlayoutBook>,
    rates: CallRatesCell,
    config: PumpConfig,
    clock: SharedClock,
}

impl PumpWiring {
    /// Starts the pump over `session`, delivering into the running pipeline's two inputs
    /// (`RunningPipeline::input` and `RunningPipeline::upstream_input`). The pump publishes the
    /// call's rates for the TTS stage and writes marks and positions to the shared book.
    pub fn spawn(
        &self,
        session: TransportSession,
        input: &FrameSender,
        upstream: &FrameSender,
        cancel: &CancellationToken,
    ) -> PumpHandle {
        Pump::spawn_publishing(
            session,
            PumpLinks {
                input: input.clone(),
                upstream: Some(upstream.clone()),
            },
            Arc::clone(&self.book),
            self.clock.clone(),
            self.config,
            cancel,
            self.rates.clone(),
        )
    }
}

/// A built, not yet started call pipeline.
pub struct SessionPipeline {
    /// The pipeline to start (`Pipeline::start`).
    pub pipeline: Pipeline,
    /// The counters and probes of its stages.
    pub handles: SessionHandles,
    /// How to start the pump once the pipeline runs.
    pub pump: PumpWiring,
}

impl std::fmt::Debug for SessionPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionPipeline")
            .field("stages", &self.pipeline.processor_count())
            .finish_non_exhaustive()
    }
}

/// Wires the stages of one call over the transport behind `commands` (the session's
/// [`TransportSession::commands`]); see the [module docs](self).
///
/// # Errors
/// [`Error::Flow`] when the flow of [`SessionBuilder::with_flow`] cannot start.
pub fn build_session_pipeline<S, L, T>(
    builder: SessionBuilder<S, L, T>,
    commands: CommandSender,
) -> Result<SessionPipeline>
where
    S: SttProvider,
    L: LlmProvider,
    T: TtsProvider,
{
    let SessionBuilder {
        config,
        clock,
        timeline,
        stt,
        llm,
        tts,
        offload,
        audio_in,
        smart_turn,
        turn_offload,
        language,
        first_chunk_words,
        voice,
        interruption,
        system_prompt,
        tools,
        flow,
        summarizer,
        phrases,
        filler_policy,
        filler_clips,
        wrap,
        tap,
        caller_tap,
        probe,
        book_capacity,
        events,
        text_input,
        say_input,
        edits,
        s2s,
        languages,
    } = builder;
    let s2s_events = events.clone();
    let tracker = languages.map(|settings| {
        let tracker = LanguageTracker::new(settings);
        Arc::new(match &events {
            Some(events) => tracker.with_events(events.clone()),
            None => tracker,
        })
    });
    let driver_events = events.clone();
    let mut observer = events.map(|events| Observer::new(events, timeline.clone()));
    let book = PlayoutBook::shared(book_capacity);
    let rates = CallRatesCell::new(config.rates.fallback_call);
    let tap_upstream_dropped = Arc::new(AtomicU64::new(0));
    // What the speech-to-speech stage needs of the options the classic stages consume below.
    let s2s_tools = tools.clone();
    let s2s_prompt = system_prompt.clone();
    let s2s_voice = voice.clone();

    let audio_in = match caller_tap {
        Some(tap) => audio_in.with_boxed_tap(tap),
        None => audio_in,
    };
    let audio_stats = audio_in.stats();
    let vad_stats = audio_in.vad_stats();

    let mut stt_stage = SttStage::new(stt, SttStageConfig::from_engine(&config), clock.clone())
        .with_timeline(timeline.clone());
    if let Some(tracker) = &tracker {
        stt_stage = stt_stage.with_languages(Arc::clone(tracker));
    }
    let stt_stats = stt_stage.stats();

    let mut driver = TurnDriver::new(
        TurnDriverConfig::from_engine(&config),
        clock.clone(),
        timeline.clone(),
    )
    // The book learns of the clear BEFORE the command: without it every mid-utterance
    // barge-in waits out the settle wait.
    .with_clear_hook(clear_hook(commands.clone(), Arc::clone(&book)));
    if let Some(policy) = interruption {
        driver = driver.with_interruption(policy);
    }
    if let Some(model) = smart_turn {
        driver = driver.with_smart_turn(model, turn_offload.unwrap_or_else(|| offload.clone()));
    }
    if let Some(probe) = probe.clone() {
        driver = driver.with_probe(probe);
    }
    if let Some(input) = text_input {
        driver = driver.with_text_input(input);
    }
    if let Some(events) = driver_events {
        driver = driver.with_events(events);
    }
    if let Some(input) = say_input {
        driver = driver.with_say_input(input);
    }
    let driver_stats = driver.stats();

    let source = Arc::new(PlayoutTruncation::new(
        Arc::clone(&book),
        clock.clone(),
        PlayoutTruncationConfig::from_engine(&config),
    ));
    let mut runner = LlmRunner::builder(llm, &config, clock.clone(), timeline.clone())
        .with_offload(offload.clone())
        .with_book(Arc::clone(&book));
    let truncation: Arc<dyn TruncationSource> = match &wrap {
        Some(wrap) => wrap(Arc::clone(&source) as Arc<dyn TruncationSource>),
        None => Arc::clone(&source) as Arc<dyn TruncationSource>,
    };
    runner = runner.with_truncation(truncation);
    if let Some(prompt) = system_prompt {
        runner = runner.with_system_prompt(prompt);
    }
    if let Some((registry, engine)) = tools {
        runner = runner.with_tools(registry, engine);
    }
    if let Some(flow) = flow {
        runner = runner.with_flow(flow).map_err(Error::Flow)?;
    }
    if let Some(summarizer) = summarizer {
        runner = runner.with_summarizer(summarizer);
    }
    if let Some(policy) = filler_policy {
        runner = runner.with_filler_policy(policy);
    }
    if let Some(store) = &filler_clips {
        runner = runner.with_filler_clips(Arc::clone(store) as Arc<dyn FillerClips>);
    }
    if let Some((filler, fallback)) = phrases {
        runner = runner.with_phrases(filler, fallback);
    }
    if let Some(edits) = edits {
        runner = runner.with_conversation_edits(edits);
    }
    if let Some(tracker) = &tracker {
        runner = runner.with_languages(Arc::clone(tracker));
    }
    let llm_stage = runner.build();
    let llm_stats = llm_stage.stats();
    observer = observer.map(|observer| observer.with_llm(llm_stats.clone()));
    let speculation = llm_stage.speculation_stats();
    let requests = llm_stage.request_stats();
    let context = llm_stage.context();
    let truncations = llm_stage.truncations();

    let mut segmenter_config = SegmenterStageConfig::new(language);
    if let Some(words) = first_chunk_words {
        segmenter_config.segmenter.first_min_words = words;
    }
    let mut segmenter =
        SegmenterStage::new(segmenter_config).with_timeline(timeline.clone(), clock.clone());
    if let Some(tracker) = &tracker {
        segmenter = segmenter.with_languages(Arc::clone(tracker));
    }
    let segmenter_stats = segmenter.stats();

    let mut tts_config = TtsStageConfig::from_engine(&config).with_language(language);
    if let Some(voice) = voice {
        tts_config = tts_config.with_voice(voice);
    }
    let mut tts_stage = TtsStage::new(tts, tts_config, clock.clone(), timeline.clone())
        .with_book(Arc::clone(&book))
        .with_rates(rates.clone())
        .with_offload(offload.clone());
    if let Some(store) = filler_clips {
        tts_stage = tts_stage.with_clips(store);
    }
    if let Some(tracker) = &tracker {
        tts_stage = tts_stage.with_languages(Arc::clone(tracker));
    }
    let tts_stats = tts_stage.stats();

    // A call on a speech-to-speech model replaces the stages above (built only for their
    // counters and the context) with one stage; it clears the transport like the turn driver.
    let s2s_stage = s2s.map(|(provider, options)| {
        let instructions = s2s_prompt.unwrap_or_else(|| Arc::from(""));
        let mut stage_config = S2sStageConfig::from_engine(&config, instructions, options);
        stage_config.voice = s2s_voice;
        stage_config.language = Some(Arc::from(match language {
            Language::En => "en",
            Language::Tr => "tr",
        }));
        S2sStage::new(
            provider,
            stage_config,
            S2sWiring {
                clock: clock.clone(),
                timeline: timeline.clone(),
                book: Arc::clone(&book),
                rates: rates.clone(),
                offload: offload.clone(),
                hook: clear_hook(commands.clone(), Arc::clone(&book)),
                context: Arc::clone(&context),
                tools: s2s_tools,
                events: s2s_events,
            },
        )
    });
    let s2s_stats = s2s_stage.as_ref().map(S2sStage::stats).unwrap_or_default();

    let transport_out = TransportOut::new(
        commands,
        Arc::clone(&book),
        TransportOutConfig::from_engine(&config),
        clock.clone(),
        timeline,
    );
    let out_stats = transport_out.stats();

    let queues = config.queues;
    let observed = |site| observer.clone().map(|observer| (site, observer));
    let mut pipeline = Pipeline::new("call").then_with_queue(audio_in, queues.input);
    pipeline = match s2s_stage {
        // The runner's observation site sees what the S2S stage's neighbours say about the bot
        // (`TtsStarted`, `TtsEnded` coming back upstream); the stage publishes the rest itself.
        Some(stage) => {
            pipeline.then_with_queue(Observed::new(stage, observed(Site::Runner)), queues.user)
        }
        None => pipeline
            .then_with_queue(stt_stage, queues.user)
            .then_with_queue(Observed::new(driver, observed(Site::Driver)), queues.user)
            .then_with_queue(Observed::new(llm_stage, observed(Site::Runner)), queues.bot)
            .then_with_queue(
                Observed::new(segmenter, observed(Site::Segmenter)),
                queues.bot,
            )
            .then_with_queue(Observed::new(tts_stage, observed(Site::Tts)), queues.bot),
    };
    if let Some(tap) = tap {
        let stage = TapStage {
            tap,
            upstream_dropped: Arc::clone(&tap_upstream_dropped),
        };
        pipeline = pipeline.then_with_queue(stage, queues.sink);
    }
    let pipeline = pipeline.then_with_queue(
        Observed::new(transport_out, observed(Site::Out)),
        queues.sink,
    );

    Ok(SessionPipeline {
        pipeline,
        handles: SessionHandles {
            audio_in: audio_stats,
            vad: vad_stats,
            stt: stt_stats,
            driver: driver_stats,
            llm: llm_stats,
            speculation,
            requests,
            segmenter: segmenter_stats,
            tts: tts_stats,
            s2s: s2s_stats,
            out: out_stats,
            book: Arc::clone(&book),
            context,
            truncations,
            turn_probe: probe,
            language: tracker,
            source,
            tap_upstream_dropped,
        },
        pump: PumpWiring {
            book,
            rates,
            config: PumpConfig::from_engine(&config),
            clock,
        },
    })
}

/// A running call: the started pipeline, the pump that feeds it from the transport, and the
/// handles on its stages. Dropping it cancels the pipeline's tasks and aborts the pump (which
/// drops the transport session, ending the call).
pub struct Call {
    running: RunningPipeline,
    pump: PumpHandle,
    handles: SessionHandles,
    stop_timeout: Duration,
    /// The call's own token (a child of the owner's): the pipeline and the pump run under it.
    cancel: CancellationToken,
    /// The end-of-call [`CallStats::trace`] was written (it is written once).
    traced: AtomicBool,
}

impl std::fmt::Debug for Call {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Call")
            .field("pump", &self.pump)
            .finish_non_exhaustive()
    }
}

/// A point-in-time view of a call: queue depths and drops per stage, the pump, and every
/// stage's counters. [`Call::stats`] takes it; [`Self::trace`] writes its headline
/// numbers to the tracing target `brisvox::engine`, which the call itself does once when it
/// ends ([`Call::wait`], [`Call::stop`], [`Call::cancel`] or dropping it).
#[derive(Debug, Clone)]
pub struct CallStats {
    /// Queue depth, overflow drops and stale drops of every stage.
    pub pipeline: PipelineStats,
    /// The transport pump's counters.
    pub pump: PumpStats,
    /// The counters of every stage, the playout book and the played-text truncation.
    pub stages: StageCounters,
}

/// Every stage's counters at one moment (see [`SessionHandles`] for the live handles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageCounters {
    /// `AudioIn`.
    pub audio_in: AudioInStatsSnapshot,
    /// The VAD worker: jobs, pool saturations and timeouts, fail-open, warm-up.
    pub vad: VadStatsSnapshot,
    /// The STT stage.
    pub stt: SttStatsSnapshot,
    /// The turn driver: finals-gate timeouts, analyses, clears, warm-up.
    pub driver: TurnDriverStatsSnapshot,
    /// The LLM runner.
    pub llm: LlmStatsSnapshot,
    /// Speculative generation.
    pub speculation: SpeculationStatsSnapshot,
    /// The request builder: windows, offloaded builds and their fallbacks.
    pub requests: RequestStatsSnapshot,
    /// The segmenter stage.
    pub segmenter: SegmenterStatsSnapshot,
    /// The TTS stage.
    pub tts: TtsStatsSnapshot,
    /// The transport stage: stale drops, marks, clears.
    pub out: TransportOutStatsSnapshot,
    /// The playout book: settle timeouts, evictions, clears noted.
    pub book: BookStats,
    /// Played-text truncations recorded in the context.
    pub truncations: TruncationStats,
    /// See [`SessionHandles::unnoted_clear_cuts`] (always zero in an assembled call).
    pub unnoted_clear_cuts: u64,
    /// The language tracker (all zero without [`SessionBuilder::with_languages`]).
    pub language: LanguageStatsSnapshot,
}

impl StageCounters {
    /// Reads every counter of `handles` now.
    pub fn read(handles: &SessionHandles) -> Self {
        Self {
            audio_in: handles.audio_in.snapshot(),
            vad: handles.vad.snapshot(),
            stt: handles.stt.snapshot(),
            driver: handles.driver.snapshot(),
            llm: handles.llm.snapshot(),
            speculation: handles.speculation.snapshot(),
            requests: handles.requests.snapshot(),
            segmenter: handles.segmenter.snapshot(),
            tts: handles.tts.snapshot(),
            out: handles.out.snapshot(),
            book: handles.book.stats(),
            truncations: *handles
                .truncations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            unnoted_clear_cuts: handles.unnoted_clear_cuts(),
            language: handles
                .language
                .as_ref()
                .map(|tracker| tracker.stats())
                .unwrap_or_default(),
        }
    }

    /// Jobs the inference pool refused or did not finish in time: VAD batches (saturated or
    /// timed out), Smart Turn analyses that failed, request builds that fell back, TTS
    /// normalisations that failed, warm-ups skipped or failed.
    pub const fn pool_trouble(&self) -> u64 {
        self.vad
            .saturated
            .saturating_add(self.vad.timeouts)
            .saturating_add(self.driver.analyses_failed)
            .saturating_add(self.requests.offload_fallbacks)
            .saturating_add(self.tts.normalize_failures)
            .saturating_add(self.vad.warm_skips)
            .saturating_add(self.vad.warm_failures)
            .saturating_add(self.driver.warm_skips)
            .saturating_add(self.driver.warm_failures)
    }
}

impl CallStats {
    /// Frames queued in the fullest inbox now.
    pub fn max_queued(&self) -> usize {
        self.pipeline
            .stages
            .iter()
            .map(|stage| stage.queued)
            .max()
            .unwrap_or(0)
    }

    /// Outgoing frames dropped as stale, over all stages (the pipeline's own count plus the
    /// transport stage's audio and control frames of ended epochs).
    pub fn stale_dropped(&self) -> u64 {
        self.pipeline
            .stages
            .iter()
            .map(|stage| stage.stale_dropped)
            .fold(0, u64::saturating_add)
            .saturating_add(self.stages.out.stale_dropped)
            .saturating_add(self.stages.out.stale_control)
    }

    /// The stages that lost frames to an overflow policy, as `name:oldest/newest` joined by
    /// spaces (empty when nothing was dropped), so a drop count in [`Self::trace`] says where.
    pub fn dropped_by_stage(&self) -> String {
        stage_drops(&self.pipeline)
    }

    /// Writes the headline numbers as one `info` event on the tracing target `brisvox::engine`:
    /// queue depth and drops (and which stages dropped), stale drops, pool trouble, truncations,
    /// finals-gate and settle timeouts. Call it at the end of a call, or at most about once a
    /// second.
    pub fn trace(&self) {
        let s = &self.stages;
        tracing::info!(
            target: "brisvox::engine",
            max_queued = self.max_queued(),
            dropped = self.pipeline.total_dropped(),
            dropped_by_stage = %self.dropped_by_stage(),
            stale_dropped = self.stale_dropped(),
            vad_saturated = s.vad.saturated,
            vad_timeouts = s.vad.timeouts,
            vad_failed_open = s.vad.failed_open,
            pool_trouble = s.pool_trouble(),
            truncations = s.truncations.truncations,
            truncations_estimated = s.truncations.estimated,
            finals_gate_timeouts = s.driver.gate_timeouts,
            settle_timeouts = s.book.settle_timeouts,
            mark_timeouts = s.out.mark_timeouts,
            "call stats"
        );
    }
}

impl Call {
    /// The running pipeline: [`RunningPipeline::stats`], [`RunningPipeline::stop`],
    /// [`RunningPipeline::cancel`], [`RunningPipeline::wait`].
    pub fn running(&mut self) -> &mut RunningPipeline {
        &mut self.running
    }

    /// The transport pump (rates, end of the call, counters).
    pub fn pump(&mut self) -> &mut PumpHandle {
        &mut self.pump
    }

    /// The counters and probes of the stages.
    pub const fn handles(&self) -> &SessionHandles {
        &self.handles
    }

    /// Queue depths, drops, the pump's and every stage's counters now (poll at 1 Hz or slower,
    /// never per frame: see [`RunningPipeline::stats`]). [`CallStats::trace`] logs them.
    pub fn stats(&self) -> CallStats {
        CallStats {
            pipeline: self.running.stats(),
            pump: self.pump.stats(),
            stages: StageCounters::read(&self.handles),
        }
    }

    /// Waits until the pipeline ends by itself: the caller hung up (the pump sent `Stop` and
    /// every stage drained and forwarded it) or a stage failed. Either way the call's token is
    /// then cancelled, so the pump ends and drops the transport session: after a stage failure
    /// the caller is not left on an open line of dead air while the owner keeps the `Call`
    /// (for [`Self::stats`], say). Cancel-safe.
    ///
    /// # Errors
    /// The root-cause processor error, as [`RunningPipeline::wait`].
    pub async fn wait(&mut self) -> brisvox_core::Result<()> {
        let result = self.running.wait().await;
        self.cancel.cancel();
        self.trace_end();
        result
    }

    /// Graceful stop, bounded by `Timeouts::stop` of the call's configuration: sends `Stop` and
    /// waits for every stage to drain; on expiry the pipeline is cancelled and its tasks
    /// aborted (see [`RunningPipeline::stop`]). Then the pump ends, which drops the transport
    /// session: the call is over either way.
    ///
    /// # Errors
    /// A processor failure, or `Error::Timeout` when the drain did not finish in time.
    pub async fn stop(&mut self) -> brisvox_core::Result<()> {
        let timeout = self.stop_timeout;
        let result = self.running.stop(timeout).await;
        self.cancel.cancel();
        self.trace_end();
        result
    }

    /// Cancels every task of the call at once, the pump's included; queued frames are dropped.
    /// Dropping the call does the same.
    pub fn cancel(&self) {
        self.cancel.cancel();
        self.trace_end();
    }

    /// Writes [`CallStats::trace`] once, when the call ends (`wait`, `stop`, `cancel` or drop,
    /// whichever comes first), so the end-of-call numbers reach `brisvox::engine` without the
    /// owner asking for them.
    fn trace_end(&self) {
        if !self.traced.swap(true, Ordering::Relaxed) {
            self.stats().trace();
        }
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        self.trace_end();
    }
}

/// Starts one call over `session`: builds the pipeline of `builder`, starts it under a child of
/// `cancel` (cancelling `cancel` ends the call), then starts the pump that feeds it from the
/// transport under the same child. Stages warm their
/// connections and models when `Start` reaches them (see the [module docs](self)).
///
/// The returned [`Call`] ends when the caller hangs up ([`Call::wait`]), or when the owner stops
/// ([`Call::stop`]) or cancels ([`Call::cancel`]) it. A stage failure cancels nothing by itself:
/// the pump keeps the transport session open until the owner reacts. [`Call::wait`] returns the
/// failure and ends the pump, as do [`Call::stop`], [`Call::cancel`] and dropping the call, so
/// an owner should always await one of them. Dropping the call, or the future of
/// an owner that awaits it, aborts every task: no task is left behind.
///
/// # Errors
/// [`Error::Flow`] when the flow cannot start; a pipeline start failure as [`Error::Core`].
pub async fn run_call<S, L, T>(
    session: TransportSession,
    builder: SessionBuilder<S, L, T>,
    cancel: &CancellationToken,
) -> Result<Call>
where
    S: SttProvider,
    L: LlmProvider,
    T: TtsProvider,
{
    let stop_timeout = builder.config.timeouts.stop;
    let SessionPipeline {
        pipeline,
        handles,
        pump,
    } = build_session_pipeline(builder, session.commands())?;
    let call = cancel.child_token();
    let running = pipeline.start(Links::default(), &call).await?;
    let pump = pump.spawn(session, running.input(), running.upstream_input(), &call);
    Ok(Call {
        running,
        pump,
        handles,
        stop_timeout,
        cancel: call,
        traced: AtomicBool::new(false),
    })
}

/// `name:oldest/newest` for every stage of `stats` that lost frames to an overflow policy, joined
/// by spaces; empty when none did.
fn stage_drops(stats: &PipelineStats) -> String {
    let mut out = String::new();
    for stage in &stats.stages {
        let q = stage.queue;
        if q.dropped_oldest == 0 && q.dropped_newest == 0 {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        let _ = std::fmt::Write::write_fmt(
            &mut out,
            format_args!("{}:{}/{}", stage.name, q.dropped_oldest, q.dropped_newest),
        );
    }
    out
}

#[cfg(test)]
mod wrap_tests {
    use std::sync::Mutex;

    use super::*;
    use crate::truncation::KeepAll;

    /// A wrap that records its name when applied.
    fn named(name: &'static str, log: &Arc<Mutex<Vec<&'static str>>>) -> SourceWrap {
        let log = Arc::clone(log);
        Arc::new(move |source| {
            if let Ok(mut log) = log.lock() {
                log.push(name);
            }
            source
        })
    }

    #[test]
    fn wraps_compose_with_the_earlier_one_innermost() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let first = named("first", &log);
        let second = named("second", &log);
        let composed = compose_wrap(Some(first), second);
        let _ = composed(Arc::new(KeepAll));
        assert_eq!(*log.lock().unwrap(), ["first", "second"]);
        // Without an earlier wrap the new one is used as it is.
        let solo = compose_wrap(None, named("solo", &log));
        let _ = solo(Arc::new(KeepAll));
        assert_eq!(log.lock().unwrap().last(), Some(&"solo"));
    }
}

#[cfg(test)]
mod stage_drop_tests {
    use brisvox_core::{PipelineStats, QueueStats, StageStats};

    use super::stage_drops;

    fn stage(name: &'static str, oldest: u64, newest: u64) -> StageStats {
        StageStats {
            name,
            queue: QueueStats {
                dropped_oldest: oldest,
                dropped_newest: newest,
            },
            queued: 0,
            stale_dropped: 0,
        }
    }

    #[test]
    fn only_stages_that_dropped_are_named() {
        let none = PipelineStats {
            stages: vec![stage("vad", 0, 0), stage("stt", 0, 0)],
        };
        assert_eq!(stage_drops(&none), "");
        let some = PipelineStats {
            stages: vec![
                stage("vad", 0, 0),
                stage("transport_out", 66, 0),
                stage("tts", 1, 2),
            ],
        };
        assert_eq!(stage_drops(&some), "transport_out:66/0 tts:1/2");
    }
}
