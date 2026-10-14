//! Brisvox, a Rust engine for real-time voice agents, as one dependency.
//!
//! This crate holds no logic of its own. It re-exports the crates an application uses and forwards
//! the Cargo features of `brisvox-server`, so `cargo add brisvox --features all-providers` is the
//! whole setup of a server with your own Rust tools:
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use brisvox::AppTools;
//! use brisvox::agent::tools::{RegistryError, ToolRegistry};
//! use brisvox::core::SharedClock;
//!
//! /// Everything this application adds to the server (see `brisvox::agent::tools` for tools).
//! struct MyTools;
//!
//! impl AppTools for MyTools {
//!     fn register(
//!         &self,
//!         _registry: &mut ToolRegistry,
//!         _clock: &SharedClock,
//!     ) -> Result<(), RegistryError> {
//!         Ok(())
//!     }
//! }
//!
//! fn main() -> std::process::ExitCode {
//!     brisvox::run_with_tools(Arc::new(MyTools))
//! }
//! ```
//!
//! | Module | Crate | What it is |
//! |---|---|---|
//! | [`server`] | `brisvox-server` | The multi-tenant server: configuration, sessions, transports, [`server::app`] |
//! | [`agent`] | `brisvox-agent` | Conversation logic and the tools API ([`agent::tools`]) |
//! | [`providers`] | `brisvox-providers` | The speech and LLM provider adapters and their traits |
//! | [`core`] | `brisvox-core` | Frames, the injected clock, turn epochs |
//!
//! The other crates of the engine (audio, inference, transport, engine, agent link) are
//! dependencies of these and are published separately on crates.io for those who need them alone.

#![forbid(unsafe_code)]

/// The conversation logic and the tools API: [`brisvox_agent`].
pub use brisvox_agent as agent;
/// Frames, the injected clock and turn epochs: [`brisvox_core`].
pub use brisvox_core as core;
/// The speech-to-text, LLM and text-to-speech provider adapters: [`brisvox_providers`].
pub use brisvox_providers as providers;
/// The server: configuration, sessions, transports and the program behind `main`:
/// [`brisvox_server`].
pub use brisvox_server as server;

pub use brisvox_server::AppTools;
pub use brisvox_server::app::{run, run_with_tools};

/// The version of this crate, which is the version of every brisvox crate it re-exports.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use std::process::ExitCode;
    use std::sync::Arc;

    use super::*;

    /// The umbrella and the crates behind it are released together: one version.
    #[test]
    fn versions_agree() {
        assert_eq!(VERSION, server::VERSION);
    }

    /// The re-exported paths of the README resolve to the items they name.
    #[test]
    fn reexports_resolve() {
        let _clock: Option<core::SharedClock> = None;
        let _kind = agent::tools::ToolKind::ReadOnly;
        let _entry: fn(Arc<dyn AppTools>) -> ExitCode = run_with_tools;
        let _same: fn(Arc<dyn server::AppTools>) -> ExitCode = server::app::run_with_tools;
        let _stock: fn() -> ExitCode = run;
    }
}
