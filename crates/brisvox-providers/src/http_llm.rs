//! Helpers shared by the HTTP/SSE LLM adapters ([`openai`](crate::openai) and
//! [`openai_compatible`](crate::openai_compatible)): deadlines on the injected clock, the
//! plain-HTTP guard, sensitive header values, bounded reads of error bodies and of the tail of a
//! finished response, and the offload of CPU-bound work to the blocking pool.
//!
//! Everything here is provider-neutral: the provider name and the `what` of a timeout are
//! arguments, so each adapter keeps its own log and error text.

use std::future::Future;
use std::time::Duration;

use brisvox_core::Error as CoreError;
use brisvox_core::SharedClock;
use reqwest::header::HeaderValue;

use crate::error::{Error, Result};

/// Bytes of an error response body read for logging.
pub(crate) const ERROR_BODY_LIMIT: usize = 8 * 1024;

/// Bound on reading an error response body.
pub(crate) const ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(1);

/// After the terminal event, the rest of the body (normally `[DONE]` and the chunk terminator)
/// is drained so an HTTP/1.1 connection can return to the pool: at most this many bytes...
pub(crate) const DRAIN_LIMIT: usize = 16 * 1024;

/// ... and at most this long in total.
pub(crate) const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// Longest code or type kept in a log field or an error detail.
const MAX_CODE: usize = 64;

/// `s` reduced to a short, log-safe token (`[A-Za-z0-9_.-]`, at most 64 characters).
pub(crate) fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        .take(MAX_CODE)
        .collect()
}

/// A [`Error::Protocol`] for `provider`.
pub(crate) fn protocol(provider: &'static str, detail: impl Into<String>) -> Error {
    Error::Protocol {
        provider,
        detail: detail.into(),
    }
}

/// Runs `future` with a deadline on the injected clock.
pub(crate) async fn within<F: Future>(
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

/// Refuses a base URL that would send credentials in cleartext to a remote host: `https://` is
/// accepted; `http://` only for loopback hosts or with `allow_insecure_http`.
///
/// # Errors
/// [`Error::Protocol`] (the URL itself is never echoed: it may carry credentials).
pub(crate) fn check_base_url(
    provider: &'static str,
    base_url: &str,
    allow_insecure_http: bool,
) -> Result<()> {
    let url = reqwest::Url::parse(base_url)
        .map_err(|_| protocol(provider, "base_url is not a valid absolute URL"))?;
    match url.scheme() {
        "https" => Ok(()),
        "http" => {
            let host = url.host_str().unwrap_or_default();
            let host = host.trim_start_matches('[').trim_end_matches(']');
            let loopback = host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback());
            if loopback || allow_insecure_http {
                Ok(())
            } else {
                Err(protocol(
                    provider,
                    "base_url is plain http:// on a non-loopback host: the API key would be sent \
                     unencrypted (use https://, or set allow_insecure_http)",
                ))
            }
        }
        _ => Err(protocol(
            provider,
            "base_url must be an http:// or https:// URL",
        )),
    }
}

/// `text` as a header value marked sensitive (kept out of the HTTP client's debug output).
///
/// # Errors
/// [`Error::Protocol`] naming `what` (never the value) if `text` is not a valid header value.
pub(crate) fn sensitive_header_value(
    provider: &'static str,
    text: &str,
    what: &str,
) -> Result<HeaderValue> {
    let mut value = HeaderValue::from_str(text)
        .map_err(|_| protocol(provider, format!("{what} is not a valid HTTP header value")))?;
    value.set_sensitive(true);
    Ok(value)
}

/// Reads at most [`ERROR_BODY_LIMIT`] bytes of a refusal's body, for at most
/// [`ERROR_BODY_TIMEOUT`] on the injected clock. A body that stalls or fails to read is simply
/// shorter.
pub(crate) async fn read_error_body(
    clock: &SharedClock,
    response: &mut reqwest::Response,
    what: &'static str,
) -> Vec<u8> {
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
    // A stalled body is cut at the deadline; what was read so far is still used.
    let _ = within(clock, ERROR_BODY_TIMEOUT, what, read).await;
    body
}

/// The `Retry-After` header of `response` in whole seconds, if it has one in that form.
pub(crate) fn retry_after_secs(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// Reads what is left of a finished response so the connection can be reused: at most
/// [`DRAIN_LIMIT`] bytes and [`DRAIN_TIMEOUT`] in total, whichever comes first.
pub(crate) async fn drain(
    clock: &SharedClock,
    mut response: reqwest::Response,
    what: &'static str,
) {
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
    let _ = within(clock, DRAIN_TIMEOUT, what, read).await;
}

/// Runs CPU-bound `work` inline when it is small, otherwise on tokio's blocking pool (never on a
/// worker thread).
///
/// # Errors
/// [`Error::Core`] (`ProcessorPanicked(what)`) if the work panicked or was cancelled by a runtime
/// shutdown.
pub(crate) async fn run_cpu<T, F>(what: &'static str, heavy: bool, work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    if !heavy {
        return Ok(work());
    }
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| Error::Core(CoreError::ProcessorPanicked(what)))
}

/// A transport error as a provider error: a timeout stays a timeout, everything else (refused,
/// reset, TLS failure, truncated body) is a lost connection.
pub(crate) fn transport_error(
    provider: &'static str,
    error: &reqwest::Error,
    limit: Duration,
    what: &'static str,
) -> Error {
    if error.is_timeout() {
        Error::Core(CoreError::Timeout { what, after: limit })
    } else {
        Error::Disconnected { provider }
    }
}
