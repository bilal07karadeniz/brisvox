//! `brisvox-server check <agent-file>` and `check --env`: validates an agent definition offline,
//! before a deploy. `--env` checks what the server would start with: `BRISVOX_AGENT_DEFINITION`
//! (the text, with `BRISVOX_AGENT_FORMAT`) or the file `BRISVOX_AGENT_FILE` names.
//!
//! It runs the server's own loader ([`config::load_str_as`], the one `BRISVOX_AGENT_FILE` goes
//! through at startup), so every problem the server would refuse the file for is reported with
//! its key path and line and column: unknown keys, out-of-range values, provider options that do
//! not belong, a provider whose cargo feature is not compiled into this binary. On top of that it
//! reports what a start would only reveal later or warn about:
//!
//! - **Secrets.** A variable named by `api_key_env`/`token_env` that is not set is a *warning*
//!   (the machine that checks is often not the one that runs), an *error* with
//!   [`CheckOptions::require_secrets`]. The loader runs with a placeholder for each missing
//!   variable, so one run reports every problem at once; a variable that is set but empty or
//!   malformed is a real error either way.
//! - **Tools.** A tool in `agent.tools` that this binary does not register is an *error*: the
//!   server refuses to start with it. (A Python or TypeScript SDK agent declares its own tools
//!   over the agent link; they do not belong in `agent.tools`.) The telephony tools count as
//!   registered when the `call-control` feature is compiled in and `BRISVOX_CALL_CONTROL` names
//!   a carrier.
//! - **Adaptive turns.** Without the `smart-turn-ort` feature, or without the model file, adaptive
//!   turns end at their fallback deadline: a warning.
//!
//! Nothing here opens a socket, reads a secret's value into a message, or starts a runtime.

use std::collections::BTreeSet;
use std::env::VarError;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use brisvox_agent::tools::ToolRegistry;
use brisvox_agent::turn::TurnPolicy;
use brisvox_core::SystemClock;
use toml::Spanned;

use crate::AppTools;
use crate::config::{
    self, AgentConfig, ConfigError, Env, Format, Issue, Provider, schema::AgentFile,
};
use crate::hosting::{ENV_AGENT_DEFINITION, ENV_AGENT_FILE, inline_agent};
use crate::runner::{ENV_MODELS_DIR, ENV_SMART_TURN_MODEL};
use crate::settings::var;

/// The value the loader gets for a variable that is not set, so it reports the rest of the file.
const PLACEHOLDER: &str = "placeholder-secret";

/// What `check` does beyond reading the file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CheckOptions {
    /// A secret named by the file that is not set in the environment is an error (default: a
    /// warning).
    pub require_secrets: bool,
}

/// The outcome of [`check_file`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    /// The file as given.
    pub source_name: String,
    /// What the server would refuse the file for; the check fails when there is any.
    pub errors: Vec<Issue>,
    /// What would work but probably not as intended.
    pub warnings: Vec<Issue>,
    /// `agent "name" (stt X, llm Y, tts Z)` when the file loaded.
    pub summary: Option<String>,
}

impl CheckReport {
    /// The file has no errors.
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// The report as printed: one line per problem (`file:line:col: \`key\`: message`), then a
    /// verdict line.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (label, issues) in [("error", &self.errors), ("warning", &self.warnings)] {
            for issue in issues {
                let _ = writeln!(out, "{label}: {}", self.line(issue));
            }
        }
        let warnings = match self.warnings.len() {
            0 => String::new(),
            1 => " (1 warning)".to_owned(),
            n => format!(" ({n} warnings)"),
        };
        if self.is_ok() {
            let _ = writeln!(
                out,
                "OK: {}{warnings}",
                self.summary.as_deref().unwrap_or("the file is valid")
            );
        } else {
            let n = self.errors.len();
            let _ = writeln!(
                out,
                "FAILED: {} has {n} error{}{}",
                self.source_name,
                if n == 1 { "" } else { "s" },
                if self.warnings.is_empty() {
                    String::new()
                } else {
                    format!(" and {} warning(s)", self.warnings.len())
                }
            );
        }
        out
    }

    /// `file:line:col: \`key\`: message`.
    fn line(&self, issue: &Issue) -> String {
        let mut line = self.source_name.clone();
        if let Some((l, c)) = issue.position {
            let _ = write!(line, ":{l}:{c}");
        }
        line.push(':');
        if !issue.key.is_empty() {
            let _ = write!(line, " `{}`:", issue.key);
        }
        let _ = write!(line, " {}", issue.message);
        line
    }
}

/// An [`Env`] that answers a placeholder for a variable that is not set, and remembers which.
struct PlaceholderEnv<'a> {
    inner: &'a dyn Env,
    missing: Mutex<BTreeSet<String>>,
}

impl Env for PlaceholderEnv<'_> {
    fn var(&self, name: &str) -> Result<String, VarError> {
        match self.inner.var(name) {
            Err(VarError::NotPresent) => {
                self.missing
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(name.to_owned());
                Ok(PLACEHOLDER.to_owned())
            }
            other => other,
        }
    }
}

/// Validates the agent file at `path`, with secrets and settings from `env`. `app` is the
/// application's own tools, when the binary has some. Blocking file IO; no network, no runtime.
pub fn check_file(
    path: &Path,
    env: &dyn Env,
    options: CheckOptions,
    app: Option<&dyn AppTools>,
) -> CheckReport {
    let source_name = path.display().to_string();
    match config::read_bounded(path) {
        Ok(text) => check_text(
            &text,
            &source_name,
            Format::from_path(path),
            env,
            options,
            app,
        ),
        Err(message) => failed_report(source_name, message),
    }
}

/// Validates the agent definition the environment names, as the server would start with it:
/// `BRISVOX_AGENT_DEFINITION` (with `BRISVOX_AGENT_FORMAT`; source name
/// `$BRISVOX_AGENT_DEFINITION`) or the file `BRISVOX_AGENT_FILE`. Setting both, or neither, is an
/// error, as at startup.
pub fn check_environment(
    env: &dyn Env,
    options: CheckOptions,
    app: Option<&dyn AppTools>,
) -> CheckReport {
    let file = match var(env, ENV_AGENT_FILE) {
        Ok(file) => file,
        Err(e) => return failed_report(ENV_AGENT_FILE.to_owned(), e.to_string()),
    };
    let inline = match inline_agent(env) {
        Ok(inline) => inline,
        Err(e) => return failed_report(ENV_AGENT_DEFINITION.to_owned(), e.to_string()),
    };
    match (file, inline) {
        (Some(_), Some(_)) => failed_report(
            ENV_AGENT_DEFINITION.to_owned(),
            format!(
                "{ENV_AGENT_FILE} and {ENV_AGENT_DEFINITION} are both set: use one (a file path, \
                 or the definition's text)"
            ),
        ),
        (Some(path), None) => check_file(Path::new(&path), env, options, app),
        (None, Some(inline)) => {
            if let Err(message) = config::check_inline_size(&inline.text) {
                return failed_report(config::INLINE_SOURCE_NAME.to_owned(), message);
            }
            check_text(
                &inline.text,
                config::INLINE_SOURCE_NAME,
                inline.format,
                env,
                options,
                app,
            )
        }
        (None, None) => failed_report(
            ENV_AGENT_DEFINITION.to_owned(),
            format!("neither {ENV_AGENT_DEFINITION} nor {ENV_AGENT_FILE} is set: nothing to check"),
        ),
    }
}

/// A report with one error that has no position.
fn failed_report(source_name: String, message: String) -> CheckReport {
    CheckReport {
        source_name,
        errors: vec![Issue {
            key: String::new(),
            position: None,
            message,
        }],
        warnings: Vec::new(),
        summary: None,
    }
}

/// [`check_file`] for a definition already read.
fn check_text(
    text: &str,
    source_name: &str,
    format: Format,
    env: &dyn Env,
    options: CheckOptions,
    app: Option<&dyn AppTools>,
) -> CheckReport {
    let source_name = source_name.to_owned();
    let mut report = CheckReport {
        source_name: source_name.clone(),
        errors: Vec::new(),
        warnings: Vec::new(),
        summary: None,
    };
    let recording = PlaceholderEnv {
        inner: env,
        missing: Mutex::new(BTreeSet::new()),
    };
    let loaded = config::load_str_as(text, &source_name, format, &recording);
    let missing = std::mem::take(
        &mut *recording
            .missing
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
    );
    // The file's text once more, for the positions of the secrets and tools.
    let file = config::parse_as(text, &source_name, format).ok();
    if let Some(file) = &file {
        report_missing_secrets(&mut report, text, file, &missing, options);
    }
    match loaded {
        Err(ConfigError { issues, .. }) => report.errors.extend(issues),
        Ok(agent) => {
            report.summary = Some(summary(&agent));
            check_tools(&mut report, text, env, &agent, app);
            check_turns(&mut report, env, &agent);
        }
    }
    report
}

/// `agent "name" (stt X, llm Y, tts Z)`.
fn summary(agent: &AgentConfig) -> String {
    let file = agent.definition();
    format!(
        "agent \"{}\" (stt {}, llm {}, tts {})",
        agent.name(),
        Provider::from(*file.stt.provider.get_ref()).name(),
        Provider::from(*file.llm.provider.get_ref()).name(),
        Provider::from(*file.tts.provider.get_ref()).name(),
    )
}

/// Every place the file names an environment variable for a secret: key path, the name, and the
/// byte offset of the name.
fn secret_sites(file: &AgentFile) -> Vec<(String, String, usize)> {
    let mut sites = Vec::new();
    let mut add = |key: String, name: &Option<Spanned<crate::config::types::EnvVarName>>| {
        if let Some(name) = name {
            sites.push((key, name.get_ref().as_str().to_owned(), name.span().start));
        }
    };
    add("stt.api_key_env".to_owned(), &file.stt.api_key_env);
    add("llm.api_key_env".to_owned(), &file.llm.api_key_env);
    add("tts.api_key_env".to_owned(), &file.tts.api_key_env);
    if let Some(s) = &file.stt.secondary {
        add(
            "stt.secondary.api_key_env".to_owned(),
            &s.get_ref().api_key_env,
        );
    }
    if let Some(s) = &file.llm.secondary {
        add(
            "llm.secondary.api_key_env".to_owned(),
            &s.get_ref().api_key_env,
        );
    }
    if let Some(s) = &file.tts.secondary {
        add(
            "tts.secondary.api_key_env".to_owned(),
            &s.get_ref().api_key_env,
        );
    }
    add(
        "agent_link.token_env".to_owned(),
        &file.agent_link.token_env,
    );
    for (i, route) in file.agent_link.routes.iter().enumerate() {
        add(
            format!("agent_link.routes[{i}].token_env"),
            &route.token_env,
        );
    }
    sites
}

/// A warning (an error with `--require-secrets`) for each secret variable that is not set.
fn report_missing_secrets(
    report: &mut CheckReport,
    text: &str,
    file: &AgentFile,
    missing: &BTreeSet<String>,
    options: CheckOptions,
) {
    for (key, name, offset) in secret_sites(file) {
        if !missing.contains(&name) {
            continue;
        }
        let issue = Issue {
            key,
            position: Some(config::error::line_column(text, offset)),
            message: format!(
                "environment variable `{name}` is not set; the server needs it at startup \
                 (secrets are read from the environment, never from the file)"
            ),
        };
        if options.require_secrets {
            report.errors.push(issue);
        } else {
            report.warnings.push(issue);
        }
    }
}

/// The tools this binary registers: the application's and the demo tools (feature
/// `demo-tools`). The telephony tools of the `call-control` feature are not in this registry
/// (they exist per call); [`telephony_offered`] says whether the server would offer them.
fn registered_tools(report: &mut CheckReport, app: Option<&dyn AppTools>) -> ToolRegistry {
    let clock = SystemClock::shared();
    let mut registry = ToolRegistry::new();
    #[cfg(feature = "demo-tools")]
    if let Err(e) = crate::tools::demo::DemoTools.register(&mut registry, &clock) {
        report.errors.push(Issue {
            key: String::new(),
            position: None,
            message: format!("demo tools: {e}"),
        });
    }
    if let Some(app) = app
        && let Err(e) = app.register(&mut registry, &clock)
    {
        report.errors.push(Issue {
            key: String::new(),
            position: None,
            message: format!("application tools: {e}"),
        });
    }
    registry
}

/// Whether the server would offer the telephony tools: the `call-control` feature is compiled in
/// and `BRISVOX_CALL_CONTROL` names a carrier. (A malformed value is the startup's error to
/// report; here it counts as not set.)
fn telephony_offered(env: &dyn Env) -> bool {
    cfg!(feature = "call-control")
        && crate::tools::call_control_carriers(env).is_ok_and(|carriers| !carriers.is_empty())
}

/// An error for each tool of `agent.tools` this binary does not register: the server refuses to
/// start with it.
fn check_tools(
    report: &mut CheckReport,
    text: &str,
    env: &dyn Env,
    agent: &AgentConfig,
    app: Option<&dyn AppTools>,
) {
    let registry = registered_tools(report, app);
    let telephony = telephony_offered(env);
    for (i, tool) in agent.definition().agent.tools.iter().enumerate() {
        let name = tool.get_ref().as_str();
        if registry.spec(name).is_some()
            || (telephony && crate::tools::TELEPHONY_TOOLS.contains(&name))
        {
            continue;
        }
        let hint = tool_hint(name);
        report.errors.push(Issue {
            key: format!("agent.tools[{i}]"),
            position: Some(config::error::line_column(text, tool.span().start)),
            message: format!(
                "tool `{name}` is not registered by this binary, and the server refuses to \
                 start with it; {hint}"
            ),
        });
    }
}

/// What would provide the tool `name`.
fn tool_hint(name: &str) -> String {
    if crate::tools::TELEPHONY_TOOLS.contains(&name) {
        return if cfg!(feature = "call-control") {
            "set BRISVOX_CALL_CONTROL to a carrier (twilio or telnyx) so the server offers it \
             (docs/TELEPHONY.md)"
                .to_owned()
        } else {
            "it needs a build with the `call-control` feature and BRISVOX_CALL_CONTROL \
             (docs/TELEPHONY.md)"
                .to_owned()
        };
    }
    if crate::tools::DEMO_TOOLS.contains(&name) {
        return "it is a demo tool: build with the `demo-tools` feature to try the examples \
                (never for callers), or register your own with the same name"
            .to_owned();
    }
    "if it is a Python/TypeScript SDK tool, remove it from agent.tools: the SDK agent declares \
     it; if it is a Rust tool, register it with AppTools in your own binary (docs/TOOLS.md)"
        .to_owned()
}

/// A warning when adaptive turns cannot use Smart Turn in this build or environment.
fn check_turns(report: &mut CheckReport, env: &dyn Env, agent: &AgentConfig) {
    if !matches!(agent.engine().turn, TurnPolicy::Adaptive { .. }) {
        return;
    }
    let setting = crate::settings::var(env, ENV_SMART_TURN_MODEL)
        .ok()
        .flatten();
    if setting
        .as_deref()
        .is_some_and(|v| v.eq_ignore_ascii_case("off"))
    {
        return;
    }
    let key = "turn.policy".to_owned();
    #[cfg(feature = "smart-turn-ort")]
    {
        let path = match setting {
            Some(path) => std::path::PathBuf::from(path),
            None => std::path::PathBuf::from(
                crate::settings::var(env, ENV_MODELS_DIR)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "models".to_owned()),
            )
            .join(brisvox_inference::smart_turn::MODEL_FILE),
        };
        if !path.is_file() {
            report.warnings.push(Issue {
                key,
                position: None,
                message: format!(
                    "adaptive turns need the Smart Turn model, but {} does not exist: the server \
                     does not start without it (fetch it with scripts/models/fetch_models.py, \
                     point {ENV_SMART_TURN_MODEL} at it, or set {ENV_SMART_TURN_MODEL}=off to end \
                     turns at the fallback deadline)",
                    path.display()
                ),
            });
        }
    }
    #[cfg(not(feature = "smart-turn-ort"))]
    {
        let _ = ENV_MODELS_DIR;
        match setting {
            Some(_) => report.warnings.push(Issue {
                key,
                position: None,
                message: format!(
                    "{ENV_SMART_TURN_MODEL} is set, but this binary is built without the \
                     `smart-turn-ort` feature: the server refuses to start with it (unset it, or \
                     set it to off; adaptive turns then end at their fallback deadline)"
                ),
            }),
            None => report.warnings.push(Issue {
                key,
                position: None,
                message: "adaptive turns without a Smart Turn model end at their fallback \
                          deadline: build with the `smart-turn-ort` feature (docs/CONFIG.md)"
                    .to_owned(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn report_for(text: &str, name: &str) -> CheckReport {
        let dir = std::env::temp_dir().join(format!("brisvox-check-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        check_file(&path, &BTreeMap::new(), CheckOptions::default(), None)
    }

    /// A build without the Deepgram adapter reports it by name and with the feature to enable.
    #[cfg(not(feature = "provider-deepgram"))]
    #[test]
    fn a_provider_that_is_not_compiled_in_is_an_error_naming_the_feature() {
        let text = r#"
[agent]
name = "x"
language = "en"
system_prompt = "Hi."
[stt]
provider = "deepgram"
api_key_env = "DG_KEY"
[llm]
provider = "openai"
api_key_env = "OA_KEY"
[tts]
provider = "cartesia"
api_key_env = "CA_KEY"
voice = "v"
"#;
        let report = report_for(text, "feature.toml");
        assert!(!report.is_ok());
        let issue = report
            .errors
            .iter()
            .find(|i| i.key == "stt.provider")
            .unwrap_or_else(|| panic!("{}", report.render()));
        assert!(issue.message.contains("provider-deepgram"), "{issue}");
        assert!(issue.position.is_some());
    }

    #[test]
    fn a_syntax_error_has_a_position_and_the_report_says_failed() {
        let report = report_for("[agent\nname = 1\n", "broken.toml");
        assert!(!report.is_ok());
        let rendered = report.render();
        assert!(rendered.starts_with("error: "), "{rendered}");
        assert!(rendered.contains("broken.toml:"), "{rendered}");
        assert!(rendered.contains("FAILED"), "{rendered}");
    }

    #[test]
    fn a_report_without_errors_says_ok_and_counts_warnings() {
        let report = CheckReport {
            source_name: "a.toml".to_owned(),
            errors: Vec::new(),
            warnings: vec![Issue {
                key: "agent.tools[0]".to_owned(),
                position: Some((3, 9)),
                message: "tool `x` is not registered".to_owned(),
            }],
            summary: Some("agent \"a\"".to_owned()),
        };
        assert_eq!(
            report.render(),
            "warning: a.toml:3:9: `agent.tools[0]`: tool `x` is not registered\nOK: agent \"a\" (1 warning)\n"
        );
    }
}
