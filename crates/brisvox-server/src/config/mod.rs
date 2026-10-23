//! Agent definitions (`docs/CONFIG.md`): one TOML or YAML file per agent describes who
//! the agent is (system prompt, language, tools by name), how turns and barge-ins are judged, and
//! one provider per stage (STT, LLM, TTS) with its options.
//!
//! # Loading
//! [`load_file`] (or [`load_str`] / [`load_str_as`]) reads a definition in three steps, and every
//! problem it finds names the file, the key path and the line and column ([`ConfigError`],
//! [`Issue`]):
//! 1. **Syntax and types.** Every table rejects unknown keys; values are range-checked while they
//!    are read ([`types`]). The first such error stops reading.
//! 2. **Cross-field checks.** Rules between values: a provider whose cargo feature is not
//!    compiled in, an options table for a provider that is not the chosen one, a language the
//!    chosen provider does not speak (Turkish on Deepgram Aura), a sample rate the provider does
//!    not deliver, settings of one turn policy given with the other, early analysis without the
//!    adaptive policy, a frame length that is not a whole number of samples. All are reported
//!    together.
//! 3. **Secrets.** The file never holds a secret: it names the environment variable
//!    (`api_key_env = "DEEPGRAM_API_KEY"`) and loading reads it into a [`Secret`]. A missing,
//!    empty or non-UTF-8 variable is an error naming the variable and the key, never a value;
//!    surrounding whitespace (a trailing newline) is trimmed, whitespace inside is an error. A
//!    name that looks like a pasted key is masked in every message.
//!
//! The result, [`AgentConfig`], maps the definition onto the engine's [`EngineConfig`] and the
//! provider adapters' configuration structs with pure functions ([`map`]).
//!
//! # Formats
//! TOML is the reference format. YAML (`.yaml`/`.yml`, cargo feature `yaml`, on by default) is a
//! second front-end over the same [`AgentFile`]: the same keys, checks and
//! messages, with positions in the YAML text ([`Format`], module `yaml`).
//!
//! # Blocking
//! [`load_file`] reads a file synchronously: call it at startup or from
//! `tokio::task::spawn_blocking`, never on a tokio worker.

pub mod error;
#[cfg(feature = "schema")]
pub mod json_schema;
pub mod map;
pub mod mcp;
pub mod schema;
pub mod types;
mod validate;
#[cfg(feature = "yaml")]
mod yaml;

use std::collections::{BTreeMap, HashMap};
use std::env::VarError;
use std::fmt;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use brisvox_agent::Language;
use brisvox_agent::telephony::TelephonyPolicy;
use brisvox_agent::tools::ToolRegistry;
use brisvox_core::Secret;
use brisvox_engine::EngineConfig;
use brisvox_engine::session::SessionBuilder;
use brisvox_providers::{LlmProvider, SttProvider, TtsProvider};
use toml::Spanned;

use crate::privacy::{PiiKinds, RecordingFileSettings};

pub use error::{ConfigError, Issue};
pub use mcp::{
    MAX_MCP_SERVERS, MAX_MCP_TOOLS, McpServerSettings, McpSettings, McpToolSettings,
    McpTransportSettings, exposed_name,
};
pub use schema::VoicemailUnknown;
use schema::{
    AgentFile, LlmProviderKind, LlmSection, SttProviderKind, SttSection, TtsProviderKind,
    TtsSection,
};
use types::{EnvVarName, LanguageTag};
pub use validate::{
    ENGINE_LANGUAGES, MAX_AGENT_ROUTES, MAX_SYSTEM_PROMPT_BYTES, MAX_TERM_CHARS, MAX_TERMS,
    MAX_VOICEMAIL_CHARS,
};

/// The `[agent_link]` section of a definition with its tokens read.
#[derive(Debug, Clone, Default)]
pub struct AgentLinkSettings {
    /// The definition's own agent process URL (wins over `BRISVOX_AGENT_URL`).
    pub url: Option<String>,
    /// The token for `url` (`token_env`); `None`: the server-wide default token.
    pub token: Option<Secret<String>>,
    /// Plaintext `ws://` to a non-private host accepted (`allow_insecure`).
    pub allow_insecure: bool,
    /// The definition's own `caller_identity` choice; `None`: `BRISVOX_AGENT_CALLER_IDENTITY`.
    pub caller_identity: Option<bool>,
    /// The named agent processes a start message may pick.
    pub routes: Vec<AgentRouteSettings>,
}

/// One `[[agent_link.routes]]` entry with its token read.
#[derive(Debug, Clone)]
pub struct AgentRouteSettings {
    /// The route name.
    pub name: String,
    /// The agent process URL.
    pub url: String,
    /// This route's token (never the default one).
    pub token: Option<Secret<String>>,
    /// Plaintext `ws://` to a non-private host accepted for this route.
    pub allow_insecure: bool,
    /// The route's agent process gets who is on the line (never inherited).
    pub caller_identity: bool,
}

/// The `[telephony.voicemail]` section of a definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoicemailSettings {
    /// The message left on a machine (trimmed, validated).
    pub message: String,
    /// What to do when the detector cannot tell.
    pub on_unknown: VoicemailUnknown,
    /// The detector never reports the end of a greeting: hang up on machines.
    pub detect_only: bool,
}

/// Largest accepted definition file (1 MiB): a definition is a few kilobytes; a bigger file is
/// a mistake, and the limit bounds the memory a load can take.
pub const MAX_FILE_BYTES: u64 = 1 << 20;

/// A provider adapter an agent definition can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    /// Deepgram STT.
    Deepgram,
    /// AssemblyAI STT.
    AssemblyAi,
    /// Gladia STT.
    Gladia,
    /// sherpa-onnx streaming STT (self-hosted).
    SherpaOnnx,
    /// OpenAI-Realtime-transcription STT (OpenAI, NVIDIA Speech NIM, Speaches, vLLM).
    OpenAiRealtimeStt,
    /// OpenAI LLM.
    OpenAi,
    /// Anthropic LLM.
    Anthropic,
    /// Gemini LLM.
    Gemini,
    /// OpenAI-compatible LLM servers.
    OpenAiCompatible,
    /// Cartesia TTS.
    Cartesia,
    /// ElevenLabs TTS.
    ElevenLabs,
    /// Deepgram Aura TTS.
    DeepgramAura,
    /// A local server speaking the OpenAI `/audio/speech` protocol (TTS).
    OpenAiSpeech,
}

impl Provider {
    /// The name used in definition files (`stt.provider = "deepgram"`, and the options table
    /// `[stt.deepgram]`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Deepgram => "deepgram",
            Self::AssemblyAi => "assemblyai",
            Self::Gladia => "gladia",
            Self::SherpaOnnx => "sherpa_onnx",
            Self::OpenAiRealtimeStt => "openai_realtime_stt",
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
            Self::OpenAiCompatible => "openai_compatible",
            Self::Cartesia => "cartesia",
            Self::ElevenLabs => "elevenlabs",
            Self::DeepgramAura => "deepgram_aura",
            Self::OpenAiSpeech => "openai_speech",
        }
    }

    /// The `brisvox-server` cargo feature that compiles the adapter in.
    pub const fn feature(self) -> &'static str {
        match self {
            Self::Deepgram => "provider-deepgram",
            Self::AssemblyAi => "provider-assemblyai",
            Self::Gladia => "provider-gladia",
            Self::SherpaOnnx => "provider-sherpa-onnx",
            Self::OpenAiRealtimeStt => "provider-openai-realtime-stt",
            Self::OpenAi => "provider-openai",
            Self::Anthropic => "provider-anthropic",
            Self::Gemini => "provider-gemini",
            Self::OpenAiCompatible => "provider-openai-compatible",
            Self::Cartesia => "provider-cartesia",
            Self::ElevenLabs => "provider-elevenlabs",
            Self::DeepgramAura => "provider-deepgram-aura",
            Self::OpenAiSpeech => "provider-openai-speech",
        }
    }

    /// Whether this binary was built with the adapter.
    pub const fn compiled_in(self) -> bool {
        match self {
            Self::Deepgram => cfg!(feature = "provider-deepgram"),
            Self::AssemblyAi => cfg!(feature = "provider-assemblyai"),
            Self::Gladia => cfg!(feature = "provider-gladia"),
            Self::SherpaOnnx => cfg!(feature = "provider-sherpa-onnx"),
            Self::OpenAiRealtimeStt => cfg!(feature = "provider-openai-realtime-stt"),
            Self::OpenAi => cfg!(feature = "provider-openai"),
            Self::Anthropic => cfg!(feature = "provider-anthropic"),
            Self::Gemini => cfg!(feature = "provider-gemini"),
            Self::OpenAiCompatible => cfg!(feature = "provider-openai-compatible"),
            Self::Cartesia => cfg!(feature = "provider-cartesia"),
            Self::ElevenLabs => cfg!(feature = "provider-elevenlabs"),
            Self::DeepgramAura => cfg!(feature = "provider-deepgram-aura"),
            Self::OpenAiSpeech => cfg!(feature = "provider-openai-speech"),
        }
    }

    /// The conventional environment variable of the provider's key (used in error hints and
    /// the examples).
    pub const fn example_env(self) -> &'static str {
        match self {
            Self::Deepgram | Self::DeepgramAura => "DEEPGRAM_API_KEY",
            Self::AssemblyAi => "ASSEMBLYAI_API_KEY",
            Self::Gladia => "GLADIA_API_KEY",
            Self::SherpaOnnx => "SHERPA_ONNX_TOKEN",
            Self::OpenAiRealtimeStt | Self::OpenAi => "OPENAI_API_KEY",
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::Gemini => "GEMINI_API_KEY",
            Self::OpenAiCompatible => "LLM_API_KEY",
            Self::Cartesia => "CARTESIA_API_KEY",
            Self::ElevenLabs => "ELEVENLABS_API_KEY",
            Self::OpenAiSpeech => "TTS_API_KEY",
        }
    }
}

impl From<SttProviderKind> for Provider {
    fn from(kind: SttProviderKind) -> Self {
        match kind {
            SttProviderKind::Deepgram => Self::Deepgram,
            SttProviderKind::Assemblyai => Self::AssemblyAi,
            SttProviderKind::Gladia => Self::Gladia,
            SttProviderKind::SherpaOnnx => Self::SherpaOnnx,
            SttProviderKind::OpenaiRealtimeStt => Self::OpenAiRealtimeStt,
        }
    }
}

impl From<LlmProviderKind> for Provider {
    fn from(kind: LlmProviderKind) -> Self {
        match kind {
            LlmProviderKind::Openai => Self::OpenAi,
            LlmProviderKind::Anthropic => Self::Anthropic,
            LlmProviderKind::Gemini => Self::Gemini,
            LlmProviderKind::OpenaiCompatible => Self::OpenAiCompatible,
        }
    }
}

impl From<TtsProviderKind> for Provider {
    fn from(kind: TtsProviderKind) -> Self {
        match kind {
            TtsProviderKind::Cartesia => Self::Cartesia,
            TtsProviderKind::Elevenlabs => Self::ElevenLabs,
            TtsProviderKind::DeepgramAura => Self::DeepgramAura,
            TtsProviderKind::OpenaiSpeech => Self::OpenAiSpeech,
        }
    }
}

/// Where secrets come from: the process environment in production ([`ProcessEnv`]), a map in
/// tests.
pub trait Env {
    /// The value of variable `name`.
    ///
    /// # Errors
    /// [`VarError::NotPresent`] when it is not set, [`VarError::NotUnicode`] when it is not
    /// UTF-8.
    fn var(&self, name: &str) -> Result<String, VarError>;
}

/// The process environment (`std::env::var`).
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn var(&self, name: &str) -> Result<String, VarError> {
        std::env::var(name)
    }
}

impl<S: std::hash::BuildHasher> Env for HashMap<String, String, S> {
    fn var(&self, name: &str) -> Result<String, VarError> {
        self.get(name).cloned().ok_or(VarError::NotPresent)
    }
}

impl Env for BTreeMap<String, String> {
    fn var(&self, name: &str) -> Result<String, VarError> {
        self.get(name).cloned().ok_or(VarError::NotPresent)
    }
}

/// Reads and validates the agent definition at `path`, with secrets from `env`. Blocking file
/// IO: call at startup or from `spawn_blocking`.
///
/// # Errors
/// [`ConfigError`] when the file cannot be read, is larger than [`MAX_FILE_BYTES`], or the
/// definition is invalid (see the [module docs](self)).
pub fn load_file(path: &Path, env: &dyn Env) -> Result<AgentConfig, ConfigError> {
    let name = path.display().to_string();
    let text = read_bounded(path).map_err(|message| ConfigError::single(&name, "", message))?;
    load_str_as(&text, &name, Format::from_path(path), env)
}

/// Source name of an agent definition that came from the environment (`BRISVOX_AGENT_DEFINITION`),
/// as it appears in error messages: `$BRISVOX_AGENT_DEFINITION:3:1: ...`.
pub const INLINE_SOURCE_NAME: &str = "$BRISVOX_AGENT_DEFINITION";

/// Validates the agent definition `text` that came from the environment variable
/// `BRISVOX_AGENT_DEFINITION`: the loader of [`load_file`] (same size limit, checks and messages)
/// with [`INLINE_SOURCE_NAME`] as the source name.
///
/// # Errors
/// [`ConfigError`] when the text is larger than [`MAX_FILE_BYTES`] or the definition is invalid.
pub fn load_inline(text: &str, format: Format, env: &dyn Env) -> Result<AgentConfig, ConfigError> {
    check_inline_size(text)
        .map_err(|message| ConfigError::single(INLINE_SOURCE_NAME, "", message))?;
    load_str_as(text, INLINE_SOURCE_NAME, format, env)
}

/// The size limit of [`read_bounded`] for a definition held in memory.
pub(crate) fn check_inline_size(text: &str) -> Result<(), String> {
    if u64::try_from(text.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
        return Err(format!(
            "the definition is larger than {MAX_FILE_BYTES} bytes (an agent definition is a few kilobytes)"
        ));
    }
    Ok(())
}

/// The syntax of an agent definition. Both read into the same [`AgentFile`] with the same checks
/// and messages; positions refer to the file as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Format {
    /// TOML 1.1, the reference format.
    #[default]
    Toml,
    /// YAML 1.2 (core schema; no anchors, aliases or tags). Needs the `yaml` cargo feature.
    Yaml,
}

impl Format {
    /// The format of a file by its extension: `.yaml` and `.yml` (any case) are YAML, everything
    /// else is TOML.
    pub fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml") => {
                Self::Yaml
            }
            _ => Self::Toml,
        }
    }

    /// The format named by a setting (`toml`, `yaml` or `yml`, any case); `None` for anything else.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "toml" => Some(Self::Toml),
            "yaml" | "yml" => Some(Self::Yaml),
            _ => None,
        }
    }

    /// Whether this build reads the format (YAML needs the `yaml` feature).
    pub const fn compiled_in(self) -> bool {
        match self {
            Self::Toml => true,
            Self::Yaml => cfg!(feature = "yaml"),
        }
    }
}

/// Reads at most [`MAX_FILE_BYTES`] of UTF-8 text from `path`.
pub(crate) fn read_bounded(path: &Path) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("cannot open the file: {e}"))?;
    let mut text = String::new();
    file.take(MAX_FILE_BYTES.saturating_add(1))
        .read_to_string(&mut text)
        .map_err(|e| format!("cannot read the file as UTF-8 text: {e}"))?;
    if u64::try_from(text.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
        return Err(format!(
            "the file is larger than {MAX_FILE_BYTES} bytes (an agent definition is a few \
             kilobytes)"
        ));
    }
    Ok(text)
}

/// Validates the agent definition `text`; `source_name` (a path, or e.g. `<inline>`) is used in
/// error messages.
///
/// # Errors
/// [`ConfigError`] listing every problem (see the [module docs](self)).
pub fn load_str(text: &str, source_name: &str, env: &dyn Env) -> Result<AgentConfig, ConfigError> {
    load_str_as(text, source_name, Format::Toml, env)
}

/// [`load_str`] for a definition in `format`.
///
/// # Errors
/// [`ConfigError`] listing every problem (see the [module docs](self)); a YAML definition in a
/// build without the `yaml` feature is an error naming the feature.
pub fn load_str_as(
    text: &str,
    source_name: &str,
    format: Format,
    env: &dyn Env,
) -> Result<AgentConfig, ConfigError> {
    let agent = load_with(text, source_name, format, env, &Provider::compiled_in)?;
    mcp::require_compiled(&agent, cfg!(feature = "mcp"))?;
    Ok(agent)
}

/// Reads the TOML `text` into an [`AgentFile`]: syntax, types, ranges and unknown keys only (no
/// cross-field checks, no secrets).
///
/// # Errors
/// [`ConfigError`] with the first such problem.
pub fn parse(text: &str, source_name: &str) -> Result<AgentFile, ConfigError> {
    parse_as(text, source_name, Format::Toml)
}

/// [`parse`] for a definition in `format`.
///
/// # Errors
/// [`ConfigError`] with the first problem; a YAML definition in a build without the `yaml`
/// feature is an error naming the feature.
pub fn parse_as(text: &str, source_name: &str, format: Format) -> Result<AgentFile, ConfigError> {
    let issue = match format {
        Format::Toml => match toml::from_str::<AgentFile>(text) {
            Ok(file) => return Ok(file),
            Err(e) => toml_issue(text, &e),
        },
        Format::Yaml => match parse_yaml(text) {
            Ok(file) => return Ok(file),
            Err(issue) => issue,
        },
    };
    Err(ConfigError {
        source_name: source_name.to_owned(),
        issues: vec![issue],
    })
}

/// Reads a YAML definition (module `yaml`).
#[cfg(feature = "yaml")]
fn parse_yaml(text: &str) -> Result<AgentFile, Issue> {
    yaml::parse(text)
}

/// Reads a YAML definition: not compiled into this build.
#[cfg(not(feature = "yaml"))]
fn parse_yaml(_text: &str) -> Result<AgentFile, Issue> {
    Err(Issue {
        key: String::new(),
        position: None,
        message: "YAML agent definitions need the `yaml` feature of brisvox-server; this build \
                  reads TOML only"
            .to_owned(),
    })
}

/// A read error message with the hints both formats give: a literal `api_key` is pointed at
/// `api_key_env`.
fn with_hints(mut message: String) -> String {
    if message.contains("unknown field `api_key`") {
        message.push_str(
            "; secrets never go in the file: name the environment variable with `api_key_env`",
        );
    }
    message
}

/// An [`Issue`] for a TOML read error: its position and the key path at that position.
fn toml_issue(text: &str, error: &toml::de::Error) -> Issue {
    let span = error.span();
    let key = span
        .as_ref()
        .and_then(|s| error::key_path_at(text, s.start))
        .unwrap_or_default();
    Issue {
        key,
        position: span.map(|s| error::line_column(text, s.start)),
        message: with_hints(error.message().trim_end().to_owned()),
    }
}

/// [`load_str`] with an injectable "is this adapter compiled in" predicate.
fn load_with(
    text: &str,
    source_name: &str,
    format: Format,
    env: &dyn Env,
    compiled: &dyn Fn(Provider) -> bool,
) -> Result<AgentConfig, ConfigError> {
    let file = parse_as(text, source_name, format)?;
    let mut issues = validate::Issues::new(text);
    validate::check(&file, &mut issues, compiled);

    let stt_key = file
        .stt
        .api_key_env
        .as_ref()
        .map(|name| secret(env, &mut issues, "stt.api_key_env", name));
    let llm_key = file
        .llm
        .api_key_env
        .as_ref()
        .map(|name| secret(env, &mut issues, "llm.api_key_env", name));
    let tts_key = file
        .tts
        .api_key_env
        .as_ref()
        .map(|name| secret(env, &mut issues, "tts.api_key_env", name));
    // The secondaries' keys: a missing one fails the start like a primary's.
    let secondary_keys = SecondaryKeys {
        stt: file
            .stt
            .secondary
            .as_ref()
            .and_then(|s| s.get_ref().api_key_env.as_ref())
            .and_then(|name| secret(env, &mut issues, "stt.secondary.api_key_env", name)),
        llm: file
            .llm
            .secondary
            .as_ref()
            .and_then(|s| s.get_ref().api_key_env.as_ref())
            .and_then(|name| secret(env, &mut issues, "llm.secondary.api_key_env", name)),
        tts: file
            .tts
            .secondary
            .as_ref()
            .and_then(|s| s.get_ref().api_key_env.as_ref())
            .and_then(|name| secret(env, &mut issues, "tts.secondary.api_key_env", name)),
    };

    // The agent link's tokens: a named but missing one fails the start.
    let agent_link = AgentLinkSettings {
        url: file.agent_link.url.as_ref().map(|u| u.get_ref().clone()),
        token: file
            .agent_link
            .token_env
            .as_ref()
            .and_then(|name| secret(env, &mut issues, "agent_link.token_env", name)),
        allow_insecure: file.agent_link.allow_insecure.unwrap_or(false),
        caller_identity: file.agent_link.caller_identity,
        routes: file
            .agent_link
            .routes
            .iter()
            .enumerate()
            .map(|(i, route)| AgentRouteSettings {
                name: route.name.get_ref().clone(),
                url: route.url.get_ref().clone(),
                allow_insecure: route.allow_insecure.unwrap_or(false),
                caller_identity: route.caller_identity.unwrap_or(false),
                token: route.token_env.as_ref().and_then(|name| {
                    secret(
                        env,
                        &mut issues,
                        &format!("agent_link.routes[{i}].token_env"),
                        name,
                    )
                }),
            })
            .collect(),
    };

    // The MCP servers' secrets (`bearer_env`, `env_from`): a missing variable fails the start.
    let mcp = mcp::resolve(&file, env, &mut issues);

    let engine = map::engine_config(&file);
    if issues.found.is_empty()
        && let Err(e) = engine.validate()
    {
        issues.general("", format!("the engine rejects the settings: {e}"));
    }
    if !issues.found.is_empty() {
        return Err(ConfigError {
            source_name: source_name.to_owned(),
            issues: issues.into_sorted(),
        });
    }
    let llm_key = match llm_key {
        Some(Some(key)) => Some(key),
        _ => None,
    };
    // Every key is optional at this point (a required one that is missing was recorded as an
    // issue above): keyless local providers (local TTS and STT) have none.
    let stt_key = match stt_key {
        Some(Some(key)) => Some(key),
        _ => None,
    };
    let tts_key = match tts_key {
        Some(Some(key)) => Some(key),
        _ => None,
    };
    Ok(AgentConfig {
        language: map::engine_language(file.agent.language.get_ref()),
        engine,
        stt_key,
        llm_key,
        tts_key,
        secondary_keys,
        agent_link,
        mcp,
        text: text.into(),
        source_name: source_name.to_owned(),
        file,
    })
}

/// Reads the secret named at `name` (the value of key `key`), recording a problem instead.
fn secret(
    env: &dyn Env,
    issues: &mut validate::Issues<'_>,
    key: &str,
    name: &Spanned<EnvVarName>,
) -> Option<Secret<String>> {
    let var = name.get_ref();
    let problem = match env.var(var.as_str()) {
        // Surrounding whitespace (the trailing newline of an env-file or a mounted secret file)
        // is never part of a key: it would make the auth header invalid at connect time.
        Ok(value) => match value.trim() {
            "" => "is set but empty",
            trimmed if trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) => {
                "contains spaces or control characters inside the value"
            }
            trimmed => return Some(Secret::new(trimmed.to_owned())),
        },
        Err(VarError::NotPresent) => "is not set",
        Err(VarError::NotUnicode(_)) => "is not valid UTF-8",
    };
    issues.at(
        key,
        name.span(),
        format!(
            "environment variable {} (named by {key}) {problem}; secrets are read from the \
             environment, never from the file",
            var.shown()
        ),
    );
    None
}

/// A validated agent definition with its secrets, ready to configure a session.
///
/// `Debug` never prints a key (they are [`Secret`]s).
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// The definition as read.
    file: AgentFile,
    /// The engine configuration it maps to (validated).
    engine: EngineConfig,
    /// The conversation language.
    language: Language,
    /// The STT key (`None` for a self-hosted server without one).
    stt_key: Option<Secret<String>>,
    /// The LLM key (`None` only for an `openai_compatible` server without one).
    llm_key: Option<Secret<String>>,
    /// The TTS key (`None` only for an `openai_speech` server without one).
    tts_key: Option<Secret<String>>,
    /// The keys of the secondary providers (`[stt|llm|tts].secondary`).
    secondary_keys: SecondaryKeys,
    /// The `[agent_link]` section with its tokens.
    agent_link: AgentLinkSettings,
    /// The `[[mcp.servers]]` with their secrets.
    mcp: McpSettings,
    /// The file text (positions of later checks such as [`Self::check_tools`]).
    text: Arc<str>,
    /// Where the definition came from.
    source_name: String,
}

/// The API keys of the secondary providers of a definition. A key is `None` when the
/// stage has no secondary, or (LLM) its secondary needs none.
#[derive(Debug, Clone, Default)]
struct SecondaryKeys {
    stt: Option<Secret<String>>,
    llm: Option<Secret<String>>,
    tts: Option<Secret<String>>,
}

/// The STT adapter configuration of a definition. One variant per adapter compiled in.
#[non_exhaustive]
pub enum SttProviderConfig {
    /// Deepgram.
    #[cfg(feature = "provider-deepgram")]
    Deepgram(brisvox_providers::deepgram::DeepgramConfig),
    /// AssemblyAI.
    #[cfg(feature = "provider-assemblyai")]
    AssemblyAi(brisvox_providers::assemblyai::AssemblyAiConfig),
    /// Gladia.
    #[cfg(feature = "provider-gladia")]
    Gladia(brisvox_providers::gladia::GladiaConfig),
    /// A sherpa-onnx streaming server.
    #[cfg(feature = "provider-sherpa-onnx")]
    SherpaOnnx(brisvox_providers::sherpa_onnx::SherpaOnnxConfig),
    /// A server speaking the OpenAI Realtime transcription protocol.
    #[cfg(feature = "provider-openai-realtime-stt")]
    OpenAiRealtimeStt(brisvox_providers::openai_realtime_stt::OpenAiRealtimeSttConfig),
}

/// The LLM adapter configuration of a definition. One variant per adapter compiled in.
#[non_exhaustive]
pub enum LlmProviderConfig {
    /// OpenAI.
    #[cfg(feature = "provider-openai")]
    OpenAi(brisvox_providers::openai::OpenAiConfig),
    /// Anthropic.
    #[cfg(feature = "provider-anthropic")]
    Anthropic(brisvox_providers::anthropic::AnthropicConfig),
    /// Gemini.
    #[cfg(feature = "provider-gemini")]
    Gemini(brisvox_providers::gemini::GeminiConfig),
    /// An OpenAI-compatible server.
    #[cfg(feature = "provider-openai-compatible")]
    OpenAiCompatible(brisvox_providers::openai_compatible::OpenAiCompatibleConfig),
}

/// The TTS adapter configuration of a definition. One variant per adapter compiled in.
#[non_exhaustive]
pub enum TtsProviderConfig {
    /// Cartesia.
    #[cfg(feature = "provider-cartesia")]
    Cartesia(brisvox_providers::cartesia::CartesiaConfig),
    /// ElevenLabs.
    #[cfg(feature = "provider-elevenlabs")]
    ElevenLabs(brisvox_providers::elevenlabs::ElevenLabsConfig),
    /// Deepgram Aura.
    #[cfg(feature = "provider-deepgram-aura")]
    DeepgramAura(brisvox_providers::deepgram_aura::DeepgramAuraConfig),
    /// A local server speaking the OpenAI `/audio/speech` protocol.
    #[cfg(feature = "provider-openai-speech")]
    OpenAiSpeech(brisvox_providers::openai_speech::OpenAiSpeechConfig),
}

/// `Debug` for the provider configuration enums: the variant only (the configurations hold
/// keys; some have no `Debug` of their own).
macro_rules! variant_debug {
    ($ty:ident { $($variant:ident: $feature:literal),* $(,)? }) => {
        impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                #[allow(unreachable_patterns)]
                match *self {
                    $(
                        #[cfg(feature = $feature)]
                        Self::$variant(_) => f.write_str(concat!(stringify!($ty), "::", stringify!($variant))),
                    )*
                    #[allow(unreachable_patterns)]
                    _ => f.write_str(stringify!($ty)),
                }
            }
        }
    };
}

variant_debug!(SttProviderConfig {
    Deepgram: "provider-deepgram",
    AssemblyAi: "provider-assemblyai",
    Gladia: "provider-gladia",
    SherpaOnnx: "provider-sherpa-onnx",
    OpenAiRealtimeStt: "provider-openai-realtime-stt",
});
variant_debug!(LlmProviderConfig {
    OpenAi: "provider-openai",
    Anthropic: "provider-anthropic",
    Gemini: "provider-gemini",
    OpenAiCompatible: "provider-openai-compatible",
});
variant_debug!(TtsProviderConfig {
    Cartesia: "provider-cartesia",
    ElevenLabs: "provider-elevenlabs",
    DeepgramAura: "provider-deepgram-aura",
    OpenAiSpeech: "provider-openai-speech",
});

/// A configured provider and the model the calls use on it, as the call summary and the usage
/// metrics name them. Both come from the agent definition, never from a caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderModel {
    /// The provider's name in definition files (`deepgram`, `openai_compatible`).
    pub provider: &'static str,
    /// The model the adapter sends; [`DEFAULT_MODEL`] when the provider picks it.
    pub model: String,
}

/// The model name used where the adapter sends none and the provider's own default applies.
pub const DEFAULT_MODEL: &str = "default";

/// The primary STT, LLM and TTS of an agent definition ([`AgentConfig::provider_models`]). A
/// stage is `None` when a call does not run it as a separate provider (a speech-to-speech
/// call).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderModels {
    /// Speech to text.
    pub stt: Option<ProviderModel>,
    /// The language model.
    pub llm: Option<ProviderModel>,
    /// Text to speech.
    pub tts: Option<ProviderModel>,
}

impl SttProviderConfig {
    /// The model the adapter sends (`None`: the server's own).
    fn model(&self) -> Option<String> {
        match self {
            #[cfg(feature = "provider-deepgram")]
            Self::Deepgram(c) => Some(c.model.clone()),
            #[cfg(feature = "provider-assemblyai")]
            Self::AssemblyAi(c) => Some(c.speech_model.clone()),
            #[cfg(feature = "provider-gladia")]
            Self::Gladia(c) => Some(c.model.clone()),
            #[cfg(feature = "provider-sherpa-onnx")]
            Self::SherpaOnnx(_) => None,
            #[cfg(feature = "provider-openai-realtime-stt")]
            Self::OpenAiRealtimeStt(c) => c
                .model
                .clone()
                .or_else(|| c.dialect.default_model().map(str::to_owned)),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }
}

impl LlmProviderConfig {
    /// The model the adapter sends.
    fn model(&self) -> Option<String> {
        match self {
            #[cfg(feature = "provider-openai")]
            Self::OpenAi(c) => Some(c.model.clone()),
            #[cfg(feature = "provider-anthropic")]
            Self::Anthropic(c) => Some(c.model.clone()),
            #[cfg(feature = "provider-gemini")]
            Self::Gemini(c) => Some(c.model.clone()),
            #[cfg(feature = "provider-openai-compatible")]
            Self::OpenAiCompatible(c) => Some(c.model.clone()),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }
}

impl TtsProviderConfig {
    /// The model the adapter sends (`None`: the voice is the model, as with Deepgram Aura).
    fn model(&self) -> Option<String> {
        match self {
            #[cfg(feature = "provider-cartesia")]
            Self::Cartesia(c) => Some(c.model_id.clone()),
            #[cfg(feature = "provider-elevenlabs")]
            Self::ElevenLabs(c) => Some(c.model_id.clone()),
            #[cfg(feature = "provider-deepgram-aura")]
            Self::DeepgramAura(_) => None,
            #[cfg(feature = "provider-openai-speech")]
            Self::OpenAiSpeech(c) => Some(c.model.clone()),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }
}

impl AgentConfig {
    /// The definition as read.
    pub fn definition(&self) -> &AgentFile {
        &self.file
    }

    /// Where the definition came from (as given to the loader).
    pub fn source_name(&self) -> &str {
        &self.source_name
    }

    /// The agent's name.
    pub fn name(&self) -> &str {
        self.file.agent.name.as_str()
    }

    /// The engine configuration (already validated by [`EngineConfig::validate`]).
    pub fn engine(&self) -> EngineConfig {
        self.engine
    }

    /// The conversation language (segmenter and speech normalizer).
    pub fn language(&self) -> Language {
        self.language
    }

    /// The system prompt.
    pub fn system_prompt(&self) -> Arc<str> {
        self.file.agent.system_prompt.get_ref().as_str().into()
    }

    /// The primary providers and models the definition's calls run on (the secondaries a
    /// failover reaches are not named: usage is attributed to the configured primary). A model
    /// the adapter leaves to the provider is [`DEFAULT_MODEL`]; Deepgram Aura's voice is its
    /// model. An adapter that is not compiled into this build keeps its provider name.
    pub fn provider_models(&self) -> ProviderModels {
        let named = |provider: Provider, model: Option<String>| ProviderModel {
            provider: provider.name(),
            model: model.unwrap_or_else(|| DEFAULT_MODEL.to_owned()),
        };
        let stt = named(
            (*self.file.stt.provider.get_ref()).into(),
            self.stt().ok().and_then(|c| c.model()),
        );
        let llm = named(
            (*self.file.llm.provider.get_ref()).into(),
            self.llm().ok().and_then(|c| c.model()),
        );
        let tts_kind = *self.file.tts.provider.get_ref();
        let tts_model = if tts_kind == TtsProviderKind::DeepgramAura {
            Some(self.voice().to_string())
        } else {
            self.tts(
                brisvox_core::SystemClock::shared(),
                tokio_util::sync::CancellationToken::new(),
            )
            .ok()
            .and_then(|c| c.model())
        };
        ProviderModels {
            stt: Some(stt),
            llm: Some(llm),
            tts: Some(named(tts_kind.into(), tts_model)),
        }
    }

    /// The TTS voice id.
    pub fn voice(&self) -> Arc<str> {
        self.file.tts.voice.get_ref().as_str().into()
    }

    /// The tool names the agent may call, in file order.
    pub fn tool_names(&self) -> impl Iterator<Item = &str> + '_ {
        self.file.agent.tools.iter().map(|t| t.get_ref().as_str())
    }

    /// The telephony tools' policy of `[telephony]`: its destinations (validated at load), DTMF
    /// as `telephony.dtmf` says (default on), warm transfers as `telephony.warm` says (default
    /// off). The carrier bindings and the callback listener may switch either off further (the
    /// server only offers a warm transfer when something can finish it).
    pub fn telephony_policy(&self) -> TelephonyPolicy {
        let section = &self.file.telephony;
        let policy = section
            .destinations
            .iter()
            .fold(TelephonyPolicy::new(), |policy, d| {
                // Validation checked every destination; one that fails here is skipped.
                policy
                    .clone()
                    .destination(d.name.get_ref(), d.number.get_ref(), &d.description)
                    .unwrap_or(policy)
            });
        policy
            .with_dtmf(section.dtmf.unwrap_or(true))
            .with_warm(section.warm.unwrap_or(false))
    }

    /// The `[recording]` table as overrides of the environment's recording settings (validated
    /// at load).
    pub fn recording(&self) -> RecordingFileSettings {
        let section = &self.file.recording;
        RecordingFileSettings {
            transcripts: section.transcript,
            audio: section.audio,
            redact_pii: section.redact,
            redact_kinds: section
                .redact_kinds
                .as_ref()
                .map(|kinds| PiiKinds::of(kinds.get_ref().iter().copied())),
            retention: section
                .retention_days
                .as_ref()
                .map(|days| Duration::from_secs(u64::from(*days.get_ref()) * 86_400)),
        }
    }

    /// The `[agent_link]` section: the definition's own agent process and the routes a start
    /// message may pick (empty without the section).
    pub fn agent_link(&self) -> &AgentLinkSettings {
        &self.agent_link
    }

    /// The `[[mcp.servers]]` of the definition (empty without the section): where the MCP
    /// servers are and which of their tools to offer (`docs/TOOLS.md`, "MCP tools").
    pub fn mcp(&self) -> &McpSettings {
        &self.mcp
    }

    /// A [`ConfigError`] about the text at `span` of this definition (`key` is the key path),
    /// for problems that only show when the server starts, such as an MCP server that does not
    /// offer a tool its allowlist names.
    pub fn error_at(
        &self,
        key: &str,
        span: std::ops::Range<usize>,
        message: String,
    ) -> ConfigError {
        let mut issues = validate::Issues::new(&self.text);
        issues.at(key, span, message);
        ConfigError {
            source_name: self.source_name.clone(),
            issues: issues.into_sorted(),
        }
    }

    /// The `[telephony.voicemail]` settings, if the definition has them.
    pub fn voicemail(&self) -> Option<VoicemailSettings> {
        self.file
            .telephony
            .voicemail
            .as_ref()
            .map(|section| VoicemailSettings {
                message: section.message.get_ref().trim().to_owned(),
                on_unknown: section
                    .on_unknown
                    .as_ref()
                    .map_or(VoicemailUnknown::TreatAsHuman, |v| *v.get_ref()),
                detect_only: section.detect_only.unwrap_or(false),
            })
    }

    /// The STT language: `stt.language`, else `agent.language`.
    pub fn stt_language(&self) -> &LanguageTag {
        self.file
            .stt
            .language
            .as_ref()
            .unwrap_or(&self.file.agent.language)
            .get_ref()
    }

    /// The TTS language: `tts.language`, else `agent.language`.
    pub fn tts_language(&self) -> &LanguageTag {
        self.file
            .tts
            .language
            .as_ref()
            .unwrap_or(&self.file.agent.language)
            .get_ref()
    }

    /// Checks that every tool the definition names is registered in `registry`.
    ///
    /// # Errors
    /// [`ConfigError`] naming each unknown tool with its position in the file.
    pub fn check_tools(&self, registry: &ToolRegistry) -> Result<(), ConfigError> {
        let mut issues = validate::Issues::new(&self.text);
        for (i, tool) in self.file.agent.tools.iter().enumerate() {
            if registry.spec(tool.get_ref().as_str()).is_none() {
                issues.at(
                    &format!("agent.tools[{i}]"),
                    tool.span(),
                    format!(
                        "tool `{}` is not registered in this server ({} tools are)",
                        tool.get_ref(),
                        registry.len()
                    ),
                );
            }
        }
        if issues.found.is_empty() {
            Ok(())
        } else {
            Err(ConfigError {
                source_name: self.source_name.clone(),
                issues: issues.into_sorted(),
            })
        }
    }

    /// The agent's own tools: the tools of `registry` (every tool the server registers) that the
    /// definition names in `agent.tools`, and no other. Give this registry, not the server's, to
    /// the session's `ToolEngine` and [`SessionBuilder::with_tools`], so the LLM is offered (and
    /// can call) only what the definition declares.
    ///
    /// # Errors
    /// [`ConfigError`] naming each listed tool that `registry` does not have
    /// ([`Self::check_tools`]).
    pub fn tool_registry(&self, registry: &ToolRegistry) -> Result<ToolRegistry, ConfigError> {
        self.check_tools(registry)?;
        let mine = registry.filtered(|spec| {
            self.file
                .agent
                .tools
                .iter()
                .any(|t| t.get_ref().as_str() == &*spec.name)
        });
        // `confirm_tools`: the tools held for the user's confirmation. A name must be a tool of
        // this definition and a SideEffect tool (a ReadOnly one changes nothing to confirm).
        let mut issues = validate::Issues::new(&self.text);
        let mut mine = mine;
        for (i, tool) in self.file.agent.confirm_tools.iter().enumerate() {
            if let Err(error) = mine.require_confirmation([tool.get_ref().as_str()]) {
                issues.at(
                    &format!("agent.confirm_tools[{i}]"),
                    tool.span(),
                    error.to_string(),
                );
            }
        }
        if issues.found.is_empty() {
            Ok(mine)
        } else {
            Err(ConfigError {
                source_name: self.source_name.clone(),
                issues: issues.into_sorted(),
            })
        }
    }

    /// Applies the definition's language, voice and system prompt to a session builder (the
    /// engine configuration itself goes to [`SessionBuilder::new`]: [`Self::engine`]).
    #[must_use]
    pub fn configure<S, L, T>(&self, builder: SessionBuilder<S, L, T>) -> SessionBuilder<S, L, T>
    where
        S: SttProvider,
        L: LlmProvider,
        T: TtsProvider,
    {
        builder
            .with_language(self.language)
            .with_voice(self.voice())
            .with_system_prompt(self.system_prompt())
    }

    /// The error for a provider whose adapter is not compiled in (validation reports it first,
    /// so only a definition validated by another build can reach it).
    fn not_compiled(&self, key: &str, provider: Provider) -> ConfigError {
        ConfigError::single(
            &self.source_name,
            key,
            format!(
                "provider \"{}\" is not compiled into this server (feature `{}`)",
                provider.name(),
                provider.feature()
            ),
        )
    }

    /// The error for a key that a validated definition always has.
    fn missing_key(&self, key: &str) -> ConfigError {
        ConfigError::single(
            &self.source_name,
            key,
            "is required for this provider".to_owned(),
        )
    }

    /// The STT adapter configuration.
    ///
    /// # Errors
    /// [`ConfigError`] when the adapter is not compiled in.
    pub fn stt(&self) -> Result<SttProviderConfig, ConfigError> {
        self.stt_of(&self.file.stt, self.stt_key.clone(), "stt")
    }

    /// The configuration of `[stt.secondary]`, if the definition has one: the provider calls
    /// fail over to (it speaks the primary's language).
    ///
    /// # Errors
    /// [`ConfigError`] when the adapter is not compiled in.
    pub fn stt_secondary(&self) -> Result<Option<SttProviderConfig>, ConfigError> {
        let Some(section) = &self.file.stt.secondary else {
            return Ok(None);
        };
        self.stt_of(
            section.get_ref(),
            self.secondary_keys.stt.clone(),
            "stt.secondary",
        )
        .map(Some)
    }

    /// The adapter configuration of the STT table `stt` (written at `prefix`).
    fn stt_of(
        &self,
        stt: &SttSection,
        key: Option<Secret<String>>,
        prefix: &str,
    ) -> Result<SttProviderConfig, ConfigError> {
        let language = self.stt_language();
        let missing_key = || self.missing_key(&format!("{prefix}.api_key_env"));
        match *stt.provider.get_ref() {
            #[cfg(feature = "provider-deepgram")]
            SttProviderKind::Deepgram => Ok(SttProviderConfig::Deepgram(map::deepgram(
                stt.deepgram.as_ref().map(Spanned::get_ref),
                key.ok_or_else(missing_key)?,
                language,
            ))),
            #[cfg(feature = "provider-assemblyai")]
            SttProviderKind::Assemblyai => Ok(SttProviderConfig::AssemblyAi(map::assemblyai(
                stt.assemblyai.as_ref().map(Spanned::get_ref),
                key.ok_or_else(missing_key)?,
                language,
            ))),
            #[cfg(feature = "provider-gladia")]
            SttProviderKind::Gladia => Ok(SttProviderConfig::Gladia(map::gladia(
                stt.gladia.as_ref().map(Spanned::get_ref),
                key.ok_or_else(missing_key)?,
                language,
            ))),
            #[cfg(feature = "provider-sherpa-onnx")]
            SttProviderKind::SherpaOnnx => Ok(SttProviderConfig::SherpaOnnx(map::sherpa_onnx(
                stt.sherpa_onnx.as_ref().map(Spanned::get_ref),
                key,
            ))),
            #[cfg(feature = "provider-openai-realtime-stt")]
            SttProviderKind::OpenaiRealtimeStt => Ok(SttProviderConfig::OpenAiRealtimeStt(
                map::openai_realtime_stt(
                    stt.openai_realtime_stt.as_ref().map(Spanned::get_ref),
                    key,
                ),
            )),
            #[allow(unreachable_patterns)]
            other => {
                let _unused = (key, language, missing_key);
                Err(self.not_compiled(&format!("{prefix}.provider"), other.into()))
            }
        }
    }

    /// The LLM adapter configuration.
    ///
    /// # Errors
    /// [`ConfigError`] when the adapter is not compiled in, or (for a definition that skipped
    /// validation) a required key is missing.
    pub fn llm(&self) -> Result<LlmProviderConfig, ConfigError> {
        self.llm_of(&self.file.llm, self.llm_key.clone(), "llm")
    }

    /// The configuration of `[llm.secondary]`, if the definition has one.
    ///
    /// # Errors
    /// [`ConfigError`] when the adapter is not compiled in, or (for a definition that skipped
    /// validation) a required key is missing.
    pub fn llm_secondary(&self) -> Result<Option<LlmProviderConfig>, ConfigError> {
        let Some(section) = &self.file.llm.secondary else {
            return Ok(None);
        };
        self.llm_of(
            section.get_ref(),
            self.secondary_keys.llm.clone(),
            "llm.secondary",
        )
        .map(Some)
    }

    /// The adapter configuration of the LLM table `llm` (written at `prefix`).
    fn llm_of(
        &self,
        llm: &LlmSection,
        key: Option<Secret<String>>,
        prefix: &str,
    ) -> Result<LlmProviderConfig, ConfigError> {
        let missing_key = || self.missing_key(&format!("{prefix}.api_key_env"));
        match *llm.provider.get_ref() {
            #[cfg(feature = "provider-openai")]
            LlmProviderKind::Openai => Ok(LlmProviderConfig::OpenAi(map::openai(
                llm.openai.as_ref().map(Spanned::get_ref),
                key.ok_or_else(missing_key)?,
            ))),
            #[cfg(feature = "provider-anthropic")]
            LlmProviderKind::Anthropic => Ok(LlmProviderConfig::Anthropic(map::anthropic(
                llm.anthropic.as_ref().map(Spanned::get_ref),
                key.ok_or_else(missing_key)?,
            ))),
            #[cfg(feature = "provider-gemini")]
            LlmProviderKind::Gemini => Ok(LlmProviderConfig::Gemini(map::gemini(
                llm.gemini.as_ref().map(Spanned::get_ref),
                key.ok_or_else(missing_key)?,
            ))),
            #[cfg(feature = "provider-openai-compatible")]
            LlmProviderKind::OpenaiCompatible => Ok(LlmProviderConfig::OpenAiCompatible(
                map::openai_compatible(llm.openai_compatible.as_ref().map(Spanned::get_ref), key),
            )),
            #[allow(unreachable_patterns)]
            other => {
                let _unused = (key, missing_key);
                Err(self.not_compiled(&format!("{prefix}.provider"), other.into()))
            }
        }
    }

    /// The TTS adapter configuration, on `clock` and stopped by `shutdown` (the TTS adapters
    /// own a warm connection and its timers).
    ///
    /// # Errors
    /// [`ConfigError`] when the adapter is not compiled in.
    pub fn tts(
        &self,
        clock: brisvox_core::SharedClock,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<TtsProviderConfig, ConfigError> {
        self.tts_of(&self.file.tts, self.tts_key.clone(), "tts", clock, shutdown)
    }

    /// The configuration of `[tts.secondary]`, if the definition has one (on the same `clock`
    /// and `shutdown` as [`Self::tts`]). It speaks the primary's language at the primary's rate,
    /// with the voice [`Self::tts_secondary_voice`].
    ///
    /// # Errors
    /// [`ConfigError`] when the adapter is not compiled in.
    pub fn tts_secondary(
        &self,
        clock: brisvox_core::SharedClock,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<Option<TtsProviderConfig>, ConfigError> {
        let Some(section) = &self.file.tts.secondary else {
            return Ok(None);
        };
        self.tts_of(
            section.get_ref(),
            self.secondary_keys.tts.clone(),
            "tts.secondary",
            clock,
            shutdown,
        )
        .map(Some)
    }

    /// The voice id of `[tts.secondary]`, if the definition has one (a voice belongs to one
    /// provider, so the secondary has its own).
    pub fn tts_secondary_voice(&self) -> Option<Arc<str>> {
        self.file
            .tts
            .secondary
            .as_ref()
            .map(|s| s.get_ref().voice.get_ref().as_str().into())
    }

    /// The adapter configuration of the TTS table `tts` (written at `prefix`).
    fn tts_of(
        &self,
        tts: &TtsSection,
        key: Option<Secret<String>>,
        prefix: &str,
        clock: brisvox_core::SharedClock,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<TtsProviderConfig, ConfigError> {
        let language = self.tts_language();
        let missing_key = || self.missing_key(&format!("{prefix}.api_key_env"));
        match *tts.provider.get_ref() {
            #[cfg(feature = "provider-cartesia")]
            TtsProviderKind::Cartesia => Ok(TtsProviderConfig::Cartesia(map::cartesia(
                tts.cartesia.as_ref().map(Spanned::get_ref),
                key.ok_or_else(missing_key)?,
                language,
                clock,
                shutdown,
            ))),
            #[cfg(feature = "provider-elevenlabs")]
            TtsProviderKind::Elevenlabs => Ok(TtsProviderConfig::ElevenLabs(map::elevenlabs(
                tts.elevenlabs.as_ref().map(Spanned::get_ref),
                key.ok_or_else(missing_key)?,
                language,
                clock,
                shutdown,
            ))),
            #[cfg(feature = "provider-deepgram-aura")]
            TtsProviderKind::DeepgramAura => {
                Ok(TtsProviderConfig::DeepgramAura(map::deepgram_aura(
                    tts.deepgram_aura.as_ref().map(Spanned::get_ref),
                    key.ok_or_else(missing_key)?,
                    language,
                    clock,
                    shutdown,
                )))
            }
            #[cfg(feature = "provider-openai-speech")]
            TtsProviderKind::OpenaiSpeech => {
                Ok(TtsProviderConfig::OpenAiSpeech(map::openai_speech(
                    tts.openai_speech.as_ref().map(Spanned::get_ref),
                    key,
                    language,
                    clock,
                    shutdown,
                )))
            }
            #[allow(unreachable_patterns)]
            other => {
                let _unused = (key, language, clock, shutdown, missing_key);
                Err(self.not_compiled(&format!("{prefix}.provider"), other.into()))
            }
        }
    }
}

// Its tests read the adapter configurations of several providers. (A plain `#[cfg(test)]` so
// clippy treats the module as test code; the provider features are a separate attribute.)
#[cfg(test)]
#[cfg(all(
    feature = "provider-deepgram",
    feature = "provider-assemblyai",
    feature = "provider-openai",
    feature = "provider-anthropic",
    feature = "provider-openai-compatible",
    feature = "provider-cartesia",
    feature = "provider-elevenlabs"
))]
mod secondary_tests;
// Tests of the self-hosted STT providers.
#[cfg(test)]
#[cfg(all(
    feature = "provider-sherpa-onnx",
    feature = "provider-openai-realtime-stt",
    feature = "provider-openai-compatible",
    feature = "provider-cartesia",
    feature = "provider-deepgram"
))]
mod local_stt_tests;
#[cfg(test)]
mod tests;
// The `[[mcp.servers]]` tables.
#[cfg(test)]
mod mcp_tests;
// The JSON Schema of the agent file.
#[cfg(all(test, feature = "schema"))]
mod json_schema_tests;
