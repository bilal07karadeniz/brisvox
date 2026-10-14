//! The engine behind the server's sessions: [`EngineRunner`] is the
//! [`SessionRunner`] that runs one call of an agent definition on the
//! engine's [`run_call`].
//!
//! # One call
//! 1. The accept loop of a hosted transport ([`crate::hosting`]) hands the runner an
//!    [`AcceptedCall`] inside the session the manager admitted for it.
//! 2. [`TransportSession::open`] wraps the call's link under the session's token, seeded with
//!    the session id (the output pacer's phase seed, used only when its phase is enabled).
//! 3. The [`ProviderFactory`] gives the call its STT, LLM and TTS instances (production:
//!    [`ConfiguredProviders`](crate::providers::ConfiguredProviders), one set per process).
//! 4. A [`SessionBuilder`] over the agent's [`EngineConfig`](brisvox_engine::EngineConfig) runs
//!    every heavy job on the shared inference pool ([`Offload`]); the definition sets language,
//!    voice and system prompt ([`AgentConfig::configure`]); the VAD is the bundled Silero model
//!    on tract when the `silero-tract` feature is on ([`Vad`]); an adaptive turn policy gets the
//!    Smart Turn model when one was loaded ([`EngineRunner::with_smart_turn`],
//!    [`load_smart_turn`]). The call gets its tools ([`crate::tools`]): its own `ToolEngine`
//!    over the agent's tools. An RTVI call also gets the RTVI bridge: engine events to
//!    the client, its `send-text` into the call.
//! 5. [`run_call`] starts the pipeline; the session then waits for the caller to hang up
//!    ([`Call::wait`](brisvox_engine::session::Call::wait)) or for its token (drain timeout,
//!    shutdown), on which the call is stopped gracefully, bounded by the engine's `stop`
//!    timeout. Dropping the session future (the manager's abort) drops the call, which aborts
//!    every task of it. A call with the telephony tools (feature `call-control`) also follows
//!    its engine events: after `end_call` it waits for the farewell, hangs up through the
//!    carrier and stops the call (`crate::tools::telephony`).
//!
//! With an agent link ([`EngineRunner::with_agent_link`]) the builder is handed to
//! [`brisvox_agent_link::attach`] last: the call's events go to the agent process, its commands
//! and remote tools come back. The link never fails the call (without an agent the call runs on
//! the definition); an `end` from the agent stops the call like a drain, and the link says
//! `session.end` when the call is over.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use brisvox_agent::turn::TurnPolicy;
use brisvox_agent_link::{AgentLink, AgentLinkConfig, LinkCall};
use brisvox_core::{SharedClock, SystemClock, TimelineRecorder};
use brisvox_engine::rtvi::{RtviBridge, RtviBridgeConfig};
use brisvox_engine::session::{Call, SessionBuilder};
use brisvox_engine::{
    EngineEvents, EventStream, Offload, SayReceiver, SaySender, TaskGuard, run_call,
};
use brisvox_inference::smart_turn::TurnModel;
use brisvox_providers::{LlmProvider, SttProvider, TtsProvider};
use brisvox_transport::rtvi::RtviCall;
use brisvox_transport::{CallLink, CallStarted, SessionConfig, TransportSession};
use tokio::time::Instant;

use crate::config::{AgentConfig, Env};
use crate::observability::{Observability, call::CallGuard};
use crate::privacy::{CallInfo, Recording};
use crate::providers::{CallProviders, ProviderFactory};
use crate::session::{SessionContext, SessionRunner};
use crate::settings::{count_var, duration_var, flag_var, var};
use crate::tools::{
    AgentTools, CallAction, CallTelephony, FAREWELL_TIMEOUT, ToolSetup, VOICEMAIL_PLAY_TIMEOUT,
};
use crate::{Error, Result};

/// A call a hosted transport accepted, ready to run.
pub enum AcceptedCall {
    /// A plain voice call (generic websocket, Twilio, Telnyx).
    Link(CallLink),
    /// An RTVI call: its link plus the side channels the RTVI bridge uses.
    Rtvi(RtviCall),
}

impl fmt::Debug for AcceptedCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Link(_) => "AcceptedCall::Link",
            Self::Rtvi(_) => "AcceptedCall::Rtvi",
        })
    }
}

/// The voice activity detector of every call.
#[derive(Clone, Default)]
#[non_exhaustive]
pub enum Vad {
    /// None: turns end on the turn policy's deadlines and the STT's finals only. For tests
    /// and builds without a VAD.
    #[default]
    Off,
    /// The bundled Silero VAD on tract, loaded once and shared by every call (each call has
    /// its own stream state).
    #[cfg(feature = "silero-tract")]
    Silero(Arc<brisvox_inference::silero_tract::SileroTractModel>),
}

impl fmt::Debug for Vad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Off => "Vad::Off",
            #[cfg(feature = "silero-tract")]
            Self::Silero(_) => "Vad::Silero",
        })
    }
}

impl Vad {
    /// The production VAD of this build: Silero on tract with the `silero-tract` feature,
    /// [`Vad::Off`] without.
    ///
    /// # Errors
    /// [`Error::Config`] when the bundled model does not load.
    pub fn production() -> Result<Self> {
        #[cfg(feature = "silero-tract")]
        {
            brisvox_inference::silero_tract::SileroTractModel::bundled()
                .map(Self::Silero)
                .map_err(|e| Error::Config(format!("cannot load the Silero VAD: {e}")))
        }
        #[cfg(not(feature = "silero-tract"))]
        {
            Ok(Self::Off)
        }
    }

    /// Installs this VAD on `builder`, its jobs on `offload`.
    #[cfg_attr(not(feature = "silero-tract"), allow(unused_variables))]
    fn apply<S, L, T>(
        &self,
        builder: SessionBuilder<S, L, T>,
        offload: &Offload,
        agent: &AgentConfig,
    ) -> SessionBuilder<S, L, T>
    where
        S: SttProvider,
        L: LlmProvider,
        T: TtsProvider,
    {
        match self {
            Self::Off => builder,
            #[cfg(feature = "silero-tract")]
            Self::Silero(model) => {
                let model = Arc::clone(model);
                builder.with_vad(brisvox_engine::VadSetup::new(
                    offload.clone(),
                    brisvox_engine::VadWorkerConfig::from_engine(&agent.engine()),
                    move || model.stream(),
                ))
            }
        }
    }
}

/// Environment variable: the Smart Turn model file of adaptive turns, or `off` (feature
/// `smart-turn-ort`; default `<BRISVOX_MODELS_DIR>/smart-turn-v3.2-cpu.onnx`).
pub const ENV_SMART_TURN_MODEL: &str = "BRISVOX_SMART_TURN_MODEL";
/// Environment variable: the directory of the model files (default `models`).
pub const ENV_MODELS_DIR: &str = "BRISVOX_MODELS_DIR";

/// Environment variable: threads of the dedicated Smart Turn executor, `0` to run the
/// analyses on the VAD's inference pool as before. Default:
/// [`brisvox_engine::offload::default_smart_turn_threads`].
pub const ENV_SMART_TURN_THREADS: &str = "BRISVOX_SMART_TURN_THREADS";

/// The most threads the Smart Turn executor may be given.
pub const MAX_SMART_TURN_THREADS: usize = 64;

/// The threads of the Smart Turn executor for an inference pool of `pool_threads` threads:
/// `BRISVOX_SMART_TURN_THREADS` when set (0 = no executor, the analyses share the pool), else the
/// engine's default.
///
/// # Errors
/// [`Error::Config`] when the variable is not a whole number from 0 to [`MAX_SMART_TURN_THREADS`].
pub fn smart_turn_threads(env: &dyn Env, pool_threads: usize) -> Result<usize> {
    Ok(
        count_var(env, ENV_SMART_TURN_THREADS, 0, MAX_SMART_TURN_THREADS)?
            .unwrap_or_else(|| brisvox_engine::offload::default_smart_turn_threads(pool_threads)),
    )
}

/// ONNX Runtime intra-op threads of one Smart Turn analysis (the calling thread takes part).
#[cfg(feature = "smart-turn-ort")]
const SMART_TURN_INTRA_THREADS: usize =
    brisvox_inference::smart_turn::ort_backend::DEFAULT_INTRA_THREADS;
/// Without the ONNX Runtime backend no analysis runs; one thread keeps the arithmetic neutral.
#[cfg(not(feature = "smart-turn-ort"))]
const SMART_TURN_INTRA_THREADS: usize = 1;

/// Threads the inference setup wants at once, when that is more than `cores` (`None` when it
/// fits). Two checks: the VAD pool's and the executor's threads together
/// (`vad_threads + turn_threads`), and the executor busy at full tilt, each analysis on its
/// ONNX Runtime intra-op threads (`turn_threads` times `SMART_TURN_INTRA_THREADS`); the VAD's
/// threads are idle most of the time, so the executor may use every core. The defaults (half the
/// cores each) fit from two cores up; a raised `BRISVOX_SMART_TURN_THREADS` or a single-core
/// machine does not.
pub fn inference_thread_oversubscription(
    vad_threads: usize,
    turn_threads: usize,
    cores: usize,
) -> Option<usize> {
    if turn_threads == 0 {
        return None;
    }
    let threads = vad_threads.saturating_add(turn_threads);
    let busy = turn_threads.saturating_mul(SMART_TURN_INTRA_THREADS);
    let wanted = threads.max(busy);
    (wanted > cores).then_some(wanted)
}

/// Loads the Smart Turn model of `agent`'s adaptive turns (`docs/OPERATIONS.md`). Blocking
/// (the file read and the first ONNX Runtime session take ~100 ms): call it before the
/// runtime, or on a blocking thread.
///
/// `Ok(None)`: the agent's turn policy is not adaptive, the variable says `off`, or the build
/// lacks the `smart-turn-ort` feature (adaptive turns then end at their fallback deadline; the
/// caller should say so in its log).
///
/// # Errors
/// [`Error::Config`] when the model does not load (missing file, wrong size, rejected by ONNX
/// Runtime) or when the variable names a file in a build without the feature: a server asked
/// for Smart Turn does not start without it.
pub fn load_smart_turn(env: &dyn Env, agent: &AgentConfig) -> Result<Option<Arc<dyn TurnModel>>> {
    if !matches!(agent.engine().turn, TurnPolicy::Adaptive { .. }) {
        return Ok(None);
    }
    let setting = var(env, ENV_SMART_TURN_MODEL)?;
    if setting
        .as_deref()
        .is_some_and(|v| v.eq_ignore_ascii_case("off"))
    {
        return Ok(None);
    }
    #[cfg(feature = "smart-turn-ort")]
    {
        let path = match setting {
            Some(path) => std::path::PathBuf::from(path),
            None => std::path::PathBuf::from(
                var(env, ENV_MODELS_DIR)?.unwrap_or_else(|| "models".to_owned()),
            )
            .join(brisvox_inference::smart_turn::MODEL_FILE),
        };
        let model = brisvox_inference::smart_turn::ort_backend::SmartTurnOrt::load(&path).map_err(|e| {
            Error::Config(format!(
                "{ENV_SMART_TURN_MODEL}: cannot load the Smart Turn model {}: {e} (fetch it with scripts/models/fetch_models.py, or set {ENV_SMART_TURN_MODEL}=off)",
                path.display()
            ))
        })?;
        Ok(Some(model))
    }
    #[cfg(not(feature = "smart-turn-ort"))]
    match setting {
        Some(_) => Err(Error::Config(format!(
            "{ENV_SMART_TURN_MODEL} is set, but this binary is built without the `smart-turn-ort` feature"
        ))),
        None => Ok(None),
    }
}

/// Settings of an [`EngineRunner`] beyond the agent definition.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunnerConfig {
    /// Queues of each call's transport session (its `seed` is replaced by the session id).
    pub session: SessionConfig,
    /// The RTVI bridge of RTVI calls.
    pub rtvi: RtviBridgeConfig,
}

/// Environment variable: the RTVI bridge sends the bot-speaking events from the engine
/// ([`RtviBridgeConfig::bot_speaking_events`], default `true`).
pub const ENV_RTVI_BOT_SPEAKING_EVENTS: &str = "BRISVOX_RTVI_BOT_SPEAKING_EVENTS";
/// Environment variable: the RTVI bridge sends `metrics` once per turn
/// ([`RtviBridgeConfig::metrics`], default `true`).
pub const ENV_RTVI_METRICS: &str = "BRISVOX_RTVI_METRICS";
/// Environment variable: how long a client message's handler may take
/// ([`RtviBridgeConfig::reply_timeout`], default `5s`).
pub const ENV_RTVI_REPLY_TIMEOUT: &str = "BRISVOX_RTVI_REPLY_TIMEOUT";
/// Environment variable: the engine event ring of an RTVI call
/// ([`RtviBridgeConfig::event_capacity`]).
pub const ENV_RTVI_EVENT_CAPACITY: &str = "BRISVOX_RTVI_EVENT_CAPACITY";
/// Environment variable: typed messages waiting for the turn driver
/// ([`RtviBridgeConfig::text_capacity`]).
pub const ENV_RTVI_TEXT_CAPACITY: &str = "BRISVOX_RTVI_TEXT_CAPACITY";

impl RunnerConfig {
    /// The defaults with the RTVI bridge's `BRISVOX_RTVI_*` settings from `env`
    /// (`docs/OPERATIONS.md`).
    ///
    /// # Errors
    /// [`Error::Config`] naming the variable when a value does not parse or is out of range
    /// (a zero reply timeout or capacity included).
    pub fn from_env(env: &dyn Env) -> Result<Self> {
        let mut config = Self::default();
        let rtvi = &mut config.rtvi;
        if let Some(on) = flag_var(env, ENV_RTVI_BOT_SPEAKING_EVENTS)? {
            rtvi.bot_speaking_events = on;
        }
        if let Some(on) = flag_var(env, ENV_RTVI_METRICS)? {
            rtvi.metrics = on;
        }
        if let Some(timeout) = duration_var(env, ENV_RTVI_REPLY_TIMEOUT)? {
            if timeout.is_zero() {
                return Err(Error::Config(format!(
                    "{ENV_RTVI_REPLY_TIMEOUT}: must be longer than zero"
                )));
            }
            rtvi.reply_timeout = timeout;
        }
        if let Some(n) = count_var(
            env,
            ENV_RTVI_EVENT_CAPACITY,
            1,
            brisvox_engine::MAX_EVENT_CAPACITY,
        )? {
            rtvi.event_capacity = n;
        }
        if let Some(n) = count_var(
            env,
            ENV_RTVI_TEXT_CAPACITY,
            1,
            brisvox_engine::MAX_TEXT_CAPACITY,
        )? {
            rtvi.text_capacity = n;
        }
        Ok(config)
    }
}

/// Runs the server's calls on the engine; see the [module docs](self).
pub struct EngineRunner<F: ProviderFactory> {
    agent: AgentConfig,
    factory: F,
    offload: Offload,
    vad: Vad,
    config: RunnerConfig,
    tools: AgentTools,
    smart_turn: Option<Arc<dyn TurnModel>>,
    /// Where the Smart Turn analyses run when not on `offload`.
    turn_offload: Option<Offload>,
    /// The out-of-process agent of every call, if one is configured.
    agent_link: Option<Arc<AgentLinkConfig>>,
    /// Named agent processes a call may pick in its start message, by route name.
    agent_routes: HashMap<String, Arc<AgentLinkConfig>>,
    /// Metrics and turn traces of every call, if wanted.
    observability: Option<Arc<Observability>>,
    /// Transcript and audio recording of every call, if wanted.
    recording: Option<Arc<Recording>>,
    /// The server redacts phone numbers (recording or log redaction): an agent process that
    /// opted in to the caller identity gets the carrier ids but not the numbers.
    redact_caller_numbers: bool,
}

impl<F: ProviderFactory> fmt::Debug for EngineRunner<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineRunner")
            .field("agent", &self.agent.name())
            .field("vad", &self.vad)
            .field("config", &self.config)
            .field("tools", &self.tools)
            .field("smart_turn", &self.smart_turn.is_some())
            .finish_non_exhaustive()
    }
}

impl<F: ProviderFactory> EngineRunner<F> {
    /// A runner for `agent`'s calls, on `factory`'s providers, with every heavy job on
    /// `offload` (the shared inference pool: [`Offload::Pool`]; `Offload::Inline` only in
    /// paused-time tests, see [`SessionBuilder::new`]).
    ///
    /// The calls have no tools ([`Self::with_tools`] offers some).
    ///
    /// # Errors
    /// [`Error::Config`] when the definition names tools: a call without its tools would
    /// answer wrongly instead of failing.
    pub fn new(
        agent: AgentConfig,
        factory: F,
        offload: Offload,
        vad: Vad,
        config: RunnerConfig,
    ) -> Result<Self> {
        Self::with_tools(
            agent,
            factory,
            offload,
            vad,
            config,
            ToolSetup::none(),
            &SystemClock::shared(),
        )
    }

    /// [`Self::new`] with the calls' tools from `tools` ([`crate::tools`]); `clock` is the
    /// calls' clock (the telephony tools time their waits on it).
    ///
    /// # Errors
    /// [`Error::Config`] naming each tool the definition lists that `tools` does not provide,
    /// with its position in the file and what would provide it.
    pub fn with_tools(
        agent: AgentConfig,
        factory: F,
        offload: Offload,
        vad: Vad,
        config: RunnerConfig,
        tools: ToolSetup,
        clock: &SharedClock,
    ) -> Result<Self> {
        let tools = AgentTools::bind(tools, &agent, clock)?;
        Ok(Self {
            agent,
            factory,
            offload,
            vad,
            config,
            tools,
            smart_turn: None,
            turn_offload: None,
            agent_link: None,
            agent_routes: HashMap::new(),
            observability: None,
            recording: None,
            redact_caller_numbers: false,
        })
    }

    /// Decides the end of the user's turn with `model` when the definition's turn policy is
    /// adaptive (without a model an adaptive turn ends at its fallback deadline). Shared by
    /// every call; the engine warms it on the inference pool.
    #[must_use]
    pub fn with_smart_turn(mut self, model: Arc<dyn TurnModel>) -> Self {
        self.smart_turn = Some(model);
        self
    }

    /// Runs the Smart Turn analyses and the model's warm-up on `offload`, a dedicated executor
    /// of their own ([`brisvox_engine::offload::start_smart_turn_executor`]), instead of the
    /// shared pool the VAD windows use. Without it they share `offload` with the VAD.
    #[must_use]
    pub fn with_smart_turn_offload(mut self, offload: Offload) -> Self {
        self.turn_offload = Some(offload);
        self
    }

    /// Whether adaptive turns are decided by a Smart Turn model.
    pub const fn has_smart_turn(&self) -> bool {
        self.smart_turn.is_some()
    }

    /// Links every call to the agent process `config` names
    /// (`docs/AGENT_PROTOCOL.md`).
    #[must_use]
    pub fn with_agent_link(mut self, config: Arc<AgentLinkConfig>) -> Self {
        self.agent_link = Some(config);
        self
    }

    /// The agent processes of [`AgentLinks`](crate::AgentLinks): the default one and the named routes.
    #[must_use]
    pub fn with_agent_links(mut self, links: crate::AgentLinks) -> Self {
        self.agent_link = links.default;
        self.agent_routes = links.routes.into_iter().collect();
        self
    }

    /// Adds a named agent process: a call whose start message names `route` (the
    /// metadata key [`ROUTE_KEY`] of a generic websocket client, the TwiML parameter
    /// `agent_route` of Twilio) links to it instead of the default agent process
    /// ([`Self::with_agent_link`]). The name picks among the processes the operator configured;
    /// a start message never carries a URL of its own, so a caller cannot make the server
    /// connect to an address of its choosing. A call naming an unknown route gets the default
    /// (or no agent process, when there is none).
    #[must_use]
    pub fn with_agent_route(
        mut self,
        route: impl Into<String>,
        config: Arc<AgentLinkConfig>,
    ) -> Self {
        self.agent_routes.insert(route.into(), config);
        self
    }

    /// The agent process of a call that started as `info` says (`None`: no agent process).
    fn agent_link_for(
        &self,
        info: Option<&brisvox_transport::CallInfo>,
    ) -> Option<&Arc<AgentLinkConfig>> {
        let route = info.and_then(|info| {
            [ROUTE_KEY, ROUTE_KEY_TWILIO]
                .iter()
                .find_map(|key| info.metadata.get(*key))
        });
        match route {
            Some(route) => match self.agent_routes.get(route.as_str()) {
                Some(config) => Some(config),
                None => {
                    tracing::warn!(target: "brisvox::server", route = %route, "the call names an unknown agent route: using the default agent process");
                    self.agent_link.as_ref()
                }
            },
            None => self.agent_link.as_ref(),
        }
    }

    /// Whether any call may link to an agent process.
    fn has_agent_link(&self) -> bool {
        self.agent_link.is_some() || !self.agent_routes.is_empty()
    }

    /// Feeds every call's turns, traces and end-of-call counters into `observability`
    /// ([`crate::observability`]). Without it the calls are not observed.
    #[must_use]
    pub fn with_observability(mut self, observability: Arc<Observability>) -> Self {
        self.observability = Some(observability);
        self
    }

    /// Records every call's transcript and audio as `recording` is set up to
    /// ([`crate::privacy`]). Without it nothing is recorded.
    #[must_use]
    pub fn with_recording(mut self, recording: Arc<Recording>) -> Self {
        self.recording = Some(recording);
        self
    }

    /// Whether the server redacts phone numbers anywhere
    /// (`caller_identity::phone_numbers_redacted`): an agent process that opted in to the caller
    /// identity then gets the carrier and the carrier's call id but no numbers. Default `false`.
    #[must_use]
    pub const fn with_redact_caller_numbers(mut self, redact: bool) -> Self {
        self.redact_caller_numbers = redact;
        self
    }

    /// The agent definition the calls run.
    pub const fn agent(&self) -> &AgentConfig {
        &self.agent
    }

    /// Where the calls' providers come from.
    pub const fn factory(&self) -> &F {
        &self.factory
    }

    /// The agent process of this call: the named route of its start message, else the default.
    /// Waits (bounded) for the start message only when routes exist or the default agent
    /// process asked for the caller identity, and gives the start message back with the pick.
    async fn pick_agent_link(
        &self,
        ctx: &SessionContext,
        started: &CallStarted,
    ) -> Option<(Arc<AgentLinkConfig>, Option<brisvox_transport::CallInfo>)> {
        if !self.has_agent_link() {
            return None;
        }
        let wants_start = !self.agent_routes.is_empty()
            || self.agent_link.as_ref().is_some_and(|c| c.caller_identity);
        if !wants_start {
            return self.agent_link.clone().map(|config| (config, None));
        }
        let info = wait_for_start(&ctx.cancel, &ctx.clock, started).await;
        let config = self.agent_link_for(info)?.clone();
        Some((config, info.cloned()))
    }

    /// Runs one call to its end (see the [module docs](self)).
    async fn drive(&self, ctx: SessionContext, call: AcceptedCall) -> Result<()> {
        let (link, rtvi) = match call {
            AcceptedCall::Link(link) => (link, None),
            AcceptedCall::Rtvi(call) => (call.link, Some((call.sender, call.inbox))),
        };
        let session_config = SessionConfig {
            seed: ctx.id.get(),
            ..self.config.session
        };
        let session = TransportSession::open(ctx.clock.clone(), link, session_config, &ctx.cancel)
            .await
            .map_err(|e| Error::Call(format!("transport: {e}")))?;
        let started = session.call_started();
        let tools = self
            .tools
            .for_call(
                ctx.id.get(),
                &ctx.clock,
                &self.offload,
                started.clone(),
                self.has_agent_link(),
            )
            .await?;
        let providers = self.factory.providers();
        let observer = self.observability.as_ref().map(|obs| {
            let observer = obs.call(ctx.id.get(), self.agent.name(), ctx.clock.clone());
            observer.watch_resilience(self.factory.resilience_probes(&providers));
            observer
        });
        let timeline = observer
            .as_ref()
            .map_or_else(TimelineRecorder::new, |observer| observer.recorder());
        let CallProviders { stt, llm, tts } = providers;
        let builder = SessionBuilder::new(
            self.agent.engine(),
            ctx.clock.clone(),
            timeline.clone(),
            self.offload.clone(),
            stt,
            llm,
            tts,
        )
        .map_err(|e| Error::Call(format!("engine configuration: {e}")))?;
        let builder = self.agent.configure(builder);
        let builder = self.vad.apply(builder, &self.offload, &self.agent);
        let builder = match &self.smart_turn {
            Some(model) if matches!(self.agent.engine().turn, TurnPolicy::Adaptive { .. }) => {
                let builder = builder.with_smart_turn(Arc::clone(model));
                match &self.turn_offload {
                    Some(offload) => builder.with_smart_turn_offload(offload.clone()),
                    None => builder,
                }
            }
            _ => builder,
        };
        let (builder, telephony) = match tools {
            // A call with telephony state only for its agent process has no tools of
            // its own: the builder keeps none.
            Some(tools) if tools.registry.is_empty() => (builder, tools.telephony),
            Some(tools) => (
                builder.with_tools(tools.registry, tools.engine),
                tools.telephony,
            ),
            None => (builder, None),
        };
        // Lines the bot speaks on its own account (a voicemail message, the apology after a
        // failed warm transfer) come in through the engine's say input.
        // The engine takes a single line input and an agent link owns it: with a link up, the
        // lines are forwarded into the link's input instead (below).
        let say_receiver = telephony
            .as_ref()
            .and_then(CallTelephony::take_say_receiver);
        // The bridge lives as long as the call: dropping it stops its tasks.
        let (builder, _bridge) = match rtvi {
            Some((sender, inbox)) => {
                let (builder, bridge) = RtviBridge::attach(
                    builder,
                    sender,
                    inbox,
                    self.config.rtvi,
                    None,
                    ctx.clock.clone(),
                    &ctx.cancel,
                );
                (builder, Some(bridge))
            }
            None => (builder, None),
        };
        // The recording before the ring is created below: its transcript reads the
        // same ring as the RTVI bridge and the telephony tools (one ring, several subscribers).
        let (builder, recording) = match &self.recording {
            Some(recording) => {
                let info = CallInfo {
                    session: ctx.id.get(),
                    agent: self.agent.name(),
                };
                let (builder, call_recording) = recording.attach(builder, info);
                (builder, Some(call_recording))
            }
            None => (builder, None),
        };
        // The telephony tools follow the bot's speech through the call's engine events (the
        // RTVI bridge's when it set them: one ring, two subscribers).
        let (builder, events) = match &telephony {
            Some(_) => {
                let events = builder
                    .events()
                    .cloned()
                    .unwrap_or_else(|| EngineEvents::new(TELEPHONY_EVENT_CAPACITY));
                let stream = events.subscribe();
                (builder.with_events(events), Some(stream))
            }
            None => (builder, None),
        };
        // The agent link last, so it shares the events of the RTVI bridge and of the
        // telephony tools.
        let (builder, link) = match self.pick_agent_link(&ctx, &started).await {
            Some((config, start_info)) => {
                // The carrier's call control of this call (a `transfer` of the agent goes
                // through it); none for a call without a carrier binding.
                let ops = match &telephony {
                    Some(telephony) => telephony.link_ops(LINK_START_WAIT).await,
                    None => None,
                };
                // Who is on the line: only for an agent process that opted in, and without the
                // numbers while the server redacts phone numbers.
                let identity = if config.caller_identity {
                    start_info.as_ref().and_then(|info| {
                        crate::caller_identity::call_identity(info, self.redact_caller_numbers)
                    })
                } else {
                    None
                };
                let call = LinkCall {
                    session_id: ctx.id.get(),
                    agent: self.agent.name().to_owned(),
                    clock: ctx.clock.clone(),
                    cancel: ctx.cancel.clone(),
                    ops,
                    identity,
                };
                brisvox_agent_link::attach(&config, builder, call).await
            }
            None => (builder, None),
        };
        let (builder, _say_forward) = match (say_receiver, &link) {
            (Some(receiver), Some(link)) => (
                builder,
                Some(TaskGuard::spawn(
                    "telephony_say_forward",
                    forward_lines(receiver, link.say_sender()),
                )),
            ),
            (Some(receiver), None) => (builder.with_say_input(receiver), None),
            (None, _) => (builder, None),
        };
        // From here on, however this future ends (returns, is dropped, panics or leaves
        // through `?`), the guard finishes the last turn and folds the call's counters in.
        let mut guard = CallGuard::new(observer, timeline, |call: &Call| Some(call.stats()));
        let started = run_call(session, builder, &ctx.cancel)
            .await
            .map_err(|e| Error::Call(format!("start: {e}")))?;
        let call = guard.attach(started);
        let ended = match (&telephony, events) {
            (Some(telephony), Some(events)) => {
                telephony_call(&ctx, call, telephony, events, link.as_ref()).await
            }
            _ => wait_call(&ctx, call, link.as_ref()).await,
        };
        if let Some(telephony) = &telephony {
            // A warm transfer nobody finished must not leave the caller in a conference alone.
            telephony.finish(ctx.id.get()).await;
        }
        if let Some(link) = link {
            link.close("call_ended").await;
        }
        drop(guard);
        // The pipeline is gone: the recording drains what is queued and finishes its files
        // (bounded, and it never changes how the call ended).
        if let Some(recording) = recording {
            recording.finish().await;
        }
        ended
    }
}

/// Moves the lines of the telephony tools into the agent link's line input, one at a time and
/// without waiting: a line the (bounded) input cannot take is dropped and logged. Ends when
/// the telephony state drops its sender or the call drops the guard of the task.
async fn forward_lines(mut receiver: SayReceiver, sender: SaySender) {
    while let Some(line) = receiver.recv().await {
        if let Err(error) = sender.try_send(line) {
            tracing::warn!(target: "brisvox::server", %error, "a telephony line was not queued for the engine");
        }
    }
}

/// How long a call waits for its start message before it links to an agent process: the route
/// of the call and the carrier binding for `transfer` come from it. A carrier sends it at once.
const LINK_START_WAIT: Duration = Duration::from_secs(1);

/// The call's start message, waiting at most [`LINK_START_WAIT`] for it; `None` when the wait runs
/// out (a client that never sends one, such as a generic websocket peer that stays silent) or the
/// call is cancelled first. The call then links with what it has (the default agent process, and
/// no caller identity).
async fn wait_for_start<'a>(
    cancel: &tokio_util::sync::CancellationToken,
    clock: &SharedClock,
    started: &'a CallStarted,
) -> Option<&'a brisvox_transport::CallInfo> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        info = started.wait() => Some(info),
        () = clock.sleep(LINK_START_WAIT) => None,
    }
}

/// The metadata key (generic websocket start message) naming the agent route of a call.
pub const ROUTE_KEY: &str = "agent_route";

/// The same as a Twilio stream parameter (`<Parameter name="agent_route" .../>`).
const ROUTE_KEY_TWILIO: &str = "custom.agent_route";

/// Engine events a call with telephony tools buffers for them (only the bot's speech matters
/// to them; a lost event costs at most a settle or farewell wait, both bounded).
const TELEPHONY_EVENT_CAPACITY: usize = 256;

/// Waits for `call` to end, or stops it when the session is cancelled or the agent process
/// ended it.
async fn wait_call(ctx: &SessionContext, call: &mut Call, link: Option<&AgentLink>) -> Result<()> {
    tokio::select! {
        biased;
        () = AgentLink::ended_opt(link) => {
            stop_after_agent_end(ctx, call).await;
            Ok(())
        }
        () = ctx.cancel.cancelled() => {
            stop_cancelled(ctx, call).await;
            Ok(())
        }
        ended = call.wait() => ended.map_err(|e| Error::Call(format!("pipeline: {e}"))),
    }
}

/// [`wait_call`] for a call with the telephony tools: also follows the bot's speech, ends the
/// call after the farewell once `end_call` asked for it, and carries out what the call's
/// callbacks and voicemail handling decide ([`CallAction`], module docs of
/// `crate::tools::telephony`).
async fn telephony_call(
    ctx: &SessionContext,
    call: &mut Call,
    telephony: &CallTelephony,
    mut events: EventStream,
    link: Option<&AgentLink>,
) -> Result<()> {
    let mut events_open = true;
    // Once `end_call` ran: the bot speeches counted then, and the latest hang-up time.
    let mut farewell: Option<(u64, Instant)> = None;
    // Whether the farewell is the voicemail message (its handling is told once it was played).
    let mut voicemail_message = false;
    // The call enters the hub when it started, so the callback listener can reach it.
    let register = telephony.register();
    tokio::pin!(register);
    let mut registered = false;
    loop {
        tokio::select! {
            biased;
            () = AgentLink::ended_opt(link) => {
                stop_after_agent_end(ctx, call).await;
                return Ok(());
            }
            () = ctx.cancel.cancelled() => {
                stop_cancelled(ctx, call).await;
                return Ok(());
            }
            ended = call.wait() => {
                return ended.map_err(|e| Error::Call(format!("pipeline: {e}")));
            }
            () = &mut register, if !registered => registered = true,
            event = events.recv(), if events_open => match event {
                Some(event) => telephony.observe(&event),
                None => events_open = false,
            },
            action = telephony.next_action() => match action {
                CallAction::HandOff => {
                    // The caller is on someone else's line: stop the bot, leave the call alone.
                    tracing::debug!(target: "brisvox::server", session = %ctx.id, "the call was handed off: stopping the bot");
                    if let Err(error) = call.stop().await {
                        tracing::debug!(target: "brisvox::server", session = %ctx.id, %error, "call stop after a hand-off");
                    }
                    return Ok(());
                }
                CallAction::HangUp => {
                    farewell = Some((telephony.speech_stops(), ctx.clock.now()));
                }
                CallAction::Say { text, then_hang_up } => {
                    let stops = telephony.speech_stops();
                    let spoken = telephony.say(&text);
                    if !spoken {
                        tracing::warn!(target: "brisvox::server", session = %ctx.id, "the engine takes no lines: the bot cannot speak");
                    }
                    if then_hang_up {
                        // A line that cannot be spoken ends the call at once.
                        let wait = if spoken { VOICEMAIL_PLAY_TIMEOUT } else { Duration::ZERO };
                        farewell = Some((stops, ctx.clock.now() + wait));
                        voicemail_message = spoken;
                    }
                }
            },
            () = telephony.end_requested(), if farewell.is_none() => {
                let deadline = ctx.clock.now() + FAREWELL_TIMEOUT;
                farewell = Some((telephony.speech_stops(), deadline));
            }
            () = sleep_until(&ctx.clock, farewell.map(|(_, deadline)| deadline)) => {}
        }
        if let Some((stops, deadline)) = farewell
            && (telephony.farewell_played(stops) || ctx.clock.now() >= deadline)
        {
            tracing::debug!(target: "brisvox::server", session = %ctx.id, "hanging up after the farewell");
            if voicemail_message {
                telephony.message_played();
            }
            telephony.hang_up(ctx.id.get(), &ctx.cancel).await;
            if let Err(error) = call.stop().await {
                tracing::debug!(target: "brisvox::server", session = %ctx.id, %error, "call stop after end_call");
            }
            return Ok(());
        }
    }
}

/// The agent process ended the call (after its farewell): a graceful stop.
async fn stop_after_agent_end(ctx: &SessionContext, call: &mut Call) {
    if let Err(error) = call.stop().await {
        tracing::debug!(target: "brisvox::server", session = %ctx.id, %error, "call stop after the agent's end");
    }
}

/// Sleeps until `deadline` on `clock`; never resolves without one.
async fn sleep_until(clock: &SharedClock, deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => clock.sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Drain timeout or shutdown: a graceful stop, bounded by `timeouts.stop`. The manager counts
/// the session as cancelled whatever the stop returns.
async fn stop_cancelled(ctx: &SessionContext, call: &mut Call) {
    if let Err(error) = call.stop().await {
        tracing::debug!(target: "brisvox::server", session = %ctx.id, %error, "call stop after cancel");
    }
}

impl<F: ProviderFactory> SessionRunner for EngineRunner<F> {
    type Request = AcceptedCall;
    type Output = ();
    type Error = Error;

    async fn run(self: Arc<Self>, ctx: SessionContext, request: AcceptedCall) -> Result<()> {
        self.drive(ctx, request).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn the_wait_for_the_start_message_times_out_cleanly() {
        use tokio_util::sync::CancellationToken;

        let clock = SystemClock::shared();
        let cancel = CancellationToken::new();
        // A start message that never comes: the wait ends at its bound, not before, not after.
        let silent = CallStarted::default();
        let begun = clock.now();
        assert!(wait_for_start(&cancel, &clock, &silent).await.is_none());
        assert_eq!((clock.now() - begun), LINK_START_WAIT);
        assert!(silent.get().is_none());
        // One that is already there is returned at once.
        let there = CallStarted::from_info(brisvox_transport::CallInfo {
            call_id: "c".to_owned(),
            metadata: std::collections::BTreeMap::new(),
        });
        let begun = clock.now();
        assert_eq!(
            wait_for_start(&cancel, &clock, &there)
                .await
                .map(|i| i.call_id.as_str()),
            Some("c")
        );
        assert_eq!((clock.now() - begun), Duration::ZERO);
        // A cancelled call stops waiting at once.
        cancel.cancel();
        let begun = clock.now();
        assert!(wait_for_start(&cancel, &clock, &silent).await.is_none());
        assert_eq!((clock.now() - begun), Duration::ZERO);
    }

    #[test]
    fn the_smart_turn_executor_threads_come_from_the_environment() {
        let default = brisvox_engine::offload::default_smart_turn_threads(2);
        assert_eq!(smart_turn_threads(&env(&[]), 2).unwrap(), default);
        let set = |value: &str| smart_turn_threads(&env(&[(ENV_SMART_TURN_THREADS, value)]), 2);
        assert_eq!(set("0").unwrap(), 0, "0 shares the VAD's pool");
        assert_eq!(set("3").unwrap(), 3);
        assert_eq!(
            set(&MAX_SMART_TURN_THREADS.to_string()).unwrap(),
            MAX_SMART_TURN_THREADS
        );
        for bad in ["-1", "many", "65", "1.5"] {
            let error = set(bad).unwrap_err().to_string();
            assert!(error.contains(ENV_SMART_TURN_THREADS), "{bad}: {error}");
        }
    }

    #[test]
    fn oversubscribed_inference_threads_are_reported() {
        // The defaults (half the cores each) fit, from two cores up.
        for cores in [2, 3, 4, 8, 16, 64] {
            let pool = (cores / 2usize).max(1);
            let turn = brisvox_engine::offload::default_smart_turn_threads(pool);
            assert_eq!(
                inference_thread_oversubscription(pool, turn, cores),
                None,
                "{cores} cores with the defaults"
            );
        }
        assert_eq!(
            inference_thread_oversubscription(4, 0, 1),
            None,
            "no executor"
        );
        // More executor threads than the cores leave.
        assert_eq!(
            inference_thread_oversubscription(2, 3, 4),
            Some(5.max(3 * SMART_TURN_INTRA_THREADS))
        );
        assert!(inference_thread_oversubscription(1, 8, 4).is_some());
        // The executor's intra-op workers alone can exceed the cores.
        assert_eq!(
            inference_thread_oversubscription(1, 4, 6).is_some(),
            SMART_TURN_INTRA_THREADS * 4 > 6
        );
        assert_eq!(
            inference_thread_oversubscription(usize::MAX, usize::MAX, 4),
            Some(usize::MAX),
            "no overflow"
        );
    }

    #[test]
    fn the_rtvi_bridge_settings_come_from_the_environment() {
        let defaults = RunnerConfig::from_env(&env(&[])).unwrap();
        assert_eq!(defaults.rtvi, RtviBridgeConfig::default());
        let config = RunnerConfig::from_env(&env(&[
            (ENV_RTVI_BOT_SPEAKING_EVENTS, "false"),
            (ENV_RTVI_METRICS, "off"),
            (ENV_RTVI_REPLY_TIMEOUT, "2s"),
            (ENV_RTVI_EVENT_CAPACITY, "128"),
            (ENV_RTVI_TEXT_CAPACITY, "4"),
        ]))
        .unwrap();
        assert_eq!(
            config.rtvi,
            RtviBridgeConfig {
                bot_speaking_events: false,
                metrics: false,
                reply_timeout: Duration::from_secs(2),
                event_capacity: 128,
                text_capacity: 4,
            }
        );
        for (name, value) in [
            (ENV_RTVI_METRICS, "maybe"),
            (ENV_RTVI_REPLY_TIMEOUT, "0s"),
            (ENV_RTVI_EVENT_CAPACITY, "0"),
            (ENV_RTVI_TEXT_CAPACITY, "100000"),
        ] {
            let e = RunnerConfig::from_env(&env(&[(name, value)])).unwrap_err();
            assert!(e.to_string().contains(name), "{name}: {e}");
        }
    }
}
