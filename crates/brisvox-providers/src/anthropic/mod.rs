//! Anthropic LLM adapter: streaming chat with tools over the Messages API.
//!
//! # API surface
//! `POST {base}/v1/messages` with `stream: true`, replaying the whole history each turn (the API
//! is stateless), over raw HTTP and Server-Sent Events: there is no official Rust SDK, and the
//! OpenAI-compatibility layer does not support prompt caching or return thinking. Headers:
//! `x-api-key`, `anthropic-version: 2023-06-01`, `content-type`, `accept: text/event-stream`,
//! plus the optional `anthropic-workspace-id` and `anthropic-beta`. The decision and its sources
//! are recorded in `docs/providers/anthropic.md`.
//!
//! # Model, thinking and effort
//! The model is configuration ([`AnthropicConfig::model`], default [`DEFAULT_MODEL`]). Thinking
//! is **on by default** on the Claude 5.5 models and a thinking pause is dead air on a phone call,
//! so the adapter states [`ThinkingMode::Disabled`] unless configured otherwise (Haiku 5.5: ok at
//! effort `high` or below; Sonnet 5.5 needs [`ThinkingMode::BetweenTools`], Opus 5.5 cannot turn
//! thinking off). `temperature`, `top_p` and `top_k` are not sent: the 5.x models reject
//! non-default values. [`AnthropicConfig::effort`] sets `output_config.effort`. The docs warn
//! that Haiku 5.5 sometimes writes reasoning-like text into its reply, more often with thinking
//! off or `low` effort, and a voice agent would speak it: if that shows up, switch to
//! [`ThinkingMode::Adaptive`] with [`Effort::Medium`]. Which setting wins on latency is
//! UNVERIFIED until the live test (`tests/anthropic/live.rs`) measures it. Changing the thinking mode, the
//! effort, the tool choice or the tools between requests invalidates the message cache: pick them
//! once per call.
//!
//! # Prompt caching
//! `cache_control` breakpoints (configurable, [`CacheConfig`]) go on the last tool, the last
//! block of the top-level `system` and the last block of the last *stable* history turn, i.e.
//! the turn before the newest user turn (three of the four allowed breakpoints). The newest turn
//! is new every time, so caching it would only pay the write premium; each request's lookback
//! finds the previous request's breakpoint, so a turn adds just its own delta to the cache.
//! Callers must keep the system prompt and tools stable, keep the history append-only, and put
//! volatile text (time, caller name, retrieved snippets) in the newest user turn. The tool array
//! is rendered once and reused byte for byte while the [`ToolSpec`](crate::ToolSpec)s are equal.
//! `Usage::input_tokens` is the sum of Anthropic's `input_tokens` (which counts only the tokens
//! after the last breakpoint), `cache_creation_input_tokens` and `cache_read_input_tokens`;
//! `Usage::cached_input_tokens` is the cache read. Prompts under the model's minimum (512 tokens
//! on the 5.5 models) are processed uncached without an error.
//!
//! # Streaming
//! The body is parsed incrementally as SSE with a bounded buffer
//! ([`AnthropicConfig::max_event_bytes`]). `text_delta` becomes [`LlmEvent::TextDelta`]; a
//! `tool_use` block becomes [`LlmEvent::ToolCallDelta`] (dense index, `id` and `name` first) and
//! its `input_json_delta` lumps; `ping` and unknown events are ignored; an `error` event after
//! the HTTP 200 is terminal; `message_stop` becomes [`LlmEvent::Done`] with the `stop_reason`
//! mapped (`end_turn`/`stop_sequence` -> `Stop`, `tool_use` -> `ToolCalls`, `max_tokens` and
//! `model_context_window_exceeded` -> `Length`; `refusal` and `pause_turn` have no variant and
//! become `Stop` with a log line) and the cumulative usage. A turn that ends with no text and no
//! call (the documented "empty response") is logged and delivered as an empty `Stop`.
//!
//! `provider_data` is the JSON array of the response's thinking and `redacted_thinking` blocks
//! (unmodified, with their signatures) plus placeholders that keep the position of the text and
//! of each `tool_use`; it is `None` for a response without thinking. The next request echoes the
//! thinking blocks back in place, as the API requires within a tool turn. Only the text that was
//! not played is removed: thinking in front of it stays, directly in front of the next kept
//! block (a surviving `tool_use`); thinking in front of a call that is dropped (or of nothing
//! kept) goes with it. Thinking blocks only work in the account
//! that produced them: replay a conversation through the same key, or turn
//! [`AnthropicConfig::replay_thinking`] off.
//!
//! # Memory and CPU bounds
//! Every response is bounded: SSE events by [`AnthropicConfig::max_event_bytes`], content blocks
//! (1024), tool calls (128), replayable blocks (256 entries, 16 MiB) and streamed tool-call
//! arguments (1 MiB per call, 4 MiB per response; more is [`Error::Protocol`]); the event
//! channel is bounded and applies backpressure. Building a request re-parses and re-serialises
//! the whole history on every turn, and rendering the tool definitions (on a cache miss: the
//! first request of a call, or any change to the tools or the cache settings) parses every
//! schema: past 32 KiB of history plus not-yet-rendered tools that runs on the blocking pool,
//! not on a tokio worker; below it, inline. The tool cache lock is never held while rendering.
//!
//! # Timeouts and cancellation
//! Every wait is bounded on the injected clock: connect and response headers
//! ([`AnthropicConfig::first_byte_timeout`], `Core(Timeout)` from `stream()`); the time from the
//! headers to the first content event ([`AnthropicConfig::first_output_timeout`]; neither
//! `ping`s, comments nor `message_start` extend it); the silence between content events after
//! that ([`AnthropicConfig::idle_timeout`], or [`AnthropicConfig::tool_idle_timeout`] while a
//! `tool_use` block is open, because the docs say arguments stream "one complete key and value"
//! at a time with gaps, or [`AnthropicConfig::first_output_timeout`] while a `thinking` or
//! `redacted_thinking` block is open, because with `display: omitted` the block opens and then
//! nothing but a signature arrives when the thinking ends; `ping`s do not count and time the
//! consumer spends not reading is not charged); and the total duration of a response. All three body deadlines are
//! `Core(Timeout)` as an event. Dropping the [`LlmStream`] ends the reader task at once (even
//! while the server is silent) and drops the HTTP response, which closes the connection
//! (HTTP/1.1) or resets the stream (HTTP/2): generation stops. The docs describe no cancel
//! endpoint, and whether tokens generated before the drop are billed is UNVERIFIED. Dropping the
//! `stream()` future before the headers arrive aborts the request the same way.
//!
//! # Errors and retries
//! Nothing the server sent as free text (error messages, JSON error details, the request URL)
//! reaches an error or a log; a `base_url` with credentials in it (`user:pass@host`) is refused.
//! A non-2xx status is [`Error::Rejected`] with that status (400, 401, 402, 403, 413, 429, 500,
//! 504, 529, ...). The adapter reads `retry-after`, `request-id` and the error `type` and logs
//! them (never the message: it can echo a key fragment), but it does **not** retry: the caller
//! decides, and a retry of a streaming request belongs to the turn logic, not the transport. An
//! `error` event after the 200 maps by its `type` (`overloaded_error` 529, `rate_limit_error`
//! 429, `api_error` 500, `invalid_request_error` 400, ...); an unknown type is
//! [`Error::Protocol`] naming the sanitised type. A stream that ends without `message_stop` is
//! `Disconnected`. A 429 with `enforced_spend_limit_reached` has no `retry-after` and fails until
//! the first of the month: retrying is pointless (see the doc).
//!
//! # Warm-up
//! [`warm`](LlmProvider::warm) sends an unauthenticated `HEAD /v1/messages`, ignores the status
//! and keeps the connection in the client's pool, so the next [`stream`](LlmProvider::stream)
//! skips DNS, TCP and TLS. A cache warm-up with `max_tokens: 0` ("populates the prompt cache
//! without generating a response") is **not** done: the docs do not say whether it is allowed
//! with `stream: true`, nor its latency or response shape (UNVERIFIED), and `warm()` has no
//! request to cache. HTTP/2 connections are kept alive with pings while idle, and the pool keeps
//! an idle connection for [`AnthropicConfig::pool_idle_timeout`] (10 minutes by default).
//!
//! # Gaps against the Messages API
//! A late `System` message is folded into the next user turn unless
//! [`AnthropicConfig::mid_conversation_system`] is on (the mid-conversation role is
//! model-dependent and its wire shape UNVERIFIED). `Effort::Xhigh` and `Effort::Max` with
//! `ThinkingMode::Disabled` or `ThinkingMode::BetweenTools` are refused by [`AnthropicLlm::new`]
//! (documented HTTP 400).
//!
//! brisvox has no per-result error flag: a [`Role::Tool`](crate::Role::Tool) message becomes a
//! `tool_result` without `is_error`, so a call cancelled by a barge-in is answered with the
//! caller's own text. `Message` carries no images or documents.
//!
//! # Plain HTTP
//! `http://` base URLs are accepted for loopback hosts only, unless
//! [`AnthropicConfig::allow_insecure_http`] is set, so the key never crosses the network in
//! cleartext by accident.

mod config;
mod events;
mod request;
mod sse;

use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_core::Error as CoreError;
use brisvox_core::SharedClock;
use brisvox_core::clock::deadline_after;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use tokio::sync::mpsc;

pub use config::{
    API_VERSION, AnthropicConfig, CacheConfig, CacheTtl, DEFAULT_BASE_URL, DEFAULT_MAX_EVENT_BYTES,
    DEFAULT_MAX_TOKENS, DEFAULT_MODEL, DEFAULT_POOL_IDLE_TIMEOUT, Effort, ThinkingDisplay,
    ThinkingMode, ToolChoice,
};

use self::events::{Flow, Mapper, sanitize};
use self::request::{RenderedTools, build_body};
use self::sse::SseParser;
use crate::error::{Error, Result};
use crate::llm::{LLM_EVENT_CAPACITY, LlmEvent, LlmProvider, LlmRequest, LlmStream};

/// Provider name used in errors and logs.
pub(crate) const PROVIDER: &str = "anthropic";

/// Bytes of an error response body read for logging.
const ERROR_BODY_LIMIT: usize = 8 * 1024;

/// Bound on reading an error response body.
const ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(1);

/// After `message_stop`, the rest of the body (normally just the chunk terminator) is drained so
/// an HTTP/1.1 connection can return to the pool: at most this many bytes...
const DRAIN_LIMIT: usize = 16 * 1024;

/// ... and at most this long in total.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// Streaming chat with tools on the Anthropic Messages API. See the [module docs](self).
///
/// Cheap to share: wrap it in an `Arc`. The HTTP client (and its connection pool) is NOT owned
/// by the value: it comes from the process-wide pool (`net::shared_client`, keyed by origin and
/// connection settings), outlives the adapter and is shared by every instance and every
/// runtime that talks to the same origin the same way, so building one adapter per call is
/// fine and a call's first request finds the connection a previous call or `warm()` opened.
#[derive(Debug)]
pub struct AnthropicLlm {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    config: AnthropicConfig,
    clock: SharedClock,
    client: reqwest::Client,
    messages_url: String,
    /// `x-api-key`, `anthropic-version`, and the optional workspace and beta headers, sent with
    /// `stream` only.
    auth_headers: HeaderMap,
    tools: Mutex<Option<Arc<RenderedTools>>>,
    /// Bodies built on a thread other than the caller's, i.e. on the blocking pool (tests only;
    /// per instance, so tests running in parallel threads cannot see each other's builds).
    #[cfg(test)]
    offloaded_builds: std::sync::atomic::AtomicUsize,
}

/// Size of a request's history (message text, tool-call arguments and replayed `provider_data`)
/// above which the body is built on the blocking pool instead of the tokio worker. Below it the
/// parse and serialise passes take on the order of 30 us (one pass over the bytes each), the
/// budget CONTRIBUTING.md allows on a worker.
const OFFLOAD_BYTES: usize = 32 * 1024;

/// Bytes of history `request` carries, as far as building the body has to read them.
fn request_bytes(request: &LlmRequest) -> usize {
    request.messages.iter().fold(0usize, |total, m| {
        let calls: usize = m
            .tool_calls
            .iter()
            .map(|c| c.id.len() + c.name.len() + c.arguments.len())
            .sum();
        total
            .saturating_add(m.content.len())
            .saturating_add(calls)
            .saturating_add(m.provider_data.as_deref().map_or(0, str::len))
    })
}

/// Bytes of tool definitions `request` carries, as far as rendering them has to read them.
fn tool_bytes(request: &LlmRequest) -> usize {
    request.tools.iter().fold(0usize, |total, t| {
        total
            .saturating_add(t.name.len())
            .saturating_add(t.description.len())
            .saturating_add(t.parameters_schema.len())
    })
}

/// Runs CPU-bound `work` inline when it is small, otherwise on tokio's blocking pool (never on a
/// worker thread).
///
/// # Errors
/// [`Error::Core`] (`ProcessorPanicked`) if the work panicked or was cancelled by a runtime
/// shutdown.
async fn run_cpu<T, F>(heavy: bool, work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    if !heavy {
        return Ok(work());
    }
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| Error::Core(CoreError::ProcessorPanicked("anthropic request builder")))
}

/// The JSON body for `request`. Building it re-parses and re-serialises the whole history,
/// including the replayed thinking blocks (cost linear in the history, repeated every turn), so
/// past [`OFFLOAD_BYTES`] it runs on the blocking pool, not on a tokio worker.
async fn build_request_body(inner: &Arc<Inner>, request: LlmRequest) -> Result<bytes::Bytes> {
    // A cache hit is a cheap comparison; a miss renders (parses) every schema, which counts
    // towards the offload size.
    let cached = inner.cached_tools(&request);
    let pending_tools = if cached.is_none() {
        tool_bytes(&request)
    } else {
        0
    };
    let heavy = request_bytes(&request).saturating_add(pending_tools) > OFFLOAD_BYTES;
    #[cfg(test)]
    let caller = std::thread::current().id();
    let build = Arc::clone(inner);
    run_cpu(heavy, move || {
        // Counted where the work actually runs: on another thread than the caller's.
        #[cfg(test)]
        if std::thread::current().id() != caller {
            build
                .offloaded_builds
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        let tools = match cached {
            Some(tools) => Some(tools),
            None if request.tools.is_empty() => None,
            None => Some(build.render_tools(&request)?),
        };
        build_body(&build.config, &request, tools.as_deref())
    })
    .await?
}

fn protocol(detail: impl Into<String>) -> Error {
    Error::Protocol {
        provider: PROVIDER,
        detail: detail.into(),
    }
}

/// Runs `future` with a deadline on the injected clock.
async fn within<F: Future>(
    clock: &SharedClock,
    limit: Duration,
    what: &'static str,
    future: F,
) -> std::result::Result<F::Output, CoreError> {
    tokio::select! {
        biased;
        out = future => Ok(out),
        () = clock.sleep(limit) => Err(CoreError::Timeout { what, after: limit }),
    }
}

/// A transport error as a provider error: a timeout stays a timeout, everything else (refused,
/// reset, TLS failure, truncated body) is a lost connection. The request URL is stripped before
/// anything is logged: the `Display` of a `reqwest` error includes it.
fn transport_error(error: reqwest::Error, limit: Duration, what: &'static str) -> Error {
    let error = error.without_url();
    tracing::debug!(%error, "anthropic transport error");
    if error.is_timeout() {
        Error::Core(CoreError::Timeout { what, after: limit })
    } else {
        Error::Disconnected { provider: PROVIDER }
    }
}

/// Refuses a base URL that would send the key in cleartext to a remote host.
fn check_base_url(config: &AnthropicConfig) -> Result<()> {
    // The URL itself is never echoed: it may carry credentials.
    let url = reqwest::Url::parse(&config.base_url)
        .map_err(|_| protocol("base_url is not a valid absolute URL"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(protocol(
            "base_url carries credentials (user:password@host): they would end up in logs and \
             error messages; pass the API key in the config instead",
        ));
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" => {
            let host = url.host_str().unwrap_or_default();
            let host = host.trim_start_matches('[').trim_end_matches(']');
            let loopback = host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback());
            if loopback || config.allow_insecure_http {
                Ok(())
            } else {
                Err(protocol(
                    "base_url is plain http:// on a non-loopback host: the API key would be sent \
                     unencrypted (use https://, or set allow_insecure_http)",
                ))
            }
        }
        _ => Err(protocol("base_url must be an http:// or https:// URL")),
    }
}

/// Refuses settings the docs list as a guaranteed HTTP 400, before any I/O.
fn check_settings(config: &AnthropicConfig) -> Result<()> {
    let high_effort = matches!(config.effort, Some(Effort::Xhigh | Effort::Max));
    let low_thinking = matches!(
        config.thinking,
        ThinkingMode::Disabled | ThinkingMode::BetweenTools
    );
    if high_effort && low_thinking {
        return Err(protocol(
            "effort xhigh/max is an HTTP 400 together with thinking disabled or between_tools \
             (docs/providers/anthropic.md): use ThinkingMode::Adaptive, or lower the effort",
        ));
    }
    Ok(())
}

fn header_value(text: &str, what: &str, sensitive: bool) -> Result<HeaderValue> {
    let mut value = HeaderValue::from_str(text)
        .map_err(|_| protocol(format!("{what} is not a valid HTTP header value")))?;
    value.set_sensitive(sensitive);
    Ok(value)
}

impl AnthropicLlm {
    /// Builds the adapter and its HTTP client (rustls with the shared config of
    /// [`net::tls_config`](crate::net::tls_config), ALPN `h2` then `http/1.1`, no system proxy,
    /// no redirects so the key never follows one).
    ///
    /// # Errors
    /// [`Error::Protocol`] if the API key, the workspace id or a beta name is not a valid header
    /// value (the value is never included), the base URL is not `https://` (or `http://` on a
    /// loopback host, or on any host with `allow_insecure_http`) or carries credentials, the
    /// effort is `xhigh`/`max` with thinking `Disabled` or `BetweenTools` (a documented HTTP
    /// 400), or the HTTP client cannot be built.
    pub fn new(config: AnthropicConfig, clock: SharedClock) -> Result<Self> {
        check_base_url(&config)?;
        check_settings(&config)?;
        let client = crate::net::shared_client(&crate::net::HttpClientSpec {
            provider: PROVIDER,
            base_url: &config.base_url,
            connect_timeout: config.connect_timeout,
            pool_idle_timeout: config.pool_idle_timeout,
            http2_prior_knowledge: false,
        })?;
        let mut auth_headers = HeaderMap::new();
        auth_headers.insert(
            HeaderName::from_static("x-api-key"),
            header_value(config.api_key.expose(), "the API key", true)?,
        );
        auth_headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static(API_VERSION),
        );
        if let Some(workspace) = &config.workspace_id {
            auth_headers.insert(
                HeaderName::from_static("anthropic-workspace-id"),
                header_value(workspace, "the workspace id", false)?,
            );
        }
        if !config.beta.is_empty() {
            auth_headers.insert(
                HeaderName::from_static("anthropic-beta"),
                header_value(&config.beta.join(","), "a beta name", false)?,
            );
        }
        let messages_url = format!("{}/v1/messages", config.base_url.trim_end_matches('/'));
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                clock,
                client,
                messages_url,
                auth_headers,
                tools: Mutex::new(None),
                #[cfg(test)]
                offloaded_builds: std::sync::atomic::AtomicUsize::new(0),
            }),
        })
    }

    /// The configuration in use (the API key prints as `[REDACTED]`).
    pub fn config(&self) -> &AnthropicConfig {
        &self.inner.config
    }
}

impl Inner {
    /// The rendered tools for `request` when the cache still holds them (`None`: no tools, or
    /// they changed). The lock is held for the comparison only.
    fn cached_tools(&self, request: &LlmRequest) -> Option<Arc<RenderedTools>> {
        if request.tools.is_empty() {
            return None;
        }
        let strict = self.config.strict_tools;
        let cache = self.config.cache.tools.then_some(self.config.cache.ttl);
        let slot = self.tools.lock().unwrap_or_else(PoisonError::into_inner);
        slot.as_ref()
            .filter(|c| c.matches(&request.tools, strict, cache))
            .map(Arc::clone)
    }

    /// Renders the tools of `request` (parses every schema: CPU-bound, run it off the workers
    /// when it is large) and keeps the result for the next request. Rendering happens outside
    /// the lock, so concurrent calls do not queue behind it.
    fn render_tools(&self, request: &LlmRequest) -> Result<Arc<RenderedTools>> {
        let strict = self.config.strict_tools;
        let cache = self.config.cache.tools.then_some(self.config.cache.ttl);
        let rendered = RenderedTools::render(&request.tools, strict, cache)?;
        *self.tools.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&rendered));
        Ok(rendered)
    }

    /// Reads a refusal's `retry-after`, `request-id` and error type for the log, and maps it.
    async fn rejection(&self, mut response: reqwest::Response) -> Error {
        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(sanitize)
        };
        let retry_after = header("retry-after").and_then(|v| v.parse::<u64>().ok());
        let request_id = header("request-id");
        let mut body = Vec::new();
        let read = async {
            while let Ok(Some(chunk)) = response.chunk().await {
                let room = ERROR_BODY_LIMIT.saturating_sub(body.len());
                body.extend_from_slice(chunk.get(..room.min(chunk.len())).unwrap_or_default());
                if body.len() >= ERROR_BODY_LIMIT {
                    break;
                }
            }
        };
        let _ = within(
            &self.clock,
            ERROR_BODY_TIMEOUT,
            "anthropic error body",
            read,
        )
        .await;
        let (kind, code) = parse_error_body(&body);
        // The error message is deliberately not logged: a 401 echoes part of the key.
        tracing::warn!(
            status,
            retry_after_secs = retry_after,
            request_id = request_id.as_deref(),
            error_type = kind.as_deref(),
            error_code = code.as_deref(),
            "anthropic rejected the request (not retried by the adapter)"
        );
        Error::Rejected {
            provider: PROVIDER,
            status,
        }
    }
}

/// The `error.type` and `error.details.error_code` of an error body, sanitised for logging.
fn parse_error_body(body: &[u8]) -> (Option<String>, Option<String>) {
    #[derive(serde::Deserialize)]
    struct Envelope {
        error: Option<Detail>,
    }
    #[derive(serde::Deserialize)]
    struct Detail {
        r#type: Option<String>,
        details: Option<Details>,
    }
    #[derive(serde::Deserialize)]
    struct Details {
        error_code: Option<String>,
    }
    let Ok(Envelope {
        error: Some(detail),
    }) = serde_json::from_slice(body)
    else {
        return (None, None);
    };
    (
        detail.r#type.as_deref().map(sanitize),
        detail
            .details
            .and_then(|d| d.error_code)
            .as_deref()
            .map(sanitize),
    )
}

impl LlmProvider for AnthropicLlm {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmStream> {
        let inner = &self.inner;
        let config = &inner.config;
        let body = build_request_body(inner, request).await?;
        let mut headers = inner.auth_headers.clone();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        let send = inner
            .client
            .post(&inner.messages_url)
            .headers(headers)
            .body(body)
            .send();
        let response = within(
            &inner.clock,
            config.first_byte_timeout,
            "anthropic first byte",
            send,
        )
        .await?
        .map_err(|e| transport_error(e, config.connect_timeout, "anthropic connect"))?;
        if !response.status().is_success() {
            return Err(inner.rejection(response).await);
        }
        if let Some(id) = response
            .headers()
            .get("request-id")
            .and_then(|v| v.to_str().ok())
        {
            tracing::debug!(request_id = sanitize(id), "anthropic response started");
        }
        let (tx, rx) = mpsc::channel(LLM_EVENT_CAPACITY);
        tokio::spawn(pump(Arc::clone(inner), response, tx));
        Ok(LlmStream::new(rx))
    }

    async fn warm(&self) -> Result<()> {
        let inner = &self.inner;
        let config = &inner.config;
        // No credentials: any HTTP answer (401, 404, 405...) proves the connection works and
        // leaves it in the pool.
        let head = inner.client.head(&inner.messages_url).send();
        within(
            &inner.clock,
            config.first_byte_timeout,
            "anthropic warm",
            head,
        )
        .await?
        .map(drop)
        .map_err(|e| transport_error(e, config.connect_timeout, "anthropic warm"))
    }
}

/// The silence allowed after a content event, and the name of the timeout, given what is open:
/// a `tool_use` block (arguments stream a key and value at a time), a `thinking` block (with
/// `display: omitted` nothing but a signature arrives when it ends), or neither.
fn idle_allowance(mapper: &Mapper, config: &AnthropicConfig) -> (&'static str, Duration) {
    if mapper.tool_block_open() {
        ("anthropic idle in a tool call", config.tool_idle_timeout)
    } else if mapper.thinking_block_open() {
        ("anthropic idle in thinking", config.first_output_timeout)
    } else {
        ("anthropic idle", config.idle_timeout)
    }
}

/// Reads the response body, maps its events and sends them until `message_stop`, an error, or
/// the consumer drops the stream.
///
/// Deadlines: until the first content event the only body deadline is `first_output_timeout`;
/// after it, the idle deadline (`idle_timeout`, `tool_idle_timeout` while a `tool_use` block is
/// open, `first_output_timeout` while a thinking block is open) is re-armed by every content
/// event (`Flow::Progress`; a `ping`, a comment or
/// `message_start` does not re-arm it) and only once the event's outputs have been handed to the
/// consumer, so time spent waiting for a slow consumer is not charged to the server.
async fn pump(inner: Arc<Inner>, mut response: reqwest::Response, tx: mpsc::Sender<LlmEvent>) {
    let config = &inner.config;
    let clock = &inner.clock;
    let mut parser = SseParser::new(config.max_event_bytes);
    let mut mapper = Mapper::new();
    let mut out: Vec<LlmEvent> = Vec::with_capacity(4);
    let stream_deadline = deadline_after(clock.now(), config.max_stream_duration);
    let mut got_content = false;
    let mut event_deadline = deadline_after(clock.now(), config.first_output_timeout);
    let timer = clock.sleep_until(event_deadline.min(stream_deadline));
    tokio::pin!(timer);
    loop {
        let chunk = tokio::select! {
            biased;
            () = tx.closed() => return,
            () = timer.as_mut() => {
                let (what, after) = if clock.now() >= stream_deadline {
                    ("anthropic stream", config.max_stream_duration)
                } else if !got_content {
                    ("anthropic first output", config.first_output_timeout)
                } else {
                    idle_allowance(&mapper, config)
                };
                let _ = tx.send(LlmEvent::Error(Error::Core(CoreError::Timeout { what, after }))).await;
                return;
            }
            chunk = response.chunk() => chunk,
        };
        let bytes = match chunk {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                // The body ended without `message_stop`.
                let _ = tx
                    .send(LlmEvent::Error(Error::Disconnected { provider: PROVIDER }))
                    .await;
                return;
            }
            Err(e) => {
                let error = transport_error(e, config.idle_timeout, "anthropic idle");
                let _ = tx.send(LlmEvent::Error(error)).await;
                return;
            }
        };
        parser.push(&bytes);
        let mut progressed = false;
        loop {
            out.clear();
            let flow = match parser.next_event() {
                Ok(Some(data)) => mapper.handle(data, &mut out),
                Ok(None) => break,
                Err(e) => Err(protocol(e.to_string())),
            };
            progressed |= matches!(flow, Ok(Flow::Progress));
            for event in out.drain(..) {
                // Backpressure: wait for room (the socket is not read meanwhile).
                if tx.send(event).await.is_err() {
                    return;
                }
            }
            match flow {
                Ok(Flow::Idle | Flow::Progress) => {}
                Ok(Flow::Finished) => {
                    drop(tx);
                    drain(&inner, response).await;
                    return;
                }
                Err(error) => {
                    let _ = tx.send(LlmEvent::Error(error)).await;
                    return;
                }
            }
        }
        if progressed {
            got_content = true;
            let (_, limit) = idle_allowance(&mapper, config);
            event_deadline = deadline_after(clock.now(), limit);
        }
        // A new timer from the clock, not `Sleep::reset`: `reset` takes a tokio deadline, which
        // is the clock's own deadline only for the tokio-backed clocks.
        timer.set(clock.sleep_until(event_deadline.min(stream_deadline)));
    }
}

/// Reads what is left of a finished response so the connection can be reused: at most
/// [`DRAIN_LIMIT`] bytes and [`DRAIN_TIMEOUT`] in total, whichever comes first.
async fn drain(inner: &Inner, mut response: reqwest::Response) {
    let read = async {
        let mut left = DRAIN_LIMIT;
        while let Ok(Some(chunk)) = response.chunk().await {
            left = left.saturating_sub(chunk.len());
            if left == 0 {
                break;
            }
        }
    };
    // End of body, an error, or the deadline: dropping the response is the right outcome.
    let _ = within(&inner.clock, DRAIN_TIMEOUT, "anthropic drain", read).await;
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::thread;

    use super::*;
    use crate::llm::{Message, Role};

    fn request_with(provider_data_bytes: usize) -> LlmRequest {
        let data: Arc<str> = Arc::from("x".repeat(provider_data_bytes));
        LlmRequest {
            messages: vec![
                Message::new(Role::User, "hi"),
                Message::assistant("hello", Vec::new(), Some(data)),
                Message::new(Role::User, "again"),
            ],
            tools: Vec::new(),
        }
    }

    #[test]
    fn replayed_provider_data_counts_towards_the_offload_size() {
        assert!(request_bytes(&request_with(0)) <= OFFLOAD_BYTES);
        assert!(request_bytes(&request_with(OFFLOAD_BYTES)) > OFFLOAD_BYTES);
    }

    #[tokio::test]
    async fn small_work_runs_inline_and_large_work_leaves_the_calling_thread() {
        let here = thread::current().id();
        let small = run_cpu(false, || thread::current().id()).await.unwrap();
        assert_eq!(small, here);
        let large = run_cpu(true, || thread::current().id()).await.unwrap();
        assert_ne!(
            large, here,
            "heavy work must not run on the caller's thread"
        );
    }

    #[tokio::test]
    async fn only_large_histories_are_built_on_the_blocking_pool() {
        let config = AnthropicConfig::new(brisvox_core::Secret::new("sk-ant-test".to_owned()))
            .with_base_url("http://127.0.0.1:1");
        let llm = AnthropicLlm::new(config, brisvox_core::SystemClock::shared()).unwrap();
        let before = llm.inner.offloaded_builds.load(Ordering::SeqCst);
        // Whether the odd provider data replays or not is beside the point: the heavy request
        // must have gone through the blocking pool.
        let _ = build_request_body(&llm.inner, request_with(OFFLOAD_BYTES)).await;
        assert!(llm.inner.offloaded_builds.load(Ordering::SeqCst) > before);
        let before = llm.inner.offloaded_builds.load(Ordering::SeqCst);
        let _ = build_request_body(&llm.inner, request_with(0)).await;
        assert_eq!(llm.inner.offloaded_builds.load(Ordering::SeqCst), before);
    }

    #[tokio::test]
    async fn rendering_large_tool_definitions_runs_off_the_worker_and_only_on_a_cache_miss() {
        use crate::llm::ToolSpec;
        let config = AnthropicConfig::new(brisvox_core::Secret::new("sk-ant-test".to_owned()))
            .with_base_url("http://127.0.0.1:1");
        let llm = AnthropicLlm::new(config, brisvox_core::SystemClock::shared()).unwrap();
        // A schema of ~40 KB behind a tiny history.
        let padding = "p".repeat(OFFLOAD_BYTES + 8 * 1024);
        let schema = format!(r#"{{"type":"object","description":"{padding}","properties":{{}}}}"#);
        let request = LlmRequest {
            messages: vec![Message::new(Role::User, "hi")],
            tools: vec![ToolSpec {
                name: Arc::from("big"),
                description: Arc::from("d"),
                parameters_schema: Arc::from(schema),
            }],
        };
        assert!(request_bytes(&request) < OFFLOAD_BYTES);
        let before = llm.inner.offloaded_builds.load(Ordering::SeqCst);
        build_request_body(&llm.inner, request.clone())
            .await
            .unwrap();
        assert!(
            llm.inner.offloaded_builds.load(Ordering::SeqCst) > before,
            "the miss renders on the blocking pool"
        );
        let before = llm.inner.offloaded_builds.load(Ordering::SeqCst);
        build_request_body(&llm.inner, request).await.unwrap();
        assert_eq!(
            llm.inner.offloaded_builds.load(Ordering::SeqCst),
            before,
            "the hit renders nothing and the history is small"
        );
    }

    #[tokio::test]
    async fn a_panic_in_offloaded_work_is_an_error_not_a_crash() {
        let result = run_cpu(true, || -> u8 { panic!("boom") }).await;
        assert!(matches!(
            result,
            Err(Error::Core(CoreError::ProcessorPanicked(_)))
        ));
    }

    #[test]
    fn error_bodies_yield_a_sanitised_type_and_code_and_never_the_message() {
        let body = br#"{"type":"error","error":{"type":"rate_limit_error","message":"key sk-ant-SECRET","details":{"error_code":"enforced_spend_limit_reached"}},"request_id":"req_1"}"#;
        assert_eq!(
            parse_error_body(body),
            (
                Some("rate_limit_error".to_owned()),
                Some("enforced_spend_limit_reached".to_owned())
            )
        );
        assert_eq!(parse_error_body(b"<html>"), (None, None));
    }
}
