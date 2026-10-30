//! Deepgram streaming speech-to-text (`wss://api.deepgram.com/v1/listen`), feature
//! `provider-deepgram`.
//!
//! [`DeepgramStt`] implements [`SttProvider`]. Facts about the protocol come from
//! `docs/providers/deepgram.md` (fetched 2026-10-08); the points that document marks UNVERIFIED
//! are handled defensively and listed in its "Adapter" section.
//!
//! # Behaviour in one page
//! - **Auth.** `Authorization: Token <key>` (header marked sensitive); the key is a
//!   [`Secret`](brisvox_core::Secret) and appears in no log, error or `Debug` output.
//! - **Audio.** Binary frames, the caller's [`Bytes`](bytes::Bytes) passed through uncopied.
//!   Raw audio needs `encoding` and `sample_rate`; both are always sent.
//! - **Results.** Interims are forwarded as interim transcripts (dropped, never blocking, when
//!   the events channel is full); each final segment is forwarded as a final transcript and
//!   `speech_final` marks the end of the utterance (rules: "Result semantics" and "Adapter" in
//!   `docs/providers/deepgram.md`). Unknown message types and fields are ignored.
//! - **Finalize.** [`SttInput::Finalize`](crate::stt::SttInput::Finalize) sends
//!   `{"type":"Finalize"}`. Deepgram does not promise an answer when little audio is buffered, so
//!   after [`DeepgramConfig::finalize_timeout`] the adapter ends the utterance itself with a final
//!   (`speech_final`, `from_finalize`) carrying the last interim, or empty text; the bound runs
//!   from the request, also across a reconnect. When the server has already reported on all audio
//!   but the last [`DeepgramConfig::finalize_settle_gap`] and nothing is in progress, the answer
//!   is that empty final at once and nothing is sent. The turn machine therefore never waits for
//!   an answer that will not come.
//! - **Keepalive.** `{"type":"KeepAlive"}` after [`DeepgramConfig::keepalive_interval`] without
//!   audio; a connection that has seen no audio at all gets 20 ms of silence instead (KeepAlive
//!   alone does not prevent the 10 s `NET-0001` close).
//! - **Errors.** Handshake HTTP status (401, 402, 403, 429, 5xx...) is `Rejected { status }`; a
//!   `1008` close (audio cannot be decoded) is `Rejected { status: 1008 }`; a malformed message or
//!   an `Error` message of variant `SchemaError` / `InvalidConfigureMessage` is `Protocol` (other
//!   `Error` messages are documented as non-fatal and only logged); a connection lost for good is
//!   `Disconnected`; a slow connect, or a server that stays silent after the connection was
//!   replaced as often as allowed, is `Core(Timeout)`.
//! - **Reconnect.** An unexpected drop opens a new connection (up to
//!   [`DeepgramConfig::max_reconnects`] attempts, exponential backoff on the injected clock) and
//!   replays the audio the server has not finalized yet. See the `session` module docs.
//! - **Hang-up.** Dropping the session's input ends it: events close at once, `CloseStream` goes
//!   out and the socket is drained for at most [`DeepgramConfig::close_timeout`].
//!
//! # `warm`
//! Builds (once per process) the shared rustls config and resolves the host name, both bounded by
//! `io_timeout`. It does not hold a connection open: Deepgram closes a connection that gets no
//! audio within 10 s. All sessions share one TLS config, so TLS 1.3 session tickets from earlier
//! connections speed up later handshakes.

mod assembler;
mod config;
mod connection;
mod protocol;
mod replay;
mod session;

use std::sync::Arc;

use brisvox_core::SharedClock;
use tokio::sync::mpsc;

pub use config::{DeepgramConfig, Encoding, Endpointing, Region};

use crate::error::{Error, Result};
use crate::stt::{STT_CHANNEL_CAPACITY, SttConfig, SttProvider, SttSession};
use connection::{Target, open, timed};

/// Provider name used in errors and logs.
pub(crate) const PROVIDER: &str = "deepgram";

/// Deepgram streaming STT.
#[derive(Debug, Clone)]
pub struct DeepgramStt {
    cfg: Arc<DeepgramConfig>,
    clock: SharedClock,
}

impl DeepgramStt {
    /// An adapter with `config`; all deadlines and backoffs run on `clock`.
    pub fn new(config: DeepgramConfig, clock: SharedClock) -> Self {
        Self {
            cfg: Arc::new(config),
            clock,
        }
    }

    /// The settings in use.
    pub fn config(&self) -> &DeepgramConfig {
        &self.cfg
    }

    fn target(&self, stt: &SttConfig) -> Result<Target> {
        let (_, tls) = self.cfg.authority();
        let tls = if tls {
            Some(crate::net::tls_config()?)
        } else {
            None
        };
        Ok(Target::new(self.cfg.url(stt), tls))
    }
}

impl SttProvider for DeepgramStt {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    async fn connect(&self, config: SttConfig) -> Result<SttSession> {
        self.cfg.validate()?;
        let target = self.target(&config)?;
        let ws = open(&self.cfg, &*self.clock, &target).await?;
        let (input_tx, input_rx) = mpsc::channel(STT_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel(STT_CHANNEL_CAPACITY);
        let params = session::Params {
            cfg: Arc::clone(&self.cfg),
            clock: Arc::clone(&self.clock),
            bytes_per_second: self.cfg.bytes_per_second(&config),
            multi: config
                .language
                .as_deref()
                .or(self.cfg.language.as_deref())
                .is_some_and(|language| language == "multi"),
            target,
        };
        // Ends when the caller drops the input sender or the events receiver (see `session`).
        tokio::spawn(session::run(params, ws, input_rx, event_tx));
        Ok(SttSession {
            input: input_tx,
            events: event_rx,
        })
    }

    async fn warm(&self) -> Result<()> {
        self.cfg.validate()?;
        let (authority, tls) = self.cfg.authority();
        if tls {
            crate::net::tls_config()?;
        }
        let address = with_default_port(&authority, if tls { 443 } else { 80 });
        let lookup = timed(
            &*self.clock,
            self.cfg.io_timeout,
            "deepgram warm",
            tokio::net::lookup_host(address),
        )
        .await?;
        lookup
            .map(drop)
            .map_err(|_| Error::Disconnected { provider: PROVIDER })
    }
}

/// `host[:port]` with `default` appended when no port is given (IPv6 literals are bracketed).
fn with_default_port(authority: &str, default: u16) -> String {
    let has_port = match authority.strip_prefix('[') {
        Some(rest) => rest.contains("]:"),
        None => authority.contains(':'),
    };
    if has_port {
        authority.to_owned()
    } else {
        format!("{authority}:{default}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ports_are_appended_only_when_missing() {
        assert_eq!(
            with_default_port("api.deepgram.com", 443),
            "api.deepgram.com:443"
        );
        assert_eq!(with_default_port("127.0.0.1:9", 80), "127.0.0.1:9");
        assert_eq!(with_default_port("[::1]", 80), "[::1]:80");
        assert_eq!(with_default_port("[::1]:9", 80), "[::1]:9");
    }
}
