//! OpenAI Realtime speech-to-speech adapter (facts and sources in
//! `docs/providers/openai-realtime.md`, design in `docs/S2S.md`).
//!
//! One WebSocket per session (`wss://api.openai.com/v1/realtime?model=...`), JSON events both
//! ways, audio as base64 PCM16 at 24 kHz. [`OpenAiRealtime::connect`](S2sProvider::connect)
//! opens the socket, waits for `session.created`, sends one `session.update` (instructions,
//! tools, voice, formats, turn detection) and waits for `session.updated`; every wait has the
//! `io_timeout` deadline. A background task then pumps the three channels of the
//! [`S2sSession`] and the socket until either side closes.
//!
//! # What the adapter does and does not do
//! - It maps provider events to the neutral [`S2sEvent`](crate::s2s::S2sEvent)s. Tool calls are delivered **once** per
//!   `call_id`, whether the server reports them as `response.function_call_arguments.done` or only
//!   inside `response.done`.
//! - It never decides anything about turns: with [`S2sTurnDetection::Provider`](crate::s2s::S2sTurnDetection::Provider) the server's VAD
//!   runs (`create_response` and `interrupt_response` on), with `Manual` it is off and the engine
//!   commits and creates responses.
//! - It does not truncate on its own: on a WebSocket the server cannot know what the caller
//!   heard, so the engine sends [`S2sControl::Truncate`](crate::s2s::S2sControl::Truncate) with the played duration.
//! - It does not reconnect: a lost socket ends the event stream with an [`S2sEvent::Error`](crate::s2s::S2sEvent::Error) and
//!   the engine decides (the server-side history of the lost session cannot be recovered).
//! - Errors from the server that answer our own `response.cancel` or `conversation.item.truncate`
//!   (the response or item was already gone) are expected races: logged at debug level, not
//!   reported. Every other `error` event is delivered as [`S2sEvent::Warning`](crate::s2s::S2sEvent::Warning).
//!
//! # Bounds
//! Audio in: [`S2S_AUDIO_CAPACITY`] frames; control: [`S2S_CONTROL_CAPACITY`]; events out:
//! [`S2S_EVENT_CAPACITY`]. A write that takes longer than `io_timeout`, an event the engine does
//! not take within `io_timeout`, or a server message over `max_message_bytes` ends the session
//! with an error. The API key only appears in the `Authorization` header (marked sensitive) and
//! never in a URL, log or error text.
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
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

pub use config::{Eagerness, OpenAiRealtimeConfig, ServerVad};

use crate::error::{Error, Result};
use crate::net::connect_ws;
use crate::s2s::{
    S2S_AUDIO_CAPACITY, S2S_CONTROL_CAPACITY, S2S_EVENT_CAPACITY, S2sConfig, S2sProvider,
    S2sSession,
};
use wire::Decoded;

/// Provider name used in errors and logs.
pub(crate) const PROVIDER: &str = "openai-realtime";

/// The client side of an open connection.
pub(crate) type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// OpenAI Realtime speech-to-speech.
#[derive(Debug, Clone)]
pub struct OpenAiRealtime {
    cfg: Arc<OpenAiRealtimeConfig>,
    clock: SharedClock,
}

impl OpenAiRealtime {
    /// An adapter with `config`; all deadlines run on `clock`.
    pub fn new(config: OpenAiRealtimeConfig, clock: SharedClock) -> Self {
        Self {
            cfg: Arc::new(config),
            clock,
        }
    }

    /// The settings in use.
    pub fn config(&self) -> &OpenAiRealtimeConfig {
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

impl S2sProvider for OpenAiRealtime {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    async fn connect(&self, config: S2sConfig) -> Result<S2sSession> {
        self.cfg.validate()?;
        let update = wire::session_update(&self.cfg, &config)?;
        let seed = wire::history_events(&self.cfg, &config.history)?;
        let mut ws = open(&self.cfg, &*self.clock).await?;
        await_setup(&mut ws, &self.cfg, &*self.clock, Setup::Created).await?;
        timed(
            &*self.clock,
            self.cfg.io_timeout,
            "openai-realtime session.update",
            ws.send(Message::text(update)),
        )
        .await?
        .map_err(|e| ws_error(&e))?;
        await_setup(&mut ws, &self.cfg, &*self.clock, Setup::Updated).await?;
        // A reconnect or rotation: the conversation so far goes in before any caller audio.
        // Rejections come back as `error` events (Warning); the session still works.
        for text in seed {
            timed(
                &*self.clock,
                self.cfg.io_timeout,
                "openai-realtime history item",
                ws.send(Message::text(text)),
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
            },
            ws,
            audio_rx,
            control_rx,
            event_tx,
        ));
        Ok(S2sSession {
            audio: audio_tx,
            control: control_tx,
            events: event_rx,
        })
    }

    fn session_limit(&self) -> Option<Duration> {
        Some(self.cfg.max_session)
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
            "openai-realtime warm",
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
async fn open(cfg: &OpenAiRealtimeConfig, clock: &dyn Clock) -> Result<Ws> {
    let (_, tls) = cfg.authority();
    let tls = if tls {
        Some(crate::net::tls_config()?)
    } else {
        None
    };
    let mut request = cfg
        .url()
        .as_str()
        .into_client_request()
        .map_err(|e| ws_error(&e))?;
    let mut auth =
        HeaderValue::from_str(&format!("Bearer {}", cfg.api_key.expose())).map_err(|_| {
            Error::Protocol {
                provider: PROVIDER,
                detail: "the API key is not a valid HTTP header value".to_owned(),
            }
        })?;
    auth.set_sensitive(true);
    request.headers_mut().insert(AUTHORIZATION, auth);
    if let Some(id) = &cfg.safety_identifier {
        let value = HeaderValue::from_str(id).map_err(|_| Error::Protocol {
            provider: PROVIDER,
            detail: "the safety identifier is not a valid HTTP header value".to_owned(),
        })?;
        request
            .headers_mut()
            .insert("OpenAI-Safety-Identifier", value);
    }
    let ws_config = WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(0)
        .max_message_size(Some(cfg.max_message_bytes))
        .max_frame_size(Some(cfg.max_message_bytes));
    let connector = tls.map(Connector::Rustls);
    // Boxed: the TLS/WebSocket handshake future is large; a connect is rare.
    let handshake = Box::pin(connect_ws(request, Some(ws_config), connector, clock));
    match timed(clock, cfg.io_timeout, "openai-realtime connect", handshake).await? {
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
                "openai-realtime refused the connection"
            );
            Error::Rejected {
                provider: PROVIDER,
                status: response.status().as_u16(),
            }
        }
        WsError::Io(io) => {
            tracing::debug!(kind = ?io.kind(), "openai-realtime I/O error");
            Error::Disconnected { provider: PROVIDER }
        }
        WsError::ConnectionClosed | WsError::AlreadyClosed => {
            Error::Disconnected { provider: PROVIDER }
        }
        WsError::Tls(e) => Error::Protocol {
            provider: PROVIDER,
            detail: format!("TLS error: {e}"),
        },
        WsError::Url(e) => Error::Protocol {
            provider: PROVIDER,
            detail: format!("invalid URL: {e}"),
        },
        other => Error::Protocol {
            provider: PROVIDER,
            detail: format!("websocket error: {other}"),
        },
    }
}

/// Which setup answer to wait for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Setup {
    Created,
    Updated,
}

/// Reads until the server sent the setup answer `want`, within `io_timeout` for the wait as a
/// whole.
async fn await_setup(
    ws: &mut Ws,
    cfg: &OpenAiRealtimeConfig,
    clock: &dyn Clock,
    want: Setup,
) -> Result<()> {
    let mut decoder = wire::Decoder::default();
    let wait = async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => match decoder.decode(text.as_str())? {
                    Decoded::SessionCreated if want == Setup::Created => return Ok(()),
                    Decoded::SessionUpdated if want == Setup::Updated => return Ok(()),
                    Decoded::Error { code, message, .. } => {
                        return Err(Error::Protocol {
                            provider: PROVIDER,
                            detail: format!("setup rejected: {code}: {message}"),
                        });
                    }
                    _ => {}
                },
                Some(Ok(Message::Close(frame))) => {
                    return Err(match frame {
                        Some(frame) => Error::Rejected {
                            provider: PROVIDER,
                            status: u16::from(frame.code),
                        },
                        None => Error::Disconnected { provider: PROVIDER },
                    });
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(ws_error(&e)),
                None => return Err(Error::Disconnected { provider: PROVIDER }),
            }
        }
    };
    timed(clock, cfg.io_timeout, "openai-realtime setup", wait).await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ports_are_appended_only_when_missing() {
        assert_eq!(
            with_default_port("api.openai.com", 443),
            "api.openai.com:443"
        );
        assert_eq!(with_default_port("127.0.0.1:9", 80), "127.0.0.1:9");
        assert_eq!(with_default_port("[::1]", 80), "[::1]:80");
    }
}
