//! The reference agent process in Rust: a WebSocket server that accepts the links of a brisvox
//! server, one per call, and speaks the protocol (`docs/AGENT_PROTOCOL.md`). The tests use it;
//! it is also the smallest working example of an agent (the Python and TypeScript SDKs follow
//! the same shape).
//!
//! ```no_run
//! # use brisvox_agent_link::client::AgentServer;
//! # use brisvox_agent_link::protocol::{AgentMessage, ServerBody, SessionAccept};
//! # async fn run(server: AgentServer) -> Result<(), brisvox_agent_link::client::ClientError> {
//! let mut session = server.accept().await?;
//! session.accept_session(SessionAccept { version: 1, ..SessionAccept::default() }).await?;
//! while let Some(message) = session.recv().await? {
//!     if let ServerBody::UserText { text, .. } = &message.body {
//!         let say = AgentMessage::Say { id: None, re: Some(message.seq), text: format!("You said {text}"), interrupt: false };
//!         session.send(&say).await?;
//!     }
//! }
//! # Ok(()) }
//! ```

use std::net::SocketAddr;
use std::time::Duration;

use brisvox_core::{Clock, SharedClock};
use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::protocol::{
    AgentMessage, MAX_MESSAGE_BYTES, PROTOCOL, ServerBody, ServerMessage, SessionAccept,
};

/// Default bound on every network step of the reference agent (for `recv`: on each frame; the
/// server pings every 10 s, so a live link never trips it).
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// What went wrong on the agent side.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
    /// Socket or WebSocket failure.
    #[error("agent I/O: {0}")]
    Io(String),
    /// A step did not finish within the I/O timeout.
    #[error("agent I/O timed out ({0})")]
    Timeout(&'static str),
    /// The server sent something that is not a protocol message.
    #[error("bad server message: {0}")]
    Protocol(String),
}

/// Listens for the links of a brisvox server.
#[derive(Debug)]
pub struct AgentServer {
    listener: TcpListener,
    clock: SharedClock,
    io_timeout: Duration,
}

impl AgentServer {
    /// Listens on `addr` (`127.0.0.1:0` picks a free port).
    ///
    /// # Errors
    /// [`ClientError::Io`] when the address cannot be bound.
    pub async fn bind(addr: SocketAddr, clock: SharedClock) -> Result<Self, ClientError> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| ClientError::Io(e.to_string()))?;
        Ok(Self {
            listener,
            clock,
            io_timeout: DEFAULT_IO_TIMEOUT,
        })
    }

    /// Bounds every network step by `timeout` instead of [`DEFAULT_IO_TIMEOUT`].
    #[must_use]
    pub const fn with_io_timeout(mut self, timeout: Duration) -> Self {
        self.io_timeout = timeout;
        self
    }

    /// The bound address.
    ///
    /// # Errors
    /// [`ClientError::Io`] when the socket has none.
    pub fn local_addr(&self) -> Result<SocketAddr, ClientError> {
        self.listener
            .local_addr()
            .map_err(|e| ClientError::Io(e.to_string()))
    }

    /// The `ws://` URL a brisvox server connects to.
    ///
    /// # Errors
    /// As [`Self::local_addr`].
    pub fn url(&self) -> Result<String, ClientError> {
        Ok(format!("ws://{}/agent", self.local_addr()?))
    }

    /// The next link (a call), after the WebSocket handshake.
    ///
    /// # Errors
    /// [`ClientError::Timeout`] when no server connects within the I/O timeout,
    /// [`ClientError::Io`] when the handshake fails.
    pub async fn accept(&self) -> Result<AgentSession, ClientError> {
        let accept = async {
            let (stream, _) = self
                .listener
                .accept()
                .await
                .map_err(|e| ClientError::Io(e.to_string()))?;
            stream
                .set_nodelay(true)
                .map_err(|e| ClientError::Io(e.to_string()))?;
            let config = WebSocketConfig::default()
                .max_message_size(Some(MAX_MESSAGE_BYTES))
                .max_frame_size(Some(MAX_MESSAGE_BYTES));
            tokio_tungstenite::accept_async_with_config(stream, Some(config))
                .await
                .map_err(|e| ClientError::Io(e.to_string()))
        };
        let ws = timed(&*self.clock, self.io_timeout, "accept", accept).await??;
        Ok(AgentSession {
            ws,
            clock: self.clock.clone(),
            io_timeout: self.io_timeout,
        })
    }
}

/// One call's link, on the agent side.
#[derive(Debug)]
pub struct AgentSession {
    ws: WebSocketStream<TcpStream>,
    clock: SharedClock,
    io_timeout: Duration,
}

impl AgentSession {
    /// The next protocol message; `None` once the server closed the link. Pings are answered
    /// on the way.
    ///
    /// # Errors
    /// [`ClientError::Timeout`] when no frame at all (not even the server's keepalive ping,
    /// every 10 s) arrives within the I/O timeout, [`ClientError::Protocol`] for a message that
    /// does not parse.
    pub async fn recv(&mut self) -> Result<Option<ServerMessage>, ClientError> {
        let clock = self.clock.clone();
        loop {
            // The bound is per frame: a quiet but live call keeps the session.
            match timed(&*clock, self.io_timeout, "recv", self.ws.next()).await? {
                Some(Ok(WsMessage::Text(text))) => {
                    return serde_json::from_str(text.as_str())
                        .map(Some)
                        .map_err(|e| ClientError::Protocol(e.to_string()));
                }
                Some(Ok(WsMessage::Close(_))) | None => return Ok(None),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(ClientError::Io(e.to_string())),
            }
        }
    }

    /// The next message whose body matches `want`, skipping the others.
    ///
    /// # Errors
    /// As [`Self::recv`]; [`ClientError::Protocol`] when the link closes first.
    pub async fn recv_until(
        &mut self,
        want: impl Fn(&ServerBody) -> bool,
    ) -> Result<ServerMessage, ClientError> {
        loop {
            match self.recv().await? {
                Some(message) if want(&message.body) => return Ok(message),
                Some(_) => {}
                None => {
                    return Err(ClientError::Protocol(
                        "the link closed before the expected message".to_owned(),
                    ));
                }
            }
        }
    }

    /// Sends `message`.
    ///
    /// # Errors
    /// [`ClientError::Io`] or [`ClientError::Timeout`].
    pub async fn send(&mut self, message: &AgentMessage) -> Result<(), ClientError> {
        let text =
            serde_json::to_string(message).map_err(|e| ClientError::Protocol(e.to_string()))?;
        self.send_raw(&text).await
    }

    /// Sends `text` as it is (for tests of malformed messages).
    ///
    /// # Errors
    /// [`ClientError::Io`] or [`ClientError::Timeout`].
    pub async fn send_raw(&mut self, text: &str) -> Result<(), ClientError> {
        let clock = self.clock.clone();
        timed(
            &*clock,
            self.io_timeout,
            "send",
            self.ws.send(WsMessage::text(text.to_owned())),
        )
        .await?
        .map_err(|e| ClientError::Io(e.to_string()))
    }

    /// Waits for `session.start`, checks it, and answers with `accept`. Returns the start.
    ///
    /// # Errors
    /// [`ClientError::Protocol`] when the first message is not a `session.start` of this
    /// protocol offering `accept.version`; the I/O errors of [`Self::recv`] and [`Self::send`].
    pub async fn accept_session(
        &mut self,
        accept: SessionAccept,
    ) -> Result<ServerMessage, ClientError> {
        let start = self
            .recv()
            .await?
            .ok_or_else(|| ClientError::Protocol("closed before session.start".to_owned()))?;
        match &start.body {
            ServerBody::SessionStart {
                protocol, versions, ..
            } if protocol == PROTOCOL && versions.contains(&accept.version) => {}
            other => {
                return Err(ClientError::Protocol(format!(
                    "expected a session.start offering version {}, got {other:?}",
                    accept.version
                )));
            }
        }
        self.send(&AgentMessage::SessionAccept(accept)).await?;
        Ok(start)
    }

    /// Closes the link.
    ///
    /// # Errors
    /// [`ClientError::Io`] or [`ClientError::Timeout`].
    pub async fn close(mut self) -> Result<(), ClientError> {
        let clock = self.clock.clone();
        timed(&*clock, self.io_timeout, "close", self.ws.close(None))
            .await?
            .map_err(|e| ClientError::Io(e.to_string()))
    }
}

async fn timed<F: std::future::Future>(
    clock: &dyn Clock,
    timeout: Duration,
    what: &'static str,
    future: F,
) -> Result<F::Output, ClientError> {
    tokio::select! {
        biased;
        output = future => Ok(output),
        () = clock.sleep(timeout) => Err(ClientError::Timeout(what)),
    }
}
