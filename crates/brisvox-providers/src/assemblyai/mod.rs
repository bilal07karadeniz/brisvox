//! AssemblyAI streaming speech-to-text (v3, `wss://streaming.assemblyai.com/v3/ws`), feature
//! `provider-assemblyai`.
//!
//! [`AssemblyAiStt`] implements [`SttProvider`]. Facts about the protocol come from
//! `docs/providers/assemblyai.md` (fetched 2026-10-08); the points that document marks UNVERIFIED
//! are handled defensively and listed in its "Adapter" section.
//!
//! # Behaviour in one page
//! - **Auth.** `Authorization: <key>` (header marked sensitive, no `Bearer`); the key is a
//!   [`Secret`](brisvox_core::Secret) and appears in no log, error or `Debug` output.
//! - **Connect.** The WebSocket upgrade and the server's `Begin` message are awaited together
//!   under one `io_timeout`, so a bad key surfaces from `connect` whether the server refuses the
//!   upgrade (HTTP status) or closes right after it (close code `1008`).
//! - **Audio.** Binary frames. The server closes a session on a chunk shorter than 50 ms, so the
//!   engine's 10-20 ms frames are batched to 50 ms or more (up to 500 ms; the minimum is rounded
//!   up to whole samples so it is never under 50 ms, whatever the sample rate). A shorter
//!   remainder is held until the next audio; it is padded with silence only when a `Finalize`
//!   needs it out (or after [`AssemblyAiConfig::batch_flush`], off by default). Audio is never
//!   sent ahead of real time except for a replay after a reconnect, which is paced at
//!   [`AssemblyAiConfig::replay_speed`] (default 1.0).
//! - **Results.** Every `Turn` re-transcribes the whole turn: partial turns are forwarded as
//!   interim transcripts (dropped, never blocking, when the events channel is full); the final
//!   turn (`end_of_turn`) is one final transcript with `speech_final` (rules: "Turn semantics"
//!   and "Adapter" in `docs/providers/assemblyai.md`). Unknown message types and fields are
//!   ignored.
//! - **Finalize.** [`SttInput::Finalize`](crate::stt::SttInput::Finalize) sends
//!   `{"type":"ForceEndpoint"}`. The docs promise an immediate final turn but say nothing about
//!   a turn-less moment, so after [`AssemblyAiConfig::finalize_timeout`] (a turn is in progress,
//!   or audio was given since the last final or `Finalize`: the first partial of a short
//!   utterance may not exist yet) or [`AssemblyAiConfig::finalize_idle_timeout`] (none is) the adapter ends the utterance
//!   itself with a final (`speech_final`, `from_finalize`) carrying the last interim, or empty
//!   text; the bound runs from the request, also across a reconnect. The turn machine therefore
//!   never waits for an answer that will not come.
//! - **Keepalive.** `session_heartbeat` is requested so that the server speaks every 5 s: that
//!   is the liveness signal of the response watchdog (`io_timeout`; without heartbeats a
//!   WebSocket `Ping` probes a silent connection first). With
//!   [`AssemblyAiConfig::inactivity_timeout`] set, `{"type":"KeepAlive"}` goes out after
//!   [`AssemblyAiConfig::keepalive_interval`] without audio.
//! - **Errors.** Handshake HTTP status (401, 402, 403, 429, 5xx...) is `Rejected { status }`; a
//!   `1008` close is `Rejected { status: 1008 }` (credentials, billing), or `Rejected { status:
//!   429 }` when the reason says "too many" (the rate limit, also close code `3009`); closes
//!   `3006` and `3007` (invalid message, chunk size) and malformed messages are
//!   `Protocol` (a `3006` for inactivity and a `3007` for the transmission rate are transient
//!   instead: the connection is replaced and the audio replayed); a connection lost for good is `Disconnected`; a slow connect, or a server that
//!   stays silent after the connection was replaced as often as allowed, is `Core(Timeout)`.
//!   `Error` messages only precede a close and are logged without content.
//! - **Reconnect.** An unexpected drop (including `3005`, `3008`, `1011`, a `Termination` the
//!   caller did not ask for) opens a new connection (up to
//!   [`AssemblyAiConfig::max_reconnects`] attempts, exponential backoff on the injected clock)
//!   and replays the audio of the turn in progress (plus a short pre-roll; silence before it is
//!   skipped so the new connection is not left behind real time). See the `session` module docs.
//! - **Hang-up.** Dropping the session's input ends it: events close at once, `Terminate` goes
//!   out and the socket is drained until `Termination` for at most
//!   [`AssemblyAiConfig::close_timeout`]. Always sent, because AssemblyAI bills by connection
//!   time.
//!
//! # `warm`
//! Builds (once per process) the shared rustls config and resolves the host name, both bounded by
//! `io_timeout`. It does not hold a connection open: billing is by session duration, so an idle
//! open socket costs money. All sessions share one TLS config, so TLS 1.3 session tickets from
//! earlier connections speed up later handshakes.

mod assembler;
mod batch;
mod config;
mod connection;
mod protocol;
mod replay;
mod session;

use std::sync::Arc;

use brisvox_core::SharedClock;
use tokio::sync::mpsc;

pub use config::{AssemblyAiConfig, Encoding, Region};

use crate::error::{Error, Result};
use crate::stt::{STT_CHANNEL_CAPACITY, SttConfig, SttProvider, SttSession};
use config::{MAX_CHUNK, MIN_CHUNK, Round};
use connection::{Target, open, timed};

/// Provider name used in errors and logs.
pub(crate) const PROVIDER: &str = "assemblyai";

/// AssemblyAI streaming STT.
#[derive(Debug, Clone)]
pub struct AssemblyAiStt {
    cfg: Arc<AssemblyAiConfig>,
    clock: SharedClock,
}

impl AssemblyAiStt {
    /// An adapter with `config`; all deadlines and backoffs run on `clock`.
    pub fn new(config: AssemblyAiConfig, clock: SharedClock) -> Self {
        Self {
            cfg: Arc::new(config),
            clock,
        }
    }

    /// The settings in use.
    pub fn config(&self) -> &AssemblyAiConfig {
        &self.cfg
    }

    fn target(&self, stt: &SttConfig) -> Result<Target> {
        let (_, tls) = self.cfg.authority();
        let tls = if tls {
            Some(crate::net::tls_config()?)
        } else {
            None
        };
        Ok(Target::new(
            self.cfg.url(stt),
            tls,
            self.cfg.speech_model.clone(),
        ))
    }
}

impl SttProvider for AssemblyAiStt {
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
            min_chunk: self.cfg.bytes_in(&config, MIN_CHUNK, Round::Up),
            max_chunk: self.cfg.bytes_in(&config, MAX_CHUNK, Round::Down),
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
            "assemblyai warm",
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
            with_default_port("streaming.assemblyai.com", 443),
            "streaming.assemblyai.com:443"
        );
        assert_eq!(with_default_port("127.0.0.1:9", 80), "127.0.0.1:9");
        assert_eq!(with_default_port("[::1]", 80), "[::1]:80");
        assert_eq!(with_default_port("[::1]:9", 80), "[::1]:9");
    }
}
