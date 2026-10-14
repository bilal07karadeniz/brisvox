//! The whole server, with this project's tools: `cargo run -p brisvox-example-rust-tools`.
//!
//! The settings are the stock server's environment variables (`BRISVOX_AGENT_FILE` and friends,
//! `docs/OPERATIONS.md`), and `my-voice-agent check agent.toml` validates the agent file offline.

use std::process::ExitCode;
use std::sync::Arc;

use brisvox_example_rust_tools::MyTools;

fn main() -> ExitCode {
    brisvox::run_with_tools(Arc::new(MyTools::default()))
}
