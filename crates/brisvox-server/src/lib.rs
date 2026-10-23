//! Multi-tenant brisvox server: session manager, admission control, graceful drain, config
//! loading, health endpoints, the hosted transports and the engine runner behind them.

pub mod admission;
pub mod agent_links;
pub mod app;
#[cfg(feature = "call-control")]
pub mod callbacks;
pub mod caller_identity;
pub mod check;
pub mod config;
pub mod error;
pub mod fdlimit;
pub mod health;
pub mod hosting;
pub mod lifecycle;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod observability;
pub mod privacy;
pub mod providers;
pub mod resilient;
pub mod runner;
pub mod session;
pub mod settings;
pub mod summary;
pub mod telemetry;
pub mod tools;

pub use admission::{AdmissionPolicy, AdmissionSettings, LoadProbe, LoadSample, OverloadConfig};
pub use agent_links::AgentLinks;
pub use config::{AgentConfig, ConfigError};
pub use error::{Error, Result};
pub use health::{HealthConfig, HealthServer, Readiness, ReadyStatus};
pub use hosting::{HostSettings, ListenerAddrs, Listeners, StreamLinks};
pub use lifecycle::{
    DrainOutcome, DrainReport, EXIT_STARTUP_FAILURE, RUNTIME_SHUTDOWN_TIMEOUT, Server,
    ShutdownSignal, ShutdownSignals, ShutdownTrigger, drain, shutdown_runtime,
};
pub use observability::{Observability, ObservabilitySettings};
pub use privacy::{Recording, RecordingSettings};
pub use providers::{CallProviders, ConfiguredProviders, ProviderFactory};
pub use resilient::ResilientProviders;
pub use runner::{AcceptedCall, EngineRunner, RunnerConfig, Vad};
pub use session::{
    AdmissionError, EndCounts, EndReason, ManagerConfig, SessionContext, SessionEnded,
    SessionHandle, SessionId, SessionManager, SessionRunner, SessionSlot, SessionStats,
    ShutdownPolicy, ShutdownReport,
};
pub use settings::ServerSettings;
pub use tools::{AppTools, ToolSetup};

/// Version of this build, as recorded in Cargo metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn version_matches_workspace() {
        // The workspace's `[workspace.package]` version, read from the root manifest: a release
        // bump needs no edit here.
        let manifest = include_str!("../../../Cargo.toml");
        let workspace = manifest
            .split("[workspace.package]")
            .nth(1)
            .and_then(|rest| rest.lines().find(|l| l.trim_start().starts_with("version")))
            .and_then(|line| line.split('"').nth(1));
        assert_eq!(Some(super::VERSION), workspace);
    }
}
