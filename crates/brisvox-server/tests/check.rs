//! `brisvox-server check` (docs/CONFIG.md "Checking a file"): the offline validation of an agent
//! file reports what the server would refuse, warns about what only a running server can know,
//! and passes every example agent in the repository. Needs every provider adapter (the example
//! files name several).
#![cfg(feature = "all-providers")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
// test helpers

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use brisvox_agent::tools::{
    RegistryError, Tool, ToolCallCtx, ToolFuture, ToolKind, ToolOutput, ToolRegistry, ToolSpec,
};
use brisvox_core::SharedClock;
use brisvox_server::AppTools;
use brisvox_server::check::{CheckOptions, CheckReport, check_environment, check_file};
use serde_json::{Value, json};

/// A working agent file; `{extra}` is spliced into `[agent]`, `{turn}` is a whole table.
fn agent_text(extra: &str, turn: &str) -> String {
    format!(
        r#"[agent]
name = "check-test"
language = "en"
system_prompt = "You are a test agent. Answer in one short sentence."
{extra}

{turn}

[stt]
provider = "deepgram"
api_key_env = "CHECK_TEST_DEEPGRAM_KEY"

[llm]
provider = "openai"
api_key_env = "CHECK_TEST_OPENAI_KEY"

[tts]
provider = "cartesia"
api_key_env = "CHECK_TEST_CARTESIA_KEY"
voice = "a-voice-id"
"#
    )
}

fn write(name: &str, text: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "brisvox-check-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

fn all_keys() -> BTreeMap<String, String> {
    [
        "CHECK_TEST_DEEPGRAM_KEY",
        "CHECK_TEST_OPENAI_KEY",
        "CHECK_TEST_CARTESIA_KEY",
    ]
    .into_iter()
    .map(|k| (k.to_owned(), "a-key".to_owned()))
    .collect()
}

fn check(path: &Path, env: &BTreeMap<String, String>, require_secrets: bool) -> CheckReport {
    check_file(path, env, CheckOptions { require_secrets }, None)
}

fn fixed_turns() -> &'static str {
    "[turn]\npolicy = \"fixed_silence\"\nsilence_ms = 600\n"
}

#[test]
fn a_good_file_is_ok_without_warnings() {
    let path = write("agent.toml", &agent_text("", fixed_turns()));
    let report = check(&path, &all_keys(), false);
    assert!(report.is_ok(), "{}", report.render());
    assert!(report.warnings.is_empty(), "{}", report.render());
    let text = report.render();
    assert!(
        text.contains("OK: agent \"check-test\" (stt deepgram, llm openai, tts cartesia)"),
        "{text}"
    );
}

#[test]
fn a_good_yaml_file_is_ok_too() {
    let yaml = r#"
agent:
  name: yaml-test
  language: en
  system_prompt: Answer in one short sentence.
turn:
  policy: fixed_silence
  silence_ms: 600
stt:
  provider: deepgram
  api_key_env: CHECK_TEST_DEEPGRAM_KEY
llm:
  provider: openai
  api_key_env: CHECK_TEST_OPENAI_KEY
tts:
  provider: cartesia
  api_key_env: CHECK_TEST_CARTESIA_KEY
  voice: a-voice-id
"#;
    let path = write("agent.yaml", yaml);
    let report = check(&path, &all_keys(), false);
    assert!(report.is_ok(), "{}", report.render());
}

fn check_env(pairs: &[(&str, &str)], require_secrets: bool) -> CheckReport {
    let mut env = all_keys();
    env.extend(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
    );
    check_environment(&env, CheckOptions { require_secrets }, None)
}

#[test]
fn check_env_validates_the_inline_definition_like_the_file() {
    let text = agent_text("", fixed_turns());
    let report = check_env(&[("BRISVOX_AGENT_DEFINITION", &text)], true);
    assert!(report.is_ok(), "{}", report.render());
    assert!(
        report.render().contains("OK: agent \"check-test\""),
        "{}",
        report.render()
    );
    // The same problems with the same messages, under the variable's source name.
    let typo = text.replace("silence_ms", "silence_msx");
    let report = check_env(&[("BRISVOX_AGENT_DEFINITION", &typo)], false);
    assert!(!report.is_ok());
    assert_eq!(report.source_name, "$BRISVOX_AGENT_DEFINITION");
    assert_eq!(report.errors[0].key, "turn.silence_msx");
    assert!(
        report.render().contains("$BRISVOX_AGENT_DEFINITION:"),
        "{}",
        report.render()
    );
    // A missing secret is a warning, or an error when required.
    let mut env = BTreeMap::new();
    env.insert("BRISVOX_AGENT_DEFINITION".to_owned(), text);
    let lax = check_environment(&env, CheckOptions::default(), None);
    assert!(lax.is_ok() && !lax.warnings.is_empty(), "{}", lax.render());
    let strict = check_environment(
        &env,
        CheckOptions {
            require_secrets: true,
        },
        None,
    );
    assert!(!strict.is_ok(), "{}", strict.render());
}

#[cfg(feature = "yaml")]
#[test]
fn check_env_reads_yaml_only_when_told_to() {
    let yaml = "agent:\n  name: y\n  language: en\n  system_prompt: Hi.\nturn:\n  policy: fixed_silence\n  silence_ms: 600\nstt:\n  provider: deepgram\n  api_key_env: CHECK_TEST_DEEPGRAM_KEY\nllm:\n  provider: openai\n  api_key_env: CHECK_TEST_OPENAI_KEY\ntts:\n  provider: cartesia\n  api_key_env: CHECK_TEST_CARTESIA_KEY\n  voice: v\n";
    let report = check_env(
        &[
            ("BRISVOX_AGENT_DEFINITION", yaml),
            ("BRISVOX_AGENT_FORMAT", "yaml"),
        ],
        true,
    );
    assert!(report.is_ok(), "{}", report.render());
    // No sniffing: without the format variable the text is read as TOML and fails.
    let report = check_env(&[("BRISVOX_AGENT_DEFINITION", yaml)], true);
    assert!(!report.is_ok());
}

#[test]
fn check_env_follows_the_startup_rules() {
    let text = agent_text("", fixed_turns());
    let path = write("agent.toml", &text);
    let path = path.to_str().unwrap();
    // The file variable works like `check <file>`.
    let report = check_env(&[("BRISVOX_AGENT_FILE", path)], true);
    assert!(report.is_ok(), "{}", report.render());
    // Both set: refused, as at startup.
    let report = check_env(
        &[
            ("BRISVOX_AGENT_FILE", path),
            ("BRISVOX_AGENT_DEFINITION", &text),
        ],
        false,
    );
    assert!(!report.is_ok());
    assert!(report.render().contains("both set"), "{}", report.render());
    // Neither set: nothing to check is an error, not a silent pass.
    assert!(!check_env(&[], false).is_ok());
    // A format without a definition, and an unknown format.
    assert!(!check_env(&[("BRISVOX_AGENT_FORMAT", "yaml")], false).is_ok());
    assert!(
        !check_env(
            &[
                ("BRISVOX_AGENT_DEFINITION", &text),
                ("BRISVOX_AGENT_FORMAT", "json")
            ],
            false
        )
        .is_ok()
    );
}

#[test]
fn a_typo_names_the_key_and_the_position() {
    let text = agent_text("", fixed_turns()).replace("silence_ms", "silence_msx");
    let path = write("agent.toml", &text);
    let report = check(&path, &all_keys(), false);
    assert!(!report.is_ok());
    let issue = &report.errors[0];
    assert!(issue.message.contains("silence_msx"), "{issue}");
    assert_eq!(issue.key, "turn.silence_msx");
    let (line, _) = issue.position.expect("a position");
    assert_eq!(
        text.lines().nth(line - 1).unwrap().trim(),
        "silence_msx = 600"
    );
    let rendered = report.render();
    assert!(rendered.contains("error: "), "{rendered}");
    assert!(
        rendered.contains(&format!("agent.toml:{line}:")),
        "{rendered}"
    );
    assert!(rendered.contains("FAILED"), "{rendered}");
}

/// OpenRouter routing on another server is an error that `check` prints, with the
/// position of the key.
#[test]
fn openrouter_routing_without_openrouter_is_an_error_in_the_check_output() {
    let compatible = |table: &str| {
        agent_text("", fixed_turns()).replace(
            "[llm]
provider = \"openai\"
api_key_env = \"CHECK_TEST_OPENAI_KEY\"",
            &format!(
                "[llm]
provider = \"openai_compatible\"
api_key_env = \"CHECK_TEST_OPENAI_KEY\"

                 [llm.openai_compatible]
{table}"
            ),
        )
    };
    let routing = "openrouter_order = [\"openai\"]
openrouter_allow_fallbacks = false
";
    let wrong = compatible(&format!(
        "base_url = \"https://api.example.com/v1\"
model = \"m\"
{routing}"
    ));
    let report = check(&write("agent.toml", &wrong), &all_keys(), false);
    assert!(!report.is_ok(), "{}", report.render());
    let rendered = report.render();
    assert!(rendered.contains("OpenRouter-only"), "{rendered}");
    assert!(rendered.contains("openrouter_order"), "{rendered}");
    assert!(
        rendered.contains("openrouter_allow_fallbacks"),
        "{rendered}"
    );
    assert!(rendered.contains("FAILED"), "{rendered}");

    for ok in [
        format!(
            "preset = \"openrouter\"
model = \"a/m\"
{routing}"
        ),
        format!(
            "base_url = \"https://openrouter.ai/api/v1\"
model = \"a/m\"
{routing}"
        ),
    ] {
        let report = check(&write("agent.toml", &compatible(&ok)), &all_keys(), false);
        assert!(report.is_ok(), "{}", report.render());
    }
}

#[test]
fn every_problem_is_reported_not_just_the_first() {
    // Two cross-field problems: options tables of providers that are not the chosen one.
    let text = agent_text("", fixed_turns()).replace(
        "[llm]",
        "[llm.anthropic]
max_tokens = 100

[llm.gemini]
model = \"gemini-x\"

[llm]",
    );
    let path = write("agent.toml", &text);
    let report = check(&path, &all_keys(), false);
    assert!(report.errors.len() >= 2, "{}", report.render());
}

#[test]
fn a_missing_secret_is_a_warning_and_an_error_with_require_secrets() {
    let path = write("agent.toml", &agent_text("", fixed_turns()));
    let mut env = all_keys();
    env.remove("CHECK_TEST_OPENAI_KEY");

    let report = check(&path, &env, false);
    assert!(report.is_ok(), "{}", report.render());
    assert_eq!(report.warnings.len(), 1, "{}", report.render());
    let warning = &report.warnings[0];
    assert_eq!(warning.key, "llm.api_key_env");
    assert!(warning.message.contains("CHECK_TEST_OPENAI_KEY"));
    assert!(warning.position.is_some());
    assert!(report.render().contains("OK: "));

    let strict = check(&path, &env, true);
    assert!(!strict.is_ok());
    assert_eq!(strict.errors.len(), 1, "{}", strict.render());
    assert_eq!(strict.errors[0].key, "llm.api_key_env");
    assert!(strict.warnings.is_empty());
}

#[test]
fn an_empty_secret_is_an_error_even_without_require_secrets() {
    let path = write("agent.toml", &agent_text("", fixed_turns()));
    let mut env = all_keys();
    env.insert("CHECK_TEST_OPENAI_KEY".to_owned(), "  ".to_owned());
    let report = check(&path, &env, false);
    assert!(!report.is_ok());
    assert!(
        report.errors[0].message.contains("empty"),
        "{}",
        report.render()
    );
}

#[test]
fn a_range_error_is_an_error_with_no_secrets_set() {
    let text = agent_text("", fixed_turns()).replace("silence_ms = 600", "silence_ms = 0");
    let path = write("agent.toml", &text);
    let report = check(&path, &BTreeMap::new(), false);
    assert!(!report.is_ok());
    // A value out of range stops the reading before the secrets are looked at: only the error.
    assert!(!report.errors.is_empty());
}

#[test]
fn an_unknown_tool_is_an_error_pointing_at_the_sdk_agent() {
    let path = write(
        "agent.toml",
        &agent_text("tools = [\"get_weather\"]", fixed_turns()),
    );
    let report = check(&path, &all_keys(), false);
    assert!(!report.is_ok(), "{}", report.render());
    let error = &report.errors[0];
    assert_eq!(error.key, "agent.tools[0]");
    assert!(error.message.contains("get_weather"));
    assert!(error.message.contains("refuses to start"), "{error}");
    assert!(
        error
            .message
            .contains("remove it from agent.tools: the SDK agent declares it"),
        "{error}"
    );
    assert!(error.position.is_some());
}

/// The environment of a server with Twilio call control.
fn with_call_control(mut env: BTreeMap<String, String>) -> BTreeMap<String, String> {
    env.insert("BRISVOX_CALL_CONTROL".to_owned(), "twilio".to_owned());
    env
}

#[test]
fn the_telephony_tools_are_registered_only_with_the_feature_and_a_carrier() {
    let path = write(
        "agent.toml",
        &agent_text("tools = [\"transfer_call\", \"end_call\"]", fixed_turns()),
    );
    // No carrier named: the server would refuse these, with the variable in the hint.
    let report = check(&path, &all_keys(), false);
    assert_eq!(report.errors.len(), 2, "{}", report.render());
    assert!(
        report.errors[0].message.contains("BRISVOX_CALL_CONTROL"),
        "{}",
        report.render()
    );
    // A carrier named: fine when the feature is compiled in, still refused when it is not.
    let report = check(&path, &with_call_control(all_keys()), false);
    if cfg!(feature = "call-control") {
        assert!(report.is_ok(), "{}", report.render());
        assert!(report.warnings.is_empty(), "{}", report.render());
    } else {
        assert_eq!(report.errors.len(), 2, "{}", report.render());
        assert!(
            report.errors[0].message.contains("`call-control` feature"),
            "{}",
            report.render()
        );
    }
    // `off` is not a carrier.
    let mut env = all_keys();
    env.insert("BRISVOX_CALL_CONTROL".to_owned(), "off".to_owned());
    assert!(!check(&path, &env, false).is_ok());
}

struct Weather;

impl Tool for Weather {
    fn call<'a>(&'a self, _args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async { Ok(ToolOutput::Json(json!({"sky": "clear"}))) })
    }
}

struct MyTools;

impl AppTools for MyTools {
    fn register(
        &self,
        registry: &mut ToolRegistry,
        _clock: &SharedClock,
    ) -> Result<(), RegistryError> {
        registry.register(
            ToolSpec::new(
                "get_weather",
                "The weather.",
                json!({"type": "object", "properties": {}}),
                ToolKind::ReadOnly,
            ),
            Weather,
        )
    }
}

#[test]
fn a_tool_the_application_registers_is_known() {
    let path = write(
        "agent.toml",
        &agent_text("tools = [\"get_weather\"]", fixed_turns()),
    );
    let report = check_file(&path, &all_keys(), CheckOptions::default(), Some(&MyTools));
    assert!(report.is_ok(), "{}", report.render());
    assert!(report.warnings.is_empty(), "{}", report.render());
}

#[test]
fn adaptive_turns_warn_when_smart_turn_cannot_run() {
    // No model in the directory the check is pointed at (and, in a build without the
    // `smart-turn-ort` feature, no model support at all): both warn.
    let path = write(
        "agent.toml",
        &agent_text("", "[turn]\npolicy = \"adaptive\"\n"),
    );
    let mut env = all_keys();
    env.insert(
        "BRISVOX_MODELS_DIR".to_owned(),
        "no-such-models-directory".to_owned(),
    );
    let report = check(&path, &env, false);
    assert!(report.is_ok(), "{}", report.render());
    let warning = report
        .warnings
        .iter()
        .find(|w| w.key == "turn.policy")
        .unwrap_or_else(|| panic!("{}", report.render()));
    assert!(warning.message.contains("Smart Turn"), "{warning}");

    // Switched off on purpose: no warning.
    env.insert("BRISVOX_SMART_TURN_MODEL".to_owned(), "off".to_owned());
    let report = check(&path, &env, false);
    assert!(report.warnings.is_empty(), "{}", report.render());
}

#[test]
fn a_file_that_does_not_exist_is_an_error() {
    let report = check(Path::new("no-such-agent.toml"), &BTreeMap::new(), false);
    assert!(!report.is_ok());
    assert!(report.render().contains("cannot open the file"));
}

/// Every `*.toml` / `*.yaml` / `*.yml` under `dir`, recursively.
fn agent_files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            agent_files_under(&path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("toml" | "yaml" | "yml")
        ) && path.file_name().is_some_and(|n| n != "Cargo.toml")
        {
            out.push(path);
        }
    }
}

/// The tools the example agent files list that a stock build of this test does not register: the
/// Rust-tools example's own, and (by feature) the demo and telephony tools. Stubs, for the check
/// only looks at the names.
struct ExampleTools;

impl AppTools for ExampleTools {
    fn register(
        &self,
        registry: &mut ToolRegistry,
        _clock: &SharedClock,
    ) -> Result<(), RegistryError> {
        let mut names = vec!["get_time", "save_note"];
        if !cfg!(feature = "demo-tools") {
            names.extend(["lookup_order", "book_appointment"]);
        }
        if !cfg!(feature = "call-control") {
            names.extend(["transfer_call", "end_call", "send_dtmf"]);
        }
        for name in names {
            registry.register(
                ToolSpec::new(
                    name,
                    "A stub.",
                    json!({"type": "object", "properties": {}}),
                    ToolKind::ReadOnly,
                ),
                Weather,
            )?;
        }
        Ok(())
    }
}

#[test]
fn every_example_agent_file_passes_the_check() {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut files = Vec::new();
    agent_files_under(&examples, &mut files);
    files.sort();
    assert!(files.len() >= 10, "{files:?}");
    for path in files {
        // Secrets are not set here: warnings, never errors. The tools the files list resolve as
        // on the examples' servers: the Rust-tools example's own (stubs here), the demo tools,
        // and the telephony tools of a server with a carrier's call control.
        let report = check_file(
            &path,
            &with_call_control(BTreeMap::new()),
            CheckOptions::default(),
            Some(&ExampleTools),
        );
        assert!(report.is_ok(), "{}: {}", path.display(), report.render());
    }
}
