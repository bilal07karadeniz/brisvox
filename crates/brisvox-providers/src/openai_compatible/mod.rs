//! Generic OpenAI-compatible LLM adapter: streaming Chat Completions with tools for Groq,
//! Together AI, Fireworks AI, vLLM, Ollama and any other server that speaks
//! `POST {base_url}/chat/completions` with `stream: true`.
//!
//! # Configuration
//! [`OpenAiCompatibleConfig`] holds the base URL (with its version prefix), the model, an
//! optional API key and the header and scheme it is sent in ([`AuthStyle`]: `Authorization:
//! Bearer` by default), any number of extra headers, extra top-level body fields and the
//! vendor's [`Quirks`]. [`Preset`]s (Groq, Together, Fireworks, vLLM, Ollama, OpenRouter) fill in the
//! documented endpoint and quirks; see `docs/providers/openai-compatible.md` (fetched
//! 2026-10-08) for where each value comes from and what is UNVERIFIED. The chat URL is the path
//! of the base URL plus [`chat_path`](OpenAiCompatibleConfig::chat_path); a query on the base URL
//! (`?key=...`) stays a query after the path and fragments are dropped.
//!
//! # What is sent
//! `model`, `messages`, `stream: true`, `tools` (omitted when empty), the output token limit
//! under the field name the vendor wants, `temperature`, and only behind explicit quirks
//! `stream_options.include_usage`, `parallel_tool_calls` and `tool_choice`: Groq answers 400 to
//! fields it does not know, Ollama has no `tool_choice`. An assistant turn that only called
//! tools is sent with `content: null`; a replayed call without arguments is sent with `{}`.
//!
//! # What is read
//! The SSE body is parsed incrementally with a bounded buffer and every chunk is read by one
//! tolerant mapper (module `chunks`): tool calls as fragments or whole, missing `index` or `id`
//! (a synthetic `call_<n>` id is made up), usage at the top level, in `x_groq` or in the choice,
//! `finish_reason` `eos` / `function_call` / `content_filter`, error bodies as objects or strings.
//! `delta.reasoning` and `reasoning_content` are dropped: they must never reach TTS. A response
//! that called tools ends in [`FinishReason::ToolCalls`](crate::FinishReason::ToolCalls) whatever the vendor's `finish_reason`
//! says (vLLM keeps `stop` for a named `tool_choice`), except when it was cut by `length`.
//!
//! The end of a response: `Done` is emitted at the chunk with `finish_reason` when it carries
//! usage or the vendor does not send usage separately ([`UsagePlacement`]); otherwise at the
//! usage chunk, at `[DONE]`, at the end of the body, or after
//! [`OpenAiCompatibleConfig::usage_grace`], whichever comes first, so a vendor that omits usage
//! never delays a turn by more than that grace.
//! `[DONE]` without any `finish_reason` completes a text answer as a stop, but is a cut
//! ([`Error::Disconnected`]) while tool-call fragments are open: a gateway that aborted the
//! generation mid call may still write `[DONE]`, and truncated arguments must never become a
//! tool-call turn.
//!
//! # Memory and CPU bounds
//! SSE events are bounded by [`OpenAiCompatibleConfig::max_event_bytes`], tool calls (128) and
//! their arguments (1 MiB per call, 4 MiB per response) are capped (more is [`Error::Protocol`]);
//! the event channel is bounded and applies backpressure (the socket is not read while the
//! consumer is slow). A consumer that holds the stream without reading cannot keep the
//! connection forever either: handing an event over is bounded by the response's duration cap
//! ([`max_stream_duration`](OpenAiCompatibleConfig::max_stream_duration)), after which the reader
//! drops the response and the stream ends without a terminal event. Building a request body re-serialises the history; past 32 KiB it runs on
//! the blocking pool, not on a tokio worker.
//!
//! # Timeouts and cancellation
//! Every wait is bounded on the injected clock: connect and response headers
//! ([`first_byte_timeout`](OpenAiCompatibleConfig::first_byte_timeout), `Core(Timeout)` from
//! `stream()`), headers to first output ([`first_output_timeout`](
//! OpenAiCompatibleConfig::first_output_timeout); comments and reasoning do not extend it), the
//! silence between events afterwards ([`idle_timeout`](OpenAiCompatibleConfig::idle_timeout);
//! time the consumer spends not reading is not charged) and the whole response. Dropping the
//! [`LlmStream`] ends the reader task at once and drops the HTTP response, which closes the
//! connection: generation stops. Dropping the `stream()` future before the headers arrive aborts
//! the request the same way.
//!
//! # Errors
//! A non-2xx status is [`Error::Rejected`] with that status, unchanged (Together documents a
//! context overflow as 403, Groq has 498/499, ...). The adapter logs `Retry-After` and the error
//! `type` / `code` (never the message: it can echo a key fragment) and does **not** retry. An
//! `error` chunk inside the stream becomes [`LlmEvent::Error`] (`Rejected` when it names a
//! status, `Protocol` otherwise); a body that ends before the response is complete is
//! `Disconnected`. A body that ends or breaks (connection reset, cut chunked encoding) after
//! the `finish_reason` chunk is complete: at most the usage is lost.
//!
//! Nothing that identifies the endpoint beyond scheme, host and port is logged or printed: the
//! base URL and the chat path may carry credentials (userinfo, `?key=`), so transport errors are
//! logged by class only and `Debug` of the configuration shows the origin and header / body
//! field names, never their values.
//!
//! # Warm-up and plain HTTP
//! [`warm`](LlmProvider::warm) sends an unauthenticated `HEAD` to the chat URL, ignores the
//! status and keeps the connection pooled. `http://` base URLs are accepted for loopback hosts
//! only, unless [`OpenAiCompatibleConfig::allow_insecure_http`] is set.

mod chunks;
mod config;
mod request;

use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_core::Error as CoreError;
use brisvox_core::SharedClock;
use brisvox_core::clock::deadline_after;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value};
use tokio::sync::mpsc;
use tokio::time::Instant;

use self::chunks::{Flow, Mapper, error_parts};
use self::request::{RenderedTools, build_body};
pub use config::{
    AuthStyle, DEFAULT_CHAT_PATH, DEFAULT_MAX_EVENT_BYTES, DEFAULT_POOL_IDLE_TIMEOUT,
    DEFAULT_PROVIDER_NAME, DEFAULT_USAGE_GRACE, MaxTokensField, OpenAiCompatibleConfig, Preset,
    Quirks, ToolChoice, UsagePlacement,
};

use crate::error::{Error, Result};
use crate::http_llm::{self, protocol, within};
use crate::llm::{LLM_EVENT_CAPACITY, LlmEvent, LlmProvider, LlmRequest, LlmStream};
use crate::sse::SseParser;

/// Size of a request's history above which the body is built on the blocking pool instead of
/// the tokio worker (the same threshold as the OpenAI adapter).
const OFFLOAD_BYTES: usize = 32 * 1024;

/// Streaming chat with tools on any OpenAI-compatible Chat Completions server. See the
/// [module docs](self).
///
/// Cheap to share: wrap it in an `Arc`. The HTTP client (and its connection pool) is NOT owned
/// by the value: it comes from the process-wide pool (`net::shared_client`, keyed by origin and
/// connection settings), outlives the adapter and is shared by every instance and every
/// runtime that talks to the same origin the same way, so building one adapter per call is
/// fine and a call's first request finds the connection a previous call or `warm()` opened.
#[derive(Debug)]
pub struct OpenAiCompatibleLlm {
    inner: Arc<Inner>,
}

struct Inner {
    config: OpenAiCompatibleConfig,
    clock: SharedClock,
    client: reqwest::Client,
    url: String,
    /// The auth header and the extra headers, sent with `stream` only.
    headers: HeaderMap,
    extra_body: Map<String, Value>,
    tools: Mutex<Option<Arc<RenderedTools>>>,
    /// Set once `stream` has been called, only to log the first request of a configuration.
    used: AtomicBool,
}

impl fmt::Debug for Inner {
    /// Omits the URL, the headers and the extra body: they may carry credentials.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inner")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// Bytes of history `request` carries, as far as building the body has to read them.
fn request_bytes(request: &LlmRequest) -> usize {
    request.messages.iter().fold(0usize, |total, m| {
        let calls: usize = m
            .tool_calls
            .iter()
            .map(|c| c.id.len() + c.name.len() + c.arguments.len())
            .sum();
        total.saturating_add(m.content.len()).saturating_add(calls)
    })
}

/// The JSON body for `request`. Building it re-serialises the whole history, so past
/// [`OFFLOAD_BYTES`] it runs on the blocking pool, not on a tokio worker.
async fn build_request_body(inner: &Arc<Inner>, request: LlmRequest) -> Result<bytes::Bytes> {
    build_on_thread(inner, request).await.map(|(body, _)| body)
}

/// [`build_request_body`], also returning the thread the body was built on, so a test can see
/// whether a request was offloaded.
async fn build_on_thread(
    inner: &Arc<Inner>,
    request: LlmRequest,
) -> Result<(bytes::Bytes, std::thread::ThreadId)> {
    let tools = inner.tools_for(&request)?;
    let heavy = request_bytes(&request) > OFFLOAD_BYTES;
    let build = Arc::clone(inner);
    http_llm::run_cpu("llm request builder", heavy, move || {
        build_body(&build.config, &build.extra_body, &request, tools.as_deref())
            .map(|body| (body, std::thread::current().id()))
    })
    .await?
}

impl OpenAiCompatibleLlm {
    /// Builds the adapter and its HTTP client (rustls with the shared config of
    /// [`net::tls_config`](crate::net::tls_config), ALPN `h2` then `http/1.1`, no system proxy,
    /// no redirects so the key never follows one).
    ///
    /// # Errors
    /// [`Error::Protocol`] if the model is empty, the base URL is not `https://` (or `http://` on
    /// a loopback host, or on any host with `allow_insecure_http`), the auth header or an extra
    /// header has an invalid name or value (the value is never included), `extra_body` sets a
    /// field the adapter owns, or the HTTP client cannot be built.
    pub fn new(config: OpenAiCompatibleConfig, clock: SharedClock) -> Result<Self> {
        let name = config.name;
        if config.model.trim().is_empty() {
            return Err(protocol(name, "the model id is empty"));
        }
        http_llm::check_base_url(name, &config.base_url, config.allow_insecure_http)?;
        if !config.chat_path.starts_with('/') {
            return Err(protocol(name, "chat_path must start with '/'"));
        }
        let mut extra_body = Map::new();
        for (key, value) in &config.extra_body {
            if config.owns_body_key(key) {
                return Err(protocol(
                    name,
                    format!(
                        "extra_body sets {}, which the adapter owns",
                        http_llm::sanitize(key)
                    ),
                ));
            }
            extra_body.insert(key.clone(), value.clone());
        }
        let client = crate::net::shared_client(&crate::net::HttpClientSpec {
            provider: name,
            base_url: &config.base_url,
            connect_timeout: config.connect_timeout,
            pool_idle_timeout: config.pool_idle_timeout,
            http2_prior_knowledge: false,
        })?;
        let mut headers = HeaderMap::new();
        if let Some(key) = &config.api_key {
            let header = header_name(name, &config.auth.header, "the auth header name")?;
            let value = match config.auth.scheme.as_deref().filter(|s| !s.is_empty()) {
                Some(scheme) => format!("{scheme} {}", key.expose()),
                None => key.expose().clone(),
            };
            headers.insert(
                header,
                http_llm::sensitive_header_value(name, &value, "the API key")?,
            );
        }
        for (header, value) in &config.extra_headers {
            headers.insert(
                header_name(name, header, "an extra header name")?,
                http_llm::sensitive_header_value(name, value.expose(), "an extra header value")?,
            );
        }
        let url = chat_url(name, &config.base_url, &config.chat_path)?;
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                clock,
                client,
                url,
                headers,
                extra_body,
                tools: Mutex::new(None),
                used: AtomicBool::new(false),
            }),
        })
    }

    /// The configuration in use (the API key and header values print as `[REDACTED]`).
    pub fn config(&self) -> &OpenAiCompatibleConfig {
        &self.inner.config
    }
}

/// The chat URL: `chat_path` appended to the path of `base_url`, with the query of the base URL
/// (`?key=...`) kept after it and the query of `chat_path`, if it has one, appended to that.
/// The fragment of either is dropped (it is never sent). Plain string concatenation would put
/// the path after the base URL's query.
///
/// # Errors
/// [`Error::Protocol`] if `base_url` is not a URL that can carry a path (the URL is never echoed).
fn chat_url(provider: &'static str, base_url: &str, chat_path: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(base_url)
        .map_err(|_| protocol(provider, "base_url is not a valid absolute URL"))?;
    if url.cannot_be_a_base() {
        return Err(protocol(provider, "base_url cannot carry a path"));
    }
    let (path_and_query, _) = chat_path.split_once('#').unwrap_or((chat_path, ""));
    let (path, path_query) = path_and_query
        .split_once('?')
        .unwrap_or((path_and_query, ""));
    let joined = format!("{}{path}", url.path().trim_end_matches('/'));
    let query = [url.query().unwrap_or(""), path_query]
        .iter()
        .filter(|q| !q.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("&");
    url.set_path(&joined);
    url.set_query((!query.is_empty()).then_some(query.as_str()));
    url.set_fragment(None);
    Ok(url.into())
}

fn header_name(provider: &'static str, text: &str, what: &str) -> Result<HeaderName> {
    HeaderName::from_str(text)
        .map_err(|_| protocol(provider, format!("{what} is not a valid HTTP header name")))
}

impl Inner {
    fn name(&self) -> &'static str {
        self.config.name
    }

    /// The rendered tools for `request`, from the cache while the tools are unchanged.
    fn tools_for(&self, request: &LlmRequest) -> Result<Option<Arc<RenderedTools>>> {
        if request.tools.is_empty() {
            return Ok(None);
        }
        let mut cache = self.tools.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(cached) = cache.as_ref().filter(|c| c.matches(&request.tools)) {
            return Ok(Some(Arc::clone(cached)));
        }
        let rendered = RenderedTools::render(self.name(), &request.tools)?;
        *cache = Some(Arc::clone(&rendered));
        Ok(Some(rendered))
    }

    /// Reads a refusal's `Retry-After` and error type/code for the log, and maps it.
    async fn rejection(&self, mut response: reqwest::Response) -> Error {
        let status = response.status().as_u16();
        let retry_after = http_llm::retry_after_secs(&response);
        let body = http_llm::read_error_body(&self.clock, &mut response, "llm error body").await;
        let (kind, code) = parse_error_body(&body);
        // The error message is deliberately not logged: a 401 echoes part of the key.
        tracing::warn!(
            provider = self.name(),
            status,
            retry_after_secs = retry_after,
            error_type = kind.as_deref(),
            error_code = code.as_deref(),
            "the LLM server rejected the request (not retried by the adapter)"
        );
        Error::Rejected {
            provider: self.name(),
            status,
        }
    }

    fn transport_error(
        &self,
        error: &reqwest::Error,
        limit: Duration,
        what: &'static str,
    ) -> Error {
        // Never `%error`: reqwest's Display appends the request URL, and the configured base URL
        // may carry credentials (userinfo, `?key=`). Only the class of the failure is logged.
        tracing::debug!(
            provider = self.name(),
            what,
            timeout = error.is_timeout(),
            connect = error.is_connect(),
            body = error.is_body(),
            decode = error.is_decode(),
            "LLM transport error"
        );
        http_llm::transport_error(self.name(), error, limit, what)
    }
}

/// The sanitised `error.type` and `error.code` of an error body (`error` is an object, or a bare
/// string whose text is never kept).
fn parse_error_body(body: &[u8]) -> (Option<String>, Option<String>) {
    #[derive(serde::Deserialize)]
    struct Envelope<'a> {
        #[serde(default, borrow)]
        error: Option<&'a serde_json::value::RawValue>,
    }
    match serde_json::from_slice::<Envelope<'_>>(body) {
        Ok(Envelope { error }) => error_parts(error),
        Err(_) => (None, None),
    }
}

impl LlmProvider for OpenAiCompatibleLlm {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmStream> {
        let inner = &self.inner;
        let config = &inner.config;
        let body = build_request_body(inner, request).await?;
        if !inner.used.swap(true, Ordering::Relaxed) {
            tracing::debug!(provider = inner.name(), model = %config.model, "first LLM request");
        }
        let mut headers = inner.headers.clone();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        let send = inner
            .client
            .post(&inner.url)
            .headers(headers)
            .body(body)
            .send();
        let response = within(
            &inner.clock,
            config.first_byte_timeout,
            "llm first byte",
            send,
        )
        .await?
        .map_err(|e| inner.transport_error(&e, config.connect_timeout, "llm connect"))?;
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
        let head = inner.client.head(&inner.url).send();
        within(&inner.clock, config.first_byte_timeout, "llm warm", head)
            .await?
            .map(drop)
            .map_err(|e| inner.transport_error(&e, config.connect_timeout, "llm warm"))
    }
}

/// How long past the stream deadline the final timeout error may wait for room in the event
/// channel.
const TERMINAL_SEND_GRACE: Duration = Duration::from_secs(5);

/// Hands `event` to the consumer. Returns `false` when the consumer dropped the stream, or did
/// not make room before `deadline` (on the injected clock): a consumer that holds the stream
/// without reading must not keep the HTTP response open forever, so the reader gives up and the
/// stream ends without a terminal event (documented on [`LlmStream::next`]: that is a cut-off).
/// While room is short, the socket is not read (backpressure).
async fn deliver(
    clock: &SharedClock,
    tx: &mpsc::Sender<LlmEvent>,
    event: LlmEvent,
    deadline: Instant,
) -> bool {
    let event = match tx.try_send(event) {
        Ok(()) => return true,
        Err(mpsc::error::TrySendError::Closed(_)) => return false,
        Err(mpsc::error::TrySendError::Full(event)) => event,
    };
    tokio::select! {
        sent = tx.send(event) => sent.is_ok(),
        () = clock.sleep_until(deadline) => false,
    }
}

/// Reads the response body, maps its chunks and sends the events until the response ends, an
/// error, or the consumer drops the stream.
///
/// Deadlines: until the first output (a text or tool-call fragment, or the end of the response)
/// the only body deadline is `first_output_timeout`; after it, `idle_timeout` is re-armed by every
/// dispatched SSE event that produced output, and only once the outputs have been handed to the
/// consumer, so time spent waiting for a slow consumer is not charged to the server (the wait
/// itself is bounded by the stream deadline, see [`deliver`]). After the
/// chunk with `finish_reason`, while a trailing usage chunk may still come, the deadline is the
/// usage grace and its expiry ends the response normally.
async fn pump(inner: Arc<Inner>, mut response: reqwest::Response, tx: mpsc::Sender<LlmEvent>) {
    let config = &inner.config;
    let clock = &inner.clock;
    let mut parser = SseParser::new(config.max_event_bytes);
    let mut mapper = Mapper::new(inner.name(), config.quirks);
    let mut out: Vec<LlmEvent> = Vec::with_capacity(4);
    let stream_deadline = deadline_after(clock.now(), config.max_stream_duration);
    let mut got_output = false;
    let mut event_deadline = deadline_after(clock.now(), config.first_output_timeout);
    let mut grace_deadline = None;
    let timer = clock.sleep_until(event_deadline.min(stream_deadline));
    tokio::pin!(timer);
    loop {
        let chunk = tokio::select! {
            biased;
            () = tx.closed() => return,
            () = timer.as_mut() => {
                if clock.now() < stream_deadline && grace_deadline.is_some() {
                    // The usage did not come: the response is complete without it.
                    mapper.finish_now(&mut out);
                    for event in out.drain(..) {
                        if !deliver(clock, &tx, event, stream_deadline).await {
                            return;
                        }
                    }
                    return;
                }
                let (what, after) = if clock.now() >= stream_deadline {
                    ("llm stream", config.max_stream_duration)
                } else if got_output {
                    ("llm idle", config.idle_timeout)
                } else {
                    ("llm first output", config.first_output_timeout)
                };
                let error = LlmEvent::Error(Error::Core(CoreError::Timeout { what, after }));
                let until = stream_deadline.max(deadline_after(clock.now(), TERMINAL_SEND_GRACE));
                deliver(clock, &tx, error, until).await;
                return;
            }
            chunk = response.chunk() => chunk,
        };
        let bytes = match chunk {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                // The body ended: complete if the finish chunk was seen, cut off otherwise.
                let event = if mapper.finish_at_eof(&mut out) {
                    out.pop()
                } else {
                    Some(LlmEvent::Error(Error::Disconnected {
                        provider: inner.name(),
                    }))
                };
                if let Some(event) = event {
                    deliver(clock, &tx, event, stream_deadline).await;
                }
                return;
            }
            Err(e) => {
                // A reset or a cut chunked body after the finish chunk loses at most the usage:
                // the answer is complete, exactly as at a clean end of the body.
                let event = if mapper.finish_at_eof(&mut out) {
                    out.pop()
                } else {
                    let error = inner.transport_error(&e, config.idle_timeout, "llm idle");
                    Some(LlmEvent::Error(error))
                };
                if let Some(event) = event {
                    let _ = deliver(clock, &tx, event, stream_deadline).await;
                }
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
                Err(e) => Err(protocol(inner.name(), e.to_string())),
            };
            got_output |= !out.is_empty();
            if let Some(upstream) = mapper.take_upstream() {
                // Once per response: which upstream a gateway (OpenRouter) routed to.
                tracing::info!(
                    target: "brisvox::llm",
                    provider = inner.name(),
                    upstream_provider = upstream.provider.as_deref().unwrap_or("-"),
                    upstream_model = upstream.model.as_deref().unwrap_or("-"),
                    "llm upstream"
                );
            }
            for event in out.drain(..) {
                // Backpressure: wait for room (the socket is not read meanwhile).
                if !deliver(clock, &tx, event, stream_deadline).await {
                    return;
                }
            }
            match flow {
                Ok(Flow::Continue) => {}
                Ok(Flow::AwaitUsage) => {
                    got_output = true;
                    grace_deadline
                        .get_or_insert_with(|| deadline_after(clock.now(), config.usage_grace));
                }
                Ok(Flow::Finished) => {
                    drop(tx);
                    http_llm::drain(clock, response, "llm drain").await;
                    return;
                }
                Err(error) => {
                    deliver(clock, &tx, LlmEvent::Error(error), stream_deadline).await;
                    return;
                }
            }
        }
        if dispatched && got_output {
            event_deadline = deadline_after(clock.now(), config.idle_timeout);
        }
        let next = grace_deadline.unwrap_or(event_deadline);
        timer.as_mut().reset(next.min(stream_deadline));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Message, Role};

    fn config() -> OpenAiCompatibleConfig {
        OpenAiCompatibleConfig::new("http://127.0.0.1:1/v1", "m")
    }

    #[test]
    fn construction_validates_the_configuration() {
        let clock = brisvox_core::SystemClock::shared;
        assert!(OpenAiCompatibleLlm::new(config(), clock()).is_ok());
        assert!(OpenAiCompatibleLlm::new(config().with_model(" "), clock()).is_err());
        assert!(
            OpenAiCompatibleLlm::new(config().with_base_url("http://example.com/v1"), clock())
                .is_err()
        );
        assert!(
            OpenAiCompatibleLlm::new(config().with_base_url("ftp://example.com/v1"), clock())
                .is_err()
        );
        let mut open = config().with_base_url("http://example.com/v1");
        open.allow_insecure_http = true;
        assert!(OpenAiCompatibleLlm::new(open, clock()).is_ok());
        assert!(
            OpenAiCompatibleLlm::new(
                config().with_body_field("stream", serde_json::json!(false)),
                clock()
            )
            .is_err()
        );
        assert!(
            OpenAiCompatibleLlm::new(config().with_header("bad header", "v"), clock()).is_err()
        );
        assert!(
            OpenAiCompatibleLlm::new(config().with_header("x-ok", "bad\nvalue"), clock()).is_err()
        );
        let mut no_slash = config();
        no_slash.chat_path = "chat".to_owned();
        assert!(OpenAiCompatibleLlm::new(no_slash, clock()).is_err());
    }

    #[test]
    fn the_chat_url_keeps_the_query_of_the_base_url_after_the_path() {
        let url = |base: &str, path: &str| chat_url("t", base, path).unwrap();
        assert_eq!(
            url("https://gw.example/v1", "/chat/completions"),
            "https://gw.example/v1/chat/completions"
        );
        assert_eq!(
            url("https://gw.example/v1/", "/chat/completions"),
            "https://gw.example/v1/chat/completions"
        );
        assert_eq!(
            url("https://gw.example", "/chat/completions"),
            "https://gw.example/chat/completions"
        );
        assert_eq!(
            url("https://gw.example/v1?key=abc", "/chat/completions"),
            "https://gw.example/v1/chat/completions?key=abc"
        );
        assert_eq!(
            url("https://gw.example/v1/?key=abc#x", "/chat/completions"),
            "https://gw.example/v1/chat/completions?key=abc"
        );
        assert_eq!(
            url("https://gw.example/v1#x", "/chat/completions"),
            "https://gw.example/v1/chat/completions"
        );
        assert_eq!(
            url("https://gw.example/v1?key=abc", "/chat?api-version=2"),
            "https://gw.example/v1/chat?key=abc&api-version=2"
        );
        assert_eq!(
            url("http://127.0.0.1:11434/v1", "/chat/completions?k=v#f"),
            "http://127.0.0.1:11434/v1/chat/completions?k=v"
        );
        assert!(chat_url("t", "mailto:a@b.c", "/chat").is_err());
    }

    #[tokio::test]
    async fn only_large_histories_are_built_off_the_calling_thread() {
        let llm = OpenAiCompatibleLlm::new(config(), brisvox_core::SystemClock::shared()).unwrap();
        let here = std::thread::current().id();
        let request = |bytes: usize| LlmRequest {
            messages: vec![Message::new(Role::User, "x".repeat(bytes))],
            tools: Vec::new(),
        };
        let (_, small) = build_on_thread(&llm.inner, request(OFFLOAD_BYTES))
            .await
            .unwrap();
        assert_eq!(small, here, "a small body is built inline");
        let (body, large) = build_on_thread(&llm.inner, request(OFFLOAD_BYTES + 1))
            .await
            .unwrap();
        assert_ne!(large, here, "a large body must not be built on the worker");
        assert!(body.len() > OFFLOAD_BYTES);
    }

    #[test]
    fn request_size_counts_the_history() {
        let small = LlmRequest {
            messages: vec![Message::new(Role::User, "hi")],
            tools: Vec::new(),
        };
        assert!(request_bytes(&small) <= OFFLOAD_BYTES);
        let big = LlmRequest {
            messages: vec![Message::new(Role::User, "x".repeat(OFFLOAD_BYTES + 1))],
            tools: Vec::new(),
        };
        assert!(request_bytes(&big) > OFFLOAD_BYTES);
    }
}
