//! OpenAI LLM adapter: streaming chat with tools over the Responses API.
//!
//! # API surface
//! `POST {base}/v1/responses` with `stream: true` and `store: false`, replaying the whole history
//! each turn (no `previous_response_id`, no server-side conversation). Why: OpenAI recommends
//! Responses for new projects; tool calling on the GPT-6 models needs it; reasoning items are
//! first-class there; and replaying our own history keeps the invariant "after an interruption
//! the context holds only what was played". The decision and its sources are recorded in
//! `docs/providers/openai.md`.
//!
//! # Prompt-cache friendly requests
//! The adapter keeps the prefix of consecutive requests byte-stable: the tool array is rendered
//! once and reused while the [`ToolSpec`](crate::ToolSpec)s are equal; the leading system
//! messages become `instructions`; the history is append-only. Callers must keep the system
//! prompt and tools stable and put volatile text (time, caller name, retrieved snippets) at the
//! end of the newest user message. `prompt_cache_key` is configurable
//! ([`OpenAiConfig::prompt_cache_key`]); `usage.cached_input_tokens` reports the hits.
//!
//! # Streaming
//! The body is parsed incrementally as Server-Sent Events with a bounded buffer
//! ([`OpenAiConfig::max_event_bytes`]). `response.output_text.delta` becomes
//! [`LlmEvent::TextDelta`]; a `function_call` output item becomes [`LlmEvent::ToolCallDelta`]
//! (index = position among the response's calls, `id` = the item's `call_id`) followed by its
//! argument fragments; the terminal event becomes [`LlmEvent::Done`] with usage (including
//! cached tokens) and the reasoning items to replay as `provider_data`.
//!
//! # Memory and CPU bounds
//! Every response is bounded: SSE events by [`OpenAiConfig::max_event_bytes`], tool calls
//! (128), replayable items (256 items, 16 MiB) and streamed tool-call arguments (1 MiB per call,
//! 4 MiB per response; more is [`Error::Protocol`]); the event channel is bounded and applies
//! backpressure. Building a request re-parses and re-serialises the whole history on every turn
//! (replayed reasoning included): past 32 KiB of history that runs on the blocking pool, not on a
//! tokio worker; below it, inline.
//!
//! # Timeouts and cancellation
//! Every wait is bounded on the injected clock: connect and response headers
//! ([`OpenAiConfig::first_byte_timeout`], `Core(Timeout)` from `stream()`); the time from the
//! headers to the first output of the model ([`OpenAiConfig::first_output_timeout`]; neither
//! comments nor lifecycle events extend it); the silence between SSE events after that
//! ([`OpenAiConfig::idle_timeout`]; SSE comments are not events and do not count, and time the
//! consumer spends not reading is not charged); and the total duration of a response. All
//! three body deadlines are `Core(Timeout)` as an event. Dropping the [`LlmStream`] ends the
//! reader task at once (even while the server is silent) and drops the HTTP response, which
//! closes the connection (HTTP/1.1) or resets the stream (HTTP/2): generation stops. Dropping
//! the `stream()` future before the headers arrive aborts the request the same way.
//!
//! # Errors and retries
//! A non-2xx status is [`Error::Rejected`] with that status (401, 429, 5xx, ...). The adapter
//! reads `Retry-After` and the error `type`/`code` and logs them (never the message: it can echo
//! a key fragment), but it does **not** retry: the caller decides, and a retry of a streaming
//! request belongs to the turn logic, not the transport. A `response.failed` or `error` event
//! maps to `Rejected` by its code (rate limit 429, server error 500/503, rejected prompt 400);
//! a stream that ends without a terminal event is `Disconnected`.
//!
//! # Warm-up
//! [`warm`](LlmProvider::warm) sends an unauthenticated `HEAD /v1/responses`, ignores the status
//! and keeps the connection in the client's pool, so the next [`stream`](LlmProvider::stream)
//! skips DNS, TCP and TLS. HTTP/2 connections are kept alive with pings while idle, and the
//! pool keeps an idle connection for [`OpenAiConfig::pool_idle_timeout`] (10 minutes by
//! default); after a longer quiet period call `warm` again.
//!
//! # Plain HTTP
//! `http://` base URLs are accepted for loopback hosts only, unless
//! [`OpenAiConfig::allow_insecure_http`] is set, so the bearer key never crosses the network in
//! cleartext by accident.

mod config;
mod events;
mod request;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_core::Error as CoreError;
use brisvox_core::SharedClock;
use brisvox_core::clock::deadline_after;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use tokio::sync::mpsc;

pub use config::{
    DEFAULT_BASE_URL, DEFAULT_MAX_EVENT_BYTES, DEFAULT_MODEL, DEFAULT_POOL_IDLE_TIMEOUT,
    OpenAiConfig, ReasoningEffort, StrictTools,
};

use self::events::{Flow, Mapper};
use self::request::{RenderedTools, build_body};
use crate::error::{Error, Result};
use crate::http_llm::{self, sanitize, within};
use crate::llm::{LLM_EVENT_CAPACITY, LlmEvent, LlmProvider, LlmRequest, LlmStream};
use crate::sse::SseParser;

/// Provider name used in errors and logs.
pub(crate) const PROVIDER: &str = "openai";

/// Streaming chat with tools on the OpenAI Responses API. See the [module docs](self).
///
/// Cheap to share: wrap it in an `Arc`. The HTTP client (and its connection pool) is NOT owned
/// by the value: it comes from the process-wide pool (`net::shared_client`, keyed by origin and
/// connection settings), outlives the adapter and is shared by every instance and every
/// runtime that talks to the same origin the same way, so building one adapter per call is
/// fine and a call's first request finds the connection a previous call or `warm()` opened.
#[derive(Debug)]
pub struct OpenAiLlm {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    config: OpenAiConfig,
    clock: SharedClock,
    client: reqwest::Client,
    responses_url: String,
    /// `Authorization`, `OpenAI-Organization` and `OpenAI-Project`, sent with `stream` only.
    auth_headers: HeaderMap,
    tools: Mutex<Option<Arc<RenderedTools>>>,
    requests: AtomicU64,
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
    http_llm::run_cpu("openai request builder", heavy, work).await
}

/// Counts bodies built on the blocking pool (tests only).
#[cfg(test)]
static OFFLOADED_BUILDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The JSON body for `request`. Building it re-parses and re-serialises the whole history,
/// including the replayed reasoning items (cost linear in the history, repeated every turn), so
/// past [`OFFLOAD_BYTES`] it runs on the blocking pool, not on a tokio worker.
async fn build_request_body(inner: &Arc<Inner>, request: LlmRequest) -> Result<bytes::Bytes> {
    let tools = inner.tools_for(&request)?;
    let heavy = request_bytes(&request) > OFFLOAD_BYTES;
    #[cfg(test)]
    if heavy {
        OFFLOADED_BUILDS.fetch_add(1, Ordering::SeqCst);
    }
    let build = Arc::clone(inner);
    run_cpu(heavy, move || {
        build_body(&build.config, &request, tools.as_deref())
    })
    .await?
}

fn protocol(detail: impl Into<String>) -> Error {
    http_llm::protocol(PROVIDER, detail)
}

/// A transport error as a provider error: a timeout stays a timeout, everything else (refused,
/// reset, TLS failure, truncated body) is a lost connection.
fn transport_error(error: &reqwest::Error, limit: Duration, what: &'static str) -> Error {
    tracing::debug!(%error, "openai transport error");
    http_llm::transport_error(PROVIDER, error, limit, what)
}

/// Refuses a base URL that would send the key in cleartext to a remote host.
fn check_base_url(config: &OpenAiConfig) -> Result<()> {
    http_llm::check_base_url(PROVIDER, &config.base_url, config.allow_insecure_http)
}

fn header_value(text: &str, what: &str) -> Result<HeaderValue> {
    http_llm::sensitive_header_value(PROVIDER, text, what)
}

impl OpenAiLlm {
    /// Builds the adapter and its HTTP client (rustls with the shared config of
    /// [`net::tls_config`](crate::net::tls_config), ALPN `h2` then `http/1.1`, no system proxy,
    /// no redirects so the key never follows one).
    ///
    /// # Errors
    /// [`Error::Protocol`] if the API key or an organisation/project id is not a valid header
    /// value (the value is never included), the base URL is not `https://` (or `http://` on a
    /// loopback host, or on any host with `allow_insecure_http`), or the HTTP client cannot be
    /// built.
    pub fn new(config: OpenAiConfig, clock: SharedClock) -> Result<Self> {
        check_base_url(&config)?;
        let client = crate::net::shared_client(&crate::net::HttpClientSpec {
            provider: PROVIDER,
            base_url: &config.base_url,
            connect_timeout: config.connect_timeout,
            pool_idle_timeout: config.pool_idle_timeout,
            http2_prior_knowledge: false,
        })?;
        let mut auth_headers = HeaderMap::new();
        auth_headers.insert(
            AUTHORIZATION,
            header_value(
                &format!("Bearer {}", config.api_key.expose()),
                "the API key",
            )?,
        );
        if let Some(org) = &config.organization {
            auth_headers.insert(
                HeaderName::from_static("openai-organization"),
                header_value(org, "the organization id")?,
            );
        }
        if let Some(project) = &config.project {
            auth_headers.insert(
                HeaderName::from_static("openai-project"),
                header_value(project, "the project id")?,
            );
        }
        let responses_url = format!("{}/v1/responses", config.base_url.trim_end_matches('/'));
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                clock,
                client,
                responses_url,
                auth_headers,
                tools: Mutex::new(None),
                requests: AtomicU64::new(0),
            }),
        })
    }

    /// The configuration in use (the API key prints as `[REDACTED]`).
    pub fn config(&self) -> &OpenAiConfig {
        &self.inner.config
    }
}

impl Inner {
    /// The rendered tools for `request`, from the cache while the tools are unchanged.
    fn tools_for(&self, request: &LlmRequest) -> Result<Option<Arc<RenderedTools>>> {
        if request.tools.is_empty() {
            return Ok(None);
        }
        let strict = self.config.strict_tools;
        let mut cache = self.tools.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(cached) = cache.as_ref().filter(|c| c.matches(&request.tools, strict)) {
            return Ok(Some(Arc::clone(cached)));
        }
        let rendered = RenderedTools::render(&request.tools, strict)?;
        *cache = Some(Arc::clone(&rendered));
        Ok(Some(rendered))
    }

    /// A unique id for the `X-Client-Request-Id` header (traceable in OpenAI's logs).
    fn request_id(&self) -> String {
        let n = self.requests.fetch_add(1, Ordering::Relaxed);
        let ms = self
            .clock
            .wall()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        format!("brisvox-{ms:x}-{n}")
    }

    /// Reads a refusal's `Retry-After` and error type/code for the log, and maps it.
    async fn rejection(&self, mut response: reqwest::Response) -> Error {
        let status = response.status().as_u16();
        let retry_after = http_llm::retry_after_secs(&response);
        let body = http_llm::read_error_body(&self.clock, &mut response, "openai error body").await;
        let (kind, code) = parse_error_body(&body);
        // The error message is deliberately not logged: a 401 echoes part of the key.
        tracing::warn!(
            status,
            retry_after_secs = retry_after,
            error_type = kind.as_deref(),
            error_code = code.as_deref(),
            "openai rejected the request (not retried by the adapter)"
        );
        Error::Rejected {
            provider: PROVIDER,
            status,
        }
    }
}

/// The `error.type` and `error.code` of an error body, sanitised for logging.
fn parse_error_body(body: &[u8]) -> (Option<String>, Option<String>) {
    #[derive(serde::Deserialize)]
    struct Envelope {
        error: Option<Detail>,
    }
    #[derive(serde::Deserialize)]
    struct Detail {
        r#type: Option<String>,
        code: Option<serde_json::Value>,
    }
    let Ok(Envelope {
        error: Some(detail),
    }) = serde_json::from_slice(body)
    else {
        return (None, None);
    };
    let code = match detail.code {
        Some(serde_json::Value::String(s)) => Some(sanitize(&s)),
        Some(serde_json::Value::Number(n)) => Some(sanitize(&n.to_string())),
        _ => None,
    };
    (detail.r#type.as_deref().map(sanitize), code)
}

impl LlmProvider for OpenAiLlm {
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
        if let Ok(id) = HeaderValue::from_str(&inner.request_id()) {
            headers.insert(HeaderName::from_static("x-client-request-id"), id);
        }
        let send = inner
            .client
            .post(&inner.responses_url)
            .headers(headers)
            .body(body)
            .send();
        let response = within(
            &inner.clock,
            config.first_byte_timeout,
            "openai first byte",
            send,
        )
        .await?
        .map_err(|e| transport_error(&e, config.connect_timeout, "openai connect"))?;
        if !response.status().is_success() {
            return Err(inner.rejection(response).await);
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
        let head = inner.client.head(&inner.responses_url).send();
        within(&inner.clock, config.first_byte_timeout, "openai warm", head)
            .await?
            .map(drop)
            .map_err(|e| transport_error(&e, config.connect_timeout, "openai warm"))
    }
}

/// Reads the response body, maps its events and sends them until the terminal event, an error,
/// or the consumer drops the stream.
///
/// Deadlines: until the first output (a text or tool-call fragment, or the terminal event) the
/// only body deadline is `first_output_timeout`; after it, `idle_timeout` is re-armed by every
/// dispatched SSE event (a chunk that completes no event, such as a comment, does not re-arm it)
/// and only once the event's outputs have been handed to the consumer, so time spent waiting for
/// a slow consumer is not charged to the server.
async fn pump(inner: Arc<Inner>, mut response: reqwest::Response, tx: mpsc::Sender<LlmEvent>) {
    let config = &inner.config;
    let clock = &inner.clock;
    let mut parser = SseParser::new(config.max_event_bytes);
    let mut mapper = Mapper::new();
    let mut out: Vec<LlmEvent> = Vec::with_capacity(4);
    let stream_deadline = deadline_after(clock.now(), config.max_stream_duration);
    let mut got_output = false;
    let mut event_deadline = deadline_after(clock.now(), config.first_output_timeout);
    let timer = clock.sleep_until(event_deadline.min(stream_deadline));
    tokio::pin!(timer);
    loop {
        let chunk = tokio::select! {
            biased;
            () = tx.closed() => return,
            () = timer.as_mut() => {
                let (what, after) = if clock.now() >= stream_deadline {
                    ("openai stream", config.max_stream_duration)
                } else if got_output {
                    ("openai idle", config.idle_timeout)
                } else {
                    ("openai first output", config.first_output_timeout)
                };
                let _ = tx.send(LlmEvent::Error(Error::Core(CoreError::Timeout { what, after }))).await;
                return;
            }
            chunk = response.chunk() => chunk,
        };
        let bytes = match chunk {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                // The body ended without a terminal event.
                let _ = tx
                    .send(LlmEvent::Error(Error::Disconnected { provider: PROVIDER }))
                    .await;
                return;
            }
            Err(e) => {
                let error = transport_error(&e, config.idle_timeout, "openai idle");
                let _ = tx.send(LlmEvent::Error(error)).await;
                return;
            }
        };
        parser.push(&bytes);
        let mut dispatched = false;
        loop {
            out.clear();
            let flow = match parser.next_event() {
                Ok(Some(data)) => {
                    dispatched = true;
                    mapper.handle(data, &mut out)
                }
                Ok(None) => break,
                Err(e) => Err(protocol(e.to_string())),
            };
            got_output |= !out.is_empty();
            for event in out.drain(..) {
                // Backpressure: wait for room (the socket is not read meanwhile).
                if tx.send(event).await.is_err() {
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
                    let _ = tx.send(LlmEvent::Error(error)).await;
                    return;
                }
            }
        }
        if dispatched && got_output {
            event_deadline = deadline_after(clock.now(), config.idle_timeout);
        }
        timer.as_mut().reset(event_deadline.min(stream_deadline));
    }
}

/// Reads what is left of a finished response so the connection can be reused (bounded).
async fn drain(inner: &Inner, response: reqwest::Response) {
    http_llm::drain(&inner.clock, response, "openai drain").await;
}

#[cfg(test)]
mod tests {
    use std::thread;

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
        let config = OpenAiConfig::new(brisvox_core::Secret::new("sk-test".to_owned()))
            .with_base_url("http://127.0.0.1:1");
        let llm = OpenAiLlm::new(config, brisvox_core::SystemClock::shared()).unwrap();
        let before = OFFLOADED_BUILDS.load(Ordering::SeqCst);
        // Whether the odd provider data replays or not is beside the point: the heavy request
        // must have gone through the blocking pool.
        let _ = build_request_body(&llm.inner, request_with(OFFLOAD_BYTES)).await;
        assert!(OFFLOADED_BUILDS.load(Ordering::SeqCst) > before);
    }

    #[tokio::test]
    async fn a_panic_in_offloaded_work_is_an_error_not_a_crash() {
        let result = run_cpu(true, || -> u8 { panic!("boom") }).await;
        assert!(matches!(
            result,
            Err(Error::Core(CoreError::ProcessorPanicked(_)))
        ));
    }
}
