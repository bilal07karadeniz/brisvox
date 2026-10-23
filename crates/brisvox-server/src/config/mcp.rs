//! `[[mcp.servers]]` (`docs/TOOLS.md`, "MCP tools"): the checks of the section and its resolved
//! form, [`McpSettings`], with the secrets read.
//!
//! This part is always compiled: an agent file with `[mcp]` is read, checked and positioned the
//! same in every build. Only connecting to the servers needs the cargo feature `mcp`
//! (`crate::mcp`); a build without it refuses the file at load with the position of the first
//! server.

use std::collections::BTreeSet;
use std::env::VarError;
use std::ops::Range;
use std::time::Duration;

use brisvox_agent::tools::{CONFIRM_TOOL, MAX_TOOL_NAME_LEN};
use brisvox_core::Secret;
use toml::Spanned;

use super::schema::{AgentFile, McpPrefix, McpServerEntry};
use super::types::EnvVarName;
use super::validate::Issues;
use super::{AgentConfig, ConfigError, Env};
use crate::tools::TELEPHONY_TOOLS;

/// Most `[[mcp.servers]]` entries.
pub const MAX_MCP_SERVERS: usize = 8;

/// Most tools in one server's allowlist.
pub const MAX_MCP_TOOLS_PER_SERVER: usize = 64;

/// Most MCP tools of all servers together (the LLM reads every description).
pub const MAX_MCP_TOOLS: usize = 64;

/// Longest tool name an MCP server may use (the specification's limit).
pub const MAX_MCP_TOOL_NAME_LEN: usize = 128;

/// Most `args` of a stdio server.
pub const MAX_MCP_ARGS: usize = 64;

/// Most `env` plus `env_from` variables of a stdio server.
pub const MAX_MCP_ENV: usize = 64;

/// Longest `command`, argument or environment value, in bytes.
pub const MAX_MCP_STRING_BYTES: usize = 4096;

/// Default longest wait for a call's result (`timeout_ms`).
pub const DEFAULT_MCP_TIMEOUT: Duration = Duration::from_secs(5);

/// Default most calls in flight to one server (`max_in_flight`).
pub const DEFAULT_MCP_MAX_IN_FLIGHT: usize = 32;

/// Default longest wait for a server to start and list its tools (`startup_timeout_ms`).
pub const DEFAULT_MCP_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// How the server reaches one MCP server.
#[derive(Debug, Clone)]
pub enum McpTransportSettings {
    /// A program started by the server, spoken to over its standard input and output.
    Stdio {
        /// The program.
        command: String,
        /// Its arguments.
        args: Vec<String>,
        /// Its environment, in addition to `PATH` (values are masked in `Debug`).
        env: Vec<(String, Secret<String>)>,
    },
    /// A streamable HTTP endpoint.
    Http {
        /// The endpoint.
        url: String,
        /// The bearer token (`bearer_env`), when the server wants one.
        bearer: Option<Secret<String>>,
    },
}

/// One tool of an MCP server's allowlist.
#[derive(Debug, Clone)]
pub struct McpToolSettings {
    /// The server's own name of the tool.
    pub name: String,
    /// The name the LLM sees (prefix applied, characters a tool name cannot have replaced).
    pub exposed: String,
    /// `true`: in `read_only`. Everything else has side effects.
    pub read_only: bool,
    /// `true`: in `confirm` (held for the user's confirmation).
    pub confirm: bool,
    /// Position of the allowlist entry in the file text (for errors of the connection).
    pub span: Range<usize>,
    /// Key path of the allowlist entry (`mcp.servers[0].tools[1]`).
    pub key: String,
}

/// One `[[mcp.servers]]` entry, checked, with its secrets read.
#[derive(Debug, Clone)]
pub struct McpServerSettings {
    /// The server's name.
    pub name: String,
    /// How to reach it.
    pub transport: McpTransportSettings,
    /// The allowlist, in file order.
    pub tools: Vec<McpToolSettings>,
    /// Longest wait for a call's result.
    pub timeout: Duration,
    /// Time without a result after which the filler plays (`None`: the engine's default).
    pub filler_after: Option<Duration>,
    /// Longest wait for the server to start and list its tools.
    pub startup_timeout: Duration,
    /// Most calls in flight to the server at once.
    pub max_in_flight: usize,
    /// Position of the server's name in the file text.
    pub span: Range<usize>,
    /// Key path of the entry (`mcp.servers[0]`).
    pub key: String,
}

/// The `[mcp]` section, checked, with its secrets read (empty without the section).
#[derive(Debug, Clone, Default)]
pub struct McpSettings {
    /// The servers, in file order.
    pub servers: Vec<McpServerSettings>,
}

impl McpSettings {
    /// Whether the definition names no MCP server.
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// Every tool name the LLM will see, in file order.
    pub fn exposed_names(&self) -> impl Iterator<Item = &str> + '_ {
        self.servers
            .iter()
            .flat_map(|s| s.tools.iter().map(|t| t.exposed.as_str()))
    }
}

/// The name the LLM sees for the server's tool `name` under `prefix`: the prefix and the name,
/// with every character a tool name cannot have (anything but ASCII letters, digits, `_`, `-`)
/// replaced by `_`.
pub fn exposed_name(prefix: &str, name: &str) -> String {
    prefix
        .chars()
        .chain(name.chars())
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The prefix `entry` asks for (`""` for none).
fn prefix_of(entry: &McpServerEntry) -> String {
    match entry.prefix.as_ref().map(Spanned::get_ref) {
        Some(McpPrefix::Flag(true)) => format!("{}_", entry.name.get_ref()),
        Some(McpPrefix::Text(text)) => text.clone(),
        Some(McpPrefix::Flag(false)) | None => String::new(),
    }
}

/// Whether `name` is a valid MCP server name or prefix text: 1-32 letters, digits, `_`, `-`.
fn short_name_ok(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Whether `name` is a valid MCP tool name (specification: 1-128 of `[A-Za-z0-9_.-]`).
fn mcp_tool_name_ok(name: &str) -> bool {
    (1..=MAX_MCP_TOOL_NAME_LEN).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

/// Whether `name` can be the name of an environment variable of a child process.
fn env_name_ok(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && name.len() <= 128
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Refuses a definition with MCP servers in a build without the MCP client (`compiled`: cargo
/// feature `mcp`; a parameter so tests can exercise the refusal in an all-features build). The
/// error points at the first server.
///
/// # Errors
/// [`ConfigError`] at the first `[[mcp.servers]]` entry.
pub(crate) fn require_compiled(agent: &AgentConfig, compiled: bool) -> Result<(), ConfigError> {
    match agent.mcp().servers.first() {
        Some(first) if !compiled => Err(agent.error_at(
            &first.key,
            first.span.clone(),
            "this binary is built without MCP support (cargo feature `mcp` of brisvox-server); \
             rebuild with it, or remove the `[[mcp.servers]]` tables"
                .to_owned(),
        )),
        _ => Ok(()),
    }
}

/// The cross-field checks of `[[mcp.servers]]`.
pub(crate) fn check(file: &AgentFile, issues: &mut Issues<'_>) {
    let servers = &file.mcp.servers;
    if servers.is_empty() {
        return;
    }
    if servers.len() > MAX_MCP_SERVERS {
        issues.general(
            "mcp.servers",
            format!("{} servers: at most {MAX_MCP_SERVERS}", servers.len()),
        );
    }
    // Names the LLM must not get twice: the agent's own tools, the built-in ones, and the
    // exposed names of every server (a clash between MCP tools and the tools the server
    // registers itself is found when the tools are registered).
    let mut taken: BTreeSet<String> = file
        .agent
        .tools
        .iter()
        .map(|t| t.get_ref().clone())
        .collect();
    taken.extend(TELEPHONY_TOOLS.iter().map(|t| (*t).to_owned()));
    taken.insert(CONFIRM_TOOL.to_owned());
    let mut server_names: BTreeSet<&str> = BTreeSet::new();
    let mut total_tools = 0usize;
    for (i, entry) in servers.iter().enumerate() {
        let key = format!("mcp.servers[{i}]");
        let name = entry.name.get_ref();
        if !short_name_ok(name) {
            issues.at(
                &format!("{key}.name"),
                entry.name.span(),
                "must be 1 to 32 letters, digits, `_` or `-`".to_owned(),
            );
        } else if !server_names.insert(name.as_str()) {
            issues.at(
                &format!("{key}.name"),
                entry.name.span(),
                format!("server {name:?} is defined twice"),
            );
        }
        check_transport(entry, &key, issues);
        check_tools(entry, &key, issues, &mut taken, &mut total_tools);
    }
    if total_tools > MAX_MCP_TOOLS {
        issues.general(
            "mcp.servers",
            format!("{total_tools} MCP tools in all: at most {MAX_MCP_TOOLS}"),
        );
    }
}

/// The transport keys of one server: exactly one of `command` and `url`, and only the keys of
/// the chosen one.
fn check_transport(entry: &McpServerEntry, key: &str, issues: &mut Issues<'_>) {
    let name_span = entry.name.span();
    match (&entry.command, &entry.url) {
        (Some(_), Some(url)) => issues.at(
            &format!("{key}.url"),
            url.span(),
            "a server has either `command` (stdio) or `url` (streamable HTTP), not both".to_owned(),
        ),
        (None, None) => issues.at(
            key,
            name_span.clone(),
            "needs `command` (a program speaking MCP over stdio) or `url` (a streamable HTTP \
             endpoint)"
                .to_owned(),
        ),
        _ => {}
    }
    if let Some(command) = &entry.command {
        let text = command.get_ref();
        if text.trim().is_empty() || text.len() > MAX_MCP_STRING_BYTES || has_nul_or_newline(text) {
            issues.at(
                &format!("{key}.command"),
                command.span(),
                format!(
                    "must be a program name or path (1 to {MAX_MCP_STRING_BYTES} bytes, one line)"
                ),
            );
        }
        if let Some(bearer) = &entry.bearer_env {
            issues.at(
                &format!("{key}.bearer_env"),
                bearer.span(),
                "applies only to a `url` server (a stdio server gets secrets through \
                 `env_from`)"
                    .to_owned(),
            );
        }
        if entry.args.len() > MAX_MCP_ARGS {
            issues.at(
                &format!("{key}.args"),
                name_span.clone(),
                format!("{} arguments: at most {MAX_MCP_ARGS}", entry.args.len()),
            );
        }
        for (a, arg) in entry.args.iter().enumerate() {
            if arg.len() > MAX_MCP_STRING_BYTES || arg.contains('\0') {
                issues.at(
                    &format!("{key}.args[{a}]"),
                    name_span.clone(),
                    format!("must be text of at most {MAX_MCP_STRING_BYTES} bytes without NUL"),
                );
            }
        }
        if entry.env.len() + entry.env_from.len() > MAX_MCP_ENV {
            issues.at(
                &format!("{key}.env"),
                name_span.clone(),
                format!("at most {MAX_MCP_ENV} environment variables in `env` and `env_from`"),
            );
        }
        for (var, value) in &entry.env {
            if !env_name_ok(var) {
                issues.at(
                    &format!("{key}.env.{var}"),
                    name_span.clone(),
                    "is not an environment variable name (letters, digits and `_`, not \
                     starting with a digit)"
                        .to_owned(),
                );
            } else if value.len() > MAX_MCP_STRING_BYTES || value.contains('\0') {
                issues.at(
                    &format!("{key}.env.{var}"),
                    name_span.clone(),
                    format!("must be text of at most {MAX_MCP_STRING_BYTES} bytes without NUL"),
                );
            }
        }
        for (var, source) in &entry.env_from {
            if !env_name_ok(var) {
                issues.at(
                    &format!("{key}.env_from.{var}"),
                    source.span(),
                    "is not an environment variable name (letters, digits and `_`, not \
                     starting with a digit)"
                        .to_owned(),
                );
            } else if entry.env.contains_key(var) {
                issues.at(
                    &format!("{key}.env_from.{var}"),
                    source.span(),
                    format!("`{var}` is also set in `env`"),
                );
            }
        }
    } else {
        if !entry.args.is_empty() {
            issues.at(
                &format!("{key}.args"),
                name_span.clone(),
                "applies only to a `command` server".to_owned(),
            );
        }
        if !entry.env.is_empty() {
            issues.at(
                &format!("{key}.env"),
                name_span.clone(),
                "applies only to a `command` server".to_owned(),
            );
        }
        if let Some((var, source)) = entry.env_from.iter().next() {
            issues.at(
                &format!("{key}.env_from.{var}"),
                source.span(),
                "applies only to a `command` server".to_owned(),
            );
        }
    }
    if let Some(url) = &entry.url {
        let text = url.get_ref().as_str();
        // The URL is shown in logs and errors (the SDK puts it into its messages): nothing
        // secret may be in it.
        if text.contains(['?', '#']) {
            issues.at(
                &format!("{key}.url"),
                url.span(),
                "must not have a query or a fragment (put a token in `bearer_env`)".to_owned(),
            );
        }
        let allowed = entry.allow_insecure.unwrap_or(false);
        if url.get_ref().is_remote_plain_http()
            && !allowed
            && !brisvox_agent_link::security::host_is_private(text)
        {
            issues.at(
                &format!("{key}.url"),
                url.span(),
                "plain http:// to a host outside the private networks would send what the caller \
                 says (tool arguments and results) unencrypted; use https://, or set \
                 `allow_insecure = true` on this server"
                    .to_owned(),
            );
        }
    } else if entry.allow_insecure.is_some() {
        issues.at(
            &format!("{key}.allow_insecure"),
            entry.name.span(),
            "applies only to a `url` server".to_owned(),
        );
    }
}

/// Whether `s` has a NUL or a line break (a command is one line).
fn has_nul_or_newline(s: &str) -> bool {
    s.contains(['\0', '\n', '\r'])
}

/// The allowlist of one server, `read_only`, `confirm` and the names the LLM will see.
fn check_tools(
    entry: &McpServerEntry,
    key: &str,
    issues: &mut Issues<'_>,
    taken: &mut BTreeSet<String>,
    total: &mut usize,
) {
    if entry.tools.is_empty() {
        issues.at(
            &format!("{key}.tools"),
            entry.name.span(),
            "must list the tools to offer: a server's tools are only reachable through this \
             allowlist"
                .to_owned(),
        );
    }
    if entry.tools.len() > MAX_MCP_TOOLS_PER_SERVER {
        issues.at(
            &format!("{key}.tools"),
            entry.name.span(),
            format!(
                "{} tools: at most {MAX_MCP_TOOLS_PER_SERVER} per server",
                entry.tools.len()
            ),
        );
    }
    *total += entry.tools.len();
    if let Some(prefix) = &entry.prefix
        && let McpPrefix::Text(text) = prefix.get_ref()
        && !short_name_ok(text)
    {
        issues.at(
            &format!("{key}.prefix"),
            prefix.span(),
            "must be `true`, `false` or 1 to 32 letters, digits, `_` or `-`".to_owned(),
        );
    }
    let prefix = prefix_of(entry);
    let mut listed: BTreeSet<&str> = BTreeSet::new();
    for (t, tool) in entry.tools.iter().enumerate() {
        let tool_key = format!("{key}.tools[{t}]");
        let name = tool.get_ref();
        if !mcp_tool_name_ok(name) {
            issues.at(
                &tool_key,
                tool.span(),
                format!(
                    "`{}` is not an MCP tool name (1 to {MAX_MCP_TOOL_NAME_LEN} letters, digits, \
                     `_`, `-` or `.`)",
                    name.escape_debug()
                ),
            );
            continue;
        }
        if !listed.insert(name.as_str()) {
            issues.at(
                &tool_key,
                tool.span(),
                format!("tool `{name}` is listed twice"),
            );
            continue;
        }
        let exposed = exposed_name(&prefix, name);
        if exposed.len() > MAX_TOOL_NAME_LEN {
            issues.at(
                &tool_key,
                tool.span(),
                format!(
                    "the name the LLM would see, `{exposed}`, is longer than {MAX_TOOL_NAME_LEN} \
                     characters; shorten the prefix"
                ),
            );
        } else if !taken.insert(exposed.clone()) {
            issues.at(
                &tool_key,
                tool.span(),
                format!(
                    "the name the LLM would see, `{exposed}`, is already taken by another tool; \
                     set `prefix` on this server"
                ),
            );
        }
    }
    let mut subset = |field: &str, list: &[Spanned<String>], other: &[Spanned<String>]| {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for (n, item) in list.iter().enumerate() {
            let item_key = format!("{key}.{field}[{n}]");
            let name = item.get_ref();
            if !listed.contains(name.as_str()) {
                issues.at(
                    &item_key,
                    item.span(),
                    format!("tool `{name}` is not in this server's `tools`"),
                );
            } else if !seen.insert(name.as_str()) {
                issues.at(
                    &item_key,
                    item.span(),
                    format!("tool `{name}` is listed twice"),
                );
            } else if other.iter().any(|o| o.get_ref() == name) {
                issues.at(
                    &item_key,
                    item.span(),
                    format!(
                        "tool `{name}` is in both `read_only` and `confirm`: a tool that only \
                         reads has nothing to confirm"
                    ),
                );
            }
        }
    };
    subset("read_only", &entry.read_only, &entry.confirm);
    subset("confirm", &entry.confirm, &entry.read_only);
}

/// The checked `[[mcp.servers]]` with their secrets read (`bearer_env`, `env_from`). The file
/// passed [`check`] first, so this only reads the environment; a variable that is missing is
/// recorded as an issue.
pub(crate) fn resolve(file: &AgentFile, env: &dyn Env, issues: &mut Issues<'_>) -> McpSettings {
    let mut servers = Vec::with_capacity(file.mcp.servers.len());
    for (i, entry) in file.mcp.servers.iter().enumerate() {
        let key = format!("mcp.servers[{i}]");
        let transport = match (&entry.command, &entry.url) {
            (Some(command), None) => {
                let mut vars: Vec<(String, Secret<String>)> = entry
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), Secret::new(v.clone())))
                    .collect();
                for (var, source) in &entry.env_from {
                    if let Some(value) =
                        read_env_from(env, issues, &format!("{key}.env_from.{var}"), source)
                    {
                        vars.push((var.clone(), value));
                    }
                }
                McpTransportSettings::Stdio {
                    command: command.get_ref().clone(),
                    args: entry.args.clone(),
                    env: vars,
                }
            }
            (None, Some(url)) => McpTransportSettings::Http {
                url: url.get_ref().as_str().to_owned(),
                bearer: entry.bearer_env.as_ref().and_then(|name| {
                    read_env_from(env, issues, &format!("{key}.bearer_env"), name)
                        .map(|s| Secret::new(s.expose().trim().to_owned()))
                }),
            },
            // `check` reported it; the settings are never used.
            _ => continue,
        };
        let prefix = prefix_of(entry);
        let tools = entry
            .tools
            .iter()
            .enumerate()
            .map(|(t, tool)| McpToolSettings {
                name: tool.get_ref().clone(),
                exposed: exposed_name(&prefix, tool.get_ref()),
                read_only: entry
                    .read_only
                    .iter()
                    .any(|r| r.get_ref() == tool.get_ref()),
                confirm: entry.confirm.iter().any(|c| c.get_ref() == tool.get_ref()),
                span: tool.span(),
                key: format!("{key}.tools[{t}]"),
            })
            .collect();
        servers.push(McpServerSettings {
            name: entry.name.get_ref().clone(),
            transport,
            tools,
            timeout: entry
                .timeout_ms
                .as_ref()
                .map_or(DEFAULT_MCP_TIMEOUT, |t| t.get_ref().duration()),
            filler_after: entry
                .filler_after_ms
                .as_ref()
                .map(|t| t.get_ref().duration()),
            startup_timeout: entry
                .startup_timeout_ms
                .as_ref()
                .map_or(DEFAULT_MCP_STARTUP_TIMEOUT, |t| t.get_ref().duration()),
            max_in_flight: entry
                .max_in_flight
                .as_ref()
                .map_or(DEFAULT_MCP_MAX_IN_FLIGHT, |n| n.get_ref().as_usize()),
            span: entry.name.span(),
            key,
        });
    }
    McpSettings { servers }
}

/// The value of the server variable `name` for the key `key`: a missing, empty or non-UTF-8
/// variable is recorded as an issue (the value never appears in a message).
fn read_env_from(
    env: &dyn Env,
    issues: &mut Issues<'_>,
    key: &str,
    name: &Spanned<EnvVarName>,
) -> Option<Secret<String>> {
    let var = name.get_ref();
    let problem = match env.var(var.as_str()) {
        Ok(value) => match value.trim_end_matches(['\n', '\r']) {
            "" => "is set but empty",
            kept => return Some(Secret::new(kept.to_owned())),
        },
        Err(VarError::NotPresent) => "is not set",
        Err(VarError::NotUnicode(_)) => "is not valid UTF-8",
    };
    issues.at(
        key,
        name.span(),
        format!(
            "environment variable {} {problem}; the MCP server's secrets are read from the \
             environment, never from the file",
            var.shown()
        ),
    );
    None
}
