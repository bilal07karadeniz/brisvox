//! `brisvox-server` binary entry point. The program itself is `brisvox_server::app::run`; an
//! application with Rust tools of its own writes a `main` like this one over
//! `brisvox_server::app::run_with_tools` (docs/TOOLS.md, `examples/rust-tools`).

/// Per-token `Arc<str>` is allocated on the LLM task and freed on the TTS task; mimalloc handles
/// such cross-thread frees about twice as fast as the system allocator.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> std::process::ExitCode {
    brisvox_server::app::run()
}
