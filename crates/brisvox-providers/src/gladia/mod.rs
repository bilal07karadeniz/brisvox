//! Gladia live speech-to-text (`POST /v2/live` + the returned WebSocket), feature
//! `provider-gladia`.
//!
//! [`GladiaStt`] implements [`SttProvider`]. Facts about the protocol come from
//! `docs/providers/gladia.md` (fetched 2026-10-08); the points that document marks UNVERIFIED are
//! handled defensively and listed in its "Gotchas found during implementation" section.
//!
//! # Behaviour in one page
//! - **Two-step connect.** One HTTPS call (`POST /v2/live`, header `x-gladia-key`, a
//!   [`Secret`](brisvox_core::Secret) that appears in no log, error or `Debug` output) returns a
//!   WebSocket URL with a temporary session token in its query; the adapter then opens that URL
//!   (no key on the socket). The URL is a credential too and is never logged. Both steps share
//!   one `io_timeout` deadline. The URL must be `wss://` (`ws://` only on loopback).
//! - **Audio.** Binary frames, the caller's [`Bytes`](bytes::Bytes) passed through uncopied; the
//!   encoding, bit depth and sample rate declared at init always match what is sent: the rate is
//!   the session's [`SttConfig`] sample rate, and [`GladiaConfig::sample_rate_hz`] is accepted
//!   only when it equals it (the adapter does not resample; a different value fails the
//!   connect).
//! - **Results.** Partials (`is_final: false`) are forwarded as interim transcripts (dropped,
//!   never blocking, when the events channel is full). A final (`is_final: true`) is one
//!   utterance closed by Gladia's endpointing and is forwarded as a final with `speech_final`
//!   set. Unknown message types and fields are ignored; a transcript without `is_final` counts
//!   as a partial.
//! - **Finalize.** Gladia has no flush command (`stop_recording` ends the session), so
//!   [`SttInput::Finalize`](crate::stt::SttInput::Finalize) is answered by the adapter: when an
//!   utterance is open (a partial, or `speech_start` without `speech_end`) it waits up to
//!   [`GladiaConfig::finalize_timeout`] for Gladia's own final and delivers it flagged
//!   `from_finalize`; on timeout it ends the utterance with the last partial (or empty text) and
//!   drops the real final that may follow. With nothing open it answers at once with an empty
//!   final, except on a session that has not sent a single transcript or speech event yet (the
//!   first speech may still be on its way: it waits like an open utterance). The bound runs
//!   from the request, also across a reconnect, so the turn machine never waits for an answer
//!   that will not come. Every request is answered exactly once, as the engine's ledger expects:
//!   a request that arrives while another waits is counted, and the answer to the first (the
//!   real final, the timeout) is followed by one empty `from_finalize` final per extra request
//!   (at most 32 are counted, the engine's own bound). Latency of the real answer is set by
//!   [`GladiaConfig::endpointing`].
//! - **Orphaned sessions.** If init created a session but the WebSocket then fails to open (the
//!   upgrade is refused, stalls past the deadline, TLS fails) or the connect is cancelled, the
//!   adapter sends one best-effort `DELETE /v2/live/{id}` in a detached task (bounded by
//!   [`GladiaConfig::close_timeout`]) so that the session does not sit on a concurrency slot.
//!   Whether Gladia frees the slot for a session that never streamed is UNVERIFIED; a failed
//!   clean-up is only a debug log.
//! - **Keepalive.** Gladia wants audio, not control messages: after
//!   [`GladiaConfig::keepalive_interval`] without audio the adapter sends 20 ms of digital
//!   silence.
//! - **Errors.** An init or upgrade HTTP status (401, 429, 5xx...) is `Rejected { status }`; a
//!   malformed message, an unacceptable init response, an acknowledgment that says audio was
//!   refused, or a server error message below 500 is `Protocol` (a server error of 500 or more
//!   reconnects); a connection lost for good is `Disconnected`; a slow connect, or (with
//!   [`GladiaConfig::acknowledgments`]) a server that stays silent after the connection was
//!   replaced as often as allowed, is `Core(Timeout)`.
//! - **Reconnect.** An unexpected drop opens a **new session** (a new init call; up to
//!   [`GladiaConfig::max_reconnects`] attempts, exponential backoff on the injected clock) and
//!   replays the audio that has not become a final transcript yet, paced at
//!   [`GladiaConfig::replay_speed`]. See the `session` module docs.
//! - **Hang-up.** Dropping the session's input ends it: events close at once, `stop_recording`
//!   goes out and the socket is drained for at most [`GladiaConfig::close_timeout`].
//!
//! # `warm`
//! Builds (once per process) the shared rustls config and sends a credential-less `HEAD` to the
//! init URL, which resolves DNS, completes TLS and pools the connection for the next init call;
//! bounded by `io_timeout`. It never creates a session: a session holds one of the account's
//! concurrent live slots (1 on the free plan, 30 on paid plans) and Gladia documents no token
//! lifetime, so pre-creating one is unsafe.

mod assembler;
mod config;
mod connection;
mod protocol;
mod replay;
mod session;

use std::sync::Arc;

use brisvox_core::SharedClock;
use tokio::sync::mpsc;

pub use config::{Encoding, GladiaConfig, Region};

use crate::error::Result;
use crate::stt::{STT_CHANNEL_CAPACITY, SttConfig, SttProvider, SttSession};
use connection::Shared;

/// Provider name used in errors and logs.
pub(crate) const PROVIDER: &str = "gladia";

/// Gladia live STT.
///
/// Cheap to share: wrap it in an `Arc` or clone it (clones share the HTTP client and its
/// connection pool).
#[derive(Debug, Clone)]
pub struct GladiaStt {
    shared: Arc<Shared>,
}

impl GladiaStt {
    /// An adapter with `config`; all deadlines and backoffs run on `clock`. Builds the HTTP
    /// client (rustls with the shared config of [`net::tls_config`](crate::net::tls_config), no
    /// system proxy, no redirects so the key never follows one).
    ///
    /// # Errors
    /// [`Error::Protocol`](crate::Error::Protocol) if the settings are invalid (see
    /// [`GladiaConfig`]), the API key is not a valid header value (the value is never included),
    /// or the HTTP client cannot be built.
    pub fn new(config: GladiaConfig, clock: SharedClock) -> Result<Self> {
        Ok(Self {
            shared: Arc::new(Shared::new(config, clock)?),
        })
    }

    /// The settings in use (the API key prints as `[REDACTED]`).
    pub fn config(&self) -> &GladiaConfig {
        &self.shared.cfg
    }
}

impl SttProvider for GladiaStt {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    async fn connect(&self, config: SttConfig) -> Result<SttSession> {
        let cfg = &self.shared.cfg;
        cfg.validate_session(&config)?;
        let ws = self.shared.open(&config).await?;
        let (input_tx, input_rx) = mpsc::channel(STT_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel(STT_CHANNEL_CAPACITY);
        let params = session::Params {
            shared: Arc::clone(&self.shared),
            bytes_per_second: cfg.bytes_per_second(&config),
            stt: config,
        };
        // Ends when the caller drops the input sender or the events receiver (see `session`).
        tokio::spawn(session::run(params, ws, input_rx, event_tx));
        Ok(SttSession {
            input: input_tx,
            events: event_rx,
        })
    }

    async fn warm(&self) -> Result<()> {
        self.shared.warm().await
    }
}
