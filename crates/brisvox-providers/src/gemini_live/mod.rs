//! Gemini Live speech-to-speech adapter (facts and sources in
//! `docs/providers/gemini-live.md`, design in `docs/S2S.md`).
//!
//! One WebSocket per session (`BidiGenerateContent`), JSON messages both ways, caller audio as
//! base64 PCM16 (`audio/pcm;rate=<hz>`), model audio as base64 PCM16 at 24 kHz.
//! [`GeminiLive::connect`](S2sProvider::connect) opens the socket, sends one `setup` message
//! (model, voice, instructions, tools, activity detection, transcription) and waits for
//! `setupComplete`; a non-empty [`S2sConfig::history`] is then added with one `clientContent`
//! message. Every wait has the `io_timeout` deadline. A background task then pumps the three
//! channels of the [`S2sSession`] and the socket until either side closes.
//!
//! # How the neutral interface maps (differences from OpenAI Realtime)
//! - No response or item ids exist: the adapter mints `gr<n>` / `gr<n>-audio` (see
//!   `wire::Mapper`). A response ends at `turnComplete`, `interrupted` or a tool call.
//! - Provider turn detection: the server's automatic activity detection; `voiceActivity`
//!   messages (and, as a fallback, `interrupted`) become `SpeechStarted`, the start of the next
//!   response ends the speech (`SpeechStopped`).
//! - Manual turn detection: automatic detection is disabled; the adapter sends `activityStart`
//!   before the first audio of an utterance and `activityEnd` on [`crate::s2s::S2sControl::Commit`]. The
//!   server then answers by itself, so a following `CreateResponse` is absorbed.
//! - [`crate::s2s::S2sControl::CancelResponse`]: Gemini has no cancel; the response is ended locally
//!   (`ResponseDone(Cancelled)`, the rest of it dropped) and the next `activityStart` interrupts
//!   the server.
//! - [`crate::s2s::S2sControl::Truncate`] cannot be expressed (the server cuts its history where it stopped
//!   generating, not where the caller stopped listening). It is accepted and ignored; the engine's
//!   local mirror has the truth and re-seeds the next session.
//! - Tool results are batched per server message into one `toolResponse`; the server continues
//!   by itself, `CreateResponse` after it is absorbed.
//! - The connection lives about [`GeminiLiveConfig::connection_lifetime`] (10 minutes). The
//!   server announces its end with `goAway` ([`crate::s2s::S2sEvent::SessionEnding`]); the engine rotates to a
//!   fresh, re-seeded session while the bot is idle ([`S2sProvider::session_limit`]).
//!
//! # Bounds
//! Same as the OpenAI adapter: audio in [`S2S_AUDIO_CAPACITY`] frames, control
//! [`S2S_CONTROL_CAPACITY`], events [`S2S_EVENT_CAPACITY`]; a write over `io_timeout`, an event the
//! engine does not take within `io_timeout`, or a server message over `max_message_bytes` ends
//! the session. The API key travels in the `key` query parameter (the documented way) or, with
//! [`GeminiLiveConfig::key_in_header`], in a sensitive header; it is never logged and never part
//! of an error text.
//!
//! [`S2S_AUDIO_CAPACITY`]: crate::s2s::S2S_AUDIO_CAPACITY
//! [`S2S_CONTROL_CAPACITY`]: crate::s2s::S2S_CONTROL_CAPACITY
//! [`S2S_EVENT_CAPACITY`]: crate::s2s::S2S_EVENT_CAPACITY

mod config;
mod session;
mod wire;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{Clock, SharedClock};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

pub use config::{GeminiLiveConfig, Sensitivity};

use crate::error::{Error, Result};
use crate::net::connect_ws;
use crate::s2s::{
    S2S_AUDIO_CAPACITY, S2S_CONTROL_CAPACITY, S2S_EVENT_CAPACITY, S2sConfig, S2sProvider,
    S2sSession, S2sTurnDetection,
};
use wire::{Decoded, Mapper};

/// Provider name used in errors and logs.
pub(crate) const PROVIDER: &str = "gemini-live";

/// The client side of an open connection.
pub(crate) type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Gemini Live speech-to-speech.
#[derive(Debug, Clone)]
pub struct GeminiLive {
    cfg: Arc<GeminiLiveConfig>,
    clock: SharedClock,
}

impl GeminiLive {
    /// An adapter with `config`; all deadlines run on `clock`.
    pub fn new(config: GeminiLiveConfig, clock: SharedClock) -> Self {
        Self {
            cfg: Arc::new(config),
            clock,
        }
    }

    /// The settings in use.
    pub fn config(&self) -> &GeminiLiveConfig {
        &self.cfg
    }
}

/// Awaits `future` for at most `limit` on `clock`.
pub(crate) async fn timed<T>(
    clock: &dyn Clock,
    limit: Duration,
    what: &'static str,
    future: impl Future<Output = T>,
) -> std::result::Result<T, brisvox_core::Error> {
    tokio::select! {
        biased;
        out = future => Ok(out),
        () = clock.sleep(limit) => Err(brisvox_core::Error::Timeout { what, after: limit }),
    }
}

impl S2sProvider for GeminiLive {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    fn session_limit(&self) -> Option<Duration> {
        Some(self.cfg.connection_lifetime)
    }

    async fn connect(&self, config: S2sConfig) -> Result<S2sSession> {
        self.cfg.validate()?;
        let setup = wire::setup(&self.cfg, &config)?;
        let history = wire::history_message(&config.history)?;
        let mut ws = open(&self.cfg, &*self.clock).await?;
        timed(
            &*self.clock,
            self.cfg.io_timeout,
            "gemini-live setup",
            ws.send(Message::text(setup)),
        )
        .await?
        .map_err(|e| ws_error(&e))?;
        let mut mapper = Mapper::default();
        await_setup(&mut ws, &self.cfg, &*self.clock, &mut mapper).await?;
        if let Some(history) = history {
            // A reconnect or rotation: the conversation so far goes in before any caller audio.
            timed(
                &*self.clock,
                self.cfg.io_timeout,
                "gemini-live history",
                ws.send(Message::text(history)),
            )
            .await?
            .map_err(|e| ws_error(&e))?;
        }
        let (audio_tx, audio_rx) = mpsc::channel(S2S_AUDIO_CAPACITY);
        let (control_tx, control_rx) = mpsc::channel(S2S_CONTROL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel(S2S_EVENT_CAPACITY);
        // Ends when the caller drops the audio or control sender, or the event receiver.
        tokio::spawn(session::run(
            session::Params {
                cfg: Arc::clone(&self.cfg),
                clock: Arc::clone(&self.clock),
                manual: config.turn_detection == S2sTurnDetection::Manual,
                input_rate: config.input_rate,
            },
            ws,
            audio_rx,
            control_rx,
            event_tx,
            mapper,
        ));
        Ok(S2sSession {
            audio: audio_tx,
            control: control_tx,
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
            "gemini-live warm",
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

/// Opens the connection and completes the WebSocket handshake within `cfg.io_timeout`.
async fn open(cfg: &GeminiLiveConfig, clock: &dyn Clock) -> Result<Ws> {
    let (_, tls) = cfg.authority();
    let tls = if tls {
        Some(crate::net::tls_config()?)
    } else {
        None
    };
    // The URL holds the key (unless it goes in a header): it is never logged or put in an error.
    let mut request = cfg
        .url_with_key()
        .as_str()
        .into_client_request()
        .map_err(|e| ws_error(&e))?;
    if cfg.key_in_header {
        let mut key = HeaderValue::from_str(cfg.api_key.expose()).map_err(|_| Error::Protocol {
            provider: PROVIDER,
            detail: "the API key is not a valid HTTP header value".to_owned(),
        })?;
        key.set_sensitive(true);
        request.headers_mut().insert("x-goog-api-key", key);
    }
    let ws_config = WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(0)
        .max_message_size(Some(cfg.max_message_bytes))
        .max_frame_size(Some(cfg.max_message_bytes));
    let connector = tls.map(Connector::Rustls);
    // Boxed: the TLS/WebSocket handshake future is large; a connect is rare.
    let handshake = Box::pin(connect_ws(request, Some(ws_config), connector, clock));
    match timed(clock, cfg.io_timeout, "gemini-live connect", handshake).await? {
        Ok((ws, _response)) => Ok(ws),
        Err(e) => Err(ws_error(&e)),
    }
}

/// Maps a WebSocket failure to the crate error. Only status codes are logged: never the request
/// (which holds the key) and never a response body.
pub(crate) fn ws_error(error: &WsError) -> Error {
    match error {
        WsError::Http(response) => {
            tracing::warn!(
                status = response.status().as_u16(),
                "gemini-live refused the connection"
            );
            Error::Rejected {
                provider: PROVIDER,
                status: response.status().as_u16(),
            }
        }
        WsError::Io(io) => {
            tracing::debug!(kind = ?io.kind(), "gemini-live I/O error");
            Error::Disconnected { provider: PROVIDER }
        }
        WsError::ConnectionClosed | WsError::AlreadyClosed => {
            Error::Disconnected { provider: PROVIDER }
        }
        WsError::Tls(e) => Error::Protocol {
            provider: PROVIDER,
            detail: format!("TLS error: {e}"),
        },
        WsError::Url(_) => Error::Protocol {
            provider: PROVIDER,
            detail: "invalid URL".to_owned(),
        },
        other => Error::Protocol {
            provider: PROVIDER,
            detail: format!("websocket error: {other}"),
        },
    }
}

/// Reads until the server sent `setupComplete`, within `io_timeout` for the wait as a whole.
async fn await_setup(
    ws: &mut Ws,
    cfg: &GeminiLiveConfig,
    clock: &dyn Clock,
    mapper: &mut Mapper,
) -> Result<()> {
    let wait = async {
        loop {
            let payload: Option<Vec<u8>> = match ws.next().await {
                Some(Ok(Message::Text(text))) => Some(text.as_bytes().to_vec()),
                Some(Ok(Message::Binary(bytes))) => Some(bytes.to_vec()),
                Some(Ok(Message::Close(frame))) => {
                    return Err(match frame {
                        Some(frame) => Error::Rejected {
                            provider: PROVIDER,
                            status: u16::from(frame.code),
                        },
                        None => Error::Disconnected { provider: PROVIDER },
                    });
                }
                Some(Ok(_)) => None,
                Some(Err(e)) => return Err(ws_error(&e)),
                None => return Err(Error::Disconnected { provider: PROVIDER }),
            };
            let Some(payload) = payload else { continue };
            match mapper.decode(&payload)? {
                Decoded::SetupComplete => return Ok(()),
                Decoded::Error { code, message } => {
                    return Err(Error::Protocol {
                        provider: PROVIDER,
                        detail: format!("setup rejected: {code}: {message}"),
                    });
                }
                Decoded::Events(_) => {}
            }
        }
    };
    timed(clock, cfg.io_timeout, "gemini-live setup", wait).await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ports_are_appended_only_when_missing() {
        assert_eq!(
            with_default_port("generativelanguage.googleapis.com", 443),
            "generativelanguage.googleapis.com:443"
        );
        assert_eq!(with_default_port("127.0.0.1:9", 80), "127.0.0.1:9");
        assert_eq!(with_default_port("[::1]", 80), "[::1]:80");
    }
}
