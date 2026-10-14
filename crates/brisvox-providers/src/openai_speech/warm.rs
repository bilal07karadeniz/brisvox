//! The background model warm-up that [`warm`](crate::TtsProvider::warm) starts: one
//! tiny synthesis whose audio is discarded, so that the first real request after an idle
//! period does not meet a cold server (Kokoro-FastAPI on a GPU: 2.78 s for the first request
//! after idle against 52-104 ms warm). See the [module docs](super).
//!
//! The engine gives `warm` a budget of `min(connect_timeout, 400 ms)`, far less than a cold
//! synthesis takes, so the request is spawned and never awaited by `warm`. At most one runs at
//! a time, and a new one starts only when `warm_interval` has passed (injected clock) since the
//! last start, however many sessions call `warm`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::time::Instant;

use brisvox_core::frame::SampleRate;

use super::Inner;
use super::config::PROVIDER;
use super::session::Params;
use crate::http_llm::{self, within};

/// Shared warm-up state of one provider.
#[derive(Debug)]
pub(crate) struct WarmState {
    /// The clock's `now()` at construction; `last` counts from it.
    origin: Instant,
    /// Milliseconds since `origin` plus one at the latest start; 0: never started (or the
    /// latest attempt failed, so the next call may retry at once).
    last: AtomicU64,
    /// A warm-up request is running (at most one).
    in_flight: AtomicBool,
    /// The voice of the latest `connect`.
    voice: Mutex<Option<Arc<str>>>,
}

impl WarmState {
    pub(crate) fn new(origin: Instant) -> Self {
        Self {
            origin,
            last: AtomicU64::new(0),
            in_flight: AtomicBool::new(false),
            voice: Mutex::new(None),
        }
    }

    /// Remembers the voice of a session, for later warm-ups.
    pub(crate) fn remember_voice(&self, voice: &Arc<str>) {
        let mut guard = self
            .voice
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(Arc::clone(voice));
    }

    fn voice(&self) -> Option<Arc<str>> {
        self.voice
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Takes the right to start a warm-up at `now`: none is running and `interval` has passed
    /// since the last start.
    fn claim(&self, now: Instant, interval: std::time::Duration) -> bool {
        if self
            .in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        let since = u64::try_from(now.saturating_duration_since(self.origin).as_millis())
            .unwrap_or(u64::MAX - 1);
        let last = self.last.load(Ordering::Acquire);
        let interval_ms = u64::try_from(interval.as_millis()).unwrap_or(u64::MAX);
        if last != 0 && since.saturating_sub(last - 1) < interval_ms {
            self.in_flight.store(false, Ordering::Release);
            return false;
        }
        self.last.store(since.saturating_add(1), Ordering::Release);
        true
    }
}

/// Releases the in-flight flag when the task ends, whichever way.
struct Release(Arc<Inner>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.warm.in_flight.store(false, Ordering::Release);
    }
}

/// Starts the warm-up request in the background when it is enabled and due. Never blocks and
/// never fails: the outcome is only logged.
pub(crate) fn start(inner: &Arc<Inner>) {
    let config = &inner.config;
    if config.warm_text.trim().is_empty() {
        return;
    }
    let voice = inner
        .warm
        .voice()
        .or_else(|| config.warm_voice.as_deref().map(Arc::from));
    let Some(voice) = voice else { return };
    if !inner.warm.claim(config.clock.now(), config.warm_interval) {
        return;
    }
    let inner = Arc::clone(inner);
    tokio::spawn(async move {
        let release = Release(Arc::clone(&inner));
        let config = &inner.config;
        let started = config.clock.now();
        let params = Params {
            voice,
            // The adapter only sends this when `send_sample_rate` is set (Speaches).
            rate: SampleRate::Hz24000,
        };
        let outcome = tokio::select! {
            biased;
            () = config.shutdown.cancelled() => None,
            done = within(&config.clock, config.warm_timeout, "tts model warm", request(&inner, &params)) => Some(done),
        };
        let elapsed = config.clock.now().saturating_duration_since(started);
        if let Some(Ok(Ok(bytes))) = outcome {
            tracing::debug!(
                target: "brisvox::providers",
                provider = PROVIDER,
                elapsed_ms = elapsed.as_millis(),
                bytes,
                "tts model warm-up done"
            );
        } else {
            // Let the next call try again instead of waiting out the interval.
            inner.warm.last.store(0, Ordering::Release);
            tracing::debug!(
                target: "brisvox::providers",
                provider = PROVIDER,
                elapsed_ms = elapsed.as_millis(),
                "tts model warm-up failed, timed out or was cancelled"
            );
        }
        drop(release);
    });
}

/// Sends the warm-up request and drains the body; returns the audio bytes received.
async fn request(inner: &Inner, params: &Params) -> Result<u64, crate::Error> {
    let config = &inner.config;
    let body = inner.request_body(params, config.warm_text.trim())?;
    let mut headers = inner.headers.clone();
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    let mut response = inner
        .client
        .post(&inner.url)
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|e| http_llm::transport_error(PROVIDER, &e, config.warm_timeout, "tts warm-up"))?;
    if !response.status().is_success() {
        return Err(crate::Error::Rejected {
            provider: PROVIDER,
            status: response.status().as_u16(),
        });
    }
    let mut total = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| http_llm::transport_error(PROVIDER, &e, config.warm_timeout, "tts warm-up"))?
    {
        total = total.saturating_add(chunk.len() as u64);
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
    use std::time::Duration;

    use super::*;

    #[test]
    fn claims_are_spaced_by_the_interval_and_exclusive() {
        let t0 = Instant::now();
        let state = WarmState::new(t0);
        let interval = Duration::from_secs(60);
        assert!(state.claim(t0, interval));
        // One in flight: nobody else claims.
        assert!(!state.claim(t0 + Duration::from_secs(120), interval));
        state.in_flight.store(false, Ordering::Release);
        // Done, but inside the interval.
        assert!(!state.claim(t0 + Duration::from_secs(59), interval));
        assert!(state.claim(t0 + Duration::from_secs(61), interval));
        state.in_flight.store(false, Ordering::Release);
        // A failure resets, so the next call retries at once.
        state.last.store(0, Ordering::Release);
        assert!(state.claim(t0 + Duration::from_secs(62), interval));
    }
}
