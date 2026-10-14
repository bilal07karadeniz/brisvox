//! Configuration of the Gemini adapter.

use std::time::Duration;

use brisvox_core::Secret;

/// Default model: the fastest 3.5 model per `docs/providers/gemini.md` (defaults to `minimal`
/// thinking). Model ids are configuration, never hard-coded in logic.
pub const DEFAULT_MODEL: &str = "gemini-3.5-flash-lite";

/// Default base URL (the adapter appends `/v1beta/models/{model}:streamGenerateContent`).
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

/// Default cap on the bytes the SSE parser holds for one unfinished event: 4 MiB.
pub const DEFAULT_MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;

/// Default for [`GeminiConfig::pool_idle_timeout`]: 10 minutes.
pub const DEFAULT_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// The documented value that makes Gemini 3 skip thought-signature validation for a function
/// call the model did not produce (history rebuilt from played text; `docs/providers/gemini.md`,
/// "Thought signatures").
pub const SKIP_SIGNATURE_VALIDATOR: &str = "skip_thought_signature_validator";

/// Default for [`GeminiConfig::leading_user_placeholder`].
pub const DEFAULT_LEADING_USER_PLACEHOLDER: &str = "(The call has started.)";

/// `generationConfig.thinkingConfig.thinkingLevel`. Which levels a model accepts differs per
/// model (`docs/providers/gemini.md`): `gemini-3.8-flash` and `gemini-3.7-flash` reject
/// [`Self::Minimal`] with an error, models before Gemini 3 reject the field altogether.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThinkingLevel {
    /// `"minimal"`.
    Minimal,
    /// `"low"`.
    Low,
    /// `"medium"`.
    Medium,
    /// `"high"`.
    High,
}

impl ThinkingLevel {
    /// The wire value (lower case, as in the documentation's examples).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// `toolConfig.functionCallingConfig.mode`. [`LlmRequest`](crate::LlmRequest) has no
/// `tool_choice`, so by default nothing is sent (the API default is `AUTO`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FunctionCallingMode {
    /// `AUTO`: the model chooses.
    Auto,
    /// `ANY`: the model must call a function (very large or deeply nested schemas may be
    /// rejected in this mode).
    Any,
    /// `NONE`: no function calls.
    None,
    /// `VALIDATED`: like `AUTO`, with the calls validated by constrained decoding.
    Validated,
}

impl FunctionCallingMode {
    /// The wire value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "AUTO",
            Self::Any => "ANY",
            Self::None => "NONE",
            Self::Validated => "VALIDATED",
        }
    }
}

/// How a tool's JSON Schema is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SchemaMode {
    /// Convert the schema to the OpenAPI subset of `functionDeclarations[].parameters` (upper
    /// case types, `nullable`, `anyOf`; unsupported keywords dropped, see the module docs of
    /// `brisvox_providers::gemini`). The default.
    #[default]
    OpenApiSubset,
    /// Send the schema unchanged as `parametersJsonSchema`. Which JSON Schema keywords the
    /// server accepts there is UNVERIFIED (`docs/providers/gemini.md`); use it when the subset
    /// conversion drops something you need.
    JsonSchema,
}

/// Configuration of [`GeminiLlm`](super::GeminiLlm). `Debug` never prints the API key.
#[derive(Debug, Clone)]
pub struct GeminiConfig {
    /// API key (`x-goog-api-key` header; never put in the URL). Never logged: it is a
    /// [`Secret`].
    pub api_key: Secret<String>,
    /// Base URL without the `/v1beta/...` path. `https://generativelanguage.googleapis.com` by
    /// default; plain `http://` is accepted for loopback hosts (tests) and, elsewhere, only with
    /// [`Self::allow_insecure_http`].
    pub base_url: String,
    /// Model id ([`DEFAULT_MODEL`] by default); a leading `models/` is accepted and removed.
    /// Only `[A-Za-z0-9._-]` is allowed (it becomes part of the URL path).
    pub model: String,
    /// `thinkingConfig.thinkingLevel` (Gemini 3 and later); `None` leaves it out (the model's
    /// default). Mutually exclusive with [`Self::thinking_budget`].
    pub thinking_level: Option<ThinkingLevel>,
    /// `thinkingConfig.thinkingBudget` (Gemini 2.5; legacy on Gemini 3); `None` leaves it out.
    /// Mutually exclusive with [`Self::thinking_level`].
    pub thinking_budget: Option<i32>,
    /// `generationConfig.maxOutputTokens` (includes thinking tokens); `None` leaves it out.
    pub max_output_tokens: Option<u32>,
    /// `generationConfig.temperature`; `None` leaves it out. Google recommends the default of
    /// 1.0 for Gemini 3, so the adapter sets nothing unless asked.
    pub temperature: Option<f32>,
    /// `toolConfig.functionCallingConfig.mode`, sent only when the request has tools; `None`
    /// sends no `toolConfig`.
    pub function_calling_mode: Option<FunctionCallingMode>,
    /// `toolConfig.functionCallingConfig.allowedFunctionNames`, sent with the mode when it is
    /// [`FunctionCallingMode::Any`] or [`FunctionCallingMode::Validated`].
    pub allowed_function_names: Vec<String>,
    /// How tool schemas are sent.
    pub schema_mode: SchemaMode,
    /// Echo the opaque `thoughtSignature`s of earlier turns
    /// ([`Message::provider_data`](crate::Message::provider_data)). `true` by default: Gemini 3
    /// answers HTTP 400 when the signature of the first function call of the current turn is
    /// missing. Turn it off only to drop signatures from the replayed history if the API
    /// rejects a replay.
    pub replay_thought_signatures: bool,
    /// The signature put on the first `functionCall` of a replayed assistant turn that has none
    /// (history rebuilt without provider data). Defaults to [`SKIP_SIGNATURE_VALIDATOR`];
    /// `None` sends such calls unsigned (Gemini 2.5 and earlier do not need a signature).
    pub fallback_signature: Option<String>,
    /// Text of a placeholder `user` content put in front of a history that starts with a `model`
    /// turn (the agent speaks first). Whether Gemini accepts a leading model content is
    /// UNVERIFIED (`docs/providers/gemini.md`); with the placeholder the request is
    /// valid either way. `None` sends the history as it is.
    pub leading_user_placeholder: Option<String>,
    /// Bound on the TCP + TLS connect, handed to the HTTP client. The clock-based
    /// [`Self::first_byte_timeout`] covers the connect as well and is what the contract
    /// suite sees.
    pub connect_timeout: Duration,
    /// Deadline, on the injected clock, from the start of `stream()` (connect, TLS, request,
    /// waiting for the response headers) to the response headers. Expiry is
    /// `Core(Timeout)` from `stream()`.
    pub first_byte_timeout: Duration,
    /// Longest silence between two SSE events **that carry output** (text, a function call, or
    /// the end of the turn) once the response has produced its first output, on the injected
    /// clock. SSE comments, empty `data:` heartbeats, `{}` chunks, usage-only chunks and chunks
    /// without parts are **not** activity: a stream that carries only those is stalled. Expiry
    /// is `LlmEvent::Error(Core(Timeout))`.
    pub idle_timeout: Duration,
    /// Deadline, on the injected clock, from the response headers to the first output of the
    /// model (the first text or function call, or the terminal event). Comments and chunks
    /// without output do not extend it. Longer than [`Self::idle_timeout`] by default because
    /// a thinking model may think before it speaks. Expiry is `LlmEvent::Error(Core(Timeout))`.
    pub first_output_timeout: Duration,
    /// After `finishReason` has been seen without `usageMetadata`, how long to wait (on the
    /// injected clock) for a later usage-only chunk or the end of the body before the turn is
    /// completed with whatever usage is known (`None` if none). The content is complete at
    /// `finishReason`, so a server that keeps the body open must not turn the answer into a
    /// timeout. Short: usage normally rides on the finish chunk.
    pub usage_grace: Duration,
    /// Longest time the reader waits for the consumer to take an event when the bounded event
    /// channel is full, on the injected clock. A consumer that stays alive but stops reading
    /// (a stuck pipeline stage) would otherwise pin the HTTP response, the socket and the
    /// replay buffer for ever: when this expires the response is abandoned (logged; the stream
    /// ends without a `Done`). Time the consumer spends reading slowly but steadily is not
    /// charged: the limit applies to each single event.
    pub consumer_stall_timeout: Duration,
    /// Speak HTTP/2 without ALPN negotiation ("prior knowledge", h2c) instead of negotiating
    /// `h2`/`http/1.1`. For test servers that speak cleartext HTTP/2; the real endpoint
    /// negotiates over TLS and needs this off (the default).
    pub http2_prior_knowledge: bool,
    /// How long an idle connection stays in the client's pool (`None`: until the server closes
    /// it). Bounds how long a [`warm`](crate::LlmProvider::warm) connection stays useful.
    pub pool_idle_timeout: Option<Duration>,
    /// Allow a plain `http://` [`Self::base_url`] on a non-loopback host. Off by default: the
    /// API key would travel in cleartext, so `GeminiLlm::new` rejects it. Loopback
    /// (`localhost`, `127.0.0.0/8`, `::1`) is always allowed.
    pub allow_insecure_http: bool,
    /// Cap on the total duration of one streamed response, on the injected clock.
    pub max_stream_duration: Duration,
    /// Cap on the bytes held for one unfinished SSE event ([`DEFAULT_MAX_EVENT_BYTES`]); a
    /// larger event is a protocol error, so a misbehaving server cannot grow memory.
    pub max_event_bytes: usize,
}

impl GeminiConfig {
    /// A configuration for `api_key` with the defaults: [`DEFAULT_MODEL`], no thinking
    /// settings, no tool config, signatures replayed with [`SKIP_SIGNATURE_VALIDATOR`] as
    /// fallback, 2 s connect, 10 s to the response headers, 30 s to the first output, 10 s
    /// between events, 250 ms of usage grace, 30 s of consumer stall, 5 min per response.
    pub fn new(api_key: Secret<String>) -> Self {
        Self {
            api_key,
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: DEFAULT_MODEL.to_owned(),
            thinking_level: None,
            thinking_budget: None,
            max_output_tokens: None,
            temperature: None,
            function_calling_mode: None,
            allowed_function_names: Vec::new(),
            schema_mode: SchemaMode::default(),
            replay_thought_signatures: true,
            fallback_signature: Some(SKIP_SIGNATURE_VALIDATOR.to_owned()),
            leading_user_placeholder: Some(DEFAULT_LEADING_USER_PLACEHOLDER.to_owned()),
            connect_timeout: Duration::from_secs(2),
            first_byte_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(10),
            first_output_timeout: Duration::from_secs(30),
            pool_idle_timeout: Some(DEFAULT_POOL_IDLE_TIMEOUT),
            usage_grace: Duration::from_millis(250),
            consumer_stall_timeout: Duration::from_secs(30),
            http2_prior_knowledge: false,
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

    /// Sets the base URL (for example a loopback mock).
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

    /// Sets the thinking level.
    #[must_use]
    pub fn with_thinking_level(mut self, level: ThinkingLevel) -> Self {
        self.thinking_level = Some(level);
        self
    }
}
