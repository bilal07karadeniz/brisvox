//! The `brisvox-server` program as a library function, so an application with its own Rust tools
//! does not copy the binary's `main`.
//!
//! [`run`] is the whole server binary: telemetry, settings from the environment, the agent
//! definition with its providers, tools and Smart Turn model, the hosted transports, the health
//! listener, and the graceful drain on `SIGTERM`/`SIGINT` (Ctrl-C/Ctrl-Break on Windows). The
//! exit status says how the drain ended (`docs/OPERATIONS.md`). [`run_with_tools`] is the same
//! with the application's own tools ([`AppTools`]) registered next to the built-in ones.
//!
//! Both read the command line the way `brisvox-server` does: no arguments starts the server;
//! `check <agent-file>` validates an agent file offline ([`crate::check`]); `--help` and
//! `--version` print and exit. With [`run_with_tools`], `check` also knows the application's
//! tool names.
//!
//! # An embedding binary
//! ```no_run
//! use std::sync::Arc;
//!
//! use brisvox_agent::tools::{
//!     RegistryError, Tool, ToolCallCtx, ToolFuture, ToolKind, ToolOutput, ToolRegistry, ToolSpec,
//! };
//! use brisvox_core::SharedClock;
//! use brisvox_server::AppTools;
//! use serde_json::{Value, json};
//!
//! /// `ping`: answers `pong`.
//! struct Ping;
//!
//! impl Tool for Ping {
//!     fn call<'a>(&'a self, _args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
//!         Box::pin(async { Ok(ToolOutput::Json(json!({"answer": "pong"}))) })
//!     }
//! }
//!
//! /// Everything this application adds to the server.
//! struct MyTools;
//!
//! impl AppTools for MyTools {
//!     fn register(
//!         &self,
//!         registry: &mut ToolRegistry,
//!         _clock: &SharedClock,
//!     ) -> Result<(), RegistryError> {
//!         registry.register(
//!             ToolSpec::new(
//!                 "ping",
//!                 "Answers pong. Use it to test the connection.",
//!                 json!({"type": "object", "properties": {}, "additionalProperties": false}),
//!                 ToolKind::ReadOnly,
//!             ),
//!             Ping,
//!         )
//!     }
//! }
//!
//! fn main() -> std::process::ExitCode {
//!     // Name `ping` in the agent file's `agent.tools`; the settings are the same environment
//!     // variables as the stock binary's (`docs/OPERATIONS.md`).
//!     brisvox_server::app::run_with_tools(Arc::new(MyTools))
//! }
//! ```
//!
//! The binary's global allocator (`mimalloc`, feature of this crate) is set in the binary, not
//! here: an embedding binary that wants it declares its own `#[global_allocator]`.
//! `examples/rust-tools` is a complete, runnable project.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use crate::AppTools;
use crate::admission::agent_providers;
use crate::check::{self, CheckOptions};
use crate::config::{self, AgentConfig, ProcessEnv};
use crate::fdlimit::{self, NofileOutcome};
use crate::privacy::Sweeper;
use crate::resilient::resilience_from_env;
use crate::runner::load_smart_turn;
use crate::telemetry::{self, LogFormat};
use crate::{
    AdmissionPolicy, AdmissionSettings, ConfiguredProviders, EXIT_STARTUP_FAILURE, EngineRunner,
    HostSettings, LoadProbe, Observability, ObservabilitySettings, Recording, RecordingSettings,
    RunnerConfig, Server, ServerSettings, ShutdownSignals, ToolSetup, Vad, shutdown_runtime,
};
use brisvox_core::{SharedClock, SystemClock};
use brisvox_engine::Offload;
use brisvox_engine::offload::start_smart_turn_executor;
use brisvox_inference::smart_turn::TurnModel;
use brisvox_inference::{InferencePool, PoolConfig};
use tokio_util::sync::CancellationToken;

/// Runs the stock `brisvox-server` program (the command line, then the server): what the binary's
/// `main` returns. The tools are the built-in ones (the demo tools with the `demo-tools` feature,
/// the telephony tools with `call-control`); an application with tools of its own calls
/// [`run_with_tools`].
///
/// Blocks until the server has drained; the exit status is documented in `docs/OPERATIONS.md`.
pub fn run() -> ExitCode {
    run_inner(None)
}

/// [`run`] with the application's own tools registered next to the built-in ones: the agent file
/// names them in `agent.tools`. The start fails, like any tool registration, when a tool name is
/// taken.
pub fn run_with_tools(tools: Arc<dyn AppTools>) -> ExitCode {
    run_inner(Some(tools))
}

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    /// No arguments: run the server.
    Serve,
    /// `check <agent-file> | --env [--require-secrets]`.
    Check {
        /// Which definition to check.
        source: CheckSource,
        /// A missing secret is an error, not a warning.
        require_secrets: bool,
    },
    /// `--help` / `-h`.
    Help,
    /// `--version` / `-V`.
    Version,
    /// `--print-schema`: the JSON Schema of the agent file (cargo feature `schema`).
    PrintSchema,
}

/// The definition `check` reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CheckSource {
    /// The agent file named on the command line.
    File(PathBuf),
    /// What the server would start with: `BRISVOX_AGENT_DEFINITION` (with
    /// `BRISVOX_AGENT_FORMAT`), or the file `BRISVOX_AGENT_FILE` names.
    Environment,
}

/// The usage text (`--help`, and after a command line error).
pub(crate) const USAGE: &str = "\
brisvox-server: real-time voice agent server

USAGE:
    brisvox-server                  run the server (settings come from the environment, the
                                    agent from BRISVOX_AGENT_FILE or BRISVOX_AGENT_DEFINITION)
    brisvox-server check <agent-file> [--require-secrets]
                                    validate an agent file offline: no sockets, no network
    brisvox-server check --env [--require-secrets]
                                    the same for the agent the environment names:
                                    BRISVOX_AGENT_DEFINITION (text; BRISVOX_AGENT_FORMAT=yaml
                                    for YAML) or BRISVOX_AGENT_FILE
    brisvox-server --print-schema   print the JSON Schema of the agent file (for editors;
                                    needs a build with the `schema` cargo feature; the copy in
                                    docs/agent.schema.json)
    brisvox-server --version
    brisvox-server --help

`check` exits 0 when the file is valid and 1 when it has errors. A secret (api_key_env) that is
not set is a warning; with --require-secrets it is an error.

Agent file format: docs/CONFIG.md. Settings, health endpoints, exit codes: docs/OPERATIONS.md.
";

/// Reads the command line (without the program name).
///
/// # Errors
/// A message naming the problem: an unknown command or flag, a missing or extra file argument.
pub(crate) fn parse_args(args: &[OsString]) -> Result<Command, String> {
    let mut args = args.iter();
    let Some(first) = args.next() else {
        return Ok(Command::Serve);
    };
    match first.to_str() {
        Some("--help" | "-h" | "help") => Ok(Command::Help),
        Some("--version" | "-V") => Ok(Command::Version),
        Some("--print-schema") => match args.next() {
            None => Ok(Command::PrintSchema),
            Some(_) => Err("--print-schema takes no arguments".to_owned()),
        },
        Some("check") => {
            let mut path = None;
            let mut from_env = false;
            let mut require_secrets = false;
            for arg in args {
                match arg.to_str() {
                    Some("--require-secrets") => require_secrets = true,
                    Some("--env") => from_env = true,
                    Some("--help" | "-h") => return Ok(Command::Help),
                    Some(flag) if flag.starts_with('-') && flag.len() > 1 => {
                        return Err(format!("check: unknown option `{flag}`"));
                    }
                    _ if path.is_some() => {
                        return Err("check: expected one agent file".to_owned());
                    }
                    _ => path = Some(PathBuf::from(arg)),
                }
            }
            match (path, from_env) {
                (Some(_), true) => Err("check: give an agent file or --env, not both".to_owned()),
                (Some(path), false) => Ok(Command::Check {
                    source: CheckSource::File(path),
                    require_secrets,
                }),
                (None, true) => Ok(Command::Check {
                    source: CheckSource::Environment,
                    require_secrets,
                }),
                (None, false) => Err(
                    "check: missing the agent file (brisvox-server check <file>, or --env for the \
                     agent in BRISVOX_AGENT_DEFINITION / BRISVOX_AGENT_FILE)"
                        .to_owned(),
                ),
            }
        }
        _ => Err(format!("unknown argument `{}`", first.to_string_lossy())),
    }
}

/// The command line, then the command.
fn run_inner(tools: Option<Arc<dyn AppTools>>) -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match parse_args(&args) {
        Ok(Command::Serve) => serve(tools),
        Ok(Command::Help) => {
            write_out(&mut std::io::stdout(), USAGE);
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            write_out(
                &mut std::io::stdout(),
                &format!(
                    "brisvox-server {}
",
                    crate::VERSION
                ),
            );
            ExitCode::SUCCESS
        }
        Ok(Command::PrintSchema) => print_schema(),
        Ok(Command::Check {
            source,
            require_secrets,
        }) => {
            let options = CheckOptions { require_secrets };
            let report = match source {
                CheckSource::File(path) => {
                    check::check_file(&path, &ProcessEnv, options, tools.as_deref())
                }
                CheckSource::Environment => {
                    check::check_environment(&ProcessEnv, options, tools.as_deref())
                }
            };
            write_out(&mut std::io::stdout(), &report.render());
            if report.is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(EXIT_STARTUP_FAILURE)
            }
        }
        Err(message) => {
            write_out(
                &mut std::io::stderr(),
                &format!("brisvox-server: {message}\n\n{USAGE}"),
            );
            ExitCode::from(2)
        }
    }
}

/// `--print-schema`: the agent file's JSON Schema on stdout.
#[cfg(feature = "schema")]
fn print_schema() -> ExitCode {
    write_out(
        &mut std::io::stdout(),
        &crate::config::json_schema::agent_schema_json(),
    );
    ExitCode::SUCCESS
}

/// `--print-schema` in a build without the `schema` feature: an error that names it.
#[cfg(not(feature = "schema"))]
fn print_schema() -> ExitCode {
    write_out(
        &mut std::io::stderr(),
        "brisvox-server: --print-schema needs a build with the `schema` cargo feature \
         (cargo build -p brisvox-server --features schema); the committed copy is \
         docs/agent.schema.json\n",
    );
    ExitCode::from(2)
}

/// Writes `text` and ignores a failure: unlike `print!`, a closed pipe
/// (`brisvox-server check f | head`) must not panic.
fn write_out(out: &mut dyn std::io::Write, text: &str) {
    let _ = out.write_all(text.as_bytes()).and_then(|()| out.flush());
}

/// The server: everything `main` was before the command line existed.
fn serve(tools: Option<Arc<dyn AppTools>>) -> ExitCode {
    let format = match telemetry::env_value("BRISVOX_LOG_FORMAT")
        .and_then(|v| v.map_or(Ok(LogFormat::default()), |v| v.parse()))
    {
        Ok(format) => format,
        Err(e) => return startup_failure(&e),
    };
    if let Err(e) = telemetry::init(format) {
        return startup_failure(&e);
    }
    // One socket per call: the soft open-file limit (often 1024 in a container) would cap the
    // server far below its CPU. Raised to the hard limit before anything opens a file; a
    // limit that cannot be raised is a warning, never a failed start.
    match fdlimit::raise_nofile_soft_limit() {
        outcome @ NofileOutcome::Failed { .. } => tracing::warn!(%outcome, "open-file limit"),
        outcome => tracing::info!(%outcome, "open-file limit"),
    }
    let settings = match ServerSettings::from_env(&ProcessEnv) {
        Ok(settings) => settings,
        Err(e) => return ExitCode::from(failed(&e)),
    };
    let admission = match AdmissionSettings::from_env(&ProcessEnv) {
        Ok(admission) => admission,
        Err(e) => return ExitCode::from(failed(&e)),
    };
    let host = match HostSettings::from_env(&ProcessEnv) {
        Ok(host) => host,
        Err(e) => return ExitCode::from(failed(&e)),
    };
    let runner_config = match RunnerConfig::from_env(&ProcessEnv) {
        Ok(config) => config,
        Err(e) => return ExitCode::from(failed(&e)),
    };
    let observability = match ObservabilitySettings::from_env(&ProcessEnv) {
        Ok(settings) => settings,
        Err(e) => return ExitCode::from(failed(&e)),
    };
    let resilience = match resilience_from_env(&ProcessEnv) {
        Ok(config) => config,
        Err(e) => return ExitCode::from(failed(&e)),
    };
    // Call recording: off unless `BRISVOX_RECORDING_DIR` is set.
    let recording = match RecordingSettings::from_env(&ProcessEnv) {
        Ok(settings) => settings,
        Err(e) => return ExitCode::from(failed(&e)),
    };
    // File reads and the models' loads are blocking: done here, before the runtime.
    let agent = match host.load_agent(&ProcessEnv) {
        None => None,
        Some(Ok(agent)) => Some(agent),
        Some(Err(e)) => return ExitCode::from(failed(&e)),
    };
    // The agent file's `[recording]` table overrides the environment.
    let recording = match crate::privacy::resolve_settings(
        recording,
        &agent
            .as_ref()
            .map(AgentConfig::recording)
            .unwrap_or_default(),
    ) {
        Ok(settings) => settings,
        Err(e) => return ExitCode::from(failed(&e)),
    };
    let vad = match agent.as_ref().map(|_| Vad::production()).transpose() {
        Ok(vad) => vad.unwrap_or_default(),
        Err(e) => return ExitCode::from(failed(&e)),
    };
    let smart_turn = match agent
        .as_ref()
        .map(|agent| load_smart_turn(&ProcessEnv, agent))
        .transpose()
    {
        Ok(model) => model.flatten(),
        Err(e) => return ExitCode::from(failed(&e)),
    };
    // The generic websocket listener's own agent, if `BRISVOX_WS_AGENT_FILE` names one: its
    // Smart Turn model is the main agent's when that has one, else its own.
    let websocket_agent = match host.load_websocket_agent(&ProcessEnv).transpose() {
        Ok(agent) => agent,
        Err(e) => return ExitCode::from(failed(&e)),
    };
    // One model file serves the process (`BRISVOX_SMART_TURN_MODEL`): an adaptive websocket
    // agent shares the main agent's model, and loads it only when the main agent has none.
    let websocket_smart_turn = match websocket_agent.as_ref() {
        Some(ws)
            if matches!(
                ws.engine().turn,
                brisvox_agent::turn::TurnPolicy::Adaptive { .. }
            ) =>
        {
            match smart_turn.clone() {
                Some(model) => Some(model),
                None => match load_smart_turn(&ProcessEnv, ws) {
                    Ok(model) => model,
                    Err(e) => return ExitCode::from(failed(&e)),
                },
            }
        }
        _ => None,
    };
    if let Some(ws) = &websocket_agent
        && ws.recording() != crate::privacy::RecordingFileSettings::default()
    {
        tracing::warn!(
            agent = ws.name(),
            "the websocket agent's [recording] table is ignored: recording is set by the main agent and the environment"
        );
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("brisvox-server")
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => return ExitCode::from(failed(&e)),
    };
    tracing::info!(
        version = crate::VERSION,
        ?settings,
        "brisvox-server starting"
    );
    let code = runtime.block_on(async {
        // Signals first: one that arrives while the listener binds is not lost.
        let mut signals = match ShutdownSignals::install() {
            Ok(signals) => signals,
            Err(e) => return failed(&e),
        };
        let clock = SystemClock::shared();
        // Stops the TTS adapters' warm connections once the server has drained.
        let providers_shutdown = CancellationToken::new();
        let _stop_providers = providers_shutdown.clone().drop_guard();
        // Metrics, turn traces; the OTLP exporter stops with the providers' token.
        let obs = match observability.start(&clock, &providers_shutdown) {
            Ok(obs) => obs,
            Err(e) => return failed(&e),
        };
        let (server, _pool, sweeper) = match agent {
            Some(agent) => {
                let parts = Parts {
                    agent,
                    websocket_agent,
                    websocket_smart_turn,
                    vad,
                    smart_turn,
                    runner: runner_config,
                    admission,
                    resilience,
                    obs: Arc::clone(&obs),
                    recording,
                };
                match start_hosting(
                    settings,
                    &host,
                    parts,
                    tools.as_deref(),
                    &clock,
                    &providers_shutdown,
                )
                .await
                {
                    Ok(started) => started,
                    Err(e) => return failed(&e),
                }
            }
            None => match Server::start(settings, clock.clone()).await {
                Ok(server) => (server, Vec::new(), None),
                Err(e) => return failed(&e),
            },
        };
        server.manager().attach_observability(obs);
        tracing::info!(
            health = ?server.health_addr(),
            listeners = ?server.listener_addrs(),
            "brisvox-server ready"
        );
        #[cfg(feature = "call-control")]
        if let Some(addr) = server.callback_addr() {
            tracing::info!(callbacks = %addr, "callback listener ready");
        }
        let report = server.run(&mut signals).await;
        // The retention sweeper stops with the providers' token; wait for it so a panic of
        // its task is reported (and its counters are final).
        providers_shutdown.cancel();
        if let Some(sweeper) = sweeper {
            let stats = sweeper.stats();
            sweeper.join(&*clock, Duration::from_secs(5)).await;
            tracing::info!(
                sweeps = stats.sweeps(),
                removed = stats.removed(),
                failed = stats.failed(),
                panics = stats.panics(),
                "retention sweeper stopped"
            );
        }
        report.outcome().exit_code()
    });
    tracing::info!(exit_code = code, "brisvox-server exiting");
    // Not a plain drop: dropping the runtime waits without limit for every `spawn_blocking`
    // closure, and an aborted session can leave one running (inference, an offload pool
    // shutdown). The exit status must survive that, inside the documented bound.
    shutdown_runtime(runtime);
    ExitCode::from(code)
}

/// What the calls run, loaded before the runtime.
struct Parts {
    agent: AgentConfig,
    /// The generic websocket listener's own agent (`BRISVOX_WS_AGENT_FILE`).
    websocket_agent: Option<AgentConfig>,
    websocket_smart_turn: Option<Arc<dyn TurnModel>>,
    vad: Vad,
    smart_turn: Option<Arc<dyn TurnModel>>,
    runner: RunnerConfig,
    admission: AdmissionSettings,
    resilience: brisvox_providers::resilience::ResilienceConfig,
    obs: Arc<Observability>,
    recording: Option<RecordingSettings>,
}

/// Builds the agent's providers, tools and runner on the shared inference pool and starts the
/// server with its listeners. The pools are returned so they outlive the server (dropping one does
/// not join its threads).
async fn start_hosting(
    settings: ServerSettings,
    host: &HostSettings,
    parts: Parts,
    app_tools: Option<&dyn AppTools>,
    clock: &SharedClock,
    providers_shutdown: &CancellationToken,
) -> crate::Result<(Server, Vec<InferencePool>, Option<Sweeper>)> {
    let Parts {
        agent,
        websocket_agent,
        websocket_smart_turn,
        vad,
        smart_turn,
        runner,
        mut admission,
        resilience,
        obs,
        recording,
    } = parts;
    // Each line of the queue must hold the jobs of every session at once: a call has at most one
    // VAD batch outstanding (high line) and a Smart Turn analysis or two (normal line), and the
    // calls' 20 ms ticks bunch up on a busy runtime (a rejected VAD batch loses the call's VAD
    // stream). The default of 1024 covers 512 sessions; more sessions get a bigger queue.
    let defaults = PoolConfig::default();
    let pool_config = PoolConfig {
        queue_capacity: defaults
            .queue_capacity
            .max(settings.manager.max_sessions.saturating_mul(2)),
        ..defaults
    };
    let pool = InferencePool::new(&pool_config, clock.clone())
        .map_err(|e| crate::Error::Config(format!("cannot start the inference pool: {e}")))?;
    // The Smart Turn analyses (20-25 ms each) run on threads of their own, so they never hold a
    // thread another call's VAD window is waiting for.
    // Read (and validated) only for an agent that has a model to run.
    let turn_threads = if smart_turn.is_some() || websocket_smart_turn.is_some() {
        crate::runner::smart_turn_threads(&ProcessEnv, pool_config.threads)?
    } else {
        0
    };
    let turn_pool = start_smart_turn_executor(turn_threads, clock.clone())
        .map_err(|e| crate::Error::Config(format!("cannot start the Smart Turn executor: {e}")))?;
    if smart_turn.is_some() {
        tracing::info!(
            vad_pool_threads = pool_config.threads,
            smart_turn_threads = turn_threads,
            "inference threads (smart_turn_threads 0: the analyses share the pool)"
        );
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        if let Some(wanted) = crate::runner::inference_thread_oversubscription(
            pool_config.threads,
            turn_threads,
            cores,
        ) {
            tracing::warn!(
                vad_pool_threads = pool_config.threads,
                smart_turn_threads = turn_threads,
                wanted_threads = wanted,
                cores,
                "the inference pool and the Smart Turn executor (with its ONNX Runtime threads) want more threads than there are cores; lower BRISVOX_SMART_TURN_THREADS (docs/OPERATIONS.md)"
            );
        }
    }
    // The executor's queue and refusals are exported as `brisvox_smart_turn_*`; the admission
    // control watches it too (below).
    if let Some(turn_pool) = &turn_pool {
        obs.set_smart_turn_pool(turn_pool.handle());
    }
    // Admission control: shed on pool pressure, and hold a permit of each limited
    // provider the agent runs on for the life of a call.
    // With a websocket agent, the limits cover the providers of both agents, and every call
    // holds a permit of each (admission happens before the listener's agent is known).
    let mut used = agent_providers(&agent);
    if let Some(ws) = &websocket_agent {
        for name in agent_providers(ws) {
            if !used.contains(&name) {
                used.push(name);
            }
        }
    }
    for name in admission.restrict_to(&used) {
        tracing::warn!(
            provider = %name,
            "BRISVOX_PROVIDER_LIMITS names a provider the agent does not use; the limit is ignored"
        );
    }
    let probe: Arc<dyn LoadProbe> = Arc::new(pool.handle());
    let mut policy = AdmissionPolicy::new(&admission, Some(probe));
    // A late Smart Turn analysis delays the end of the caller's turn: its executor is a pool
    // the calls depend on as much as the VAD's.
    if let Some(turn_pool) = &turn_pool {
        let turn_probe: Arc<dyn LoadProbe> = Arc::new(turn_pool.handle());
        policy = policy.with_secondary_probe(turn_probe);
    }
    let providers =
        ConfiguredProviders::from_agent_with(&agent, clock, providers_shutdown, resilience)?;
    // `/metrics` reads the breakers and the process retry budget live.
    obs.set_resilience_source(providers.resilient().resilience_handle());
    // The stock binary has no application tools; an application embedding the server passes its
    // `AppTools` through `run_with_tools` (docs/TOOLS.md).
    let tools = ToolSetup::from_env_linked(&ProcessEnv, clock, app_tools, &host.stream_links())?;
    // The callback listener serves the live calls of the telephony tools.
    #[cfg(feature = "call-control")]
    let callbacks = tools.callbacks().cloned().zip(tools.call_hub());
    tracing::info!(
        agent = agent.name(),
        ?providers,
        ?vad,
        ?tools,
        smart_turn = smart_turn.is_some(),
        "agent loaded"
    );
    let adaptive = matches!(
        agent.engine().turn,
        brisvox_agent::turn::TurnPolicy::Adaptive { .. }
    );
    if adaptive && smart_turn.is_none() {
        tracing::warn!(
            agent = agent.name(),
            "adaptive turns without a Smart Turn model end at their fallback deadline (build with the `smart-turn-ort` feature)"
        );
    }
    // The out-of-process agents: `BRISVOX_AGENT_URL` and friends, refined by
    // the definition's `[agent_link]` section and its named routes.
    let links =
        crate::AgentLinks::resolve(&agent, |name| config::Env::var(&ProcessEnv, name).ok())?;
    if let Some(link) = &links.default {
        tracing::info!(
            url = %brisvox_agent_link::security::redact_url(&link.url),
            "calls link to the agent process"
        );
    }
    for (name, link) in &links.routes {
        tracing::info!(
            route = %name,
            url = %brisvox_agent_link::security::redact_url(&link.url),
            "calls may link to a named agent process"
        );
    }
    // The generic websocket listener's own agent: its own providers and agent links, the same
    // inference pool, Smart Turn executor, observability and recording (one admission control
    // for both). No application or telephony tools: those are bound to the main agent.
    let websocket_runner = match websocket_agent {
        None => None,
        Some(ws_agent) => {
            let ws_providers = ConfiguredProviders::from_agent_with(
                &ws_agent,
                clock,
                providers_shutdown,
                resilience,
            )?;
            let ws_links = crate::AgentLinks::resolve(&ws_agent, |name| {
                config::Env::var(&ProcessEnv, name).ok()
            })?;
            tracing::info!(
                agent = ws_agent.name(),
                providers = ?ws_providers,
                smart_turn = websocket_smart_turn.is_some(),
                "the generic websocket listener runs its own agent (BRISVOX_WS_AGENT_FILE)"
            );
            let mut ws = EngineRunner::with_tools(
                ws_agent,
                ws_providers,
                Offload::Pool(pool.handle()),
                vad.clone(),
                runner,
                crate::ToolSetup::none(),
                clock,
            )?;
            if let Some(model) = websocket_smart_turn {
                ws = ws.with_smart_turn(model);
                if let Some(turn_pool) = &turn_pool {
                    ws = ws.with_smart_turn_offload(Offload::Pool(turn_pool.handle()));
                }
            }
            let ws = ws.with_observability(Arc::clone(&obs));
            Some((ws, ws_links))
        }
    };
    let mut runner = EngineRunner::with_tools(
        agent,
        providers,
        Offload::Pool(pool.handle()),
        vad,
        runner,
        tools,
        clock,
    )?;
    if let Some(model) = smart_turn {
        runner = runner.with_smart_turn(model);
        if let Some(turn_pool) = &turn_pool {
            runner = runner.with_smart_turn_offload(Offload::Pool(turn_pool.handle()));
        }
    }
    let runner = runner.with_observability(obs);
    // Phone numbers the server redacts (recording or logs) are not given to an agent process
    // that asked for the caller identity either.
    let redact_numbers = crate::caller_identity::phone_numbers_redacted(
        recording.as_ref(),
        &crate::telemetry::log_redactor(&ProcessEnv)?,
    );
    let runner = runner.with_redact_caller_numbers(redact_numbers);
    let websocket_runner =
        websocket_runner.map(|(ws, links)| (ws.with_redact_caller_numbers(redact_numbers), links));
    // Recording and retention.
    let mut sweeper = None;
    let recording = match recording {
        Some(settings) => {
            let recording = Recording::new(settings, clock.clone())?;
            recording.prepare().await?;
            let settings = recording.settings();
            tracing::info!(
                dir = %settings.dir.display(),
                transcripts = settings.transcripts,
                audio = settings.audio,
                redact_pii = settings.redact_pii,
                retention = ?settings.retention,
                encrypted = settings.age_recipients.is_some(),
                "call recording is on"
            );
            if settings.audio {
                tracing::warn!(
                    "audio recording is on: audio is not redacted (BRISVOX_RECORDING_REDACT_PII applies to transcripts only); see docs/PRIVACY.md"
                );
            }
            if settings.retention.is_none() {
                tracing::warn!("recordings are kept forever (BRISVOX_RECORDING_RETENTION=forever)");
            }
            // The sweeper stops with the providers' token, once the server has drained.
            sweeper = recording.spawn_sweeper(providers_shutdown);
            Some(recording)
        }
        None => None,
    };
    let (runner, websocket_runner) = match recording {
        Some(recording) => (
            runner.with_recording(Arc::clone(&recording)),
            websocket_runner.map(|(ws, links)| (ws.with_recording(recording), links)),
        ),
        None => (runner, websocket_runner),
    };
    // The out-of-process agent: `BRISVOX_AGENT_URL` and friends.
    let runner = Arc::new(if links.is_empty() {
        runner
    } else {
        runner.with_agent_links(links)
    });
    let websocket_runner = websocket_runner.map(|(ws, links)| {
        Arc::new(if links.is_empty() {
            ws
        } else {
            ws.with_agent_links(links)
        })
    });
    // The WebRTC listener's Opus codec runs on the inference pool, not on tokio workers.
    #[cfg(feature = "webrtc")]
    let host = &host.clone().with_codec_pool(pool.handle());
    #[allow(unused_mut)] // only the `call-control` build attaches the callback listener
    let mut server = Server::start_hosting_routed(
        settings,
        policy,
        host,
        clock.clone(),
        &runner,
        websocket_runner.as_ref(),
    )
    .await?;
    #[cfg(feature = "call-control")]
    if let Some((settings, hub)) = callbacks {
        let listener = crate::callbacks::CallbackServer::bind(settings, hub, clock.clone()).await?;
        server.attach_callbacks(listener);
    }
    // Both pools outlive the server: dropping one closes its queue.
    Ok((
        server,
        std::iter::once(pool).chain(turn_pool).collect(),
        sweeper,
    ))
}

/// A failure before logging is set up: reported on stderr.
fn startup_failure(e: &dyn std::fmt::Display) -> ExitCode {
    write_out(&mut std::io::stderr(), &format!("brisvox-server: {e}\n"));
    ExitCode::from(EXIT_STARTUP_FAILURE)
}

/// A failure after logging is set up: reported through the log (which writes to stderr).
fn failed(e: &dyn std::fmt::Display) -> u8 {
    tracing::error!(error = %e, "brisvox-server failed to start");
    EXIT_STARTUP_FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(&args.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn no_arguments_run_the_server() {
        assert_eq!(parse(&[]), Ok(Command::Serve));
    }

    #[test]
    fn help_and_version() {
        for flag in ["--help", "-h", "help"] {
            assert_eq!(parse(&[flag]), Ok(Command::Help), "{flag}");
        }
        for flag in ["--version", "-V"] {
            assert_eq!(parse(&[flag]), Ok(Command::Version), "{flag}");
        }
        assert_eq!(parse(&["check", "--help"]), Ok(Command::Help));
    }

    #[test]
    fn print_schema_takes_no_arguments() {
        assert_eq!(parse(&["--print-schema"]), Ok(Command::PrintSchema));
        assert!(parse(&["--print-schema", "x"]).is_err());
    }

    #[test]
    fn check_takes_one_file_and_the_flag_in_either_order() {
        let expected = Ok(Command::Check {
            source: CheckSource::File(PathBuf::from("agent.toml")),
            require_secrets: true,
        });
        assert_eq!(
            parse(&["check", "agent.toml", "--require-secrets"]),
            expected
        );
        assert_eq!(
            parse(&["check", "--require-secrets", "agent.toml"]),
            expected
        );
        assert_eq!(
            parse(&["check", "agent.yaml"]),
            Ok(Command::Check {
                source: CheckSource::File(PathBuf::from("agent.yaml")),
                require_secrets: false
            })
        );
    }

    #[test]
    fn check_env_reads_the_environment_and_excludes_a_file() {
        let expected = Ok(Command::Check {
            source: CheckSource::Environment,
            require_secrets: true,
        });
        assert_eq!(parse(&["check", "--env", "--require-secrets"]), expected);
        assert_eq!(parse(&["check", "--require-secrets", "--env"]), expected);
        assert!(parse(&["check", "--env", "agent.toml"]).is_err());
        assert!(parse(&["check", "agent.toml", "--env"]).is_err());
        let hint = parse(&["check"]).unwrap_err();
        assert!(hint.contains("--env"), "{hint}");
    }

    #[test]
    fn mistakes_are_errors_not_a_silent_server_start() {
        for bad in [
            &["chek", "a.toml"][..],
            &["--nope"],
            &["check"],
            &["check", "a.toml", "b.toml"],
            &["check", "a.toml", "--strict"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_usage_names_the_docs() {
        assert!(USAGE.contains("docs/CONFIG.md") && USAGE.contains("docs/OPERATIONS.md"));
    }
}
