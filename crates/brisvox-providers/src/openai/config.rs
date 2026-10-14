//! Configuration of the OpenAI adapter.

use std::time::Duration;

use brisvox_core::Secret;

/// Default model: the cheapest and fastest of the GPT-6 family per `docs/providers/openai.md`.
/// Model ids are configuration, never hard-coded in logic.
pub const DEFAULT_MODEL: &str = "gpt-6-luna";

/// Default base URL (the adapter appends `/v1/responses`).
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com";

/// Default cap on the bytes the SSE parser holds for one unfinished event: 4 MiB. The terminal
/// event carries the whole `Response` object, so this must leave room for long answers.
pub const DEFAULT_MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;

/// Reasoning effort of the model (`reasoning.effort`). Which values a model accepts differs per
/// model (`docs/providers/openai.md`): `gpt-6-luna` accepts all of them, `gpt-6-astra` rejects
/// [`Self::Disabled`] with HTTP 400.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReasoningEffort {
    /// `"none"`: no reasoning (lowest latency; the default of this adapter).
    Disabled,
    /// `"low"`.
    Low,
    /// `"medium"`.
    Medium,
    /// `"high"`.
    High,
    /// `"xhigh"`.
    Xhigh,
    /// `"max"`.
    Max,
}

impl ReasoningEffort {
    /// The wire value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// How the adapter treats the JSON Schema of a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum StrictTools {
    /// Send `strict: true` for a tool whose schema already complies with OpenAI's strict mode
    /// (`additionalProperties: false` on every object, every property in `required`), and
    /// `strict: false` otherwise. The default.
    #[default]
    WhenCompliant,
    /// Always send `strict: false` (best-effort function calling).
    Never,
}

/// Default for [`OpenAiConfig::pool_idle_timeout`]: 10 minutes.
pub const DEFAULT_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// Configuration of [`OpenAiLlm`](super::OpenAiLlm). `Debug` never prints the API key.
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    /// API key (`Authorization: Bearer`). Never logged: it is a [`Secret`].
    pub api_key: Secret<String>,
    /// Base URL without the `/v1/...` path. `https://api.openai.com` by default; plain `http://`
    /// is accepted for loopback hosts (tests, local OpenAI-compatible servers that implement the
    /// Responses API) and, elsewhere, only with [`Self::allow_insecure_http`].
    pub base_url: String,
    /// Model id ([`DEFAULT_MODEL`] by default).
    pub model: String,
    /// `reasoning.effort`; `None` leaves the field out (the model's default).
    pub reasoning_effort: Option<ReasoningEffort>,
    /// `parallel_tool_calls` (sent only when the request has tools). `true` lets the model emit
    /// several calls in one response.
    pub parallel_tool_calls: bool,
    /// How tool schemas are sent.
    pub strict_tools: StrictTools,
    /// `max_output_tokens` (includes reasoning tokens); `None` leaves it out.
    pub max_output_tokens: Option<u32>,
    /// `prompt_cache_key`: a stable key per agent configuration (not per call). Optional on
    /// GPT-5.6 and later, and it influences routing only (`docs/providers/openai.md`).
    pub prompt_cache_key: Option<String>,
    /// `service_tier` (for example `"fast"`); `None` leaves it out.
    pub service_tier: Option<String>,
    /// `OpenAI-Organization` header, if any.
    pub organization: Option<String>,
    /// `OpenAI-Project` header, if any.
    pub project: Option<String>,
    /// Replay the opaque reasoning items of earlier turns (`include:
    /// ["reasoning.encrypted_content"]` plus the items echoed from
    /// [`Message::provider_data`](crate::Message::provider_data)). `true` by default; turn it off
    /// to drop reasoning from the replayed history if the API rejects a replay.
    pub replay_reasoning: bool,
    /// Ask the server to pad deltas with random `obfuscation` bytes (hides payload sizes, costs
    /// bandwidth). Off by default: the link is TLS to a trusted endpoint.
    pub include_obfuscation: bool,
    /// Bound on the TCP + TLS connect, handed to the HTTP client. The clock-based
    /// [`Self::first_byte_timeout`] covers the connect as well and is what the contract
    /// suite sees.
    pub connect_timeout: Duration,
    /// Deadline, on the injected clock, from the start of `stream()` (connect, TLS, request,
    /// waiting for the response headers) to the response headers. Expiry is
    /// `Core(Timeout)` from `stream()`.
    pub first_byte_timeout: Duration,
    /// Longest silence between two SSE events once the response has produced its first output,
    /// on the injected clock. SSE comments (keep-alives) are **not** activity: a stream that
    /// carries only comments is stalled. Expiry is `LlmEvent::Error(Core(Timeout))`.
    pub idle_timeout: Duration,
    /// Deadline, on the injected clock, from the response headers to the first output of the
    /// model (the first text or tool-call fragment, or the terminal event). Neither comments nor
    /// lifecycle events (`response.created`, `response.in_progress`, ...) extend it, so a model
    /// or proxy that stalls behind keep-alives is cut off here and not after
    /// [`Self::max_stream_duration`]. Longer than [`Self::idle_timeout`] by default because a
    /// reasoning model may think before it speaks. Expiry is `LlmEvent::Error(Core(Timeout))`.
    pub first_output_timeout: Duration,
    /// How long an idle connection stays in the client's pool (`None`: until the server closes
    /// it). Bounds how long a [`warm`](crate::LlmProvider::warm) connection stays useful. HTTP/2
    /// connections are pinged every 20 s while idle, so a long quiet period (ringing before the
    /// first user turn) does not need a new warm-up when this is large enough.
    pub pool_idle_timeout: Option<Duration>,
    /// Allow a plain `http://` [`Self::base_url`] on a non-loopback host. Off by default: the
    /// bearer key would travel in cleartext, so `OpenAiLlm::new` rejects it. Loopback
    /// (`localhost`, `127.0.0.0/8`, `::1`) is always allowed.
    pub allow_insecure_http: bool,
    /// Cap on the total duration of one streamed response, on the injected clock.
    pub max_stream_duration: Duration,
    /// Cap on the bytes held for one unfinished SSE event ([`DEFAULT_MAX_EVENT_BYTES`]); a
    /// larger event is a protocol error, so a misbehaving server cannot grow memory.
    pub max_event_bytes: usize,
}

impl OpenAiConfig {
    /// A configuration for `api_key` with the defaults: [`DEFAULT_MODEL`], reasoning `none`,
    /// parallel tool calls on, strict tools when compliant, 2 s connect, 10 s to the response
    /// headers, 30 s to the first output, 10 s between events, 5 min per response.
    pub fn new(api_key: Secret<String>) -> Self {
        Self {
            api_key,
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: DEFAULT_MODEL.to_owned(),
            reasoning_effort: Some(ReasoningEffort::Disabled),
            parallel_tool_calls: true,
            strict_tools: StrictTools::default(),
            max_output_tokens: None,
            prompt_cache_key: None,
            service_tier: None,
            organization: None,
            project: None,
            replay_reasoning: true,
            include_obfuscation: false,
            connect_timeout: Duration::from_secs(2),
            first_byte_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(10),
            first_output_timeout: Duration::from_secs(30),
            pool_idle_timeout: Some(DEFAULT_POOL_IDLE_TIMEOUT),
            allow_insecure_http: false,
            max_stream_duration: Duration::from_secs(300),
            max_event_bytes: DEFAULT_MAX_EVENT_BYTES,
        }
    }

    /// Sets the connect, first-byte, first-output and idle deadlines to `timeout` (what the
    /// provider contract suite configures).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self.first_byte_timeout = timeout;
        self.idle_timeout = timeout;
        self.first_output_timeout = timeout;
        self
    }

    /// Sets the base URL (for example a loopback mock or an OpenAI-compatible server).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Sets the model id.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
}
