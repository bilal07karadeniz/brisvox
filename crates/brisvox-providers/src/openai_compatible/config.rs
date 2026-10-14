//! Configuration, per-vendor quirks and presets of the OpenAI-compatible adapter.

use std::fmt;
use std::time::Duration;

use brisvox_core::Secret;
use serde_json::Value;

/// Provider name of a configuration built with [`OpenAiCompatibleConfig::new`].
pub const DEFAULT_PROVIDER_NAME: &str = "openai-compatible";

/// Default path appended to the base URL.
pub const DEFAULT_CHAT_PATH: &str = "/chat/completions";

/// Default cap on the bytes the SSE parser holds for one unfinished event: 1 MiB. A chunk of a
/// Chat Completions stream is a few hundred bytes; the cap only bounds a misbehaving server.
pub const DEFAULT_MAX_EVENT_BYTES: usize = 1024 * 1024;

/// Default for [`OpenAiCompatibleConfig::pool_idle_timeout`]: 10 minutes.
pub const DEFAULT_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// Default for [`OpenAiCompatibleConfig::usage_grace`]: 250 ms.
pub const DEFAULT_USAGE_GRACE: Duration = Duration::from_millis(250);

/// Request fields the adapter owns: [`OpenAiCompatibleConfig::extra_body`] must not set them.
pub(crate) const RESERVED_BODY_KEYS: [&str; 8] = [
    "model",
    "messages",
    "stream",
    "stream_options",
    "tools",
    "tool_choice",
    "max_tokens",
    "max_completion_tokens",
];

/// A vendor whose quirks and endpoint the adapter knows (from `docs/providers/openai-compatible.md`,
/// fetched 2026-10-08). A preset fills in everything except the API key; every field can be
/// overridden afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Preset {
    /// Groq (`https://api.groq.com/openai/v1`).
    Groq,
    /// Together AI (`https://api.together.ai/v1`).
    Together,
    /// Fireworks AI (`https://api.fireworks.ai/inference/v1`).
    Fireworks,
    /// A vLLM server (`http://localhost:8000/v1`: the port is the example of the vLLM docs, set
    /// the real base URL).
    Vllm,
    /// An Ollama server (`http://localhost:11434/v1`).
    Ollama,
    /// OpenRouter (`https://openrouter.ai/api/v1`), a router in front of many vendors' models
    /// (ids are `vendor/model`). Reasoning models think by default: set
    /// `reasoning_effort = "none"` for voice (`docs/providers/openai-compatible.md`).
    OpenRouter,
}

impl Preset {
    /// Every preset.
    pub const ALL: [Preset; 6] = [
        Preset::Groq,
        Preset::Together,
        Preset::Fireworks,
        Preset::Vllm,
        Preset::Ollama,
        Preset::OpenRouter,
    ];

    /// The provider name used in errors and logs.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Groq => "groq",
            Self::Together => "together",
            Self::Fireworks => "fireworks",
            Self::Vllm => "vllm",
            Self::Ollama => "ollama",
            Self::OpenRouter => "openrouter",
        }
    }

    /// The base URL (includes the version prefix; the adapter appends `/chat/completions`).
    pub const fn base_url(self) -> &'static str {
        match self {
            Self::Groq => "https://api.groq.com/openai/v1",
            Self::Together => "https://api.together.ai/v1",
            Self::Fireworks => "https://api.fireworks.ai/inference/v1",
            Self::Vllm => "http://localhost:8000/v1",
            Self::Ollama => "http://localhost:11434/v1",
            Self::OpenRouter => "https://openrouter.ai/api/v1",
        }
    }

    /// A model id taken from an example of the vendor's own documentation. Models are
    /// configuration: pick yours by measurement (latency and Turkish quality are UNVERIFIED for
    /// all of them).
    pub const fn suggested_model(self) -> &'static str {
        match self {
            Self::Groq => "llama-3.1-8b-instant",
            Self::Together => "meta-llama/Llama-3.3-70B-Instruct-Turbo",
            Self::Fireworks => "accounts/fireworks/models/glm-5p3-flash",
            Self::Vllm => "NousResearch/Meta-Llama-3-8B-Instruct",
            // The local model that called tools correctly in every live run (docs/TOOLS.md,
            // "Local models through Ollama"); pair it with `reasoning_effort = "none"`.
            Self::Ollama => "gemma4:12b-it-qat",
            // The model of the live comparison (docs/LATENCY.md); pick yours.
            Self::OpenRouter => "openai/gpt-6-luna",
        }
    }

    /// The quirks of this vendor.
    pub const fn quirks(self) -> Quirks {
        match self {
            // `max_completion_tokens` replaces the deprecated `max_tokens`; `stream_options` is
            // listed by the API reference; usage placement is UNVERIFIED (top level, `x_groq`),
            // all are read.
            Self::Groq => Quirks {
                max_tokens_field: MaxTokensField::MaxCompletionTokens,
                request_stream_usage: true,
                usage: UsagePlacement::TrailingChunk,
                parallel_tool_calls: None,
                tool_choice: None,
                synthesize_missing_ids: true,
            },
            // `stream_options` and `parallel_tool_calls` are not in Together's reference: do not
            // send them, accept usage if it comes on the finish chunk.
            Self::Together => Quirks {
                max_tokens_field: MaxTokensField::MaxTokens,
                request_stream_usage: false,
                usage: UsagePlacement::FinishChunk,
                parallel_tool_calls: None,
                tool_choice: None,
                synthesize_missing_ids: true,
            },
            // Usage is on by default; the pages disagree whether it rides the finish chunk or
            // follows it, so wait for a trailing chunk when the finish chunk has none.
            Self::Fireworks => Quirks {
                max_tokens_field: MaxTokensField::MaxTokens,
                request_stream_usage: false,
                usage: UsagePlacement::TrailingChunk,
                parallel_tool_calls: None,
                tool_choice: None,
                synthesize_missing_ids: true,
            },
            // The server code emits a trailing `choices: []` usage chunk for `include_usage`.
            Self::Vllm => Quirks {
                max_tokens_field: MaxTokensField::MaxTokens,
                request_stream_usage: true,
                usage: UsagePlacement::TrailingChunk,
                parallel_tool_calls: None,
                tool_choice: None,
                synthesize_missing_ids: true,
            },
            // OpenAI's behaviour, which the live runs of 2026-10-13 used: `max_tokens`, usage in
            // one chunk before `[DONE]` (the docs: "exactly once in the final chunk"; that chunk
            // may repeat the finish reason), so the response waits for it.
            Self::OpenRouter => Quirks {
                max_tokens_field: MaxTokensField::MaxTokens,
                request_stream_usage: true,
                usage: UsagePlacement::TrailingChunk,
                parallel_tool_calls: None,
                tool_choice: None,
                synthesize_missing_ids: true,
            },
            // `stream_options` supports only `include_usage`; `tool_choice` is not supported.
            Self::Ollama => Quirks {
                max_tokens_field: MaxTokensField::MaxTokens,
                request_stream_usage: true,
                usage: UsagePlacement::TrailingChunk,
                parallel_tool_calls: None,
                tool_choice: None,
                synthesize_missing_ids: true,
            },
        }
    }
}

/// Which request field carries the output token limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MaxTokensField {
    /// `max_tokens` (Together, Fireworks, vLLM, Ollama; the default).
    #[default]
    MaxTokens,
    /// `max_completion_tokens` (OpenAI, Groq).
    MaxCompletionTokens,
}

impl MaxTokensField {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
        }
    }
}

/// Where a vendor reports token usage in a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum UsagePlacement {
    /// Possibly in a separate chunk after the one with `finish_reason` (`choices: []`, OpenAI
    /// `include_usage`, vLLM, Ollama; Fireworks may do either). When the finish chunk carries no
    /// usage the adapter waits for `[DONE]`, the end of the body or
    /// [`OpenAiCompatibleConfig::usage_grace`], whichever is first, before it ends the response.
    /// The default.
    #[default]
    TrailingChunk,
    /// On the finish chunk or not at all (Together): the response ends at the finish chunk.
    FinishChunk,
    /// Never reported: the response ends at the finish chunk.
    Absent,
}

/// `tool_choice` to send when the request has tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolChoice {
    /// `"auto"`.
    Auto,
    /// `"required"`.
    Required,
    /// `"none"`.
    None,
}

impl ToolChoice {
    /// The wire value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Required => "required",
            Self::None => "none",
        }
    }
}

/// How a vendor deviates from OpenAI's Chat Completions, as far as the adapter needs to know.
///
/// The *reading* side is deliberately not configurable: the chunk parser accepts fragmented and
/// whole tool calls, a missing or `null` `index`, `finish_reason` `eos` / `function_call`, usage
/// at the top level / in `x_groq` / in the choice, and error bodies as objects or strings, with
/// the same code (a vendor's shape is not trusted: the docs disagree with each other, see
/// `docs/providers/openai-compatible.md`). These flags only decide what is *sent* and how long
/// the adapter waits. (The only read-side switch is [`Self::synthesize_missing_ids`]. There is
/// deliberately no flag to force fragmented-only parsing or to treat `stop` with tool calls as
/// an error: the tolerant path cannot misread a conforming stream, and a flag per observed
/// deviation would only encode guesses the docs cannot confirm; see decision 2 in the module
/// docs and the "Implementation notes" of the provider doc.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Quirks {
    /// Name of the output token limit field.
    pub max_tokens_field: MaxTokensField,
    /// Send `"stream_options": {"include_usage": true}`.
    pub request_stream_usage: bool,
    /// Where usage arrives (decides whether the end of a response waits for it).
    pub usage: UsagePlacement,
    /// `parallel_tool_calls` to send when the request has tools; `None` leaves it out.
    pub parallel_tool_calls: Option<bool>,
    /// `tool_choice` to send when the request has tools; `None` leaves it out.
    pub tool_choice: Option<ToolChoice>,
    /// A tool call that arrives without an id gets a synthetic one (`call_<n>`), so the tool
    /// message can reference it. When `false` such a call is [`Error::Protocol`].
    ///
    /// [`Error::Protocol`]: crate::Error::Protocol
    pub synthesize_missing_ids: bool,
}

impl Default for Quirks {
    /// OpenAI's own behaviour: `max_tokens`, `include_usage` requested, usage in a trailing
    /// chunk, no `parallel_tool_calls` / `tool_choice`, synthetic ids for calls without one.
    fn default() -> Self {
        Self {
            max_tokens_field: MaxTokensField::MaxTokens,
            request_stream_usage: true,
            usage: UsagePlacement::TrailingChunk,
            parallel_tool_calls: None,
            tool_choice: None,
            synthesize_missing_ids: true,
        }
    }
}

/// How the API key reaches the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthStyle {
    /// Header name (`Authorization` by default).
    pub header: String,
    /// Scheme placed before the key, separated by a space (`Bearer` by default); `None` sends
    /// the bare key (for example `x-api-key: <key>`).
    pub scheme: Option<String>,
}

impl Default for AuthStyle {
    fn default() -> Self {
        Self::bearer()
    }
}

impl AuthStyle {
    /// `Authorization: Bearer <key>`.
    pub fn bearer() -> Self {
        Self {
            header: "authorization".to_owned(),
            scheme: Some("Bearer".to_owned()),
        }
    }

    /// `<header>: <key>` with no scheme.
    pub fn header(name: impl Into<String>) -> Self {
        Self {
            header: name.into(),
            scheme: None,
        }
    }
}

/// Configuration of [`OpenAiCompatibleLlm`](super::OpenAiCompatibleLlm). `Debug` never prints
/// the API key, the value of an extra header, the extra body values, or anything of the base URL
/// but its scheme, host and port (userinfo, path and query can carry credentials).
#[derive(Clone)]
pub struct OpenAiCompatibleConfig {
    /// Provider name for errors and logs (`"groq"`, ... for a preset).
    pub name: &'static str,
    /// API key; `None` sends no auth header (local Ollama, vLLM without `--api-key`). Never
    /// logged: it is a [`Secret`].
    pub api_key: Option<Secret<String>>,
    /// Header and scheme the key is sent in.
    pub auth: AuthStyle,
    /// Further static headers sent with every request (header name, value). Values are secrets:
    /// they are marked sensitive and never printed.
    pub extra_headers: Vec<(String, Secret<String>)>,
    /// Base URL including the version prefix, without a trailing path (for example
    /// `https://api.groq.com/openai/v1`). Plain `http://` is accepted for loopback hosts and,
    /// elsewhere, only with [`Self::allow_insecure_http`].
    pub base_url: String,
    /// Path appended to the base URL ([`DEFAULT_CHAT_PATH`]).
    pub chat_path: String,
    /// Model id, sent verbatim (vendor-namespaced: `openai/gpt-oss-120b`,
    /// `accounts/fireworks/models/...`).
    pub model: String,
    /// The vendor's deviations.
    pub quirks: Quirks,
    /// Output token limit, sent in the field [`Quirks::max_tokens_field`] names; `None` leaves
    /// it out.
    pub max_tokens: Option<u32>,
    /// `temperature`; `None` leaves it out.
    pub temperature: Option<f32>,
    /// Further top-level request fields (`reasoning_effort`, `chat_template_kwargs`, ...). Keys
    /// the adapter owns (`model`, `messages`, `stream`, `stream_options`, `tools`,
    /// `tool_choice`, `max_tokens`, `max_completion_tokens`, and `temperature` /
    /// `parallel_tool_calls` while [`Self::temperature`] / [`Quirks::parallel_tool_calls`] is
    /// set) are rejected by [`OpenAiCompatibleLlm::new`](super::OpenAiCompatibleLlm::new), so a
    /// request body never carries a key twice. Among the entries a later one with the same
    /// key wins.
    pub extra_body: Vec<(String, Value)>,
    /// Bound on the TCP + TLS connect, handed to the HTTP client. The clock-based
    /// [`Self::first_byte_timeout`] covers the connect as well and is what the contract
    /// suite sees.
    pub connect_timeout: Duration,
    /// Deadline, on the injected clock, from the start of `stream()` (connect, TLS, request,
    /// waiting for the response headers) to the response headers. Expiry is `Core(Timeout)`
    /// from `stream()`.
    pub first_byte_timeout: Duration,
    /// Deadline, on the injected clock, from the response headers to the first output of the
    /// model (the first text or tool-call fragment, or the terminal chunk). Reasoning deltas and
    /// comments do not extend it. Longer than [`Self::idle_timeout`] by default because a
    /// reasoning model may think before it speaks, and a local model may have to load.
    pub first_output_timeout: Duration,
    /// Longest silence between two SSE events once the response has produced its first output,
    /// on the injected clock. SSE comments are **not** activity. Expiry is
    /// `LlmEvent::Error(Core(Timeout))`.
    pub idle_timeout: Duration,
    /// How long, after the chunk with `finish_reason`, the adapter waits for a trailing usage
    /// chunk (see [`UsagePlacement::TrailingChunk`]) when the server neither sends `[DONE]` nor
    /// closes the body. A missing usage never delays `Done` by more than this.
    pub usage_grace: Duration,
    /// Cap on the total duration of one streamed response, on the injected clock.
    pub max_stream_duration: Duration,
    /// Cap on the bytes held for one unfinished SSE event ([`DEFAULT_MAX_EVENT_BYTES`]).
    pub max_event_bytes: usize,
    /// How long an idle connection stays in the client's pool (`None`: until the server closes
    /// it).
    pub pool_idle_timeout: Option<Duration>,
    /// Allow a plain `http://` [`Self::base_url`] on a non-loopback host. Off by default: the
    /// key and the conversation would travel in cleartext.
    pub allow_insecure_http: bool,
}

impl OpenAiCompatibleConfig {
    /// A configuration for the server at `base_url` serving `model`, with OpenAI's behaviour
    /// ([`Quirks::default`]), no key, `Authorization: Bearer` once a key is set, 2 s connect, 10 s
    /// to the response headers, 30 s to the first output, 10 s between events, 5 min per
    /// response.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            name: DEFAULT_PROVIDER_NAME,
            api_key: None,
            auth: AuthStyle::bearer(),
            extra_headers: Vec::new(),
            base_url: base_url.into(),
            chat_path: DEFAULT_CHAT_PATH.to_owned(),
            model: model.into(),
            quirks: Quirks::default(),
            max_tokens: None,
            temperature: None,
            extra_body: Vec::new(),
            connect_timeout: Duration::from_secs(2),
            first_byte_timeout: Duration::from_secs(10),
            first_output_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(10),
            usage_grace: DEFAULT_USAGE_GRACE,
            max_stream_duration: Duration::from_secs(300),
            max_event_bytes: DEFAULT_MAX_EVENT_BYTES,
            pool_idle_timeout: Some(DEFAULT_POOL_IDLE_TIMEOUT),
            allow_insecure_http: false,
        }
    }

    /// The configuration of `preset`: its name, base URL, [`Preset::suggested_model`] and
    /// [`Preset::quirks`]. Add the key with [`Self::with_api_key`] (hosted vendors need one;
    /// a local Ollama or vLLM does not).
    pub fn preset(preset: Preset) -> Self {
        Self {
            name: preset.name(),
            quirks: preset.quirks(),
            ..Self::new(preset.base_url(), preset.suggested_model())
        }
    }

    /// Sets the API key.
    #[must_use]
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(Secret::new(key.into()));
        self
    }

    /// Sets the model id.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Sets the base URL (for example a loopback mock, or a vLLM server elsewhere).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Sends the key in `auth` instead of `Authorization: Bearer`.
    #[must_use]
    pub fn with_auth(mut self, auth: AuthStyle) -> Self {
        self.auth = auth;
        self
    }

    /// Adds a static header sent with every request.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers
            .push((name.into(), Secret::new(value.into())));
        self
    }

    /// Replaces the quirks.
    #[must_use]
    pub fn with_quirks(mut self, quirks: Quirks) -> Self {
        self.quirks = quirks;
        self
    }

    /// Adds a top-level request field.
    #[must_use]
    pub fn with_body_field(mut self, key: impl Into<String>, value: Value) -> Self {
        self.extra_body.push((key.into(), value));
        self
    }

    /// Sets the connect, first-byte, first-output and idle deadlines to `timeout` (what the
    /// provider contract suite configures).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self.first_byte_timeout = timeout;
        self.first_output_timeout = timeout;
        self.idle_timeout = timeout;
        self
    }
}

impl OpenAiCompatibleConfig {
    /// Whether the adapter itself sends `key`, so `extra_body` must not.
    pub(crate) fn owns_body_key(&self, key: &str) -> bool {
        RESERVED_BODY_KEYS.contains(&key)
            || (key == "temperature" && self.temperature.is_some())
            || (key == "parallel_tool_calls" && self.quirks.parallel_tool_calls.is_some())
    }
}

/// `scheme://host[:port]` of `url`, or a placeholder: userinfo, path, query and fragment may
/// carry credentials and are never shown.
fn origin_of(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(parsed) => {
            let host = parsed.host_str().unwrap_or("?");
            match parsed.port() {
                Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
                None => format!("{}://{host}", parsed.scheme()),
            }
        }
        Err(_) => "<invalid url>".to_owned(),
    }
}

impl fmt::Debug for OpenAiCompatibleConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names: Vec<&str> = self.extra_headers.iter().map(|(n, _)| n.as_str()).collect();
        let body_keys: Vec<&str> = self.extra_body.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("OpenAiCompatibleConfig")
            .field("name", &self.name)
            .field("api_key", &self.api_key)
            .field("auth", &self.auth)
            .field("extra_headers", &header_names)
            .field("base_url", &origin_of(&self.base_url))
            .field("chat_path", &self.chat_path.split(['?', '#']).next())
            .field("model", &self.model)
            .field("quirks", &self.quirks)
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
            .field("extra_body", &body_keys)
            .field("connect_timeout", &self.connect_timeout)
            .field("first_byte_timeout", &self.first_byte_timeout)
            .field("first_output_timeout", &self.first_output_timeout)
            .field("idle_timeout", &self.idle_timeout)
            .field("usage_grace", &self.usage_grace)
            .field("max_stream_duration", &self.max_stream_duration)
            .field("max_event_bytes", &self.max_event_bytes)
            .field("pool_idle_timeout", &self.pool_idle_timeout)
            .field("allow_insecure_http", &self.allow_insecure_http)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_adapter_owns_the_keys_it_serialises() {
        let plain = OpenAiCompatibleConfig::new("http://127.0.0.1:1/v1", "m");
        // Not set by the adapter: free for `extra_body`.
        assert!(!plain.owns_body_key("temperature"));
        assert!(!plain.owns_body_key("parallel_tool_calls"));
        assert!(plain.owns_body_key("stream"));
        let mut set = plain;
        set.temperature = Some(0.5);
        set.quirks.parallel_tool_calls = Some(true);
        assert!(set.owns_body_key("temperature"));
        assert!(set.owns_body_key("parallel_tool_calls"));
        assert!(!set.owns_body_key("reasoning_effort"));
    }

    #[test]
    fn debug_shows_the_origin_and_names_only() {
        let config = OpenAiCompatibleConfig::new(
            "https://user:pw-SECRET@gw.example:8443/v1/tenant-SECRET?key=SECRET#frag",
            "m",
        )
        .with_api_key("sk-SECRET")
        .with_header("x-tenant", "SECRET")
        .with_body_field("metadata", serde_json::json!({"k": "SECRET"}));
        let shown = format!("{config:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(shown.contains("https://gw.example:8443"), "{shown}");
        assert!(
            shown.contains("x-tenant") && shown.contains("metadata"),
            "{shown}"
        );
        let bad = OpenAiCompatibleConfig::new("not a url SECRET", "m");
        assert!(!format!("{bad:?}").contains("SECRET"));
    }
}
