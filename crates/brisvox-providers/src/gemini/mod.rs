//! Google Gemini LLM adapter: streaming chat with tools over `streamGenerateContent`.
//!
//! # API surface
//! `POST {base}/v1beta/models/{model}:streamGenerateContent?alt=sse`, authenticated with the
//! `x-goog-api-key` header (a key in the URL would leak into logs), stateless: the whole history
//! is replayed each turn, so after an interruption the model sees only what was played. Google
//! also documents a stateful Interactions API that streams function-call arguments
//! incrementally; the decision to stay on `generateContent` (it matches the played-text
//! invariant and is fully supported) and its sources are recorded in
//! `docs/providers/gemini.md`.
//!
//! # Streaming and tool calls
//! The body is parsed incrementally as Server-Sent Events with a bounded buffer
//! ([`GeminiConfig::max_event_bytes`]); each event is one `GenerateContentResponse`. Text parts
//! become [`LlmEvent::TextDelta`]; thought summaries (`thought: true`) are never spoken. Function
//! calls arrive **whole** on `generateContent` (no partial arguments): each becomes one
//! [`LlmEvent::ToolCallDelta`] carrying the complete `args` JSON, with a stable `index` (position
//! among the response's calls) and the API's call `id`, or a made-up `brisvox_call_<index>` when
//! the API sent none (such an id is never sent back to Gemini). `finishReason` has no
//! tool-call value: `STOP` after a call is reported as [`FinishReason::ToolCalls`](crate::FinishReason::ToolCalls). Usage
//! (`promptTokenCount`, `cachedContentTokenCount`, candidates plus thought tokens) comes from
//! the last chunk that carries it. See `events` for the full mapping and the documented
//! handling of filtered or failed finishes.
//!
//! # Thought signatures
//! Gemini 3 answers HTTP 400 when the `thoughtSignature` of the first function call of the
//! current turn is not sent back. [`LlmEvent::Done`]'s `provider_data` is the JSON array of the
//! response's parts as received (signatures included); the caller stores it on the assistant
//! [`Message`](crate::Message) and the next request replays it, substituting the text that was
//! actually played. A turn rebuilt without provider data gets the documented skip-validation
//! value on its first call ([`GeminiConfig::fallback_signature`]). See `request` for the rules.
//!
//! # Tool schemas
//! Tool schemas are converted to the OpenAPI subset Gemini's `parameters` accepts (`schema`
//! lists what is kept, rewritten, dropped and degraded; degradations such as a nested free-form
//! object sent as a string are logged as warnings, and such a string is turned back into an
//! object in the arguments of the call), or sent unchanged as `parametersJsonSchema`
//! ([`SchemaMode::JsonSchema`]). The rendered tool array is cached (the eight most recently used tool sets) and
//! byte-stable while the [`ToolSpec`]s are equal, so implicit caching sees the
//! same prefix; large tool sets, and any set that uses `$ref` or `allOf`, are rendered on the
//! blocking pool. Callers
//! must keep the system prompt and tools stable and put volatile text (time, caller name) at the
//! end of the newest user message.
//!
//! # Memory and CPU bounds
//! SSE events are capped by [`GeminiConfig::max_event_bytes`]; a response may carry at most 128
//! function calls, 1 MiB of arguments per call, 4 MiB in total and 16 MiB of replayable parts
//! (more is [`Error::Protocol`]); the event channel is bounded and applies backpressure.
//! The reader never waits for the consumer for longer than
//! [`GeminiConfig::consumer_stall_timeout`]. Building a request re-parses and re-serialises the whole history (replayed parts included) on
//! every turn: past 32 KiB of history that runs on the blocking pool, not on a tokio worker.
//!
//! # Timeouts and cancellation
//! Every wait is bounded on the injected clock: connect and response headers
//! ([`GeminiConfig::first_byte_timeout`], `Core(Timeout)` from `stream()`); the time from the
//! headers to the first output of the model ([`GeminiConfig::first_output_timeout`]; comments
//! do not extend it); the silence between SSE events after that
//! ([`GeminiConfig::idle_timeout`]: only events that carry output count, not comments, empty
//! heartbeats, `{}`, usage-only or part-less chunks; time the consumer spends not reading is
//! not charged, but a consumer that leaves the full channel unread for
//! [`GeminiConfig::consumer_stall_timeout`] gets the response abandoned); after the
//! `finishReason`, the wait for a missing usage ([`GeminiConfig::usage_grace`], then `Done` with
//! what is known); and the total duration of a response. All body deadlines are
//! `Core(Timeout)` as an event. Dropping the [`LlmStream`] ends the reader task at
//! once (even while the server is silent) and drops the HTTP response, which closes the
//! connection (HTTP/1.1) or resets the stream (HTTP/2): generation stops (whether tokens
//! generated before the drop are billed is UNVERIFIED). Dropping the `stream()` future before
//! the headers arrive aborts the request the same way.
//!
//! # Errors and retries
//! A non-2xx status is [`Error::Rejected`] with that status (400 incl. an invalid key, 403, 404,
//! 429, 5xx, ...). The adapter reads `Retry-After` and the error `status` / `ErrorInfo.reason`
//! and logs them (never the message: it may echo prompt text), but it does **not** retry: the
//! caller decides. An in-band `error` object maps to `Rejected` by its code (shape UNVERIFIED),
//! a blocked prompt (`promptFeedback.blockReason`) to `Rejected { 400 }`, a body that ends
//! before a `finishReason` is `Disconnected`.
//!
//! # Warm-up
//! [`warm`](LlmProvider::warm) sends an unauthenticated `HEAD` to the streaming endpoint,
//! ignores the status and keeps the connection in the client's pool, so the next
//! [`stream`](LlmProvider::stream) skips DNS, TCP and TLS. (The docs say nothing about warm-up
//! or HEAD: any HTTP answer, even 404 or 405, proves the connection works; a server that
//! rejects the HEAD at the TLS level only makes `warm` fail, which callers may ignore.) HTTP/2
//! connections are kept alive with pings while idle, and the pool keeps an idle connection for
//! [`GeminiConfig::pool_idle_timeout`].
//!
//! # Plain HTTP
//! `http://` base URLs are accepted for loopback hosts only, unless
//! [`GeminiConfig::allow_insecure_http`] is set, so the key never crosses the network in
//! cleartext by accident.

mod config;
mod events;
mod request;
mod schema;
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
    DEFAULT_BASE_URL, DEFAULT_MAX_EVENT_BYTES, DEFAULT_MODEL, DEFAULT_POOL_IDLE_TIMEOUT,
    FunctionCallingMode, GeminiConfig, SKIP_SIGNATURE_VALIDATOR, SchemaMode, ThinkingLevel,
};

use self::events::{Flow, Mapper, sanitize};
use self::request::{RenderedTools, build_body};
use self::sse::SseParser;
use crate::error::{Error, Result};
use crate::llm::{LLM_EVENT_CAPACITY, LlmEvent, LlmProvider, LlmRequest, LlmStream, ToolSpec};

/// Provider name used in errors and logs.
pub(crate) const PROVIDER: &str = "gemini";

/// Bytes of an error response body read for logging.
const ERROR_BODY_LIMIT: usize = 8 * 1024;

/// Bound on reading an error response body.
const ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(1);

/// After the terminal event, the rest of the body (normally only the chunk terminator) is
/// drained so an HTTP/1.1 connection can return to the pool: at most this many bytes...
const DRAIN_LIMIT: usize = 16 * 1024;

/// ... and at most this long in total.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// Streaming chat with tools on Gemini `generateContent`. See the [module docs](self).
///
/// Cheap to share: wrap it in an `Arc`. The HTTP client (and its connection pool) is NOT owned
/// by the value: it comes from the process-wide pool (`net::shared_client`, keyed by origin and
/// connection settings), outlives the adapter and is shared by every instance and every
/// runtime that talks to the same origin the same way, so building one adapter per call is
/// fine and a call's first request finds the connection a previous call or `warm()` opened.
#[derive(Debug)]
pub struct GeminiLlm {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    config: GeminiConfig,
    clock: SharedClock,
    client: reqwest::Client,
    stream_url: String,
    /// `stream_url` with `?alt=sse`, the URL of `stream`.
    sse_url: String,
    /// `x-goog-api-key`, sent with `stream` only.
    auth_headers: HeaderMap,
    /// Renderings of recently used tool sets, most recently used first (at most
    /// [`TOOL_CACHE_SLOTS`]).
    tools: Mutex<Vec<Arc<RenderedTools>>>,
}

/// Size of a request's history (message text, tool-call arguments and replayed `provider_data`)
/// above which the body is built on the blocking pool instead of the tokio worker. Below it the
/// parse and serialise passes take on the order of 30 us, the budget CONTRIBUTING.md allows on a
/// worker.
const OFFLOAD_BYTES: usize = 32 * 1024;

/// Distinct tool sets whose rendering is cached. Sessions of different agents or tenants share
/// one [`GeminiLlm`]; a single slot would make them evict each other and re-render every turn.
const TOOL_CACHE_SLOTS: usize = 8;

/// Size of a tool set's text (names, descriptions, schemas) above which a cache miss renders it
/// on the blocking pool: parsing, converting and serialising schemas costs on the order of
/// 10 us per KiB, so past this size the render would break the ~50 us budget for tokio workers.
const TOOLS_OFFLOAD_BYTES: usize = 4 * 1024;

/// Bytes of text in `specs`.
fn tool_bytes(specs: &[ToolSpec]) -> usize {
    specs.iter().fold(0usize, |total, t| {
        total
            .saturating_add(t.name.len())
            .saturating_add(t.description.len())
            .saturating_add(t.parameters_schema.len())
    })
}

/// Whether rendering `specs` can cost far more than their size suggests: `$ref` and `allOf`
/// are inlined by copying, so a few KiB of references between definitions can expand to many
/// thousands of nodes (the conversion stops at `schema::MAX_NODES`, which is still milliseconds
/// of work). Such a set is rendered on the blocking pool whatever its size.
fn may_expand(specs: &[ToolSpec]) -> bool {
    specs
        .iter()
        .any(|t| t.parameters_schema.contains("$ref") || t.parameters_schema.contains("allOf"))
}

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
        .map_err(|_| Error::Core(CoreError::ProcessorPanicked("gemini request builder")))
}

/// Where CPU-bound work really ran (tests only): the closures handed to [`run_cpu`] by
/// `build_request_body` and `tools_for` log the thread they execute on next to the thread that
/// called them, so a test can prove the work left (or stayed on) the caller's thread instead of
/// trusting the decision that was made.
#[cfg(test)]
static WORK_LOG: Mutex<Vec<WorkRecord>> = Mutex::new(Vec::new());

/// One entry of [`WORK_LOG`].
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
struct WorkRecord {
    what: &'static str,
    caller: std::thread::ThreadId,
    worker: std::thread::ThreadId,
}

/// Records that `what` ran on this thread, called by `caller` (tests only).
#[cfg(test)]
fn note_work(what: &'static str, caller: std::thread::ThreadId) {
    WORK_LOG
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(WorkRecord {
            what,
            caller,
            worker: std::thread::current().id(),
        });
}

/// The JSON body for `request` and the rendered tools it was built with (the response mapper
/// needs them). Building it re-parses and re-serialises the whole history, including the
/// replayed parts (cost linear in the history, repeated every turn), so past [`OFFLOAD_BYTES`]
/// it runs on the blocking pool, not on a tokio worker.
async fn build_request_body(
    inner: &Arc<Inner>,
    request: LlmRequest,
) -> Result<(bytes::Bytes, Option<Arc<RenderedTools>>)> {
    let tools = inner.tools_for(&request).await?;
    let heavy = request_bytes(&request) > OFFLOAD_BYTES;
    let build = Arc::clone(inner);
    #[cfg(test)]
    let caller = std::thread::current().id();
    run_cpu(heavy, move || {
        #[cfg(test)]
        note_work("build", caller);
        let body = build_body(&build.config, &request, tools.as_deref())?;
        Ok((body, tools))
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
/// reset, TLS failure, truncated body) is a lost connection.
fn transport_error(error: &reqwest::Error, limit: Duration, what: &'static str) -> Error {
    tracing::debug!(%error, "gemini transport error");
    if error.is_timeout() {
        Error::Core(CoreError::Timeout { what, after: limit })
    } else {
        Error::Disconnected { provider: PROVIDER }
    }
}

/// Refuses a base URL that would send the key in cleartext to a remote host.
fn check_base_url(config: &GeminiConfig) -> Result<()> {
    // The URL itself is never echoed: it may carry credentials.
    let url = reqwest::Url::parse(&config.base_url)
        .map_err(|_| protocol("base_url is not a valid absolute URL"))?;
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

/// The model id for the URL path: `models/` removed, only `[A-Za-z0-9._-]`.
fn model_id(config: &GeminiConfig) -> Result<&str> {
    let model = config
        .model
        .strip_prefix("models/")
        .unwrap_or(&config.model);
    if model.is_empty()
        || !model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(protocol("model is not a model id of [A-Za-z0-9._-]"));
    }
    Ok(model)
}

fn header_value(text: &str, what: &str) -> Result<HeaderValue> {
    let mut value = HeaderValue::from_str(text)
        .map_err(|_| protocol(format!("{what} is not a valid HTTP header value")))?;
    value.set_sensitive(true);
    Ok(value)
}

impl GeminiLlm {
    /// Builds the adapter and its HTTP client (rustls with the shared config of
    /// [`net::tls_config`](crate::net::tls_config), ALPN `h2` then `http/1.1`, no system proxy,
    /// no redirects so the key never follows one).
    ///
    /// # Errors
    /// [`Error::Protocol`] if the API key is not a valid header value (the value is never
    /// included), the model is not a plain model id, both a thinking level and a thinking
    /// budget are set (the API refuses the combination), the base URL is not `https://` (or
    /// `http://` on a loopback host, or on any host with `allow_insecure_http`), or the HTTP
    /// client cannot be built.
    pub fn new(config: GeminiConfig, clock: SharedClock) -> Result<Self> {
        check_base_url(&config)?;
        if config.thinking_level.is_some() && config.thinking_budget.is_some() {
            return Err(protocol(
                "thinking_level and thinking_budget are mutually exclusive",
            ));
        }
        let model = model_id(&config)?;
        let stream_url = format!(
            "{}/v1beta/models/{model}:streamGenerateContent",
            config.base_url.trim_end_matches('/')
        );
        let client = crate::net::shared_client(&crate::net::HttpClientSpec {
            provider: PROVIDER,
            base_url: &config.base_url,
            connect_timeout: config.connect_timeout,
            pool_idle_timeout: config.pool_idle_timeout,
            http2_prior_knowledge: config.http2_prior_knowledge,
        })?;
        let mut auth_headers = HeaderMap::new();
        auth_headers.insert(
            HeaderName::from_static("x-goog-api-key"),
            header_value(config.api_key.expose(), "the API key")?,
        );
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                clock,
                client,
                sse_url: format!("{stream_url}?alt=sse"),
                stream_url,
                auth_headers,
                tools: Mutex::new(Vec::new()),
            }),
        })
    }

    /// The configuration in use (the API key prints as `[REDACTED]`).
    pub fn config(&self) -> &GeminiConfig {
        &self.inner.config
    }
}

impl Inner {
    /// The rendered tools for `request`: from the cache while the tool set is one of the
    /// [`TOOL_CACHE_SLOTS`] most recently used, otherwise rendered now (on the blocking pool
    /// when the schemas are large, see [`TOOLS_OFFLOAD_BYTES`]) and cached.
    async fn tools_for(&self, request: &LlmRequest) -> Result<Option<Arc<RenderedTools>>> {
        if request.tools.is_empty() {
            return Ok(None);
        }
        let mode = self.config.schema_mode;
        if let Some(hit) = self.cached_tools(&request.tools, mode) {
            return Ok(Some(hit));
        }
        let specs = request.tools.clone();
        let heavy = tool_bytes(&specs) > TOOLS_OFFLOAD_BYTES || may_expand(&specs);
        #[cfg(test)]
        let caller = std::thread::current().id();
        let rendered = run_cpu(heavy, move || {
            #[cfg(test)]
            note_work("render", caller);
            RenderedTools::render(&specs, mode)
        })
        .await??;
        let mut cache = self.tools.lock().unwrap_or_else(PoisonError::into_inner);
        // A concurrent miss may have stored the same set meanwhile: keep one entry.
        cache.retain(|c| !c.matches(&request.tools, mode));
        cache.insert(0, Arc::clone(&rendered));
        cache.truncate(TOOL_CACHE_SLOTS);
        Ok(Some(rendered))
    }

    /// The cached rendering of `specs`, moved to the front of the cache.
    fn cached_tools(&self, specs: &[ToolSpec], mode: SchemaMode) -> Option<Arc<RenderedTools>> {
        let mut cache = self.tools.lock().unwrap_or_else(PoisonError::into_inner);
        let at = cache.iter().position(|c| c.matches(specs, mode))?;
        let hit = cache.remove(at);
        cache.insert(0, Arc::clone(&hit));
        Some(hit)
    }

    /// Reads a refusal's `Retry-After` and error status/reason for the log, and maps it.
    async fn rejection(&self, mut response: reqwest::Response) -> Error {
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
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
        let _ = within(&self.clock, ERROR_BODY_TIMEOUT, "gemini error body", read).await;
        let (kind, reason) = parse_error_body(&body);
        // The error message is deliberately not logged: it can echo prompt text or key text.
        tracing::warn!(
            status,
            retry_after_secs = retry_after,
            error_status = kind.as_deref(),
            error_reason = reason.as_deref(),
            "gemini rejected the request (not retried by the adapter)"
        );
        Error::Rejected {
            provider: PROVIDER,
            status,
        }
    }
}

/// The `error.status` and the first `ErrorInfo.reason` of an error body, sanitised for logging.
fn parse_error_body(body: &[u8]) -> (Option<String>, Option<String>) {
    #[derive(serde::Deserialize)]
    struct Envelope {
        error: Option<Detail>,
    }
    #[derive(serde::Deserialize)]
    struct Detail {
        status: Option<String>,
        #[serde(default)]
        details: Vec<Info>,
    }
    #[derive(serde::Deserialize)]
    struct Info {
        reason: Option<String>,
    }
    let Ok(Envelope {
        error: Some(detail),
    }) = serde_json::from_slice(body)
    else {
        return (None, None);
    };
    let reason = detail
        .details
        .iter()
        .find_map(|i| i.reason.as_deref())
        .map(sanitize);
    (detail.status.as_deref().map(sanitize), reason)
}

impl LlmProvider for GeminiLlm {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmStream> {
        let inner = &self.inner;
        let config = &inner.config;
        let (body, tools) = build_request_body(inner, request).await?;
        let mut headers = inner.auth_headers.clone();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        let send = inner
            .client
            .post(&inner.sse_url)
            .headers(headers)
            .body(body)
            .send();
        let response = within(
            &inner.clock,
            config.first_byte_timeout,
            "gemini first byte",
            send,
        )
        .await?
        .map_err(|e| transport_error(&e, config.connect_timeout, "gemini connect"))?;
        if !response.status().is_success() {
            return Err(inner.rejection(response).await);
        }
        let (tx, rx) = mpsc::channel(LLM_EVENT_CAPACITY);
        tokio::spawn(pump(Arc::clone(inner), response, tools, tx));
        Ok(LlmStream::new(rx))
    }

    async fn warm(&self) -> Result<()> {
        let inner = &self.inner;
        let config = &inner.config;
        // No credentials: any HTTP answer (401, 404, 405...) proves the connection works and
        // leaves it in the pool.
        let head = inner.client.head(&inner.stream_url).send();
        within(&inner.clock, config.first_byte_timeout, "gemini warm", head)
            .await?
            .map(drop)
            .map_err(|e| transport_error(&e, config.connect_timeout, "gemini warm"))
    }
}

/// Hands `event` to the consumer. `false` when the consumer is gone, or when it stopped reading
/// for [`GeminiConfig::consumer_stall_timeout`] with the channel full (the response is then
/// abandoned, see there).
async fn deliver(inner: &Inner, tx: &mpsc::Sender<LlmEvent>, event: LlmEvent) -> bool {
    // Fast path: room in the channel, no timer needed.
    let event = match tx.try_send(event) {
        Ok(()) => return true,
        Err(mpsc::error::TrySendError::Closed(_)) => return false,
        Err(mpsc::error::TrySendError::Full(event)) => event,
    };
    // Backpressure: wait for room (the socket is not read meanwhile), but not for ever.
    tokio::select! {
        biased;
        sent = tx.send(event) => sent.is_ok(),
        () = inner.clock.sleep(inner.config.consumer_stall_timeout) => {
            tracing::warn!(
                stall_secs = inner.config.consumer_stall_timeout.as_secs_f64(),
                "gemini consumer stopped reading: abandoning the response"
            );
            false
        }
    }
}

/// Completes the turn without waiting for more of the body: used when `finishReason` was seen
/// and the usage did not come within the grace period, or the connection broke after it.
async fn finish_turn(inner: &Inner, mapper: &mut Mapper, tx: &mpsc::Sender<LlmEvent>) {
    let mut out = Vec::with_capacity(1);
    let ended = mapper.end_of_stream(&mut out);
    for event in out {
        if !deliver(inner, tx, event).await {
            return;
        }
    }
    if let Err(error) = ended {
        deliver(inner, tx, LlmEvent::Error(error)).await;
    }
}

/// Reads the response body, maps its events and sends them until the terminal event, an error,
/// or the consumer drops the stream.
///
/// Deadlines (all on the injected clock): until the first output (a text or call, or the
/// terminal event) the only body deadline is `first_output_timeout`; after it, `idle_timeout`
/// is re-armed only by SSE events that **produced output** (text, a call, the end of the turn):
/// comments, empty `data:` heartbeats, `{}` and usage-only chunks do not re-arm it. It is re-armed
/// only once the event's outputs have been handed to the consumer, so time spent waiting for a
/// slow consumer is not charged to the server (each hand-over is bounded by
/// `consumer_stall_timeout`). Once `finishReason` has been seen the content is complete: the
/// remaining wait for usage is bounded by `usage_grace`, and a transport error counts like the end
/// of the body.
async fn pump(
    inner: Arc<Inner>,
    mut response: reqwest::Response,
    tools: Option<Arc<RenderedTools>>,
    tx: mpsc::Sender<LlmEvent>,
) {
    let config = &inner.config;
    let clock = &inner.clock;
    let mut parser = SseParser::new(config.max_event_bytes);
    let mut mapper = Mapper::with_tools(tools);
    let mut out: Vec<LlmEvent> = Vec::with_capacity(4);
    let stream_deadline = deadline_after(clock.now(), config.max_stream_duration);
    let mut got_output = false;
    let mut grace_armed = false;
    let mut event_deadline = deadline_after(clock.now(), config.first_output_timeout);
    let timer = clock.sleep_until(event_deadline.min(stream_deadline));
    tokio::pin!(timer);
    loop {
        let chunk = tokio::select! {
            biased;
            () = tx.closed() => return,
            () = timer.as_mut() => {
                if mapper.has_finish() {
                    // The turn is complete; only the usage (or the end of the body) is missing.
                    finish_turn(&inner, &mut mapper, &tx).await;
                    return;
                }
                let (what, after) = if clock.now() >= stream_deadline {
                    ("gemini stream", config.max_stream_duration)
                } else if got_output {
                    ("gemini idle", config.idle_timeout)
                } else {
                    ("gemini first output", config.first_output_timeout)
                };
                deliver(&inner, &tx, LlmEvent::Error(Error::Core(CoreError::Timeout { what, after }))).await;
                return;
            }
            chunk = response.chunk() => chunk,
        };
        let bytes = match chunk {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                // A clean end of the body: complete when `finishReason` was seen, else cut off.
                finish_turn(&inner, &mut mapper, &tx).await;
                return;
            }
            Err(e) => {
                if mapper.has_finish() {
                    // All content was delivered: a reset or truncated body now loses nothing.
                    tracing::debug!(%e, "gemini transport error after finishReason; completing the turn");
                    finish_turn(&inner, &mut mapper, &tx).await;
                } else {
                    let error = transport_error(&e, config.idle_timeout, "gemini idle");
                    deliver(&inner, &tx, LlmEvent::Error(error)).await;
                }
                return;
            }
        };
        parser.push(&bytes);
        let mut produced = false;
        loop {
            out.clear();
            let flow = match parser.next_event() {
                Ok(Some(data)) => mapper.handle(data, &mut out),
                Ok(None) => break,
                Err(e) => Err(protocol(e.to_string())),
            };
            produced |= !out.is_empty();
            got_output |= produced;
            for event in out.drain(..) {
                if !deliver(&inner, &tx, event).await {
                    return;
                }
            }
            match flow {
                Ok(Flow::Continue) => {}
                Ok(Flow::Finished) => {
                    drop(tx);
                    drain(&inner, response).await;
                    return;
                }
                Err(error) => {
                    deliver(&inner, &tx, LlmEvent::Error(error)).await;
                    return;
                }
            }
        }
        if mapper.has_finish() {
            if !grace_armed {
                grace_armed = true;
                event_deadline = deadline_after(clock.now(), config.usage_grace);
            }
        } else if produced {
            event_deadline = deadline_after(clock.now(), config.idle_timeout);
        }
        timer.as_mut().reset(event_deadline.min(stream_deadline));
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
    let _ = within(&inner.clock, DRAIN_TIMEOUT, "gemini drain", read).await;
}

#[cfg(test)]
mod tests {
    use std::thread;

    use brisvox_core::Secret;

    use super::*;
    use crate::llm::{Message, Role};

    fn request_with(provider_data_bytes: usize) -> LlmRequest {
        let data: Arc<str> = Arc::from("x".repeat(provider_data_bytes));
        LlmRequest {
            messages: vec![
                Message::new(Role::User, "hi"),
                Message::assistant("hello", Vec::new(), Some(data)),
            ],
            tools: Vec::new(),
        }
    }

    fn llm() -> GeminiLlm {
        let config = GeminiConfig::new(Secret::new("test-key".to_owned()))
            .with_base_url("http://127.0.0.1:1");
        GeminiLlm::new(config, brisvox_core::SystemClock::shared()).unwrap()
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

    /// The threads `what` ran on for work this test's thread started, in order. The tests are
    /// `current_thread` runtimes, so the calling thread identifies the test.
    fn ran_on(what: &str) -> Vec<thread::ThreadId> {
        let me = thread::current().id();
        WORK_LOG
            .lock()
            .unwrap()
            .iter()
            .filter(|w| w.what == what && w.caller == me)
            .map(|w| w.worker)
            .collect()
    }

    #[tokio::test]
    async fn only_large_histories_are_built_on_the_blocking_pool() {
        let llm = llm();
        let here = thread::current().id();
        build_request_body(&llm.inner, request_with(0))
            .await
            .unwrap();
        assert_eq!(ran_on("build"), vec![here], "a small body stays inline");
        // Whether the odd provider data replays or not is beside the point: the heavy request
        // must have been built on a thread of the blocking pool, not on this one.
        let _ = build_request_body(&llm.inner, request_with(OFFLOAD_BYTES)).await;
        let threads = ran_on("build");
        assert_eq!(threads.len(), 2);
        assert_ne!(
            threads[1], here,
            "a large body must leave the worker thread"
        );
    }

    fn tool_set(name: &str, padding: usize) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: Arc::from(name),
            description: Arc::from("x".repeat(padding)),
            parameters_schema: Arc::from(
                r#"{"type":"object","properties":{"q":{"type":"string"}}}"#,
            ),
        }]
    }

    fn tools_request(tools: Vec<ToolSpec>) -> LlmRequest {
        LlmRequest {
            messages: vec![Message::new(Role::User, "hi")],
            tools,
        }
    }

    #[tokio::test]
    async fn alternating_tool_sets_hit_separate_cache_slots_and_the_cache_is_bounded() {
        let llm = llm();
        let render = |tools: Vec<ToolSpec>| {
            let inner = Arc::clone(&llm.inner);
            async move {
                inner
                    .tools_for(&tools_request(tools))
                    .await
                    .unwrap()
                    .unwrap()
            }
        };
        let a = render(tool_set("a", 0)).await;
        let b = render(tool_set("b", 0)).await;
        assert!(!Arc::ptr_eq(&a, &b));
        // Alternating does not evict: the same rendering comes back.
        assert!(Arc::ptr_eq(&a, &render(tool_set("a", 0)).await));
        assert!(Arc::ptr_eq(&b, &render(tool_set("b", 0)).await));
        assert!(Arc::ptr_eq(&a, &render(tool_set("a", 0)).await));
        // A different mode or content is a different entry; the cache never grows past its bound.
        for i in 0..(2 * TOOL_CACHE_SLOTS) {
            render(tool_set(&format!("t{i}"), 0)).await;
        }
        let len = llm.inner.tools.lock().unwrap().len();
        assert_eq!(len, TOOL_CACHE_SLOTS);
        // No tools: nothing rendered, nothing cached.
        let none = llm
            .inner
            .tools_for(&tools_request(Vec::new()))
            .await
            .unwrap();
        assert!(none.is_none());
    }

    #[tokio::test]
    async fn large_tool_sets_are_rendered_on_the_blocking_pool() {
        let llm = llm();
        let here = thread::current().id();
        let small = tool_set("offload_small", 10);
        assert!(tool_bytes(&small) <= TOOLS_OFFLOAD_BYTES);
        llm.inner.tools_for(&tools_request(small)).await.unwrap();
        assert_eq!(ran_on("render"), vec![here], "a small set renders inline");
        let big = tool_set("offload_big", TOOLS_OFFLOAD_BYTES + 1);
        assert!(tool_bytes(&big) > TOOLS_OFFLOAD_BYTES);
        llm.inner
            .tools_for(&tools_request(big.clone()))
            .await
            .unwrap();
        let threads = ran_on("render");
        assert_eq!(threads.len(), 2);
        assert_ne!(threads[1], here, "a large set must leave the worker thread");
        // A cache hit renders nothing at all.
        llm.inner.tools_for(&tools_request(big)).await.unwrap();
        assert_eq!(ran_on("render").len(), 2);
    }

    #[tokio::test]
    async fn a_small_schema_that_fans_out_through_refs_is_rendered_on_the_blocking_pool() {
        // 3 KiB of text that the conversion would expand to many thousands of nodes: its size
        // says nothing about its cost, so `$ref` / `allOf` alone send it off the worker.
        let llm = llm();
        let here = thread::current().id();
        let mut defs = serde_json::Map::new();
        for k in 0..11 {
            let members: Vec<_> = (0..8)
                .map(|_| serde_json::json!({"$ref": format!("#/$defs/d{}", k + 1)}))
                .collect();
            defs.insert(format!("d{k}"), serde_json::json!({"allOf": members}));
        }
        defs.insert(
            "d11".to_owned(),
            serde_json::json!({"type":"object","properties":{"a":{"type":"string"}}}),
        );
        let schema = serde_json::json!({"$defs": defs, "$ref": "#/$defs/d0"}).to_string();
        let specs = vec![ToolSpec {
            name: Arc::from("fan_out"),
            description: Arc::from("x"),
            parameters_schema: Arc::from(schema),
        }];
        assert!(tool_bytes(&specs) <= TOOLS_OFFLOAD_BYTES, "small by size");
        assert!(may_expand(&specs));
        // The work is cut off at the node budget (an error, not a stall), on the blocking pool.
        let result = llm.inner.tools_for(&tools_request(specs)).await;
        assert!(matches!(result, Err(Error::Protocol { .. })), "{result:?}");
        let threads = ran_on("render");
        assert_eq!(threads.len(), 1);
        assert_ne!(threads[0], here);
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
    fn the_model_becomes_part_of_the_url_only_when_it_is_a_plain_id() {
        let base =
            || GeminiConfig::new(Secret::new("k".to_owned())).with_base_url("http://localhost:1");
        let ok = GeminiLlm::new(
            base().with_model("models/gemini-3.8-flash"),
            brisvox_core::SystemClock::shared(),
        )
        .unwrap();
        assert_eq!(
            ok.inner.stream_url,
            "http://localhost:1/v1beta/models/gemini-3.8-flash:streamGenerateContent"
        );
        for bad in ["", "a/b", "a?x=1", "a b", "../x", "models/"] {
            let result =
                GeminiLlm::new(base().with_model(bad), brisvox_core::SystemClock::shared());
            assert!(matches!(result, Err(Error::Protocol { .. })), "{bad:?}");
        }
    }

    #[test]
    fn a_level_and_a_budget_together_are_refused() {
        let mut config = GeminiConfig::new(Secret::new("k".to_owned()))
            .with_base_url("http://localhost:1")
            .with_thinking_level(ThinkingLevel::Low);
        config.thinking_budget = Some(0);
        let result = GeminiLlm::new(config, brisvox_core::SystemClock::shared());
        assert!(matches!(result, Err(Error::Protocol { .. })));
    }
}
