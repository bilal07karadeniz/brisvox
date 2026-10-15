//! The server side of the agent protocol: one [`AgentLink`] per call (see the crate docs and
//! `docs/AGENT_PROTOCOL.md`).

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::fmt;
use std::future::Future;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use brisvox_agent::context::Message;
use brisvox_agent::telephony::{
    PhoneNumber, TelephonyErrorKind, TelephonyOps, TransferMode, TransferRequest,
};
use brisvox_agent::tools::{SessionId, ToolEngine, ToolKind, ToolRegistry, ToolSpec};
use brisvox_core::{Clock, Secret, SharedClock};
use brisvox_engine::agent_input::{MAX_EDIT_TEXT_BYTES, MAX_LINE_BYTES};
use brisvox_engine::{
    BotLine, ConversationEdit, EditSender, EngineEvent, EngineEvents, EventStream, InputError,
    SaySender, SessionBuilder, TaskGuard, conversation_edits, say_input,
};
use brisvox_providers::{LlmProvider, SttProvider, TtsProvider};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

use crate::protocol::{
    AGENT_TYPES, AgentMessage, CallIdentity, CallSummary, Capabilities, EVENT_TYPES, ErrorCode,
    MAX_COMMAND_ID_BYTES, MAX_CONTEXT_MESSAGES, MAX_MESSAGE_BYTES, MAX_NOTE_BYTES, MAX_TOOLS,
    Metrics, PROTOCOL, Probe, Resume, SUPPORTED_VERSIONS, ServerBody, ServerMessage, SessionAccept,
    TokenUsage, ToolDecl, WireError, WireMessage, WireRole, WireToolKind, WireTransferMode,
};
use crate::security;
use crate::tool::RemoteTool;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Bot lines the link may have queued in the call at once.
const SAY_CAPACITY: usize = 16;
/// Conversation edits the link may have queued in the call at once.
const EDIT_CAPACITY: usize = 64;
/// Operations (`end`, `transfer`) waiting for the operations task.
const OPS_CAPACITY: usize = 4;
/// Events remembered for the hop latency (by `seq`).
const HOP_RING: usize = 256;
/// Longest detail text of an error the server sends, in bytes.
const MAX_DETAIL_BYTES: usize = 256;
/// Largest `reconnect_attempts`.
const MAX_RECONNECT_ATTEMPTS: u32 = 50;
/// A connection that lived this long counts as healthy: the next loss starts a fresh series of
/// reconnection attempts.
const STABLE_AFTER: Duration = Duration::from_secs(5);

/// The settings of the agent links of a server (one agent process URL for every call of the
/// agent definition).
#[derive(Clone)]
pub struct AgentLinkConfig {
    /// Where the agent process listens: `ws://host:port/path` or `wss://...`.
    pub url: String,
    /// Sent as `Authorization: Bearer <token>` in the WebSocket handshake.
    pub auth_token: Option<Secret<String>>,
    /// Accept a plaintext `ws://` URL whose host is not on a private network
    /// ([`security::host_is_private`]). Off by default: the link carries transcripts, the LLM
    /// context and tool calls, so a `ws://` URL to a public host is refused by
    /// [`Self::validate`]. `BRISVOX_AGENT_ALLOW_INSECURE=1` /
    /// `[agent_link] allow_insecure = true`. Default `false`.
    pub allow_insecure: bool,
    /// The agent definition (or route) opted in to receive who is on the line in
    /// `session.start` (`[agent_link] caller_identity = true`, or
    /// `BRISVOX_AGENT_CALLER_IDENTITY=1` for the default process). The link itself sends only
    /// what [`LinkCall::identity`] holds; the server reads this flag to decide whether to build
    /// it. Default `false`.
    pub caller_identity: bool,
    /// The TLS client config of `wss://` URLs; `None`: the process-wide one (webpki roots).
    /// Set by `BRISVOX_AGENT_TLS_CA` to trust a private CA besides the public roots.
    pub tls: Option<Arc<rustls::ClientConfig>>,
    /// Bound on the TCP/TLS/WebSocket handshake. Default 2 s.
    pub connect_timeout: Duration,
    /// Bound on the `session.start` write and the agent's `session.accept` together, counted
    /// from the connection: a call waits at most `connect_timeout + handshake_timeout` for its
    /// agent. Default 2 s.
    pub handshake_timeout: Duration,
    /// Bound on one WebSocket write; a slower write takes the link down. Default 2 s.
    pub write_timeout: Duration,
    /// Interval of the server's WebSocket pings; the link is down when nothing arrived for three
    /// intervals. Default 10 s.
    pub keepalive: Duration,
    /// Bound on the closing `session.end` and close frame. Default 1 s.
    pub close_timeout: Duration,
    /// Events queued for the writer; a newer event is dropped when it is full (counted, then
    /// reported as `events.lost`). Default 256.
    pub event_queue: usize,
    /// Tool calls, cancels and command results queued for the writer; a tool call fails at once
    /// when it is full. Default 64.
    pub control_queue: usize,
    /// Remote tool calls waiting for their result at once; more fail at once. Default 32.
    pub max_inflight_tools: usize,
    /// Timeout of a remote tool that declares none. Default 5 s.
    pub default_tool_timeout: Duration,
    /// Largest timeout a remote tool may declare. Default 30 s.
    pub max_tool_timeout: Duration,
    /// How long a `side_effect` remote call is waited for after its slot timed out (the result
    /// then becomes a late note); afterwards its outcome is unknown. Must stay under the tool
    /// engine's 120 s backstop. Default 60 s.
    pub side_effect_wait: Duration,
    /// Bound on each call-control operation of `transfer`. Default 10 s.
    pub ops_timeout: Duration,
    /// Longest an `end` waits for the bot to finish speaking (its farewell included) before
    /// the call is ended anyway. Default 30 s.
    pub end_grace: Duration,
    /// How often a lost link is reopened before the call goes on without its agent for good
    /// (counted over consecutive failures: a connection that lived at least 5 s starts the
    /// count again). Only agents that declared `resume` in `session.accept` are reconnected;
    /// `0` turns reconnecting off. Default 5.
    pub reconnect_attempts: u32,
    /// The wait before the first reconnection attempt; it doubles for each further one (with
    /// jitter, so the calls of a crashed agent do not return at the same instant). Default
    /// 100 ms.
    pub reconnect_backoff: Duration,
    /// The longest wait between two attempts. Default 2 s.
    pub reconnect_backoff_max: Duration,
    /// Reconnecting stops this long after the link was lost, whatever is left of the attempts.
    /// Default 15 s.
    pub reconnect_window: Duration,
}

impl fmt::Debug for AgentLinkConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentLinkConfig")
            .field("url", &security::redact_url(&self.url))
            .field("auth_token", &self.auth_token)
            .field("allow_insecure", &self.allow_insecure)
            .field("private_ca", &self.tls.is_some())
            .field("connect_timeout", &self.connect_timeout)
            .field("handshake_timeout", &self.handshake_timeout)
            .finish_non_exhaustive()
    }
}

impl AgentLinkConfig {
    /// The defaults for the agent process at `url`.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            auth_token: None,
            allow_insecure: false,
            caller_identity: false,
            tls: None,
            connect_timeout: Duration::from_secs(2),
            handshake_timeout: Duration::from_secs(2),
            write_timeout: Duration::from_secs(2),
            keepalive: Duration::from_secs(10),
            close_timeout: Duration::from_secs(1),
            event_queue: 256,
            control_queue: 64,
            max_inflight_tools: 32,
            default_tool_timeout: Duration::from_secs(5),
            max_tool_timeout: Duration::from_secs(30),
            side_effect_wait: Duration::from_secs(60),
            ops_timeout: Duration::from_secs(10),
            end_grace: Duration::from_secs(30),
            reconnect_attempts: 5,
            reconnect_backoff: Duration::from_millis(100),
            reconnect_backoff_max: Duration::from_secs(2),
            reconnect_window: Duration::from_secs(15),
        }
    }

    /// These settings for another agent process: `url` and `auth_token` replaced, everything
    /// else (bounds, queues, reconnecting) kept. Used for per-call routing, where each route
    /// brings its own credentials: the token of one agent is never sent to another.
    #[must_use]
    pub fn with_endpoint(
        &self,
        url: impl Into<String>,
        auth_token: Option<Secret<String>>,
    ) -> Self {
        Self {
            url: url.into(),
            auth_token,
            ..self.clone()
        }
    }

    /// Checks the settings.
    ///
    /// # Errors
    /// [`LinkError::Config`] for a URL that is not `ws://` or `wss://`, a `ws://` URL to a host
    /// outside the private networks while `allow_insecure` is off, a zero duration or queue,
    /// a side-effect wait of 110 s or more (the engine's backstop is 120 s), or a
    /// maximum tool timeout under the default.
    pub fn validate(&self) -> Result<(), LinkError> {
        if !(self.url.starts_with("ws://") || self.url.starts_with("wss://")) {
            return Err(LinkError::Config(format!(
                "the agent URL must start with ws:// or wss:// (got {:?})",
                security::redact_url(&self.url)
            )));
        }
        if self.url.starts_with("ws://")
            && !self.allow_insecure
            && !security::host_is_private(&self.url)
        {
            return Err(LinkError::Config(format!(
                "the agent URL {:?} is plaintext ws:// to a host outside the private networks: \
                 transcripts, the LLM context and tool calls would cross the network in clear. \
                 Use wss://, or set BRISVOX_AGENT_ALLOW_INSECURE=1 (allow_insecure = true) if \
                 the path is protected another way (VPN, service mesh)",
                security::redact_url(&self.url)
            )));
        }
        let durations = [
            ("connect_timeout", self.connect_timeout),
            ("handshake_timeout", self.handshake_timeout),
            ("write_timeout", self.write_timeout),
            ("keepalive", self.keepalive),
            ("close_timeout", self.close_timeout),
            ("default_tool_timeout", self.default_tool_timeout),
            ("side_effect_wait", self.side_effect_wait),
            ("ops_timeout", self.ops_timeout),
            ("end_grace", self.end_grace),
            ("reconnect_backoff", self.reconnect_backoff),
            ("reconnect_window", self.reconnect_window),
        ];
        for (name, value) in durations {
            if value.is_zero() {
                return Err(LinkError::Config(format!("{name} must not be zero")));
            }
        }
        for (name, value) in [
            ("event_queue", self.event_queue),
            ("control_queue", self.control_queue),
            ("max_inflight_tools", self.max_inflight_tools),
        ] {
            if value == 0 || value > 65_536 {
                return Err(LinkError::Config(format!("{name} must be 1..=65536")));
            }
        }
        if self.side_effect_wait >= Duration::from_secs(110) {
            return Err(LinkError::Config(
                "side_effect_wait must stay under 110 s (the tool engine's backstop is 120 s)"
                    .to_owned(),
            ));
        }
        if self.max_tool_timeout < self.default_tool_timeout {
            return Err(LinkError::Config(
                "max_tool_timeout must not be under default_tool_timeout".to_owned(),
            ));
        }
        if self.reconnect_attempts > MAX_RECONNECT_ATTEMPTS {
            return Err(LinkError::Config(format!(
                "reconnect_attempts must be at most {MAX_RECONNECT_ATTEMPTS}"
            )));
        }
        if self.reconnect_backoff_max < self.reconnect_backoff {
            return Err(LinkError::Config(
                "reconnect_backoff_max must not be under reconnect_backoff".to_owned(),
            ));
        }
        Ok(())
    }

    /// The configuration from the environment, read through `get`: `BRISVOX_AGENT_URL` (unset
    /// or empty: no agent link), `BRISVOX_AGENT_TOKEN`, `BRISVOX_AGENT_CONNECT_TIMEOUT_MS`,
    /// `BRISVOX_AGENT_HANDSHAKE_TIMEOUT_MS`, `BRISVOX_AGENT_RECONNECT_ATTEMPTS`,
    /// `BRISVOX_AGENT_RECONNECT_WINDOW_MS`, `BRISVOX_AGENT_ALLOW_INSECURE`, `BRISVOX_AGENT_TLS_CA` (`docs/AGENT_PROTOCOL.md`, "Server configuration").
    ///
    /// # Errors
    /// [`LinkError::Config`] for a value that does not parse or fails [`Self::validate`].
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, LinkError> {
        let Some(url) = get("BRISVOX_AGENT_URL").filter(|url| !url.trim().is_empty()) else {
            return Ok(None);
        };
        Self::from_env_for(get, url.trim()).map(Some)
    }

    /// [`Self::from_env`] for an agent process at `url` (given by the caller, for example from
    /// the agent definition file): the token and the tunables come from the environment.
    ///
    /// # Errors
    /// As [`Self::from_env`].
    pub fn from_env_for(
        get: impl Fn(&str) -> Option<String>,
        url: &str,
    ) -> Result<Self, LinkError> {
        Self::from_env_for_with(get, url, false)
    }

    /// [`Self::from_env_for`] where `allow_insecure` (the definition's or the route's own
    /// `allow_insecure = true`) is set besides `BRISVOX_AGENT_ALLOW_INSECURE`. Logs a warning
    /// when a plaintext `ws://` URL to a non-private host is accepted this way.
    ///
    /// # Errors
    /// As [`Self::from_env`].
    pub fn from_env_for_with(
        get: impl Fn(&str) -> Option<String>,
        url: &str,
        allow_insecure: bool,
    ) -> Result<Self, LinkError> {
        let mut config = Self::new(url);
        config.allow_insecure = allow_insecure || env_flag(get("BRISVOX_AGENT_ALLOW_INSECURE"));
        if let Some(path) = get("BRISVOX_AGENT_TLS_CA").filter(|p| !p.trim().is_empty()) {
            config.tls = Some(load_private_ca(path.trim())?);
        }
        config.auth_token = get("BRISVOX_AGENT_TOKEN")
            .filter(|token| !token.is_empty())
            .map(Secret::new);
        let millis = |name: &str| -> Result<Option<Duration>, LinkError> {
            get(name)
                .map(|value| {
                    value
                        .trim()
                        .parse::<u64>()
                        .map(Duration::from_millis)
                        .map_err(|_| LinkError::Config(format!("{name}: not a number: {value:?}")))
                })
                .transpose()
        };
        if let Some(value) = millis("BRISVOX_AGENT_CONNECT_TIMEOUT_MS")? {
            config.connect_timeout = value;
        }
        if let Some(value) = millis("BRISVOX_AGENT_HANDSHAKE_TIMEOUT_MS")? {
            config.handshake_timeout = value;
        }
        if let Some(value) = millis("BRISVOX_AGENT_RECONNECT_WINDOW_MS")? {
            config.reconnect_window = value;
        }
        if let Some(value) = get("BRISVOX_AGENT_RECONNECT_ATTEMPTS") {
            config.reconnect_attempts = value.trim().parse().map_err(|_| {
                LinkError::Config(format!(
                    "BRISVOX_AGENT_RECONNECT_ATTEMPTS: not a number: {value:?}"
                ))
            })?;
        }
        config.validate()?;
        if config.allow_insecure
            && config.url.starts_with("ws://")
            && !security::host_is_private(&config.url)
        {
            tracing::warn!(
                url = %security::redact_url(&config.url),
                "agent link: plaintext ws:// to a host outside the private networks is allowed \
                 (allow_insecure): transcripts, the LLM context and tool calls are sent in clear"
            );
        }
        Ok(config)
    }
}

/// Whether an environment flag is on (`1`, `true`, `yes`, `on`, any case).
fn env_flag(value: Option<String>) -> bool {
    value.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Largest CA bundle file read, in bytes.
const MAX_CA_FILE_BYTES: u64 = 256 * 1024;

/// The TLS client config that trusts the PEM bundle at `path` besides the public roots
/// (`BRISVOX_AGENT_TLS_CA`). Read once, at configuration time.
fn load_private_ca(path: &str) -> Result<Arc<rustls::ClientConfig>, LinkError> {
    use std::io::Read;

    let file = std::fs::File::open(path)
        .map_err(|e| LinkError::Config(format!("BRISVOX_AGENT_TLS_CA {path:?}: {e}")))?;
    let mut pem = Vec::new();
    file.take(MAX_CA_FILE_BYTES + 1)
        .read_to_end(&mut pem)
        .map_err(|e| LinkError::Config(format!("BRISVOX_AGENT_TLS_CA {path:?}: {e}")))?;
    if pem.len() as u64 > MAX_CA_FILE_BYTES {
        return Err(LinkError::Config(format!(
            "BRISVOX_AGENT_TLS_CA {path:?}: larger than {MAX_CA_FILE_BYTES} bytes"
        )));
    }
    brisvox_providers::net::tls_config_with_ca_pem(&pem)
        .map_err(|e| LinkError::Config(format!("BRISVOX_AGENT_TLS_CA {path:?}: {e}")))
}

/// Why a link could not be opened.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LinkError {
    /// The configuration is not usable.
    #[error("agent link configuration: {0}")]
    Config(String),
    /// The connection or the WebSocket handshake failed.
    #[error("cannot connect to the agent: {0}")]
    Connect(String),
    /// A step did not finish in time.
    #[error("the agent did not answer in time ({0})")]
    Timeout(&'static str),
    /// The agent answered `session.reject`.
    #[error("the agent rejected the call ({code}): {message}")]
    Rejected {
        /// The agent's code.
        code: String,
        /// The agent's explanation.
        message: String,
    },
    /// The agent broke the protocol (a bad first message, an unsupported version).
    #[error("agent protocol: {0}")]
    Protocol(String),
}

/// The call an [`AgentLink`] serves.
#[derive(Clone)]
pub struct LinkCall {
    /// The server's id of the call (also the tool engine's session id).
    pub session_id: u64,
    /// The agent definition's name.
    pub agent: String,
    /// The call's clock.
    pub clock: SharedClock,
    /// The call's token: cancelling it ends the link.
    pub cancel: CancellationToken,
    /// The carrier call-control bound to the call, for `transfer` (none: `unsupported`).
    pub ops: Option<Arc<dyn TelephonyOps>>,
    /// Who is on the line, sent in `session.start` (and again by a resumption). The caller of
    /// [`attach`] decides: the opt-in and the redaction rules are the server's. `None`: the
    /// message carries no `call` object.
    pub identity: Option<CallIdentity>,
}

impl fmt::Debug for LinkCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinkCall")
            .field("session_id", &self.session_id)
            .field("agent", &self.agent)
            .field("ops", &self.ops.is_some())
            // The numbers are personal data: never printed.
            .field("identity", &self.identity.is_some())
            .finish_non_exhaustive()
    }
}

/// Number of hop latency buckets.
pub const HOP_BUCKET_COUNT: usize = 16;

/// Upper bounds of the hop latency buckets, in microseconds (the last one is open).
pub const HOP_BUCKETS_US: [u64; HOP_BUCKET_COUNT] = [
    50,
    100,
    200,
    300,
    500,
    750,
    1_000,
    1_500,
    2_000,
    3_000,
    5_000,
    10_000,
    20_000,
    50_000,
    100_000,
    u64::MAX,
];

impl LinkStats {
    /// The hop latency at quantile `q` (0 to 1), as the upper bound of its bucket; `None`
    /// without samples or when it falls in the open last bucket.
    pub fn hop_quantile(&self, q: f64) -> Option<Duration> {
        let total: u64 = self.hop_buckets.iter().sum();
        if total == 0 {
            return None;
        }
        // The rank of the sample at `q` (1-based, at least 1).
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let rank = ((q.clamp(0.0, 1.0) * total as f64).ceil() as u64).max(1);
        let mut seen = 0u64;
        for (count, bound) in self.hop_buckets.iter().zip(HOP_BUCKETS_US) {
            seen = seen.saturating_add(*count);
            if seen >= rank {
                return (bound != u64::MAX).then(|| Duration::from_micros(bound));
            }
        }
        None
    }

    /// The mean hop latency; `None` without samples.
    pub fn hop_mean(&self) -> Option<Duration> {
        self.hop_total_us
            .checked_div(self.hops)
            .map(Duration::from_micros)
    }
}

macro_rules! counters {
    ($($(#[$doc:meta])* $name:ident),* $(,)?) => {
        #[derive(Debug, Default)]
        struct Counters {
            $($name: AtomicU64,)*
            hop_buckets: [AtomicU64; HOP_BUCKET_COUNT],
        }

        /// A copy of an [`AgentLink`]'s counters.
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct LinkStats {
            $($(#[$doc])* pub $name: u64,)*
            /// Hop samples per bucket: `hop_buckets[i]` counts the hops of at most
            /// [`HOP_BUCKETS_US`]`[i]` microseconds (and more than the bucket before).
            pub hop_buckets: [u64; HOP_BUCKET_COUNT],
            /// The link is up (neither failed nor closed).
            pub up: bool,
        }

        impl Counters {
            fn snapshot(&self, up: bool) -> LinkStats {
                LinkStats {
                    $($name: self.$name.load(Ordering::Relaxed),)*
                    hop_buckets: std::array::from_fn(|i| {
                        self.hop_buckets
                            .get(i)
                            .map_or(0, |bucket| bucket.load(Ordering::Relaxed))
                    }),
                    up,
                }
            }
        }
    };
}

counters! {
    /// Messages written to the agent (events, tool calls, results, the handshake).
    messages_sent,
    /// Events dropped: the event queue was full or the server fell behind the call.
    events_lost,
    /// Commands received (`say`, `end`, `transfer`, `context.update`, `tools.set`).
    commands,
    /// Commands and messages answered with an error.
    command_errors,
    /// Command results that could not be queued (the control queue was full).
    results_dropped,
    /// `tool.call`s sent.
    tool_calls,
    /// `tool.result`s that answered a waiting call.
    tool_results,
    /// `tool.result`s for no waiting call (late, cancelled, or unknown).
    unknown_results,
    /// Remote calls that got no result within their wait.
    tool_timeouts,
    /// `tool.cancel`s sent.
    tool_cancels,
    /// Remote calls refused at once (link down or busy, too many in flight).
    tool_refused,
    /// Connections opened again after a loss (`session.start` with `resume`, accepted).
    reconnects,
    /// Attempts to open the link again (also the ones that failed).
    reconnect_attempts,
    /// Hop samples: commands whose `re` named an event still remembered.
    hops,
    /// Sum of the hop samples, in microseconds (divide by `hops` for the mean).
    hop_total_us,
    /// Largest hop sample, in microseconds.
    hop_max_us,
}

/// What the bot's playout looks like, from the events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Speaking {
    /// The newest epoch whose audio started.
    newest: Option<u32>,
    /// Audio of `newest` is playing.
    now: bool,
}

/// Where the link stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkState {
    /// Connected: events flow, commands are applied, remote tools can be called.
    Up,
    /// The connection was lost; the server is reopening it (`docs/AGENT_PROTOCOL.md`,
    /// "Reconnecting"). Remote tools fail at once, events are not delivered.
    Reconnecting,
    /// Failed for good, or closed: nothing more is delivered.
    Down,
}

/// The state of the link and how often it came up: a resumption that completed between two
/// looks at a `watch` is still noticed (the generation moved).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Status {
    state: LinkState,
    /// 0 for the first connection, one more for each resumption.
    generation: u32,
}

/// A reply to a remote tool call: the result, or the agent's error text.
pub(crate) type ToolReply = Result<Value, String>;

enum OpsJob {
    End {
        id: Option<String>,
        say: Option<String>,
        reason: Option<String>,
    },
    Transfer {
        id: Option<String>,
        request: TransferRequest,
    },
}

/// Sent-at instants of the newest events, by `seq`, for the hop latency.
struct HopRing {
    slots: Vec<Option<(u64, Instant)>>,
}

impl HopRing {
    fn new() -> Self {
        Self {
            slots: vec![None; HOP_RING],
        }
    }

    fn slot(seq: u64) -> usize {
        usize::try_from(seq % HOP_RING as u64).unwrap_or(0)
    }

    fn record(&mut self, seq: u64, at: Instant) {
        if let Some(slot) = self.slots.get_mut(Self::slot(seq)) {
            *slot = Some((seq, at));
        }
    }

    fn sent_at(&self, seq: u64) -> Option<Instant> {
        match self.slots.get(Self::slot(seq)) {
            Some(Some((stored, at))) if *stored == seq => Some(*at),
            _ => None,
        }
    }
}

/// Everything the link's tasks and remote tools share.
pub(crate) struct LinkShared {
    config: Arc<AgentLinkConfig>,
    pub(crate) clock: SharedClock,
    t0: Instant,
    /// The server's id of the call (part of a transfer's idempotency key).
    session_id: u64,
    /// Keys of this link's transfer idempotency keys: random per link, so a command id reused
    /// by another call (or after a restart) never collides, stable for a repeat in this call.
    keys: RandomState,
    /// Tool calls, cancels, command results, errors (never dropped silently: a full queue fails
    /// the sender).
    control: mpsc::Sender<ServerBody>,
    /// Engine events (a full queue drops the newest, reported as `events.lost`).
    /// With the instant the event left the call (the hop latency counts from there).
    events: mpsc::Sender<(ServerBody, Instant)>,
    pending: Mutex<HashMap<u64, oneshot::Sender<ToolReply>>>,
    next_tool: AtomicU64,
    /// The call is over or the link was closed: every task stops.
    stop: CancellationToken,
    /// Whether the link is usable now, being reopened, or gone for good.
    status: watch::Sender<Status>,
    /// The event types the agent subscribed to ([`Subscription`] bits; renewed by a resumption).
    subscription: AtomicU32,
    /// Events not delivered while the link was down, reported as `events.lost` once it is up.
    outage_lost: AtomicU64,
    /// Events (and `events.lost` notices) queued for the writer so far.
    events_queued: AtomicU64,
    /// Events the writer is done with (written, failed, or dropped with a lost connection):
    /// `end` answers only after the events queued before it were written.
    events_written: watch::Sender<u64>,
    /// The agent sent `end`: the call is going away, a lost link is not reopened.
    ending: AtomicBool,
    /// The name of the agent definition (for the `session.start` of a resumption).
    agent: String,
    /// What `session.start` says the server can do (repeated by a resumption).
    capabilities: Capabilities,
    /// Who is on the line (repeated by a resumption).
    identity: Option<CallIdentity>,
    /// The agent asked to end the call (after its farewell).
    hangup: CancellationToken,
    say: SaySender,
    edits: EditSender,
    /// The tools the call offered before the link added remote ones (set only when it added
    /// some): offered again when the link fails.
    local_tools: OnceLock<Arc<[Arc<str>]>>,
    /// A `tools.set` of the agent was queued: undone when the link fails.
    tools_narrowed: AtomicBool,
    ops: mpsc::Sender<OpsJob>,
    speaking: watch::Sender<Speaking>,
    hops: Mutex<HopRing>,
    /// Milliseconds since `t0` of the last frame from the agent (liveness).
    last_rx_ms: AtomicU64,
    end_reason: Mutex<String>,
    /// The call's summary, sent as `session.usage` right before the closing `session.end`.
    summary: Mutex<Option<CallSummary>>,
    counters: Counters,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// `text` cut to at most `max` bytes at a character boundary.
fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default().to_owned()
}

/// `future`, or `None` once `duration` passed on `clock`.
async fn timed<F: Future>(clock: &dyn Clock, duration: Duration, future: F) -> Option<F::Output> {
    tokio::select! {
        biased;
        output = future => Some(output),
        () = clock.sleep(duration) => None,
    }
}

/// `future`, or `None` once `deadline` passed on `clock`.
async fn timed_until<F: Future>(
    clock: &dyn Clock,
    deadline: Instant,
    future: F,
) -> Option<F::Output> {
    tokio::select! {
        biased;
        output = future => Some(output),
        () = clock.sleep_until(deadline) => None,
    }
}

impl LinkShared {
    pub(crate) fn config(&self) -> &AgentLinkConfig {
        &self.config
    }

    fn ms_since_t0(&self, at: Instant) -> u64 {
        u64::try_from(at.saturating_duration_since(self.t0).as_millis()).unwrap_or(u64::MAX)
    }

    /// Whether the link is connected (and the call not over).
    pub(crate) fn is_up(&self) -> bool {
        !self.stop.is_cancelled() && self.status.borrow().state == LinkState::Up
    }

    /// Resolves when the link is not usable (at once when it is not): lost, being reopened,
    /// closed or the call over. A reopening that completes unnoticed in between is no loss for
    /// the waiter, because the calls waiting at the loss were failed ([`Self::lose`]).
    pub(crate) async fn not_up(&self) {
        let mut status = self.status.subscribe();
        tokio::select! {
            biased;
            () = self.stop.cancelled() => {}
            _ = status.wait_for(|status| status.state != LinkState::Up) => {}
        }
    }

    /// Resolves when the link is gone for good (failed without a way back, or closed).
    async fn gone(&self) {
        let mut status = self.status.subscribe();
        tokio::select! {
            biased;
            () = self.stop.cancelled() => {}
            _ = status.wait_for(|status| status.state == LinkState::Down) => {}
        }
    }

    /// The link was lost: everything that waits on it stops. With `resumable` the server will
    /// try to reopen it ([`LinkState::Reconnecting`]); otherwise it is gone and the call goes
    /// on without it.
    fn lose(&self, resumable: bool, why: &str) {
        let next = if resumable {
            LinkState::Reconnecting
        } else {
            LinkState::Down
        };
        let changed = self.status.send_if_modified(|status| {
            if status.state == LinkState::Down || status.state == next {
                return false;
            }
            status.state = next;
            true
        });
        if changed {
            if resumable {
                tracing::warn!(target: "brisvox::agent_link", why, "agent link lost; reconnecting");
            } else {
                tracing::warn!(target: "brisvox::agent_link", why, "agent link down; the call goes on without it");
            }
        }
        // Waiting remote calls get `Err` from their dropped senders.
        lock(&self.pending).clear();
    }

    /// The link is gone for good (also at the end of the call).
    fn set_down(&self) {
        self.status.send_if_modified(|status| {
            if status.state == LinkState::Down {
                return false;
            }
            status.state = LinkState::Down;
            true
        });
        lock(&self.pending).clear();
    }

    /// A reopened connection took the call up again.
    fn resumed(&self) {
        self.status.send_modify(|status| {
            status.state = LinkState::Up;
            status.generation = status.generation.wrapping_add(1);
        });
    }

    fn subscription(&self) -> Subscription {
        Subscription(self.subscription.load(Ordering::Relaxed))
    }

    pub(crate) fn next_tool_id(&self) -> u64 {
        self.next_tool.fetch_add(1, Ordering::Relaxed)
    }

    /// Registers a waiting remote call; `false` when too many wait.
    pub(crate) fn add_pending(&self, id: u64, reply: oneshot::Sender<ToolReply>) -> bool {
        let mut pending = lock(&self.pending);
        if pending.len() >= self.config.max_inflight_tools {
            return false;
        }
        pending.insert(id, reply);
        true
    }

    pub(crate) fn remove_pending(&self, id: u64) {
        lock(&self.pending).remove(&id);
    }

    /// Queues a control message (`false` when the queue is full or the link is down).
    pub(crate) fn send_control(&self, body: ServerBody) -> bool {
        self.is_up() && self.control.try_send(body).is_ok()
    }

    pub(crate) fn count_tool_call(&self) {
        bump(&self.counters.tool_calls);
    }

    pub(crate) fn count_tool_refused(&self) {
        bump(&self.counters.tool_refused);
    }

    pub(crate) fn count_tool_timeout(&self) {
        bump(&self.counters.tool_timeouts);
    }

    pub(crate) fn count_tool_cancel(&self) {
        bump(&self.counters.tool_cancels);
    }

    fn reply(&self, id: Option<String>, error: Option<(ErrorCode, String)>) {
        if error.is_some() {
            bump(&self.counters.command_errors);
        }
        let Some(id) = id else {
            return;
        };
        let body = ServerBody::CommandResult {
            id,
            ok: error.is_none(),
            error: error.map(|(code, message)| WireError {
                code,
                message: bounded(&message, MAX_DETAIL_BYTES),
            }),
        };
        if !self.send_control(body) {
            bump(&self.counters.results_dropped);
        }
    }

    fn reply_ok(&self, id: Option<String>) {
        self.reply(id, None);
    }

    fn reply_err(&self, id: Option<String>, code: ErrorCode, message: impl Into<String>) {
        self.reply(id, Some((code, message.into())));
    }

    /// An `error` event of the link itself (for messages that carry no command id).
    fn link_error(&self, message: String) {
        bump(&self.counters.command_errors);
        let body = ServerBody::Error {
            stage: "agent_link".to_owned(),
            message: bounded(&message, MAX_DETAIL_BYTES),
            fatal: false,
        };
        if !self.send_control(body) {
            bump(&self.counters.results_dropped);
        }
    }

    fn note_hop(&self, re: Option<u64>) {
        let Some(seq) = re else {
            return;
        };
        let Some(sent) = lock(&self.hops).sent_at(seq) else {
            return;
        };
        let hop = self.clock.now().saturating_duration_since(sent);
        let us = u64::try_from(hop.as_micros()).unwrap_or(u64::MAX);
        bump(&self.counters.hops);
        self.counters.hop_total_us.fetch_add(us, Ordering::Relaxed);
        self.counters.hop_max_us.fetch_max(us, Ordering::Relaxed);
        let bucket = HOP_BUCKETS_US
            .iter()
            .position(|bound| us <= *bound)
            .unwrap_or(HOP_BUCKET_COUNT - 1);
        if let Some(slot) = self.counters.hop_buckets.get(bucket) {
            slot.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// One text frame from the agent. Never waits: every command goes into a bounded queue.
    fn handle(&self, text: &str) {
        let message: AgentMessage = match serde_json::from_str(text) {
            Ok(message) => message,
            Err(error) => {
                let probe: Probe = serde_json::from_str(text).unwrap_or_default();
                let id = probe.id.map(|id| match id {
                    Value::String(id) => bounded(&id, MAX_COMMAND_ID_BYTES),
                    other => bounded(&other.to_string(), MAX_COMMAND_ID_BYTES),
                });
                let unknown = probe
                    .kind
                    .as_deref()
                    .is_some_and(|kind| !AGENT_TYPES.contains(&kind));
                let (code, message) = if unknown {
                    (
                        ErrorCode::UnknownType,
                        format!("unknown message type {:?}", probe.kind.unwrap_or_default()),
                    )
                } else {
                    (ErrorCode::BadMessage, error.to_string())
                };
                match id {
                    Some(id) => self.reply_err(Some(id), code, message),
                    None => self.link_error(format!("{}: {message}", code.as_str())),
                }
                return;
            }
        };
        match message {
            AgentMessage::SessionAccept(_) | AgentMessage::SessionReject { .. } => {
                self.link_error("not_ready: the handshake is over".to_owned());
            }
            AgentMessage::Say {
                id,
                re,
                text,
                interrupt,
            } => {
                bump(&self.counters.commands);
                self.note_hop(re);
                let id = id.map(|id| bounded(&id, MAX_COMMAND_ID_BYTES));
                let line = BotLine {
                    text: Arc::from(text),
                    interrupt,
                };
                match self.say.try_send(line) {
                    Ok(()) => self.reply_ok(id),
                    Err(error) => self.input_error(id, &error),
                }
            }
            AgentMessage::End {
                id,
                re,
                say,
                reason,
            } => {
                bump(&self.counters.commands);
                self.note_hop(re);
                let id = id.map(|id| bounded(&id, MAX_COMMAND_ID_BYTES));
                // The agent is done with the call: it may close its socket right away, which
                // is no reason to reopen the link.
                self.ending.store(true, Ordering::Relaxed);
                if let Some(text) = &say
                    && (text.trim().is_empty() || text.len() > MAX_LINE_BYTES)
                {
                    self.reply_err(
                        id,
                        ErrorCode::InvalidArgument,
                        "end.say is empty or too long",
                    );
                    return;
                }
                let reason = reason.map(|reason| bounded(&reason, MAX_NOTE_BYTES));
                if let Err(
                    mpsc::error::TrySendError::Full(job) | mpsc::error::TrySendError::Closed(job),
                ) = self.ops.try_send(OpsJob::End { id, say, reason })
                {
                    let OpsJob::End { id, .. } = job else {
                        return;
                    };
                    self.reply_err(id, ErrorCode::QueueFull, "too many operations at once");
                }
            }
            AgentMessage::Transfer {
                id,
                re,
                destination,
                mode,
                summary,
                reason,
            } => {
                bump(&self.counters.commands);
                self.note_hop(re);
                let id = id.map(|id| bounded(&id, MAX_COMMAND_ID_BYTES));
                self.transfer(id, &destination, mode, summary, reason);
            }
            AgentMessage::ContextUpdate {
                id,
                re,
                system_prompt,
                append,
            } => {
                bump(&self.counters.commands);
                self.note_hop(re);
                let id = id.map(|id| bounded(&id, MAX_COMMAND_ID_BYTES));
                self.context_update(id, system_prompt, append);
            }
            AgentMessage::ToolsSet { id, re, enabled } => {
                bump(&self.counters.commands);
                self.note_hop(re);
                let id = id.map(|id| bounded(&id, MAX_COMMAND_ID_BYTES));
                if enabled.as_ref().is_some_and(|names| names.len() > 256) {
                    self.reply_err(id, ErrorCode::TooLarge, "more than 256 tool names");
                    return;
                }
                let names = enabled.map(|names| {
                    names
                        .into_iter()
                        .map(Arc::<str>::from)
                        .collect::<Vec<_>>()
                        .into()
                });
                match self.edits.try_send(ConversationEdit::SetTools(names)) {
                    Ok(()) => {
                        self.tools_narrowed.store(true, Ordering::Relaxed);
                        self.reply_ok(id);
                    }
                    Err(error) => self.input_error(id, &error),
                }
            }
            AgentMessage::ToolResult { id, result, error } => {
                let reply = match (error, result) {
                    (Some(error), _) => Err(bounded(&error, MAX_DETAIL_BYTES * 4)),
                    (None, Some(result)) => Ok(result),
                    (None, None) => Ok(Value::Null),
                };
                match lock(&self.pending).remove(&id) {
                    Some(waiting) => {
                        bump(&self.counters.tool_results);
                        // The caller may have stopped waiting a moment ago: nothing to do then.
                        let _ = waiting.send(reply);
                    }
                    None => bump(&self.counters.unknown_results),
                }
            }
        }
    }

    fn input_error<T>(&self, id: Option<String>, error: &InputError<T>) {
        match error {
            InputError::Full(_) => {
                self.reply_err(id, ErrorCode::QueueFull, "the call's input queue is full");
            }
            InputError::Closed(_) => self.reply_err(id, ErrorCode::CallEnded, "the call is over"),
            InputError::Invalid(_, why) => self.reply_err(id, ErrorCode::InvalidArgument, *why),
        }
    }

    fn transfer(
        &self,
        id: Option<String>,
        destination: &str,
        mode: Option<WireTransferMode>,
        summary: Option<String>,
        reason: Option<String>,
    ) {
        let number = match PhoneNumber::parse(destination) {
            Ok(number) => number,
            Err(error) => {
                self.reply_err(id, ErrorCode::InvalidArgument, error.message().to_owned());
                return;
            }
        };
        let mode = match mode.unwrap_or(WireTransferMode::Cold) {
            WireTransferMode::Cold => TransferMode::Cold,
            WireTransferMode::Warm => TransferMode::Warm,
        };
        let idempotency = self.transfer_key(id.as_deref());
        let request = TransferRequest {
            destination_name: number.redacted(),
            destination: number,
            mode,
            summary: summary.map(|text| bounded(&text, MAX_NOTE_BYTES)),
            reason: reason.map(|text| bounded(&text, MAX_NOTE_BYTES)),
            idempotency,
        };
        if let Err(mpsc::error::TrySendError::Full(job) | mpsc::error::TrySendError::Closed(job)) =
            self.ops.try_send(OpsJob::Transfer { id, request })
        {
            let OpsJob::Transfer { id, .. } = job else {
                return;
            };
            self.reply_err(id, ErrorCode::QueueFull, "too many operations at once");
        }
    }

    /// The idempotency key of a `transfer`: derived from the call and the command id, so a
    /// repeated command (same `id`, after a lost `command.result` or an `outcome_unknown`) gets
    /// the same key and a carrier that deduplicates by it ignores the repeat. Without an `id`
    /// the command cannot be told from a new one: each gets a key of its own
    /// (`docs/AGENT_PROTOCOL.md`).
    fn transfer_key(&self, id: Option<&str>) -> u64 {
        match id {
            Some(id) => self.keys.hash_one(("transfer", self.session_id, id)),
            None => self.keys.hash_one((
                "transfer#",
                self.session_id,
                self.counters.commands.load(Ordering::Relaxed),
            )),
        }
    }

    /// After a failure, puts the call back on its local tools: the remote tools (which would
    /// all fail now) and an agent's `tools.set` are undone by one edit, applied at the next
    /// safe point. `false` when the edit could not be queued yet (the edit queue is full).
    fn fall_back(&self) -> bool {
        let edit = match self.local_tools.get() {
            Some(local) => ConversationEdit::SetTools(Some(Arc::clone(local))),
            None if self.tools_narrowed.load(Ordering::Relaxed) => ConversationEdit::SetTools(None),
            None => return true,
        };
        match self.edits.try_send(edit) {
            Ok(()) | Err(InputError::Closed(_) | InputError::Invalid(..)) => true,
            Err(InputError::Full(_)) => false,
        }
    }

    /// After the link came back, offers all tools again: the remote tools that were put away
    /// by [`Self::fall_back`] and, as the docs say, without an earlier `tools.set` of the
    /// agent (which the agent sends again if it wants it). `false` when the edit could not be
    /// queued yet (the edit queue is full).
    fn restore(&self) -> bool {
        if self.local_tools.get().is_none() && !self.tools_narrowed.load(Ordering::Relaxed) {
            return true;
        }
        match self.edits.try_send(ConversationEdit::SetTools(None)) {
            Ok(()) | Err(InputError::Closed(_) | InputError::Invalid(..)) => {
                self.tools_narrowed.store(false, Ordering::Relaxed);
                true
            }
            Err(InputError::Full(_)) => false,
        }
    }

    fn context_update(
        &self,
        id: Option<String>,
        system_prompt: Option<String>,
        append: Vec<WireMessage>,
    ) {
        if append.len() > MAX_CONTEXT_MESSAGES {
            self.reply_err(
                id,
                ErrorCode::TooLarge,
                format!("more than {MAX_CONTEXT_MESSAGES} messages"),
            );
            return;
        }
        let mut edits = Vec::with_capacity(append.len().saturating_add(1));
        if let Some(prompt) = system_prompt {
            edits.push(ConversationEdit::SetSystemPrompt(Arc::from(prompt)));
        }
        for message in append {
            edits.push(ConversationEdit::Append(message_of(message)));
        }
        for edit in &edits {
            if let Some(why) = invalid_edit(edit) {
                self.reply_err(id, ErrorCode::InvalidArgument, why);
                return;
            }
        }
        // All or nothing: the queue must hold every edit.
        if self.edits.capacity() < edits.len() {
            self.reply_err(id, ErrorCode::QueueFull, "the call's input queue is full");
            return;
        }
        for edit in edits {
            if let Err(error) = self.edits.try_send(edit) {
                self.input_error(id, &error);
                return;
            }
        }
        self.reply_ok(id);
    }

    /// Follows the bot's playout (for `end`).
    fn track(&self, event: &EngineEvent) {
        match event {
            EngineEvent::BotStartedSpeaking { epoch } => {
                let epoch = epoch.get();
                self.speaking.send_modify(|state| {
                    if state.newest.is_none_or(|newest| epoch >= newest) {
                        state.newest = Some(epoch);
                        state.now = true;
                    }
                });
            }
            EngineEvent::BotStoppedSpeaking { epoch } => {
                let epoch = epoch.get();
                self.speaking.send_modify(|state| {
                    if state.newest == Some(epoch) {
                        state.now = false;
                    }
                });
            }
            EngineEvent::BotInterrupted { .. } => {
                self.speaking.send_modify(|state| state.now = false);
            }
            _ => {}
        }
    }

    /// `end`: the farewell (if any) is spoken, the bot's speech ends, then the call ends.
    async fn end(&self, id: Option<String>, say: Option<String>, reason: Option<String>) {
        let deadline = brisvox_core::clock::deadline_after(self.clock.now(), self.config.end_grace);
        let mut state = self.speaking.subscribe();
        if let Some(text) = say {
            let before = state.borrow().newest;
            match self.say.try_send(BotLine::new(text)) {
                Ok(()) => {
                    let started = state.wait_for(|s| match (s.newest, before) {
                        (Some(newest), Some(before)) => newest > before,
                        (Some(_), None) => true,
                        (None, _) => false,
                    });
                    self.until(deadline, started).await;
                }
                Err(error) => {
                    tracing::debug!(target: "brisvox::agent_link", %error, "farewell not queued");
                }
            }
        }
        let silent = state.wait_for(|s| !s.now);
        self.until(deadline, silent).await;
        tracing::info!(
            target: "brisvox::agent_link",
            reason = reason.as_deref().unwrap_or(""),
            "the agent ended the call"
        );
        *lock(&self.end_reason) = "ended_by_agent".to_owned();
        // The events up to the one that ended the bot's speech are on the wire before the
        // result (control messages are otherwise written first).
        self.events_flushed().await;
        // The result first: the hang-up stops the link, whose writer still flushes it.
        self.reply_ok(id);
        self.hangup.cancel();
    }

    /// Waits until the writer is done with the events queued so far (bounded by
    /// `close_timeout`; also ends with the link).
    async fn events_flushed(&self) {
        let target = self.events_queued.load(Ordering::Acquire);
        let mut written = self.events_written.subscribe();
        let _ = timed(&*self.clock, self.config.close_timeout, async {
            tokio::select! {
                biased;
                _ = written.wait_for(|done| *done >= target) => {}
                () = self.not_up() => {}
            }
        })
        .await;
    }

    /// Waits for `future` until `deadline` (or the end of the call).
    async fn until<F: Future>(&self, deadline: Instant, future: F) {
        tokio::select! {
            biased;
            () = self.stop.cancelled() => {}
            _ = future => {}
            () = self.clock.sleep_until(deadline) => {}
        }
    }

    async fn run_transfer(
        &self,
        id: Option<String>,
        request: TransferRequest,
        ops: Option<&Arc<dyn TelephonyOps>>,
    ) {
        let Some(ops) = ops else {
            self.reply_err(
                id,
                ErrorCode::Unsupported,
                "no call-control is bound to this call",
            );
            return;
        };
        let timeout = self.config.ops_timeout;
        // The hand-off sentence first (bounded by the binding itself and by the timeout).
        if timed(&*self.clock, timeout, ops.settle_speech())
            .await
            .is_none()
        {
            tracing::debug!(target: "brisvox::agent_link", "settle before the transfer timed out");
        }
        match timed(&*self.clock, timeout, ops.transfer(&request)).await {
            Some(Ok(report)) => {
                tracing::info!(target: "brisvox::agent_link", destination = %request.destination_name, mode = report.mode.as_str(), "transfer started");
                self.reply_ok(id);
            }
            Some(Err(error)) => {
                let code = match error.kind() {
                    TelephonyErrorKind::Unsupported => ErrorCode::Unsupported,
                    TelephonyErrorKind::InvalidArgument => ErrorCode::InvalidArgument,
                    TelephonyErrorKind::Rejected => ErrorCode::Rejected,
                    TelephonyErrorKind::Unavailable => ErrorCode::Unavailable,
                    _ => ErrorCode::OutcomeUnknown,
                };
                self.reply_err(id, code, error.message().to_owned());
            }
            None => self.reply_err(
                id,
                ErrorCode::OutcomeUnknown,
                "the carrier did not answer in time; the transfer may or may not have started",
            ),
        }
    }
}

/// The engine message of a protocol message.
fn message_of(message: WireMessage) -> Message {
    match message.role {
        WireRole::System => Message::system(message.text),
        WireRole::User => Message::user(message.text),
        WireRole::Assistant => Message::assistant(message.text),
    }
}

/// Why `edit` would be refused by the call (checked before any edit of a command is queued).
fn invalid_edit(edit: &ConversationEdit) -> Option<&'static str> {
    let text = match edit {
        ConversationEdit::SetSystemPrompt(text) => text,
        ConversationEdit::Append(message) => match message {
            Message::System { text } | Message::User { text } => text,
            Message::Assistant { text, .. } => text,
            Message::Tool { .. } => return Some("tool results cannot be appended"),
        },
        _ => return None,
    };
    if text.trim().is_empty() {
        Some("a message is empty")
    } else if text.len() > MAX_EDIT_TEXT_BYTES {
        Some("a message is too long")
    } else {
        None
    }
}

/// The protocol event of an engine event (`None`: not part of the protocol).
fn body_of(event: &EngineEvent) -> Option<ServerBody> {
    let ms = |d: Option<Duration>| d.map(|d| d.as_secs_f64() * 1000.0);
    Some(match event {
        EngineEvent::UserStartedSpeaking { epoch } => {
            ServerBody::TurnUserStarted { epoch: epoch.get() }
        }
        EngineEvent::UserStoppedSpeaking { epoch } => {
            ServerBody::TurnUserEnded { epoch: epoch.get() }
        }
        EngineEvent::UserTranscript { text, is_final } => ServerBody::UserTranscript {
            text: Arc::clone(text),
            is_final: *is_final,
        },
        EngineEvent::UserText { epoch, text } => ServerBody::UserText {
            epoch: epoch.get(),
            text: Arc::clone(text),
        },
        EngineEvent::BotLlmStarted { epoch } => ServerBody::BotLlmStarted { epoch: epoch.get() },
        EngineEvent::BotLlmText { epoch, text } => ServerBody::BotText {
            epoch: epoch.get(),
            text: Arc::clone(text),
        },
        EngineEvent::BotLlmStopped { epoch, usage } => ServerBody::BotLlmStopped {
            epoch: epoch.get(),
            usage: usage.map(TokenUsage::from),
        },
        EngineEvent::BotTtsText { epoch, text } => ServerBody::BotTtsText {
            epoch: epoch.get(),
            text: Arc::clone(text),
        },
        EngineEvent::BotTtsStopped { epoch } => ServerBody::BotTtsStopped { epoch: epoch.get() },
        EngineEvent::BotStartedSpeaking { epoch } => {
            ServerBody::TurnBotStarted { epoch: epoch.get() }
        }
        EngineEvent::BotStoppedSpeaking { epoch } => {
            ServerBody::TurnBotEnded { epoch: epoch.get() }
        }
        EngineEvent::BotInterrupted { epoch } => ServerBody::TurnInterrupted { epoch: epoch.get() },
        EngineEvent::TurnMetrics { epoch, metrics } => ServerBody::TurnMetrics {
            epoch: epoch.get(),
            metrics: Metrics {
                end_of_turn_ms: ms(metrics.end_of_turn_delay),
                stt_final_ms: ms(metrics.stt_final_delay),
                llm_ttft_ms: ms(metrics.llm_time_to_first_token),
                tts_ttfb_ms: ms(metrics.tts_time_to_first_byte),
                response_ms: ms(metrics.response_latency),
            },
        },
        EngineEvent::Error {
            stage,
            message,
            fatal,
        } => ServerBody::Error {
            stage: (*stage).to_owned(),
            message: message.clone(),
            fatal: *fatal,
        },
        _ => return None,
    })
}

/// The event types an agent subscribed to, as a bit set over [`EVENT_TYPES`].
#[derive(Debug, Clone, Copy)]
struct Subscription(u32);

impl Subscription {
    const ALL: Self = Self(u32::MAX);

    /// From `session.accept.events`; unknown names are returned for the error report.
    fn of(names: Option<&[String]>) -> (Self, Vec<String>) {
        let Some(names) = names else {
            return (Self::ALL, Vec::new());
        };
        let mut bits = 0u32;
        let mut unknown = Vec::new();
        for name in names {
            match EVENT_TYPES.iter().position(|known| known == name) {
                Some(index) => {
                    bits |= 1u32
                        .checked_shl(u32::try_from(index).unwrap_or(31))
                        .unwrap_or(0)
                }
                None => unknown.push(name.clone()),
            }
        }
        (Self(bits), unknown)
    }

    fn allows(self, body: &ServerBody) -> bool {
        match EVENT_TYPES
            .iter()
            .position(|known| *known == body.type_name())
        {
            Some(index) => {
                let bit = 1u32
                    .checked_shl(u32::try_from(index).unwrap_or(31))
                    .unwrap_or(0);
                self.0 & bit != 0
            }
            // Not a subscribable event (errors, losses): always sent.
            None => true,
        }
    }
}

/// The spec of a declared remote tool (bounded by the configuration).
fn spec_of(decl: &ToolDecl, config: &AgentLinkConfig) -> ToolSpec {
    let kind = match decl.kind {
        WireToolKind::ReadOnly => ToolKind::ReadOnly,
        WireToolKind::SideEffect => ToolKind::SideEffect,
    };
    let timeout = decl
        .timeout_ms
        .map_or(config.default_tool_timeout, Duration::from_millis)
        .clamp(Duration::from_millis(1), config.max_tool_timeout);
    let filler = match decl.filler_after_ms {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(brisvox_agent::tools::DEFAULT_FILLER_AFTER),
    };
    ToolSpec::new(
        decl.name.as_str(),
        bounded(&decl.description, 4 * 1024),
        decl.parameters.clone(),
        kind,
    )
    .with_timeout(timeout)
    .with_filler_after(filler)
    .with_confirm(decl.confirm)
    .with_repeatable(decl.repeatable)
}

/// The agent link of one call: see the crate docs. Dropping it stops its tasks (the connection
/// task closes the WebSocket within `close_timeout`); [`Self::close`] says why first.
pub struct AgentLink {
    shared: Arc<LinkShared>,
    /// Owns the sockets and the queues: runs each connection and reopens a lost one.
    connection: Option<JoinHandle<()>>,
    _pump: TaskGuard,
    _ops: TaskGuard,
}

impl fmt::Debug for AgentLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentLink")
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl Drop for AgentLink {
    fn drop(&mut self) {
        // The connection task sees the stop, says `session.end` and closes, bounded by
        // `close_timeout`.
        self.shared.stop.cancel();
    }
}

impl AgentLink {
    /// The sender of the bot lines the link installed in the call's engine. The engine takes a
    /// single line input and the link owns it, so a line source of the server (the telephony
    /// tools' voicemail message, the apology after a failed warm transfer) goes through here.
    pub fn say_sender(&self) -> SaySender {
        self.shared.say.clone()
    }

    /// The link's counters.
    pub fn stats(&self) -> LinkStats {
        self.shared.counters.snapshot(self.shared.is_up())
    }

    /// Whether the link is up (connected; neither failed, being reopened nor closed).
    pub fn is_up(&self) -> bool {
        self.shared.is_up()
    }

    /// Whether the link was lost and is being reopened (`docs/AGENT_PROTOCOL.md`,
    /// "Reconnecting"). The call goes on meanwhile.
    pub fn is_reconnecting(&self) -> bool {
        !self.shared.stop.is_cancelled()
            && self.shared.status.borrow().state == LinkState::Reconnecting
    }

    /// Resolves when the agent asked to end the call (`end`), after its farewell was spoken and
    /// the bot fell silent (bounded by `end_grace`). The server then ends the call.
    pub async fn ended(&self) {
        self.shared.hangup.cancelled().await;
    }

    /// [`Self::ended`] of an optional link (never resolves without one): for a `select!`.
    pub async fn ended_opt(link: Option<&Self>) {
        match link {
            Some(link) => link.ended().await,
            None => std::future::pending().await,
        }
    }

    /// Resolves when the link went down for good (failed with no way back, or closed). A link
    /// that is being reopened is not down yet.
    pub async fn down(&self) {
        self.shared.gone().await;
    }

    /// Ends the link: `session.end` with `reason` (unless the agent ended the call), then the
    /// close frame, bounded by `close_timeout`.
    pub async fn close(self, reason: &str) {
        self.close_with(reason, None).await;
    }

    /// [`Self::close`], saying `session.usage` with `summary` right before `session.end`. Like
    /// `session.end` it is written straight to the socket by the closing connection task, so a
    /// full event queue cannot drop it; it is lost only when the link is down at the end.
    pub async fn close_with(mut self, reason: &str, summary: Option<CallSummary>) {
        *lock(&self.shared.summary) = summary;
        {
            let mut end = lock(&self.shared.end_reason);
            if end.is_empty() {
                *end = bounded(reason, MAX_NOTE_BYTES);
            }
        }
        self.shared.stop.cancel();
        if let Some(connection) = self.connection.take() {
            let clock = Arc::clone(&self.shared.clock);
            let bound = self.shared.config.close_timeout.saturating_mul(2);
            if timed(&*clock, bound, connection).await.is_none() {
                tracing::debug!(target: "brisvox::agent_link", "the connection task did not close in time");
            }
        }
    }
}

/// Opens the agent link of a call and wires it into `builder`. Never fails the call: when the
/// agent cannot be reached, does not answer in time, rejects the call or breaks the protocol,
/// the failure is logged and `builder` is returned unchanged with `None`, so the call runs on
/// the agent definition alone (`docs/AGENT_PROTOCOL.md`, "Failure and fallback").
///
/// On success the builder gets:
/// - the call's events (shared with an events sink already set, such as the RTVI bridge's);
/// - the bot lines and conversation edits of the agent's commands;
/// - the agent's system prompt, initial context and greeting;
/// - the agent's tools as remote tools, next to the tools already given to the builder (same
///   tool engine settings; a remote tool whose name is taken is refused and reported).
///
/// A link that fails later is reopened when the agent declared `resume` (see
/// [`AgentLinkConfig::reconnect_attempts`]); otherwise the call goes on without it.
pub async fn attach<S, L, T>(
    config: &Arc<AgentLinkConfig>,
    builder: SessionBuilder<S, L, T>,
    call: LinkCall,
) -> (SessionBuilder<S, L, T>, Option<AgentLink>)
where
    S: SttProvider,
    L: LlmProvider,
    T: TtsProvider,
{
    let capabilities = Capabilities {
        transfer: call.ops.is_some(),
        tools: true,
    };
    let hello = Hello {
        clock: &*call.clock,
        session_id: call.session_id,
        agent: &call.agent,
        capabilities,
        call: call.identity.as_ref(),
    };
    let opened = tokio::select! {
        biased;
        () = call.cancel.cancelled() => Err(LinkError::Timeout("the call ended")),
        opened = open(config, &hello, None) => opened,
    };
    let (ws, accept, t0) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            tracing::warn!(target: "brisvox::agent_link", session = call.session_id, %error, "no agent link; the call runs on the agent definition");
            return (builder, None);
        }
    };
    wire(config, builder, call, ws, accept, t0, capabilities)
}

/// What `session.start` says, whatever connection it opens.
struct Hello<'a> {
    clock: &'a dyn Clock,
    session_id: u64,
    agent: &'a str,
    capabilities: Capabilities,
    /// Who is on the line, when the call opted in.
    call: Option<&'a CallIdentity>,
}

/// What a reopened connection adds to its `session.start`.
struct Resumption {
    resume: Resume,
    /// The `seq` of this `session.start` (it continues the call's count).
    seq: u64,
    /// When the link first opened (`t_ms` counts from there).
    t0: Instant,
}

/// Connects, sends `session.start` and reads the answer.
async fn open(
    config: &AgentLinkConfig,
    hello: &Hello<'_>,
    resumption: Option<Resumption>,
) -> Result<(Ws, SessionAccept, Instant), LinkError> {
    config.validate()?;
    let clock = hello.clock;
    let mut request = config
        .url
        .as_str()
        .into_client_request()
        .map_err(|e| LinkError::Config(format!("bad agent URL: {e}")))?;
    if let Some(token) = &config.auth_token {
        let mut value =
            HeaderValue::from_str(&format!("Bearer {}", token.expose())).map_err(|_| {
                LinkError::Config("the agent token is not a valid header value".to_owned())
            })?;
        value.set_sensitive(true);
        request.headers_mut().insert(AUTHORIZATION, value);
    }
    let connector = if config.url.starts_with("wss://") {
        let tls = match &config.tls {
            Some(tls) => Arc::clone(tls),
            None => brisvox_providers::net::tls_config()
                .map_err(|e| LinkError::Config(format!("TLS: {e}")))?,
        };
        Some(Connector::Rustls(tls))
    } else {
        None
    };
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let handshake = Box::pin(brisvox_providers::net::connect_ws(
        request,
        Some(ws_config),
        connector,
        clock,
    ));
    let (mut ws, _response) = timed(clock, config.connect_timeout, handshake)
        .await
        .ok_or(LinkError::Timeout("connect"))?
        .map_err(|e| LinkError::Connect(e.to_string()))?;
    let t0 = clock.now();
    let (seq, t_ms, resume) = match resumption {
        Some(r) => (
            r.seq,
            u64::try_from(t0.saturating_duration_since(r.t0).as_millis()).unwrap_or(u64::MAX),
            Some(r.resume),
        ),
        None => (1, 0, None),
    };
    let start = ServerMessage {
        seq,
        t_ms,
        body: ServerBody::SessionStart {
            protocol: PROTOCOL.to_owned(),
            versions: SUPPORTED_VERSIONS.to_vec(),
            session_id: hello.session_id.to_string(),
            agent: hello.agent.to_owned(),
            capabilities: hello.capabilities,
            resume,
            call: hello.call.cloned(),
        },
    };
    let text = serde_json::to_string(&start).map_err(|e| LinkError::Protocol(e.to_string()))?;
    // `handshake_timeout` bounds the whole exchange, the `session.start` write included, so
    // the call waits at most `connect_timeout + handshake_timeout` for its agent.
    let handshake_deadline = brisvox_core::clock::deadline_after(t0, config.handshake_timeout);
    let write = timed(clock, config.write_timeout, ws.send(WsMessage::text(text)));
    timed_until(clock, handshake_deadline, write)
        .await
        .flatten()
        .ok_or(LinkError::Timeout("session.start"))?
        .map_err(|e| LinkError::Connect(e.to_string()))?;
    let answer = async {
        loop {
            match ws.next().await {
                Some(Ok(WsMessage::Text(text))) => return Ok(text),
                Some(Ok(WsMessage::Close(_))) | None => {
                    return Err(LinkError::Connect(
                        "the agent closed the connection".to_owned(),
                    ));
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(LinkError::Connect(e.to_string())),
            }
        }
    };
    let text = timed_until(clock, handshake_deadline, answer)
        .await
        .ok_or(LinkError::Timeout("session.accept"))??;
    let accept = match serde_json::from_str::<AgentMessage>(text.as_str()) {
        Ok(AgentMessage::SessionAccept(accept)) => accept,
        Ok(AgentMessage::SessionReject { code, message }) => {
            let _ = timed(clock, config.close_timeout, ws.close(None)).await;
            return Err(LinkError::Rejected {
                code: bounded(&code.unwrap_or_default(), MAX_NOTE_BYTES),
                message: bounded(&message.unwrap_or_default(), MAX_NOTE_BYTES),
            });
        }
        Ok(_) => {
            return Err(LinkError::Protocol(
                "the first message was not session.accept or session.reject".to_owned(),
            ));
        }
        Err(e) => return Err(LinkError::Protocol(format!("bad session.accept: {e}"))),
    };
    if !SUPPORTED_VERSIONS.contains(&accept.version) {
        let _ = timed(clock, config.close_timeout, ws.close(None)).await;
        return Err(LinkError::Protocol(format!(
            "the agent picked version {}, the server speaks {SUPPORTED_VERSIONS:?}",
            accept.version
        )));
    }
    Ok((ws, accept, t0))
}

/// Wires an accepted link into the builder and starts its tasks.
fn wire<S, L, T>(
    config: &Arc<AgentLinkConfig>,
    builder: SessionBuilder<S, L, T>,
    call: LinkCall,
    ws: Ws,
    accept: SessionAccept,
    t0: Instant,
    capabilities: Capabilities,
) -> (SessionBuilder<S, L, T>, Option<AgentLink>)
where
    S: SttProvider,
    L: LlmProvider,
    T: TtsProvider,
{
    let (say, say_rx) = say_input(SAY_CAPACITY);
    let (edits, edits_rx) = conversation_edits(EDIT_CAPACITY);
    let (control, control_rx) = mpsc::channel(config.control_queue);
    let (events_tx, events_rx) = mpsc::channel(config.event_queue);
    let (ops, ops_rx) = mpsc::channel(OPS_CAPACITY);
    let stop = call.cancel.child_token();
    let (subscription, unknown) = Subscription::of(accept.events.as_deref());
    let resumable = accept.resume;
    let shared = Arc::new(LinkShared {
        config: Arc::clone(config),
        clock: call.clock.clone(),
        t0,
        session_id: call.session_id,
        keys: RandomState::new(),
        control,
        events: events_tx,
        pending: Mutex::new(HashMap::new()),
        next_tool: AtomicU64::new(1),
        stop,
        status: watch::Sender::new(Status {
            state: LinkState::Up,
            generation: 0,
        }),
        subscription: AtomicU32::new(subscription.0),
        outage_lost: AtomicU64::new(0),
        events_queued: AtomicU64::new(0),
        events_written: watch::Sender::new(0),
        ending: AtomicBool::new(false),
        agent: call.agent.clone(),
        capabilities,
        identity: call.identity.clone(),
        hangup: CancellationToken::new(),
        say,
        edits,
        local_tools: OnceLock::new(),
        tools_narrowed: AtomicBool::new(false),
        ops,
        speaking: watch::Sender::new(Speaking::default()),
        hops: Mutex::new(HopRing::new()),
        last_rx_ms: AtomicU64::new(0),
        end_reason: Mutex::new(String::new()),
        summary: Mutex::new(None),
        counters: Counters::default(),
    });
    bump(&shared.counters.messages_sent); // session.start

    let events = builder
        .events()
        .cloned()
        .unwrap_or_else(|| EngineEvents::new(config.event_queue));
    let stream = events.subscribe();
    let mut builder = builder
        .with_events(events)
        .with_say_input(say_rx)
        .with_conversation_edits(edits_rx);

    let mut problems = Vec::new();
    if let Some(prompt) = accept.system_prompt.filter(|p| !p.trim().is_empty()) {
        if prompt.len() > MAX_EDIT_TEXT_BYTES {
            problems.push("the system prompt is too long".to_owned());
        } else {
            builder = builder.with_system_prompt(prompt);
        }
    }
    if accept.context.len() > MAX_CONTEXT_MESSAGES {
        problems.push(format!(
            "the initial context has more than {MAX_CONTEXT_MESSAGES} messages; ignored"
        ));
    } else {
        for message in accept.context {
            let edit = ConversationEdit::Append(message_of(message));
            if let Some(why) = invalid_edit(&edit) {
                problems.push(format!("initial context: {why}"));
            } else if let Err(error) = shared.edits.try_send(edit) {
                problems.push(format!("initial context: {error}"));
            }
        }
    }
    if let Some(greeting) = accept.greeting.filter(|g| !g.trim().is_empty())
        && let Err(error) = shared.say.try_send(BotLine::new(greeting))
    {
        problems.push(format!("greeting: {error}"));
    }
    if !unknown.is_empty() {
        problems.push(format!("unknown event types ignored: {unknown:?}"));
    }
    builder = add_tools(builder, &shared, &call, &accept.tools, &mut problems);
    for problem in problems {
        shared.link_error(problem);
    }

    let connection = tokio::spawn(connection_task(
        Arc::clone(&shared),
        ws,
        control_rx,
        events_rx,
        resumable && config.reconnect_attempts > 0,
    ));
    let pump = TaskGuard::spawn("agent_link_events", pump(stream, Arc::clone(&shared)));
    let ops_task = TaskGuard::spawn(
        "agent_link_ops",
        ops_loop(ops_rx, Arc::clone(&shared), call.ops.clone()),
    );
    tracing::info!(target: "brisvox::agent_link", session = call.session_id, version = accept.version, resumable, "agent link up");
    let link = AgentLink {
        shared,
        connection: Some(connection),
        _pump: pump,
        _ops: ops_task,
    };
    (builder, Some(link))
}

/// Registers the declared tools as remote tools next to the builder's own.
fn add_tools<S, L, T>(
    builder: SessionBuilder<S, L, T>,
    shared: &Arc<LinkShared>,
    call: &LinkCall,
    tools: &[ToolDecl],
    problems: &mut Vec<String>,
) -> SessionBuilder<S, L, T>
where
    S: SttProvider,
    L: LlmProvider,
    T: TtsProvider,
{
    if tools.is_empty() {
        return builder;
    }
    if tools.len() > MAX_TOOLS {
        problems.push(format!("more than {MAX_TOOLS} tools; none registered"));
        return builder;
    }
    let (mut registry, engine_config) = match builder.tools() {
        Some((registry, engine)) => (registry.filtered(|_| true), *engine.config()),
        None => (
            ToolRegistry::new(),
            brisvox_agent::tools::EngineConfig::default(),
        ),
    };
    let local: Arc<[Arc<str>]> = registry
        .specs()
        .map(|spec| Arc::clone(&spec.name))
        .collect();
    let mut added = 0usize;
    for decl in tools {
        let spec = spec_of(decl, shared.config());
        let tool = RemoteTool::new(Arc::clone(shared), &spec);
        match registry.register(spec, tool) {
            Ok(()) => added += 1,
            Err(error) => problems.push(format!("tool refused: {error}")),
        }
    }
    if added > 0 {
        // Set once, here, before any task of the link runs.
        let _ = shared.local_tools.set(local);
    }
    let registry = Arc::new(registry);
    let engine = Arc::new(ToolEngine::new(
        Arc::clone(&registry),
        SessionId::new(call.session_id),
        call.clock.clone(),
        engine_config,
    ));
    builder.with_tools(registry, engine)
}

/// How one connection ended.
enum ConnEnd {
    /// The server closed it (the call is over).
    Stopped,
    /// It was lost (the agent closed it, a write failed or timed out, the agent went silent).
    Lost(&'static str),
}

/// Runs the connections of the link, one after the other: each lost connection of a
/// `resumable` link is reopened (bounded, `docs/AGENT_PROTOCOL.md`, "Reconnecting"); anything
/// else that goes wrong takes the link down for good and the call goes on without it. Owns the
/// queues, so what the call queued while the link was down is dropped, never delivered late.
async fn connection_task(
    shared: Arc<LinkShared>,
    first: Ws,
    mut control: mpsc::Receiver<ServerBody>,
    mut events: mpsc::Receiver<(ServerBody, Instant)>,
    resumable: bool,
) {
    let clock = Arc::clone(&shared.clock);
    let mut ws = first;
    // The `seq` of the last message written (`session.start` was 1).
    let mut seq: u64 = 1;
    // Reconnection attempts in the current series (see `STABLE_AFTER`).
    let mut series: u32 = 0;
    loop {
        let began = clock.now();
        let why = match run_connection(&shared, ws, &mut control, &mut events, &mut seq).await {
            ConnEnd::Stopped => break,
            ConnEnd::Lost(why) => why,
        };
        let lost_at = clock.now();
        if lost_at.saturating_duration_since(began) >= STABLE_AFTER {
            series = 0;
        }
        let again =
            resumable && !shared.ending.load(Ordering::Relaxed) && !shared.stop.is_cancelled();
        shared.lose(again, why);
        if !again {
            return;
        }
        discard(&shared, &mut control, &mut events);
        let Some((next, accept)) = reconnect(&shared, &mut series, &mut seq, lost_at).await else {
            break;
        };
        // What was queued while the new connection opened belongs to the old one.
        discard(&shared, &mut control, &mut events);
        let (subscription, unknown) = Subscription::of(accept.events.as_deref());
        shared.subscription.store(subscription.0, Ordering::Relaxed);
        if !unknown.is_empty() {
            tracing::debug!(target: "brisvox::agent_link", ?unknown, "unknown event types ignored");
        }
        ws = next;
        shared.resumed();
        tracing::info!(target: "brisvox::agent_link", session = shared.session_id, "agent link resumed");
    }
    shared.set_down();
}

/// Drops what the link queued for the connection that was lost: tool calls (already failed),
/// command results and cancels (the agent lost its side of them too) and events (reported as
/// `events.lost` once the link is up again).
fn discard(
    shared: &LinkShared,
    control: &mut mpsc::Receiver<ServerBody>,
    events: &mut mpsc::Receiver<(ServerBody, Instant)>,
) {
    let mut stale = 0u64;
    while control.try_recv().is_ok() {
        stale = stale.saturating_add(1);
    }
    shared
        .counters
        .results_dropped
        .fetch_add(stale, Ordering::Relaxed);
    let mut lost = 0u64;
    while events.try_recv().is_ok() {
        lost = lost.saturating_add(1);
    }
    if lost > 0 {
        shared
            .events_written
            .send_modify(|n| *n = n.saturating_add(lost));
        shared.outage_lost.fetch_add(lost, Ordering::Relaxed);
        shared
            .counters
            .events_lost
            .fetch_add(lost, Ordering::Relaxed);
    }
}

/// The wait before reconnection attempt `n` (1-based): doubling from `reconnect_backoff` up to
/// `reconnect_backoff_max`, scaled by a stable pseudo-random factor between 0.5 and 1 so the
/// calls of one crashed agent do not return at the same instant.
fn backoff(shared: &LinkShared, attempt: u32) -> Duration {
    let config = &shared.config;
    let doubling = 1u32
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u32::MAX);
    let base = config
        .reconnect_backoff
        .saturating_mul(doubling)
        .min(config.reconnect_backoff_max);
    let jitter = shared
        .keys
        .hash_one(("backoff", shared.session_id, attempt))
        .checked_rem(501)
        .and_then(|j| u32::try_from(j).ok())
        .unwrap_or(0);
    base.saturating_mul(500 + jitter) / 1000
}

/// Reopens the lost link: attempts with backoff until one is accepted, the attempts or the
/// window run out, the agent refuses (an agent that does not know the session, or breaks the
/// protocol, is not asked again, and neither is one that accepts without `resume: true`) or the call ends. `None`: the link is gone for good.
async fn reconnect(
    shared: &Arc<LinkShared>,
    series: &mut u32,
    seq: &mut u64,
    lost_at: Instant,
) -> Option<(Ws, SessionAccept)> {
    let config = &shared.config;
    let clock = &*shared.clock;
    let deadline = brisvox_core::clock::deadline_after(lost_at, config.reconnect_window);
    let previous_seq = *seq;
    loop {
        if *series >= config.reconnect_attempts {
            tracing::warn!(target: "brisvox::agent_link", session = shared.session_id, "the agent did not come back; the call goes on without it");
            return None;
        }
        *series += 1;
        let attempt = *series;
        let at = brisvox_core::clock::deadline_after(clock.now(), backoff(shared, attempt));
        if at >= deadline {
            tracing::warn!(target: "brisvox::agent_link", session = shared.session_id, "the reconnection window is over; the call goes on without the agent");
            return None;
        }
        tokio::select! {
            biased;
            () = shared.stop.cancelled() => return None,
            () = clock.sleep_until(at) => {}
        }
        bump(&shared.counters.reconnect_attempts);
        *seq = seq.saturating_add(1);
        let now = clock.now();
        let resumption = Resumption {
            resume: Resume {
                attempt,
                previous_seq,
                down_ms: u64::try_from(now.saturating_duration_since(lost_at).as_millis())
                    .unwrap_or(u64::MAX),
            },
            seq: *seq,
            t0: shared.t0,
        };
        let hello = Hello {
            clock,
            session_id: shared.session_id,
            agent: &shared.agent,
            capabilities: shared.capabilities,
            call: shared.identity.as_ref(),
        };
        let opened = tokio::select! {
            biased;
            () = shared.stop.cancelled() => return None,
            opened = open(config, &hello, Some(resumption)) => opened,
        };
        match opened {
            Ok((mut ws, accept, _)) => {
                bump(&shared.counters.messages_sent); // session.start
                if !accept.resume {
                    // An agent that restarted and lost the call's state may still accept: it
                    // has no conversation to go on with, so the call does without it.
                    tracing::warn!(target: "brisvox::agent_link", session = shared.session_id, "the agent accepted the resumption without declaring resume; the call goes on without it");
                    let _ = timed(clock, config.close_timeout, ws.close(None)).await;
                    return None;
                }
                bump(&shared.counters.reconnects);
                return Some((ws, accept));
            }
            Err(error @ (LinkError::Connect(_) | LinkError::Timeout(_))) => {
                tracing::debug!(target: "brisvox::agent_link", session = shared.session_id, attempt, %error, "reconnection attempt failed");
            }
            Err(error) => {
                tracing::warn!(target: "brisvox::agent_link", session = shared.session_id, %error, "the agent will not resume the session; the call goes on without it");
                return None;
            }
        }
    }
}

/// Runs one connection: the writer and the reader side by side until the call ends or the
/// connection is lost.
async fn run_connection(
    shared: &Arc<LinkShared>,
    ws: Ws,
    control: &mut mpsc::Receiver<ServerBody>,
    events: &mut mpsc::Receiver<(ServerBody, Instant)>,
    seq: &mut u64,
) -> ConnEnd {
    let (sink, stream) = ws.split();
    // Cancelled by the first side that fails (or by the call's end), which stops the other.
    let conn = shared.stop.child_token();
    shared
        .last_rx_ms
        .store(shared.ms_since_t0(shared.clock.now()), Ordering::Relaxed);
    let (written, read) = tokio::join!(
        writer(sink, control, events, shared, seq, &conn),
        reader(stream, shared, &conn),
    );
    if shared.stop.is_cancelled() {
        return ConnEnd::Stopped;
    }
    ConnEnd::Lost(written.or(read).unwrap_or("the connection ended"))
}

/// Writes the queued messages, control first; pings; closes at the stop. `Some(why)` when the
/// connection failed on a write (or the ping).
async fn writer(
    mut sink: SplitSink<Ws, WsMessage>,
    control: &mut mpsc::Receiver<ServerBody>,
    events: &mut mpsc::Receiver<(ServerBody, Instant)>,
    shared: &Arc<LinkShared>,
    seq: &mut u64,
    conn: &CancellationToken,
) -> Option<&'static str> {
    let clock = Arc::clone(&shared.clock);
    let config = Arc::clone(&shared.config);
    let mut failure = None;
    let mut next_ping = brisvox_core::clock::deadline_after(clock.now(), config.keepalive);
    loop {
        let (body, event_at) = tokio::select! {
            biased;
            () = conn.cancelled() => break,
            Some(body) = control.recv() => (body, None),
            Some((body, at)) = events.recv() => (body, Some(at)),
            () = clock.sleep_until(next_ping) => {
                let now = clock.now();
                next_ping = brisvox_core::clock::deadline_after(now, config.keepalive);
                let silent_ms = shared
                    .ms_since_t0(now)
                    .saturating_sub(shared.last_rx_ms.load(Ordering::Relaxed));
                let limit = config.keepalive.saturating_mul(3);
                if Duration::from_millis(silent_ms) > limit {
                    failure = Some("nothing from the agent for three keepalive intervals");
                    break;
                }
                match timed(&*clock, config.write_timeout, sink.send(WsMessage::Ping(Vec::new().into()))).await {
                    Some(Ok(())) => {}
                    _ => {
                        failure = Some("ping failed");
                        break;
                    }
                }
                continue;
            }
        };
        *seq = seq.saturating_add(1);
        let now = clock.now();
        if let Some(at) = event_at {
            lock(&shared.hops).record(*seq, at);
        }
        let message = ServerMessage {
            seq: *seq,
            t_ms: shared.ms_since_t0(now),
            body,
        };
        let Ok(text) = serde_json::to_string(&message) else {
            if event_at.is_some() {
                shared
                    .events_written
                    .send_modify(|n| *n = n.saturating_add(1));
            }
            continue;
        };
        let sent = timed(
            &*clock,
            config.write_timeout,
            sink.send(WsMessage::text(text)),
        )
        .await;
        if event_at.is_some() {
            shared
                .events_written
                .send_modify(|n| *n = n.saturating_add(1));
        }
        match sent {
            Some(Ok(())) => bump(&shared.counters.messages_sent),
            Some(Err(_)) => {
                failure = Some("write failed");
                break;
            }
            None => {
                failure = Some("write timed out");
                break;
            }
        }
    }
    // The reader stops with the writer.
    conn.cancel();
    if !shared.stop.is_cancelled() {
        return failure;
    }
    // Stopped by the server (the call is over): say why, then close.
    let reason = {
        let reason = lock(&shared.end_reason);
        if reason.is_empty() {
            "call_ended".to_owned()
        } else {
            reason.clone()
        }
    };
    let closing = async {
        // What was queued before the stop (the result of an `end`) still goes out, in order.
        while let Ok(body) = control.try_recv() {
            *seq = seq.saturating_add(1);
            let message = ServerMessage {
                seq: *seq,
                t_ms: shared.ms_since_t0(clock.now()),
                body,
            };
            if let Ok(text) = serde_json::to_string(&message) {
                if sink.send(WsMessage::text(text)).await.is_err() {
                    return;
                }
                bump(&shared.counters.messages_sent);
            }
        }
        let summary = lock(&shared.summary).take();
        if let Some(summary) = summary {
            *seq = seq.saturating_add(1);
            let usage = ServerMessage {
                seq: *seq,
                t_ms: shared.ms_since_t0(clock.now()),
                body: ServerBody::SessionUsage { summary },
            };
            if let Ok(text) = serde_json::to_string(&usage) {
                if sink.send(WsMessage::text(text)).await.is_err() {
                    return;
                }
                bump(&shared.counters.messages_sent);
            }
        }
        *seq = seq.saturating_add(1);
        let end = ServerMessage {
            seq: *seq,
            t_ms: shared.ms_since_t0(clock.now()),
            body: ServerBody::SessionEnd { reason },
        };
        if let Ok(text) = serde_json::to_string(&end)
            && sink.send(WsMessage::text(text)).await.is_ok()
        {
            bump(&shared.counters.messages_sent);
        }
        let frame = CloseFrame {
            code: CloseCode::Normal,
            reason: "session ended".into(),
        };
        let _ = sink.send(WsMessage::Close(Some(frame))).await;
    };
    let _ = timed(&*clock, config.close_timeout, closing).await;
    None
}

/// Reads the agent's messages and applies them (never waits on the call). `Some(why)` when the
/// connection failed.
async fn reader(
    mut stream: SplitStream<Ws>,
    shared: &Arc<LinkShared>,
    conn: &CancellationToken,
) -> Option<&'static str> {
    loop {
        // No timeout on the read itself: a quiet agent is legitimate. Liveness is the writer's
        // keepalive (three silent intervals take the connection down).
        let next = tokio::select! {
            biased;
            () = conn.cancelled() => return None,
            next = stream.next() => next,
        };
        let now = shared.ms_since_t0(shared.clock.now());
        shared.last_rx_ms.store(now, Ordering::Relaxed);
        match next {
            Some(Ok(WsMessage::Text(text))) => shared.handle(text.as_str()),
            Some(Ok(WsMessage::Binary(_))) => {
                shared.link_error(
                    "bad_message: binary frames are not part of the protocol".to_owned(),
                );
            }
            Some(Ok(WsMessage::Close(_))) | None => {
                conn.cancel();
                return Some("the agent closed the connection");
            }
            Some(Err(_)) => {
                conn.cancel();
                return Some("read failed");
            }
            Some(Ok(_)) => {}
        }
    }
}

/// Maps the call's events to protocol events and queues them (drop-newest, counted).
///
/// Runs until the stop, not until the link is down: the bot's playout is followed from these
/// events, and an `end` whose agent closed its socket right after it still waits on the
/// farewell's playout (see [`ops_loop`]). While the link is down nothing is forwarded (the
/// events are counted, and reported as `events.lost` once the link is back), and the call is
/// put back on its local tools ([`LinkShared::fall_back`]); a link that came back offers the
/// remote tools again ([`LinkShared::restore`]).
async fn pump(mut stream: EventStream, shared: Arc<LinkShared>) {
    let mut status = shared.status.subscribe();
    let mut generation = status.borrow().generation;
    let mut lag_seen = 0u64;
    let mut unreported = 0u64;
    // The local tools were put back for the current outage.
    let mut fell_back = false;
    // The fallback or restoring edit could not be queued yet: retried at each event.
    let mut fall_back_pending = false;
    let mut restore_pending = false;
    loop {
        let event = tokio::select! {
            biased;
            () = shared.stop.cancelled() => return,
            Ok(()) = status.changed() => {
                let now = *status.borrow_and_update();
                if now.state == LinkState::Up {
                    if now.generation != generation {
                        generation = now.generation;
                        fell_back = false;
                        fall_back_pending = false;
                        restore_pending = !shared.restore();
                    }
                } else if !fell_back {
                    fell_back = true;
                    restore_pending = false;
                    fall_back_pending = !shared.fall_back();
                }
                continue;
            }
            event = stream.recv() => event,
        };
        let Some(event) = event else {
            return;
        };
        if fall_back_pending {
            fall_back_pending = !shared.fall_back();
        }
        if restore_pending {
            restore_pending = !shared.restore();
        }
        let state = shared.status.borrow().state;
        if state == LinkState::Up {
            let lag = stream.lost();
            if lag > lag_seen {
                let missed = lag - lag_seen;
                lag_seen = lag;
                unreported = unreported.saturating_add(missed);
                shared
                    .counters
                    .events_lost
                    .fetch_add(missed, Ordering::Relaxed);
            }
            forward(&shared, &event, &mut unreported);
        } else if state == LinkState::Reconnecting
            && body_of(&event).is_some_and(|body| shared.subscription().allows(&body))
        {
            shared.outage_lost.fetch_add(1, Ordering::Relaxed);
            bump(&shared.counters.events_lost);
        }
        // After the event was queued: whoever waits for the playout to end (`end`) answers
        // after the event that said so, not before it (control messages are written first).
        shared.track(&event);
    }
}

/// Queues `event` for the agent (drop-newest, counted in `unreported`, which the next event
/// that fits is preceded by as `events.lost`).
fn forward(shared: &LinkShared, event: &EngineEvent, unreported: &mut u64) {
    let at = shared.clock.now();
    let Some(body) = body_of(event) else {
        return;
    };
    if !shared.subscription().allows(&body) {
        return;
    }
    *unreported = unreported.saturating_add(shared.outage_lost.swap(0, Ordering::Relaxed));
    if *unreported > 0 {
        if shared
            .events
            .try_send((ServerBody::EventsLost { count: *unreported }, at))
            .is_ok()
        {
            shared.events_queued.fetch_add(1, Ordering::AcqRel);
            *unreported = 0;
        } else {
            *unreported = unreported.saturating_add(1);
            bump(&shared.counters.events_lost);
            return;
        }
    }
    if shared.events.try_send((body, at)).is_ok() {
        shared.events_queued.fetch_add(1, Ordering::AcqRel);
    } else {
        *unreported = unreported.saturating_add(1);
        bump(&shared.counters.events_lost);
    }
}

/// Runs `end` and `transfer`, one at a time (they wait on the call and the carrier).
async fn ops_loop(
    mut jobs: mpsc::Receiver<OpsJob>,
    shared: Arc<LinkShared>,
    ops: Option<Arc<dyn TelephonyOps>>,
) {
    loop {
        // On the stop, not on `down`: an agent may close its socket right after `end`.
        let job = tokio::select! {
            biased;
            () = shared.stop.cancelled() => return,
            job = jobs.recv() => job,
        };
        match job {
            Some(OpsJob::End { id, say, reason }) => shared.end(id, say, reason).await,
            Some(OpsJob::Transfer { id, request }) => {
                shared.run_transfer(id, request, ops.as_ref()).await;
            }
            None => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_validation_and_env() {
        assert!(
            AgentLinkConfig::new("ws://127.0.0.1:9000/agent")
                .validate()
                .is_ok()
        );
        assert!(AgentLinkConfig::new("http://x").validate().is_err());
        let mut config = AgentLinkConfig::new("wss://agent.example");
        config.side_effect_wait = Duration::from_secs(115);
        assert!(config.validate().is_err());
        let mut config = AgentLinkConfig::new("wss://agent.example");
        config.event_queue = 0;
        assert!(config.validate().is_err());

        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        assert!(AgentLinkConfig::from_env(env(&[])).unwrap().is_none());
        let config = AgentLinkConfig::from_env(env(&[
            ("BRISVOX_AGENT_URL", " ws://localhost:7000 "),
            ("BRISVOX_AGENT_TOKEN", "s3cret"),
            ("BRISVOX_AGENT_CONNECT_TIMEOUT_MS", "750"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(config.url, "ws://localhost:7000");
        assert_eq!(config.connect_timeout, Duration::from_millis(750));
        assert!(!format!("{config:?}").contains("s3cret"));
        // Reconnecting: on by default, tunable, bounded.
        assert_eq!(config.reconnect_attempts, 5);
        let tuned = AgentLinkConfig::from_env(env(&[
            ("BRISVOX_AGENT_URL", "ws://localhost:7000"),
            ("BRISVOX_AGENT_RECONNECT_ATTEMPTS", "0"),
            ("BRISVOX_AGENT_RECONNECT_WINDOW_MS", "3000"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(tuned.reconnect_attempts, 0);
        assert_eq!(tuned.reconnect_window, Duration::from_secs(3));
        assert!(
            AgentLinkConfig::from_env(env(&[
                ("BRISVOX_AGENT_URL", "ws://x"),
                ("BRISVOX_AGENT_RECONNECT_ATTEMPTS", "lots"),
            ]))
            .is_err()
        );
        let mut bad = AgentLinkConfig::new("ws://x");
        bad.reconnect_attempts = 1_000;
        assert!(bad.validate().is_err());
        let mut bad = AgentLinkConfig::new("ws://x");
        bad.reconnect_backoff_max = Duration::from_millis(1);
        assert!(bad.validate().is_err());
        // Another endpoint keeps the tunables but not the credentials.
        let routed = config.with_endpoint("wss://other.example", None);
        assert_eq!(routed.url, "wss://other.example");
        assert!(routed.auth_token.is_none());
        assert_eq!(routed.connect_timeout, config.connect_timeout);
        assert!(
            AgentLinkConfig::from_env(env(&[
                ("BRISVOX_AGENT_URL", "ws://x"),
                ("BRISVOX_AGENT_HANDSHAKE_TIMEOUT_MS", "soon"),
            ]))
            .is_err()
        );
    }

    #[test]
    fn plaintext_to_a_public_host_is_refused_unless_allowed() {
        // The validation matrix.
        for url in [
            "ws://agent.example.com/agent",
            "ws://8.8.8.8:9000",
            "ws://[2001:db8::1]:9000/a",
            "ws://172.32.0.1",
            "ws://localhost.evil.example",
        ] {
            let config = AgentLinkConfig::new(url);
            let err = config.validate().unwrap_err().to_string();
            assert!(err.contains("plaintext"), "{url}: {err}");
            let mut allowed = AgentLinkConfig::new(url);
            allowed.allow_insecure = true;
            assert!(allowed.validate().is_ok(), "{url}");
        }
        for url in [
            "ws://127.0.0.1:9000",
            "ws://[::1]:9000",
            "ws://10.1.2.3",
            "ws://192.168.0.9",
            "ws://172.20.0.1",
            "ws://169.254.1.1",
            "ws://[fd00::5]",
            "ws://localhost",
            "ws://agent.railway.internal:8080",
            "ws://agent:8080",
        ] {
            assert!(AgentLinkConfig::new(url).validate().is_ok(), "{url}");
        }
        // wss:// is fine for any host.
        assert!(
            AgentLinkConfig::new("wss://agent.example.com")
                .validate()
                .is_ok()
        );
        // Re-routing keeps the switch of the server it came from.
        let mut base = AgentLinkConfig::new("wss://a.example");
        base.allow_insecure = true;
        assert!(
            base.with_endpoint("ws://b.example.com", None)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn errors_and_debug_output_name_the_agent_as_scheme_host_port_only() {
        let secret_url = "ws://user:s3cret@agent.example.com:9000/path/x?token=abc";
        let config = AgentLinkConfig::new(secret_url);
        let refused = config.validate().unwrap_err().to_string();
        assert!(refused.contains("ws://agent.example.com:9000"), "{refused}");
        let shown = format!("{config:?}");
        assert!(shown.contains("ws://agent.example.com:9000"), "{shown}");
        for text in [&refused, &shown] {
            for leaked in ["s3cret", "user:", "/path", "token=", "abc"] {
                assert!(!text.contains(leaked), "{leaked} leaked: {text}");
            }
        }
        let wrong_scheme = AgentLinkConfig::new("http://u:pw@h.example.com/a?k=v");
        let refused = wrong_scheme.validate().unwrap_err().to_string();
        assert!(refused.contains("http://h.example.com"), "{refused}");
        assert!(
            !refused.contains("pw") && !refused.contains("k=v"),
            "{refused}"
        );
    }

    #[test]
    fn insecure_switch_and_private_ca_come_from_the_environment() {
        let env = |pairs: Vec<(&'static str, String)>| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.clone())
            }
        };
        let url = ("BRISVOX_AGENT_URL", "ws://agent.example.com/a".to_owned());
        assert!(AgentLinkConfig::from_env(env(vec![url.clone()])).is_err());
        for on in ["1", "true", "YES", " on "] {
            let config = AgentLinkConfig::from_env(env(vec![
                url.clone(),
                ("BRISVOX_AGENT_ALLOW_INSECURE", on.to_owned()),
            ]))
            .unwrap()
            .unwrap();
            assert!(config.allow_insecure, "{on}");
        }
        for off in ["0", "false", "", "maybe"] {
            assert!(
                AgentLinkConfig::from_env(env(vec![
                    url.clone(),
                    ("BRISVOX_AGENT_ALLOW_INSECURE", off.to_owned()),
                ]))
                .is_err(),
                "{off}"
            );
        }
        // The definition's own switch (`[agent_link] allow_insecure`).
        let config =
            AgentLinkConfig::from_env_for_with(env(vec![]), "ws://agent.example.com", true);
        assert!(config.is_ok_and(|c| c.allow_insecure));
        // A private CA: loaded at configuration time, a bad path or file is a config error.
        let ca = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../brisvox-providers/src/net/testdata/ca.pem"
        );
        let config = AgentLinkConfig::from_env(env(vec![
            ("BRISVOX_AGENT_URL", "wss://agent.example.com".to_owned()),
            ("BRISVOX_AGENT_TLS_CA", ca.to_owned()),
        ]))
        .unwrap()
        .unwrap();
        assert!(config.tls.is_some());
        assert!(format!("{config:?}").contains("private_ca: true"));
        for bad in [
            "/definitely/not/here.pem",
            concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
        ] {
            assert!(
                AgentLinkConfig::from_env(env(vec![
                    ("BRISVOX_AGENT_URL", "wss://agent.example.com".to_owned()),
                    ("BRISVOX_AGENT_TLS_CA", bad.to_owned()),
                ]))
                .is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn subscriptions_filter_events_but_never_errors() {
        let (sub, unknown) = Subscription::of(Some(&["user.text".to_owned(), "nope".to_owned()]));
        assert_eq!(unknown, ["nope"]);
        assert!(sub.allows(&ServerBody::UserText {
            epoch: 1,
            text: Arc::from("x")
        }));
        assert!(!sub.allows(&ServerBody::BotText {
            epoch: 1,
            text: Arc::from("x")
        }));
        assert!(sub.allows(&ServerBody::EventsLost { count: 1 }));
        assert!(sub.allows(&ServerBody::Error {
            stage: "x".to_owned(),
            message: String::new(),
            fatal: false
        }));
        assert!(Subscription::ALL.allows(&ServerBody::BotText {
            epoch: 0,
            text: Arc::from("")
        }));
    }

    #[test]
    fn the_hop_ring_forgets_overwritten_events() {
        let mut ring = HopRing::new();
        let at = brisvox_core::SimClock::new().now();
        ring.record(3, at);
        assert_eq!(ring.sent_at(3), Some(at));
        ring.record(3 + HOP_RING as u64, at);
        assert_eq!(ring.sent_at(3), None);
        assert_eq!(ring.sent_at(4), None);
    }

    #[test]
    fn bounded_text_cuts_at_a_character_boundary() {
        assert_eq!(bounded("abc", 5), "abc");
        assert_eq!(bounded("çaçaç", 3), "ça");
    }
}
