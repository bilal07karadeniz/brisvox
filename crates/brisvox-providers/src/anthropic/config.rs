//! Configuration of the Anthropic adapter.

use std::time::Duration;

use brisvox_core::Secret;

/// Default model: the one `docs/providers/anthropic.md` recommends for a voice loop (documented
/// as the fastest of the Claude 5.5 family). Model ids are configuration, never hard-coded in
/// logic: [`ThinkingMode`] and [`ToolChoice`] have model-specific constraints, so changing the
/// model usually means changing those too.
pub const DEFAULT_MODEL: &str = "claude-haiku-5-5";

/// Default base URL (the adapter appends `/v1/messages`).
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The `anthropic-version` header value: the only non-initial version the docs list.
pub const API_VERSION: &str = "2023-06-01";

/// Default `max_tokens`: 1024, the documented latency-example value. Spoken answers are short;
/// raise it for long answers (it is a cap, not a target).
pub const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Default cap on the bytes the SSE parser holds for one unfinished event: 4 MiB (a thinking
/// signature is "significantly longer in Claude 4 and later models").
pub const DEFAULT_MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;

/// Default for [`AnthropicConfig::pool_idle_timeout`]: 10 minutes.
pub const DEFAULT_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// The `thinking` request parameter. Thinking is **on by default** on the Claude 5.5 models, so
/// the adapter always states the mode explicitly (unless [`Self::ModelDefault`]); which values a
/// model accepts differs (`docs/providers/anthropic.md`, "Thinking / effort settings").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ThinkingMode {
    /// `{"type":"disabled"}`: no thinking. Accepted by Haiku 5.5 at effort `high` or below; HTTP
    /// 400 on Sonnet 5.5 (use [`Self::BetweenTools`]) and Opus 5.5. The default: it gives the
    /// lowest time to first token, there are no thinking blocks to echo or to invalidate when
    /// an interruption edits the history, and the mode never changes between turns (a change
    /// invalidates the message cache).
    #[default]
    Disabled,
    /// `{"type":"adaptive"}`: the model decides whether to think. Docs: if Haiku 5.5 leaks
    /// reasoning-like text into its replies (more often with thinking off or `low` effort),
    /// switch to adaptive thinking with `medium` effort.
    Adaptive,
    /// `{"type":"between_tools"}`: up-front thinking off at effort `high` or below (Sonnet 5.5,
    /// where `disabled` is a 400). HTTP 400 on Haiku 5.5.
    BetweenTools,
    /// Send no `thinking` field: the model's own default (adaptive thinking on the 5.5 models).
    ModelDefault,
}

/// How much of the thinking text the server streams (`thinking.display`), for the modes that
/// think.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThinkingDisplay {
    /// `"omitted"`: the server skips streaming thinking tokens and sends only the signature
    /// (faster time to first text). The documented default on Haiku 5.5 and Sonnet 5.5.
    Omitted,
    /// `"summarized"`.
    Summarized,
}

impl ThinkingDisplay {
    /// The wire value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Omitted => "omitted",
            Self::Summarized => "summarized",
        }
    }
}

/// `output_config.effort`: how many tokens the model spends, including on tool calls. Defaults
/// per model when left out: Haiku 5.5 `medium`, Sonnet 5.5 `high`, Opus 5.5 `medium`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Effort {
    /// `"low"`: the cheapest and fastest level.
    Low,
    /// `"medium"`.
    Medium,
    /// `"high"`.
    High,
    /// `"xhigh"` (HTTP 400 together with [`ThinkingMode::Disabled`] or
    /// [`ThinkingMode::BetweenTools`]; [`AnthropicLlm::new`](super::AnthropicLlm::new) refuses
    /// the combination before any I/O).
    Xhigh,
    /// `"max"` (HTTP 400 together with [`ThinkingMode::Disabled`] or
    /// [`ThinkingMode::BetweenTools`]; refused by [`AnthropicLlm::new`](super::AnthropicLlm::new)
    /// like [`Self::Xhigh`]).
    Max,
}

impl Effort {
    /// The wire value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// The `tool_choice` request parameter (sent only when the request has tools).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolChoice {
    /// `{"type":"auto"}`: the model answers in text or calls tools. The default; with
    /// [`AnthropicConfig::disable_parallel_tool_use`] off nothing is sent at all (the API's own
    /// default), so the field never changes between turns.
    #[default]
    Auto,
    /// `{"type":"any"}`: the model must call some tool. HTTP 400 on Sonnet 5.5 and Opus 5.5.
    ///
    /// **Forced-tool loop hazard:** the setting is per adapter, not per request, so it also
    /// applies to the request that follows a `tool_result`: the model must call a tool again and
    /// a tool loop never ends in spoken text. Use it only for single-shot, tool-only agents, or
    /// build a second adapter for the follow-up requests.
    Any,
    /// `{"type":"tool","name":...}`: the model must call this tool, which must be among the
    /// request's tools. HTTP 400 on Sonnet 5.5 and Opus 5.5. The same forced-tool loop hazard as
    /// [`Self::Any`] applies: every request of the adapter, including the one after the tool
    /// result, is forced to call the tool again.
    Tool(String),
    /// `{"type":"none"}`: the model must not call tools.
    None,
}

/// How long a prompt-cache breakpoint lives (`cache_control.ttl`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CacheTtl {
    /// 5 minutes (the API default, no `ttl` is sent): refreshed free of charge by every hit,
    /// plenty for the turns of one phone call.
    #[default]
    FiveMinutes,
    /// 1 hour (`"ttl":"1h"`): a cache write costs 2x the base input price instead of 1.25x; worth
    /// it for a long persona prompt reused across many calls.
    OneHour,
}

/// Where the adapter puts `cache_control` breakpoints (at most three of the four allowed).
///
/// A breakpoint caches the whole prompt up to and including its block (`tools`, then `system`,
/// then `messages`). Prompts shorter than the model's minimum (512 tokens on the Claude 5.5
/// models) are simply processed uncached, so the defaults are safe for short prompts too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheConfig {
    /// On the last tool definition: caches all tools.
    pub tools: bool,
    /// On the last block of the top-level `system` prompt (the leading system messages).
    pub system: bool,
    /// On the last block of the last *stable* history turn: the turn before the newest user
    /// turn, i.e. everything the previous request already sent. The newest turn (the user's
    /// words, a tool result, volatile notes) is deliberately left uncached: it is new each time,
    /// so caching it would only pay the write premium. The next request's lookback finds this
    /// request's breakpoint, so every turn adds just its own delta to the cache.
    pub history: bool,
    /// Lifetime of every breakpoint.
    pub ttl: CacheTtl,
}

impl CacheConfig {
    /// No breakpoints at all.
    pub const OFF: Self = Self {
        tools: false,
        system: false,
        history: false,
        ttl: CacheTtl::FiveMinutes,
    };
}

impl Default for CacheConfig {
    /// Tools, system and history cached, 5 minutes.
    fn default() -> Self {
        Self {
            tools: true,
            system: true,
            history: true,
            ttl: CacheTtl::FiveMinutes,
        }
    }
}

/// Configuration of [`AnthropicLlm`](super::AnthropicLlm). `Debug` never prints the API key.
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    /// API key (`x-api-key`). Never logged: it is a [`Secret`].
    pub api_key: Secret<String>,
    /// Base URL without the `/v1/...` path. `https://api.anthropic.com` by default; plain
    /// `http://` is accepted for loopback hosts (tests, local gateways) and, elsewhere, only with
    /// [`Self::allow_insecure_http`].
    pub base_url: String,
    /// Model id ([`DEFAULT_MODEL`] by default).
    pub model: String,
    /// `max_tokens` ([`DEFAULT_MAX_TOKENS`] by default). Thinking tokens count toward it.
    pub max_tokens: u32,
    /// `thinking` ([`ThinkingMode::Disabled`] by default).
    pub thinking: ThinkingMode,
    /// `thinking.display`, sent with [`ThinkingMode::Adaptive`] and [`ThinkingMode::BetweenTools`]
    /// only; `None` leaves it out (the model default, `omitted` on the 5.5 models). The
    /// placement of `display` inside the `adaptive` and `between_tools` objects is UNVERIFIED in
    /// `docs/providers/anthropic.md`, so it is off by default.
    pub thinking_display: Option<ThinkingDisplay>,
    /// `output_config.effort`; `None` leaves it out (the model's default).
    pub effort: Option<Effort>,
    /// `tool_choice` ([`ToolChoice::Auto`] by default). Applied to every request of the adapter:
    /// see the forced-tool loop hazard on [`ToolChoice::Any`].
    pub tool_choice: ToolChoice,
    /// `disable_parallel_tool_use` inside `tool_choice`: at most one tool call per response
    /// (`auto`) or exactly one (`any`/`tool`). Off by default: several `tool_use` blocks may
    /// arrive in one response, and the caller decides how to run them.
    pub disable_parallel_tool_use: bool,
    /// Prompt-cache breakpoints.
    pub cache: CacheConfig,
    /// Send `strict: true` on every tool (grammar-constrained arguments). Off by default: the
    /// first use of a schema adds latency while the grammar compiles, and the schemas must obey
    /// the strict rules (`additionalProperties: false`, no `minimum`/`maxLength`, ...) or the
    /// request is a 400. The caller validates arguments anyway.
    pub strict_tools: bool,
    /// Replay the `thinking` and `redacted_thinking` blocks of earlier turns (the ones echoed
    /// from [`Message::provider_data`](crate::Message::provider_data)). `true` by default (the
    /// API requires them within a tool turn); turn it off to drop them from the replayed history
    /// if the API rejects a replay (for example after the conversation moved to another API
    /// key: the blocks only work in the account that produced them).
    pub replay_thinking: bool,
    /// Send a late `System` message that sits right after a user turn (and before an assistant
    /// turn or the end of the history) as a mid-conversation `{"role":"system","content":"..."}`
    /// message. `false` by default: the note is folded into the next user turn as a text block,
    /// which every model accepts. `docs/providers/anthropic.md` allows the mid-conversation role
    /// on Haiku 5.5 and Sonnet 5.5 only (nothing is said about Opus 5.5 or the legacy ids) and
    /// the plain-string `content` shape is UNVERIFIED until a live test, so a wrong guess is an
    /// HTTP 400 after a full round trip. Enable it only for a model the doc lists, once the live
    /// test confirmed the wire shape. A note in any other position is always folded.
    pub mid_conversation_system: bool,
    /// Beta features: sent as the comma-separated `anthropic-beta` header when non-empty.
    pub beta: Vec<String>,
    /// `anthropic-workspace-id` header (needed only with a multi-workspace key).
    pub workspace_id: Option<String>,
    /// Bound on the TCP + TLS connect, handed to the HTTP client. The clock-based
    /// [`Self::first_byte_timeout`] covers the connect as well and is what the contract suite
    /// sees.
    pub connect_timeout: Duration,
    /// Deadline, on the injected clock, from the start of `stream()` (connect, TLS, request,
    /// waiting for the response headers) to the response headers. Expiry is `Core(Timeout)`
    /// from `stream()`.
    pub first_byte_timeout: Duration,
    /// Longest silence between two *content* events once the response has produced its first
    /// one, on the injected clock. `ping` events and SSE comments are **not** activity: a stream
    /// that carries only pings is stalled. Time spent inside an open `tool_use` block is bounded
    /// by [`Self::tool_idle_timeout`] instead, and time inside an open thinking block by
    /// [`Self::first_output_timeout`]. Expiry is `LlmEvent::Error(Core(Timeout))`.
    pub idle_timeout: Duration,
    /// Longest silence while a `tool_use` block is open. Longer than [`Self::idle_timeout`]
    /// because the docs say tool arguments stream "one complete key and value" at a time, with
    /// gaps in between while the model works.
    pub tool_idle_timeout: Duration,
    /// Deadline, on the injected clock, from the response headers to the first content event of
    /// the model (a content block starting, a delta, or the end of the message). Neither pings,
    /// comments nor `message_start` extend it, so a model or proxy that stalls behind pings is
    /// cut off here and not after [`Self::max_stream_duration`]. Longer than
    /// [`Self::idle_timeout`] by default because a thinking model may think before it speaks
    /// (with `display: omitted` nothing at all is streamed meanwhile). The same allowance covers
    /// the silence inside an open `thinking` or `redacted_thinking` block: with `display:
    /// omitted` the block opens and then only a `signature_delta` arrives when the thinking ends.
    /// Expiry is `LlmEvent::Error(Core(Timeout))`.
    pub first_output_timeout: Duration,
    /// How long an idle connection stays in the client's pool (`None`: until the server closes
    /// it). Bounds how long a [`warm`](crate::LlmProvider::warm) connection stays useful.
    pub pool_idle_timeout: Option<Duration>,
    /// Allow a plain `http://` [`Self::base_url`] on a non-loopback host. Off by default: the
    /// key would travel in cleartext, so `AnthropicLlm::new` rejects it. Loopback (`localhost`,
    /// `127.0.0.0/8`, `::1`) is always allowed.
    pub allow_insecure_http: bool,
    /// Cap on the total duration of one streamed response, on the injected clock.
    pub max_stream_duration: Duration,
    /// Cap on the bytes held for one unfinished SSE event ([`DEFAULT_MAX_EVENT_BYTES`]); a
    /// larger event is a protocol error, so a misbehaving server cannot grow memory.
    pub max_event_bytes: usize,
}

impl AnthropicConfig {
    /// A configuration for `api_key` with the defaults: [`DEFAULT_MODEL`], thinking disabled,
    /// default effort, `auto` tool choice with parallel calls, caching of tools, system and
    /// history for 5 minutes, 2 s connect, 10 s to the response headers, 30 s to the first
    /// content, 10 s between content events (30 s inside a tool call), 5 min per response.
    pub fn new(api_key: Secret<String>) -> Self {
        Self {
            api_key,
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: DEFAULT_MODEL.to_owned(),
            max_tokens: DEFAULT_MAX_TOKENS,
            thinking: ThinkingMode::default(),
            thinking_display: None,
            effort: None,
            tool_choice: ToolChoice::default(),
            disable_parallel_tool_use: false,
            cache: CacheConfig::default(),
            strict_tools: false,
            replay_thinking: true,
            mid_conversation_system: false,
            beta: Vec::new(),
            workspace_id: None,
            connect_timeout: Duration::from_secs(2),
            first_byte_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(10),
            tool_idle_timeout: Duration::from_secs(30),
            first_output_timeout: Duration::from_secs(30),
            pool_idle_timeout: Some(DEFAULT_POOL_IDLE_TIMEOUT),
            allow_insecure_http: false,
            max_stream_duration: Duration::from_secs(300),
            max_event_bytes: DEFAULT_MAX_EVENT_BYTES,
        }
    }

    /// Sets the connect, first-byte, first-output, idle and tool-idle deadlines to `timeout`
    /// (what the provider contract suite configures).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self.first_byte_timeout = timeout;
        self.idle_timeout = timeout;
        self.tool_idle_timeout = timeout;
        self.first_output_timeout = timeout;
        self
    }

    /// Sets the base URL (for example a loopback mock or a gateway).
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
