//! The tools of the server's calls (`docs/TOOLS.md` "In the server binary").
//!
//! An agent definition lists its tools by name (`agent.tools`, `docs/CONFIG.md`); the server
//! decides which implementations exist. Three sources, all registered once at startup into one
//! [`ToolSetup`]:
//!
//! - **Application tools**, through the plug-in point [`AppTools`]: an application that embeds
//!   the server (its own `main` over this library) registers its tools there, and the
//!   [`EngineRunner`](crate::EngineRunner) offers them to every call whose definition names them.
//! - **Demo tools** (cargo feature `demo-tools`, off by default): canned `lookup_order` and
//!   `book_appointment`, so the example agents (`examples/agents`) run end to end without an
//!   application. Their answers are fixed and labelled as demo data; never ship them.
//! - **Telephony tools** (`transfer_call`, `send_dtmf`, `end_call`, `docs/TELEPHONY.md`; cargo
//!   feature `call-control`): only when a carrier's call control is configured
//!   (`BRISVOX_CALL_CONTROL`, `docs/OPERATIONS.md`). They act on the call through the carrier's
//!   REST API; their destinations come from the definition's `[telephony]` section.
//!
//! # Fail closed
//! A definition that names a tool none of the sources provides is refused at startup with the
//! tool's position in the file and what would provide it: a call without its tools would
//! answer wrongly instead of failing.
//!
//! # Per call
//! Each call gets its own [`ToolEngine`] (the idempotency store is per call) over the agent's
//! tools only: the LLM is offered exactly what the definition lists. Application and demo tools
//! are shared by every call (their registry is built once, so a call pays no schema compile).
//! The telephony tools hold per-call state (one transfer per call, the call's carrier id), so a
//! call that uses them registers them on its own registry, on the inference pool
//! ([`Offload::run`]): compiling their schemas is CPU work that does not belong on a tokio
//! worker.

#[cfg(feature = "demo-tools")]
pub mod demo;
#[cfg(feature = "call-control")]
pub mod telephony;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use brisvox_agent::tools::{
    EngineConfig as ToolEngineConfig, RegistryError, SessionId as ToolSessionId, ToolEngine,
    ToolRegistry,
};
use brisvox_core::SharedClock;
use brisvox_engine::Offload;
use brisvox_transport::CallStarted;

use crate::config::{AgentConfig, Env};
use crate::hosting::StreamLinks;
use crate::{Error, Result};

/// The names of the built-in telephony tools.
pub const TELEPHONY_TOOLS: [&str; 3] = ["transfer_call", "send_dtmf", "end_call"];

/// The names of the demo tools (feature `demo-tools`).
pub const DEMO_TOOLS: [&str; 2] = ["lookup_order", "book_appointment"];

/// Environment variable: the carriers whose call control the telephony tools use, comma
/// separated (`twilio`, `telnyx`), or `off` (the default). Needs the `call-control` feature.
pub const ENV_CALL_CONTROL: &str = "BRISVOX_CALL_CONTROL";
/// Environment variable: the Twilio account SID (`AC...`), required with Twilio call control
/// (its Auth Token is `TWILIO_AUTH_TOKEN`, the listener's).
pub const ENV_TWILIO_ACCOUNT_SID: &str = "BRISVOX_TWILIO_ACCOUNT_SID";
/// Environment variable: the E.164 caller id of Twilio transfers (optional).
pub const ENV_TWILIO_CALLER_ID: &str = "BRISVOX_TWILIO_CALLER_ID";
/// Environment variable: the Telnyx API key (v2), required with Telnyx call control.
pub const ENV_TELNYX_API_KEY: &str = "TELNYX_API_KEY";
/// Environment variable: the E.164 caller id of Telnyx transfers (optional).
pub const ENV_TELNYX_CALLER_ID: &str = "BRISVOX_TELNYX_CALLER_ID";
/// Environment variable: where the callback listener binds (see `callbacks`; feature
/// `call-control`). Declared here so a build without the feature can refuse it.
#[cfg(not(feature = "call-control"))]
const ENV_CALLBACK_ADDR: &str = "BRISVOX_CALLBACK_ADDR";
/// Environment variable: the Twilio REST API base URL, for a regional endpoint or a test double
/// (default `https://api.twilio.com/2010-04-01`; anything but https needs a loopback host).
pub const ENV_TWILIO_API_BASE: &str = "BRISVOX_TWILIO_API_BASE";
/// Environment variable: the Telnyx REST API base URL, for a test double (default
/// `https://api.telnyx.com/v2`; anything but https needs a loopback host).
pub const ENV_TELNYX_API_BASE: &str = "BRISVOX_TELNYX_API_BASE";
/// Environment variable: the Twilio `waitUrl` of the conference a warm transfer's caller waits
/// in (hold music; optional).
pub const ENV_TWILIO_HOLD_URL: &str = "BRISVOX_TWILIO_HOLD_URL";
/// Environment variable: where Twilio posts the status events of a warm transfer's agent leg
/// (`StatusCallback`; optional). The application behind it calls the callback listener.
pub const ENV_TWILIO_STATUS_CALLBACK: &str = "BRISVOX_TWILIO_STATUS_CALLBACK";
/// Environment variable: the Telnyx Call Control application (connection) id of the warm
/// transfer's agent leg. Warm transfers on Telnyx need it.
pub const ENV_TELNYX_CONNECTION_ID: &str = "BRISVOX_TELNYX_CONNECTION_ID";
/// Environment variable: the audio URL a warm transfer's caller hears while waiting on Telnyx
/// (optional; without it the caller hears silence).
pub const ENV_TELNYX_HOLD_URL: &str = "BRISVOX_TELNYX_HOLD_URL";

/// Longest wait for the farewell after `end_call` before the call is hung up anyway.
pub const FAREWELL_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest wait for the voicemail message to be played before the call is hung up anyway (a
/// message is at most `MAX_VOICEMAIL_CHARS` characters: about a minute of speech).
pub const VOICEMAIL_PLAY_TIMEOUT: Duration = Duration::from_secs(90);

/// Something the runner of a call must do on behalf of the call's telephony: the
/// callbacks, the voicemail handling and the transfer tools decide, the runner executes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(feature = "call-control"), allow(dead_code))] // only telephony builds make them
pub(crate) enum CallAction {
    /// The caller left the bot (a completed transfer): stop the session, without hanging the
    /// call up.
    HandOff,
    /// Make the bot speak `text` now (cutting what it says); with `then_hang_up`, end the call
    /// once it was played.
    Say {
        /// What the bot says, verbatim.
        text: Arc<str>,
        /// End the call after the line was played.
        then_hang_up: bool,
    },
    /// End the call now through the carrier.
    HangUp,
}

/// How long a call's telephony tool registration may wait for the inference pool.
#[cfg(feature = "call-control")]
const REGISTER_TIMEOUT: Duration = Duration::from_secs(2);

/// The plug-in point for an application's own tools: registered once, at startup, into the
/// registry every call's tools come from (see the [module docs](self)).
///
/// The tools are shared by every call of the process, so a tool keeps no per-call state of its
/// own (the [`ToolCallCtx`](brisvox_agent::tools::ToolCallCtx) it gets names the session and
/// carries the idempotency key). The rules of [`Tool`](brisvox_agent::tools::Tool) hold: no
/// blocking, every IO await bounded, SideEffect tools idempotent per key.
pub trait AppTools: Send + Sync {
    /// Registers the application's tools.
    ///
    /// # Errors
    /// The [`RegistryError`] of a tool that cannot be registered (a bad name or schema, a name
    /// taken).
    fn register(
        &self,
        registry: &mut ToolRegistry,
        clock: &SharedClock,
    ) -> std::result::Result<(), RegistryError>;
}

/// Every tool the server can offer (see the [module docs](self)): build it at startup, then
/// give it to [`EngineRunner::with_tools`](crate::EngineRunner::with_tools).
pub struct ToolSetup {
    /// Application and demo tools, shared by every call.
    app: ToolRegistry,
    /// The settings of each call's [`ToolEngine`].
    engine: ToolEngineConfig,
    /// The names of the MCP tools in `app` ([`Self::with_mcp`]): every agent that has MCP
    /// servers offers them without listing them in `agent.tools`.
    mcp_names: Vec<String>,
    /// The connections to the MCP servers behind those tools.
    #[cfg(feature = "mcp")]
    mcp: Option<Arc<crate::mcp::McpHost>>,
    /// The carriers' call control, when configured.
    #[cfg(feature = "call-control")]
    call_control: Option<telephony::CallControls>,
    /// The callback listener's settings (`BRISVOX_CALLBACK_ADDR`), when configured.
    #[cfg(feature = "call-control")]
    callbacks: Option<crate::callbacks::CallbackSettings>,
}

impl fmt::Debug for ToolSetup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("ToolSetup");
        d.field(
            "app",
            &self.app.specs().map(|s| &*s.name).collect::<Vec<_>>(),
        );
        #[cfg(feature = "call-control")]
        d.field("call_control", &self.call_control);
        d.finish_non_exhaustive()
    }
}

impl Default for ToolSetup {
    fn default() -> Self {
        Self::none()
    }
}

impl ToolSetup {
    /// No tools: a definition that names one is refused.
    pub fn none() -> Self {
        Self {
            app: ToolRegistry::new(),
            engine: ToolEngineConfig::default(),
            mcp_names: Vec::new(),
            #[cfg(feature = "mcp")]
            mcp: None,
            #[cfg(feature = "call-control")]
            call_control: None,
            #[cfg(feature = "call-control")]
            callbacks: None,
        }
    }

    /// Connects to the MCP servers `agent` names (`[[mcp.servers]]`, `docs/TOOLS.md` "MCP
    /// tools") and registers their allowlisted tools: they are offered to the agent's calls
    /// like any tool of this setup. Does nothing for a definition without MCP servers.
    ///
    /// Call it once at startup, inside the runtime; the connections are shared by every call.
    /// Close them with `ToolSetup::mcp_host` and `McpHost::shutdown` when the server stops.
    ///
    /// # Errors
    /// [`Error::Config`] with the position in the file of a server that cannot be reached, a
    /// tool it does not offer, or a name another tool has; and when this build lacks the `mcp`
    /// feature.
    #[cfg_attr(not(feature = "mcp"), allow(clippy::unused_async))]
    pub async fn with_mcp(
        #[cfg_attr(not(feature = "mcp"), allow(unused_mut))] mut self,
        agent: &AgentConfig,
        clock: &SharedClock,
    ) -> Result<Self> {
        if agent.mcp().is_empty() {
            return Ok(self);
        }
        #[cfg(feature = "mcp")]
        {
            let host = crate::mcp::McpHost::connect(agent, clock, &mut self.app).await?;
            self.mcp_names = agent.mcp().exposed_names().map(str::to_owned).collect();
            self.mcp = Some(Arc::new(host));
            Ok(self)
        }
        #[cfg(not(feature = "mcp"))]
        {
            let _ = clock;
            Err(Error::Config(format!(
                "{}: this binary is built without MCP support (cargo feature `mcp` of \
                 brisvox-server)",
                agent
                    .mcp()
                    .servers
                    .first()
                    .map_or("mcp", |s| s.key.as_str())
            )))
        }
    }

    /// The connections to the MCP servers ([`Self::with_mcp`]); close them with
    /// `McpHost::shutdown` when the server stops.
    #[cfg(feature = "mcp")]
    pub fn mcp_host(&self) -> Option<Arc<crate::mcp::McpHost>> {
        self.mcp.clone()
    }

    /// A copy of this setup without its MCP tools and connections: the application, demo and
    /// telephony tools are the same instances (and the same call hub), shared with `self`. A
    /// reload builds each new agent snapshot on it ([`Self::with_mcp`] then connects the new
    /// definition's MCP servers), so the old snapshot's connections are not touched.
    #[must_use]
    pub fn without_mcp(&self) -> Self {
        Self {
            app: self
                .app
                .filtered(|spec| !self.mcp_names.iter().any(|n| **n == *spec.name)),
            engine: self.engine,
            mcp_names: Vec::new(),
            #[cfg(feature = "mcp")]
            mcp: None,
            #[cfg(feature = "call-control")]
            call_control: self.call_control.clone(),
            #[cfg(feature = "call-control")]
            callbacks: self.callbacks.clone(),
        }
    }

    /// Adds an application's tools ([`AppTools`]).
    ///
    /// # Errors
    /// [`Error::Config`] when a tool cannot be registered (its name taken by another tool).
    pub fn with_app(mut self, tools: &dyn AppTools, clock: &SharedClock) -> Result<Self> {
        tools
            .register(&mut self.app, clock)
            .map_err(|e| Error::Config(format!("application tools: {e}")))?;
        Ok(self)
    }

    /// Adds the demo tools (`lookup_order`, `book_appointment`; see [`demo`]).
    ///
    /// # Errors
    /// [`Error::Config`] when a name is already taken.
    #[cfg(feature = "demo-tools")]
    pub fn with_demo_tools(self, clock: &SharedClock) -> Result<Self> {
        self.with_app(&demo::DemoTools, clock)
    }

    /// Offers the telephony tools over `controls` (an empty set offers none).
    #[cfg(feature = "call-control")]
    #[must_use]
    pub fn with_call_control(mut self, controls: telephony::CallControls) -> Self {
        self.call_control = (!controls.is_empty()).then_some(controls);
        self
    }

    /// The registry of live calls the callback listener reaches (feature `call-control`; `None`
    /// without a carrier's call control).
    #[cfg(feature = "call-control")]
    pub fn call_hub(&self) -> Option<telephony::CallHub> {
        self.call_control.as_ref().map(telephony::CallControls::hub)
    }

    /// The callback listener's settings, when `BRISVOX_CALLBACK_ADDR` is set (start it with
    /// [`CallbackServer::bind`](crate::callbacks::CallbackServer::bind) over
    /// [`Self::call_hub`]).
    #[cfg(feature = "call-control")]
    pub const fn callbacks(&self) -> Option<&crate::callbacks::CallbackSettings> {
        self.callbacks.as_ref()
    }

    /// Sets the callback listener's settings. Warm transfers are offered only with them (the
    /// listener finishes a transfer).
    #[cfg(feature = "call-control")]
    #[must_use]
    pub fn with_callbacks(mut self, settings: crate::callbacks::CallbackSettings) -> Self {
        self.call_control = self
            .call_control
            .take()
            .map(|controls| controls.with_callbacks(true));
        self.callbacks = Some(settings);
        self
    }

    /// Sets the settings of every call's [`ToolEngine`] (default: the engine's defaults).
    #[must_use]
    pub fn with_engine_config(mut self, config: ToolEngineConfig) -> Self {
        self.engine = config;
        self
    }

    /// The application (and demo) tools.
    pub const fn app(&self) -> &ToolRegistry {
        &self.app
    }

    /// The setup the binary builds from its environment: the demo tools when the
    /// `demo-tools` feature is on, the carriers' call control `BRISVOX_CALL_CONTROL` names
    /// (feature `call-control`), and `app`'s tools when given.
    ///
    /// # Errors
    /// [`Error::Config`] naming the variable when a carrier is unknown, its credentials are
    /// missing, or this build lacks the `call-control` feature; or when tools collide.
    pub fn from_env(
        env: &dyn Env,
        clock: &SharedClock,
        app: Option<&dyn AppTools>,
    ) -> Result<Self> {
        Self::from_env_linked(env, clock, app, &StreamLinks::default())
    }

    /// [`Self::from_env`] for a server that also hosts the carriers' media listeners: `links`
    /// ([`HostSettings::stream_links`](crate::HostSettings::stream_links)) connects the
    /// carriers' call control to them, so DTMF over Twilio and the stream stop of a Telnyx
    /// transfer work. Bind the listeners with the same settings afterwards.
    ///
    /// # Errors
    /// As [`Self::from_env`], and [`Error::Config`] when `BRISVOX_CALLBACK_ADDR` is set without
    /// a carrier's call control.
    pub fn from_env_linked(
        env: &dyn Env,
        clock: &SharedClock,
        app: Option<&dyn AppTools>,
        links: &StreamLinks,
    ) -> Result<Self> {
        let carriers = call_control_carriers(env)?;
        #[cfg(feature = "demo-tools")]
        let setup = Self::none().with_demo_tools(clock)?;
        #[cfg(not(feature = "demo-tools"))]
        let setup = Self::none();
        let setup = match app {
            Some(app) => setup.with_app(app, clock)?,
            None => setup,
        };
        #[cfg(feature = "call-control")]
        let setup = {
            let setup = setup.with_call_control(telephony::CallControls::from_env(
                env, &carriers, clock, links,
            )?);
            match crate::callbacks::CallbackSettings::from_env(env)? {
                Some(callbacks) if setup.call_control.is_some() => setup.with_callbacks(callbacks),
                Some(_) => {
                    return Err(Error::Config(format!(
                        "{}: the callback listener needs a carrier's call control ({ENV_CALL_CONTROL})",
                        crate::callbacks::ENV_CALLBACK_ADDR
                    )));
                }
                None => setup,
            }
        };
        #[cfg(not(feature = "call-control"))]
        {
            let _ = links;
            if !carriers.is_empty() {
                return Err(Error::Config(format!(
                    "{ENV_CALL_CONTROL} is set, but this binary is built without the `call-control` feature"
                )));
            }
            if crate::settings::var(env, ENV_CALLBACK_ADDR)?
                .is_some_and(|v| !v.eq_ignore_ascii_case("off"))
            {
                return Err(Error::Config(format!(
                    "{ENV_CALLBACK_ADDR} is set, but this binary is built without the `call-control` feature"
                )));
            }
        }
        Ok(setup)
    }
}

/// A carrier whose call control the telephony tools may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Carrier {
    /// Twilio (REST, `docs/providers/call-control.md`).
    Twilio,
    /// Telnyx Call Control.
    Telnyx,
}

/// `BRISVOX_CALL_CONTROL` as a list of carriers (empty: off).
pub(crate) fn call_control_carriers(env: &dyn Env) -> Result<Vec<Carrier>> {
    let Some(value) = crate::settings::var(env, ENV_CALL_CONTROL)? else {
        return Ok(Vec::new());
    };
    if value.eq_ignore_ascii_case("off") {
        return Ok(Vec::new());
    }
    let mut carriers = Vec::new();
    for name in value.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        let carrier = match name.to_ascii_lowercase().as_str() {
            "twilio" => Carrier::Twilio,
            "telnyx" => Carrier::Telnyx,
            _ => {
                return Err(Error::Config(format!(
                    "{ENV_CALL_CONTROL}: unknown carrier {name:?} (twilio, telnyx or off)"
                )));
            }
        };
        if !carriers.contains(&carrier) {
            carriers.push(carrier);
        }
    }
    Ok(carriers)
}

/// The tools of one agent's calls: the [`ToolSetup`] bound to a definition, checked at startup.
pub(crate) struct AgentTools {
    /// The agent's application and demo tools (already filtered to `agent.tools`).
    app: Arc<ToolRegistry>,
    /// The settings of each call's engine.
    engine: ToolEngineConfig,
    /// The connections to the agent's MCP servers: kept for as long as the agent's calls may
    /// use their tools.
    #[cfg(feature = "mcp")]
    _mcp: Option<Arc<crate::mcp::McpHost>>,
    /// The carriers' call control for this agent, when the server has one configured.
    #[cfg(feature = "call-control")]
    telephony: Option<telephony::AgentTelephony>,
    /// The agent names telephony tools (or has voicemail handling): every call gets the
    /// telephony state and tools.
    #[cfg(feature = "call-control")]
    telephony_tools: bool,
}

impl fmt::Debug for AgentTools {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("AgentTools");
        d.field(
            "app",
            &self.app.specs().map(|s| &*s.name).collect::<Vec<_>>(),
        );
        #[cfg(feature = "call-control")]
        d.field("telephony", &self.telephony.is_some());
        d.finish_non_exhaustive()
    }
}

/// One call's tools, for [`SessionBuilder::with_tools`](brisvox_engine::session::SessionBuilder::with_tools).
pub(crate) struct CallTools {
    /// The agent's tools.
    pub(crate) registry: Arc<ToolRegistry>,
    /// The call's tool engine over them.
    pub(crate) engine: Arc<ToolEngine>,
    /// The call's telephony state, when the agent uses the telephony tools.
    pub(crate) telephony: Option<CallTelephony>,
}

#[cfg(feature = "call-control")]
pub(crate) use telephony::CallTelephony;

/// A call's telephony state; none exists in a build without the `call-control` feature.
#[cfg(not(feature = "call-control"))]
pub(crate) enum CallTelephony {}

#[cfg(not(feature = "call-control"))]
impl CallTelephony {
    pub(crate) fn observe(&self, _event: &brisvox_engine::EngineEvent) {
        match *self {}
    }

    pub(crate) async fn end_requested(&self) {
        match *self {}
    }

    pub(crate) fn speech_stops(&self) -> u64 {
        match *self {}
    }

    pub(crate) fn farewell_played(&self, _stops: u64) -> bool {
        match *self {}
    }

    pub(crate) async fn hang_up(
        &self,
        _idempotency: u64,
        _cancel: &tokio_util::sync::CancellationToken,
    ) {
        match *self {}
    }

    pub(crate) fn take_say_receiver(&self) -> Option<brisvox_engine::SayReceiver> {
        match *self {}
    }

    pub(crate) async fn link_ops(
        &self,
        _wait: std::time::Duration,
    ) -> Option<Arc<dyn brisvox_agent::telephony::TelephonyOps>> {
        match *self {}
    }

    pub(crate) fn say(&self, _text: &Arc<str>) -> bool {
        match *self {}
    }

    pub(crate) async fn register(&self) {
        match *self {}
    }

    pub(crate) async fn next_action(&self) -> CallAction {
        match *self {}
    }

    pub(crate) fn message_played(&self) {
        match *self {}
    }

    pub(crate) async fn finish(&self, _idempotency: u64) {
        match *self {}
    }
}

impl AgentTools {
    /// Binds `setup` to `agent`: every tool the definition names must exist.
    ///
    /// # Errors
    /// [`Error::Config`] naming each missing tool with its position, and what would provide
    /// it.
    pub(crate) fn bind(setup: ToolSetup, agent: &AgentConfig, clock: &SharedClock) -> Result<Self> {
        // Every tool the server offers, telephony included (over a probe call), to check the
        // definition against.
        #[cfg_attr(not(feature = "call-control"), allow(unused_mut))]
        let mut offered = setup.app.filtered(|_| true);
        #[cfg(feature = "call-control")]
        let telephony = match &setup.call_control {
            Some(controls) => {
                let agent_telephony = telephony::AgentTelephony::new(controls.clone(), agent);
                agent_telephony
                    .register_probe(&mut offered, clock)
                    .map_err(|e| Error::Config(format!("telephony tools: {e}")))?;
                Some(agent_telephony)
            }
            None => None,
        };
        #[cfg(not(feature = "call-control"))]
        let _ = clock;
        agent
            .check_tools(&offered)
            .map_err(|e| Error::Config(format!("{e}\n{}", missing_tool_hint(&offered))))?;
        // The agent's MCP tools: each of its `[[mcp.servers]]` tools must have been connected
        // (`ToolSetup::with_mcp`); they are offered without being listed in `agent.tools`.
        for server in &agent.mcp().servers {
            for tool in &server.tools {
                if !setup.mcp_names.contains(&tool.exposed) {
                    return Err(agent
                        .error_at(
                            &tool.key,
                            tool.span.clone(),
                            format!(
                                "the MCP tool `{}` of server `{}` is not connected: call \
                                 `ToolSetup::with_mcp` with this definition before starting",
                                tool.exposed, server.name
                            ),
                        )
                        .into());
                }
            }
        }
        let names: Vec<&str> = agent.tool_names().collect();
        // Only this agent's own MCP tools: a setup connected for another agent may hold more.
        let own_mcp: Vec<&str> = agent.mcp().exposed_names().collect();
        let app = setup
            .app
            .filtered(|spec| names.contains(&&*spec.name) || own_mcp.contains(&&*spec.name));
        #[cfg(feature = "call-control")]
        let telephony_tools = telephony.as_ref().is_some_and(|t| {
            names.iter().any(|n| TELEPHONY_TOOLS.contains(n)) || t.has_voicemail()
        });
        Ok(Self {
            app: Arc::new(app),
            engine: setup.engine,
            #[cfg(feature = "mcp")]
            _mcp: if own_mcp.is_empty() { None } else { setup.mcp },
            #[cfg(feature = "call-control")]
            telephony,
            #[cfg(feature = "call-control")]
            telephony_tools,
        })
    }

    /// The connections to the agent's MCP servers, when it has any.
    #[cfg(feature = "mcp")]
    pub(crate) fn mcp_host(&self) -> Option<Arc<crate::mcp::McpHost>> {
        self._mcp.clone()
    }

    /// Whether the agent has no tools.
    pub(crate) fn is_empty(&self) -> bool {
        #[cfg(feature = "call-control")]
        let telephony = self.telephony_tools;
        #[cfg(not(feature = "call-control"))]
        let telephony = false;
        self.app.is_empty() && !telephony
    }

    /// The tools of one call (`None` when the agent has none). `started` tells the telephony
    /// tools which call they act on once it started. With `agent_link` (the call has an
    /// out-of-process agent) a call of a server with call control also gets its
    /// telephony state when the definition names no telephony tool, so the agent's `transfer`
    /// reaches the carrier; the tools it offers are still the definition's.
    ///
    /// # Errors
    /// [`Error::Call`] when the call's telephony tools cannot be registered (the pool is
    /// saturated or shut down).
    #[cfg_attr(
        not(feature = "call-control"),
        allow(unused_variables, clippy::unused_async)
    )]
    pub(crate) async fn for_call(
        &self,
        session: u64,
        clock: &SharedClock,
        offload: &Offload,
        started: CallStarted,
        agent_link: bool,
    ) -> Result<Option<CallTools>> {
        #[cfg(feature = "call-control")]
        let link_telephony = agent_link && self.telephony.is_some();
        #[cfg(not(feature = "call-control"))]
        let link_telephony = false;
        if self.is_empty() && !link_telephony {
            return Ok(None);
        }
        #[cfg(feature = "call-control")]
        let (registry, telephony) = match &self.telephony {
            Some(agent_telephony) if self.telephony_tools || link_telephony => {
                let call = agent_telephony.call(started, clock.clone());
                let registry = agent_telephony
                    .registry_for_call(&self.app, &call, clock, offload, REGISTER_TIMEOUT)
                    .await?;
                (registry, Some(call))
            }
            _ => (Arc::clone(&self.app), None),
        };
        #[cfg(not(feature = "call-control"))]
        let (registry, telephony) = (Arc::clone(&self.app), None);
        let engine = Arc::new(ToolEngine::new(
            Arc::clone(&registry),
            ToolSessionId::new(session),
            clock.clone(),
            self.engine,
        ));
        Ok(Some(CallTools {
            registry,
            engine,
            telephony,
        }))
    }
}

/// What provides the tools a definition may name, for the error of a missing one.
fn missing_tool_hint(offered: &ToolRegistry) -> String {
    let names: Vec<&str> = offered.specs().map(|s| &*s.name).collect();
    let offered = if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    };
    format!(
        "tools this server offers: {offered}. Application tools plug in through \
         brisvox_server::tools::AppTools; {} are demo tools (cargo feature `demo-tools`); {} \
         need a carrier's call control ({ENV_CALL_CONTROL}, cargo feature `call-control`)",
        DEMO_TOOLS.join(" and "),
        TELEPHONY_TOOLS.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn call_control_is_off_unless_named() {
        assert!(call_control_carriers(&env(&[])).unwrap().is_empty());
        assert!(
            call_control_carriers(&env(&[(ENV_CALL_CONTROL, "off")]))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            call_control_carriers(&env(&[(ENV_CALL_CONTROL, "Twilio, telnyx,twilio")])).unwrap(),
            [Carrier::Twilio, Carrier::Telnyx]
        );
        let err = call_control_carriers(&env(&[(ENV_CALL_CONTROL, "vonage")])).unwrap_err();
        assert!(err.to_string().contains("vonage"), "{err}");
    }

    #[cfg(not(feature = "call-control"))]
    #[test]
    fn call_control_without_the_feature_is_refused() {
        let clock = brisvox_core::SystemClock::shared();
        let err =
            ToolSetup::from_env(&env(&[(ENV_CALL_CONTROL, "twilio")]), &clock, None).unwrap_err();
        assert!(err.to_string().contains("`call-control` feature"), "{err}");
        let err = ToolSetup::from_env(
            &env(&[("BRISVOX_CALLBACK_ADDR", "127.0.0.1:1")]),
            &clock,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("BRISVOX_CALLBACK_ADDR"), "{err}");
        // Off is fine.
        ToolSetup::from_env(&env(&[("BRISVOX_CALLBACK_ADDR", "off")]), &clock, None).unwrap();
    }

    #[cfg(feature = "call-control")]
    #[test]
    fn the_callback_listener_needs_a_carrier_and_a_secret() {
        let clock = brisvox_core::SystemClock::shared();
        let addr = ("BRISVOX_CALLBACK_ADDR", "127.0.0.1:0");
        let token = ("BRISVOX_CALLBACK_TOKEN", "0123456789abcdef");
        // No carrier: nothing for the listener to reach.
        let err = ToolSetup::from_env(&env(&[addr, token]), &clock, None).unwrap_err();
        assert!(err.to_string().contains(ENV_CALL_CONTROL), "{err}");
        // No secret.
        let carrier = [(ENV_CALL_CONTROL, "telnyx"), (ENV_TELNYX_API_KEY, "k")];
        let mut pairs = carrier.to_vec();
        pairs.push(addr);
        let err = ToolSetup::from_env(&env(&pairs), &clock, None).unwrap_err();
        assert!(err.to_string().contains("BRISVOX_CALLBACK_TOKEN"), "{err}");
        // Both: the listener's settings and the hub are there.
        pairs.push(token);
        let setup = ToolSetup::from_env(&env(&pairs), &clock, None).unwrap();
        assert!(setup.callbacks().is_some());
        assert!(setup.call_hub().is_some());
        // Without the listener variables there are neither settings nor a listener.
        let setup = ToolSetup::from_env(&env(&carrier), &clock, None).unwrap();
        assert!(setup.callbacks().is_none());
        assert!(setup.call_hub().is_some(), "the hub exists with a carrier");
    }
}
