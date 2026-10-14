//! The `brisvox-server` command line (docs/OPERATIONS.md): `--version`, `--help`, `check`, and
//! that a mistake is refused instead of starting a server. These run the real binary; none of
//! them opens a socket.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
// test helpers

use std::path::PathBuf;
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_brisvox-server"))
        .args(args)
        // A stray BRISVOX_* variable of the developer's machine must not change the answer.
        .env_remove("BRISVOX_AGENT_FILE")
        .env_remove("BRISVOX_AGENT_DEFINITION")
        .env_remove("BRISVOX_AGENT_FORMAT")
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn temp_file(name: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("brisvox-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn version_prints_the_version() {
    let out = run(&["--version"]);
    assert!(out.status.success());
    assert_eq!(
        text(&out.stdout).trim(),
        format!("brisvox-server {}", brisvox_server::VERSION)
    );
}

#[test]
fn help_points_at_the_docs() {
    let out = run(&["--help"]);
    assert!(out.status.success());
    let help = text(&out.stdout);
    assert!(help.contains("check <agent-file>"), "{help}");
    assert!(
        help.contains("docs/CONFIG.md") && help.contains("docs/OPERATIONS.md"),
        "{help}"
    );
}

/// With the `schema` feature `--print-schema` prints the JSON Schema of the agent file, the one
/// committed as docs/agent.schema.json.
#[cfg(feature = "schema")]
#[test]
fn print_schema_prints_the_committed_schema() {
    let out = run(&["--print-schema"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let printed = text(&out.stdout);
    let schema: serde_json::Value = serde_json::from_str(&printed).unwrap();
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    let committed = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/agent.schema.json"),
    )
    .unwrap()
    .replace("\r\n", "\n");
    assert_eq!(printed, committed);
}

/// Without the feature the flag is an error that names it, not a silent success.
#[cfg(not(feature = "schema"))]
#[test]
fn print_schema_without_the_feature_names_it() {
    let out = run(&["--print-schema"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let err = text(&out.stderr);
    assert!(err.contains("`schema` cargo feature"), "{err}");
}

#[test]
fn print_schema_takes_no_arguments() {
    let out = run(&["--print-schema", "extra"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("--print-schema takes no arguments"));
}

#[test]
fn an_unknown_command_is_refused_with_the_usage() {
    let out = run(&["chek", "agent.toml"]);
    assert_eq!(out.status.code(), Some(2));
    let err = text(&out.stderr);
    assert!(
        err.contains("unknown argument `chek`") && err.contains("USAGE"),
        "{err}"
    );
}

#[test]
fn check_exits_1_with_the_position_of_a_typo() {
    let path = temp_file(
        "typo.toml",
        "[agent]\nname = \"x\"\nlanguag = \"en\"\nsystem_prompt = \"Hi.\"\n",
    );
    let out = run(&["check", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    let report = text(&out.stdout);
    assert!(report.contains("typo.toml:3:1"), "{report}");
    assert!(report.contains("agent.languag"), "{report}");
    assert!(report.contains("FAILED"), "{report}");
}

#[test]
fn check_of_a_missing_file_exits_1() {
    let out = run(&["check", "no-such-file.toml"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stdout).contains("cannot open the file"));
}

#[test]
fn check_without_a_file_is_a_usage_error() {
    let out = run(&["check"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("--env"), "{}", text(&out.stderr));
}

#[test]
fn check_env_reads_the_inline_definition() {
    let run_env = |definition: Option<&str>| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_brisvox-server"));
        cmd.args(["check", "--env"])
            .env_remove("BRISVOX_AGENT_FILE")
            .env_remove("BRISVOX_AGENT_FORMAT")
            .env_remove("BRISVOX_AGENT_DEFINITION");
        if let Some(definition) = definition {
            cmd.env("BRISVOX_AGENT_DEFINITION", definition);
        }
        cmd.output().unwrap()
    };
    let out = run_env(Some(
        "[agent]\nname = \"x\"\nlanguag = \"en\"\nsystem_prompt = \"Hi.\"\n",
    ));
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    let report = text(&out.stdout);
    assert!(
        report.contains("$BRISVOX_AGENT_DEFINITION:3:1") && report.contains("agent.languag"),
        "{report}"
    );
    let out = run_env(None);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stdout).contains("nothing to check"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn a_file_and_a_definition_together_stop_the_server() {
    let out = Command::new(env!("CARGO_BIN_EXE_brisvox-server"))
        .env("BRISVOX_AGENT_FILE", "a.toml")
        .env("BRISVOX_AGENT_DEFINITION", "[agent]\n")
        .env("BRISVOX_WS_ADDR", "127.0.0.1:0")
        .env("BRISVOX_HEALTH_ADDR", "off")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = format!("{}{}", text(&out.stdout), text(&out.stderr));
    assert!(err.contains("BRISVOX_AGENT_DEFINITION"), "{err}");
}

/// A good example file: exit 0 (a missing secret is only a warning), exit 1 with
/// `--require-secrets`.
#[cfg(feature = "all-providers")]
#[test]
fn check_of_an_example_exits_0_and_strict_mode_wants_the_secrets() {
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/web-assistant/agent.toml");
    let path = example.to_str().unwrap();
    let no_keys = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_brisvox-server"))
            .args(args)
            .env_remove("DEEPGRAM_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("CARTESIA_API_KEY")
            .output()
            .unwrap()
    };
    let out = no_keys(&["check", path]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
    assert!(text(&out.stdout).contains("warning: "));
    let out = no_keys(&["check", path, "--require-secrets"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stdout).contains("DEEPGRAM_API_KEY"));
}
