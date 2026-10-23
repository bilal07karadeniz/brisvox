//! What the MCP tests share: where the stdio test server's binary is, agent files that name it,
//! and a temporary state directory the server writes its logs to.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
// test helpers

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use brisvox_core::{Clock, SystemClock};
use brisvox_server::config::{self, AgentConfig};

/// The built stdio test server (`examples/mcp_test_server.rs`).
pub fn server_binary() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let exe = std::env::current_exe().unwrap();
        let profile_dir = exe.parent().unwrap().parent().unwrap().to_path_buf();
        let path = profile_dir
            .join("examples")
            .join(format!("mcp_test_server{}", std::env::consts::EXE_SUFFIX));
        // Built (or rebuilt, when its sources are newer): `cargo test` builds examples with the
        // integration tests, but not when a single test target is selected.
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        let built = modified(&path);
        let stale = ["examples/mcp_test_server.rs", "tests/support/mcp_server.rs"]
            .iter()
            .any(|src| modified(&manifest.join(src)) > built);
        if built.is_none() || stale {
            let mut cargo = Command::new(env!("CARGO"));
            cargo.args(["build", "-p", "brisvox-server", "--features", "mcp"]);
            cargo.args(["--example", "mcp_test_server"]);
            if profile_dir.ends_with("release") {
                cargo.arg("--release");
            }
            assert!(
                cargo.status().unwrap().success(),
                "cannot build the MCP test server"
            );
        }
        assert!(path.exists(), "{} was not built", path.display());
        path
    })
}

/// A fresh temporary directory for one test's server logs.
pub fn state_dir(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "brisvox-mcp-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A TOML literal string for `path` (backslashes of a Windows path stay as they are).
pub fn toml_path(path: &Path) -> String {
    format!("'{}'", path.display())
}

/// The keys of the mock providers every definition names.
pub fn base_env() -> BTreeMap<String, String> {
    [
        ("MCP_TEST_STT_KEY", "stt-key"),
        ("MCP_TEST_LLM_KEY", "llm-key"),
        ("MCP_TEST_TTS_KEY", "tts-key"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

/// A definition with `servers` (`[[mcp.servers]]` tables) and `tools` in `agent.tools`.
pub fn agent_text(servers: &str, tools: &str) -> String {
    format!(
        r#"
[agent]
name = "mcp-test"
language = "en"
system_prompt = "You are a test agent."
tools = [{tools}]

[stt]
provider = "deepgram"
api_key_env = "MCP_TEST_STT_KEY"

[llm]
provider = "openai"
api_key_env = "MCP_TEST_LLM_KEY"

[tts]
provider = "cartesia"
api_key_env = "MCP_TEST_TTS_KEY"
voice = "voice-123"

{servers}
"#
    )
}

/// Loads `text` as the binary does (the real loader, with `env` as the environment).
pub fn load(text: &str, extra_env: &[(&str, &str)]) -> Result<AgentConfig, config::ConfigError> {
    let mut env = base_env();
    env.extend(
        extra_env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
    );
    config::load_str(text, "agent.toml", &env)
}

/// A `[[mcp.servers]]` table for the stdio test server named `name`, logging to `dir`, with the
/// allowlist `tools` (TOML array items), `read_only` and any `extra` lines (TOML).
pub fn stdio_entry(
    name: &str,
    dir: &Path,
    tools: &str,
    read_only: &str,
    extra: &str,
    env_extra: &str,
) -> String {
    format!(
        r#"
[[mcp.servers]]
name = "{name}"
command = {command}
env = {{ MCP_TEST_STATE_DIR = {dir}{env_extra} }}
tools = [{tools}]
read_only = [{read_only}]
{extra}
"#,
        command = toml_path(server_binary()),
        dir = toml_path(dir),
    )
}

/// Polls `condition` every 20 ms for up to `secs` seconds; panics with `what` if it never holds.
pub async fn eventually(what: &str, secs: u64, mut condition: impl FnMut() -> bool) {
    let deadline = SystemClock.now() + std::time::Duration::from_secs(secs);
    while !condition() {
        assert!(
            SystemClock.now() < deadline,
            "timed out waiting for: {what}"
        );
        SystemClock
            .sleep(std::time::Duration::from_millis(20))
            .await;
    }
}
