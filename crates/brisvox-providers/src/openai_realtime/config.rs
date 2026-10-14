//! Settings of the OpenAI Realtime adapter and the construction of its WebSocket URL.

use std::time::Duration;

use brisvox_core::Secret;

use super::PROVIDER;
use crate::error::{Error, Result};

/// The provider-side turn detection the adapter asks for when the engine leaves turn detection to
/// the provider ([`S2sTurnDetection::Provider`](crate::s2s::S2sTurnDetection::Provider)).
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum ServerVad {
    /// `server_vad`: voice activity by loudness, with silence ending the turn.
    Silence {
        /// Activation threshold, 0.0..=1.0. `None` leaves the server default (0.5).
        threshold: Option<f32>,
        /// Audio kept from before speech was detected. `None` leaves the default (300 ms).
        prefix_padding_ms: Option<u32>,
        /// Silence that ends the turn. `None` leaves the default.
        silence_duration_ms: Option<u32>,
    },
    /// `semantic_vad`: a model decides when the caller is done.
    Semantic {
        /// `low`, `medium`, `high` or `auto`. `None` leaves the default (`auto`).
        eagerness: Option<Eagerness>,
    },
}

/// How quickly `semantic_vad` ends a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Eagerness {
    /// Lets the caller take their time.
    Low,
    /// The default balance.
    Medium,
    /// Chunks audio sooner.
    High,
    /// The server picks (documented as equal to medium).
    Auto,
}

impl Eagerness {
    /// The wire value.
    pub(crate) const fn wire(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Auto => "auto",
        }
    }
}

/// Settings of the OpenAI Realtime adapter.
///
/// Create it with [`OpenAiRealtimeConfig::new`] and override fields; the struct is
/// `#[non_exhaustive]`. `Debug` never prints the API key ([`Secret`]).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct OpenAiRealtimeConfig {
    /// API key, sent only as `Authorization: Bearer <key>`.
    pub api_key: Secret<String>,
    /// Model (the `model` query parameter). Default `gpt-realtime-2.1`. Letters, digits, `.`,
    /// `_` and `-` only.
    pub model: String,
    /// Voice used when the session config has none. Default `marin`.
    pub voice: String,
    /// Replaces `wss://api.openai.com` (scheme and `host[:port]` only), e.g. `ws://127.0.0.1:9000`
    /// for tests. Plain `ws://` is accepted only for loopback hosts so the key can never travel
    /// in the clear.
    pub base_url: Option<String>,
    /// Provider-side turn detection settings (used when the engine leaves turn detection to the
    /// provider). Default: `server_vad` with the server's defaults.
    pub vad: ServerVad,
    /// The model for the caller's transcript (`audio.input.transcription.model`) when the session
    /// asks for one. `None` (default) never sends a transcription block: the field layout is
    /// UNVERIFIED in `docs/providers/openai-realtime.md`, so it is opt-in.
    pub transcription_model: Option<String>,
    /// `OpenAI-Safety-Identifier` header (a stable, privacy-preserving end-user id, for example a
    /// hash). `None` sends no header.
    pub safety_identifier: Option<String>,
    /// Deadline of every network wait: the connect (TCP, TLS and the WebSocket handshake
    /// together), each of the two setup answers (`session.created`, `session.updated`), each write
    /// and the hand-over of an event to the engine. Default 10 s.
    pub io_timeout: Duration,
    /// A WebSocket ping goes out after this long without any message RECEIVED (outbound audio
    /// proves nothing about a half-open connection); no sign of life within `io_timeout` after
    /// it ends the session with a timeout error. Default 20 s.
    pub keepalive_interval: Duration,
    /// Largest server message accepted. Default 4 MiB (audio deltas are far smaller; this only
    /// bounds a hostile server).
    pub max_message_bytes: usize,
    /// The longest a session lives at the provider (reported by
    /// [`S2sProvider::session_limit`](crate::s2s::S2sProvider::session_limit) so the engine can
    /// rotate before it). Default 60 minutes (the documented maximum).
    pub max_session: Duration,
    /// How assistant turns of [`S2sConfig::history`](crate::s2s::S2sConfig::history) are seeded:
    /// `false` (default) as assistant `message` items with `output_text`; `true` as `system`
    /// messages prefixed "The assistant said earlier:", for use when the server rejects the
    /// first form (UNVERIFIED, see `docs/providers/openai-realtime.md`).
    pub seed_assistant_as_system: bool,
}

impl OpenAiRealtimeConfig {
    /// Defaults for `api_key`.
    pub fn new(api_key: Secret<String>) -> Self {
        Self {
            api_key,
            model: "gpt-realtime-2.1".to_owned(),
            voice: "marin".to_owned(),
            base_url: None,
            vad: ServerVad::Silence {
                threshold: None,
                prefix_padding_ms: None,
                silence_duration_ms: None,
            },
            transcription_model: None,
            safety_identifier: None,
            io_timeout: Duration::from_secs(10),
            keepalive_interval: Duration::from_secs(20),
            max_message_bytes: 4 << 20,
            max_session: Duration::from_secs(60 * 60),
            seed_assistant_as_system: false,
        }
    }

    /// Checks the settings that would otherwise produce a malformed URL or an unsafe connection.
    pub(crate) fn validate(&self) -> Result<()> {
        let bad = |detail: &str| {
            Err(Error::Protocol {
                provider: PROVIDER,
                detail: format!("invalid config: {detail}"),
            })
        };
        if !is_token(&self.model) {
            return bad("model must be letters, digits, '.', '_' or '-'");
        }
        if !is_token(&self.voice) {
            return bad("voice must be letters, digits, '.', '_' or '-'");
        }
        if self.io_timeout.is_zero() || self.keepalive_interval.is_zero() {
            return bad("io_timeout and keepalive_interval must be positive");
        }
        if self.max_session.is_zero() {
            return bad("max_session must be positive");
        }
        if self.max_message_bytes < 1024 {
            return bad("max_message_bytes must be at least 1024");
        }
        if let ServerVad::Silence {
            threshold: Some(t), ..
        } = self.vad
            && !(0.0..=1.0).contains(&t)
        {
            return bad("vad threshold must be 0.0..=1.0");
        }
        if self
            .transcription_model
            .as_deref()
            .is_some_and(|m| !is_token(m))
        {
            return bad("transcription_model must be letters, digits, '.', '_' or '-'");
        }
        if let Some(id) = &self.safety_identifier
            && (id.is_empty() || !id.bytes().all(|b| b.is_ascii_graphic()))
        {
            return bad("safety_identifier must be printable ASCII without spaces");
        }
        if let Some(base) = &self.base_url {
            let Some((scheme, rest)) = base.split_once("://") else {
                return bad("base_url must be ws:// or wss://");
            };
            if !matches!(scheme, "ws" | "wss") {
                return bad("base_url must be ws:// or wss://");
            }
            let Some(host) = parse_authority(rest) else {
                return bad("base_url must be scheme and host[:port] only");
            };
            if scheme == "ws" && !is_loopback_host(host) {
                return bad("plain ws:// is only allowed for loopback hosts");
            }
        }
        Ok(())
    }

    /// The `host[:port]` and whether TLS is used.
    pub(crate) fn authority(&self) -> (String, bool) {
        match &self.base_url {
            Some(base) => {
                let (scheme, rest) = base.split_once("://").unwrap_or(("wss", base));
                (rest.to_owned(), scheme == "wss")
            }
            None => ("api.openai.com".to_owned(), true),
        }
    }

    /// The full WebSocket URL of a session (never contains the key).
    pub(crate) fn url(&self) -> String {
        let (authority, tls) = self.authority();
        let scheme = if tls { "wss" } else { "ws" };
        format!("{scheme}://{authority}/v1/realtime?model={}", self.model)
    }
}

fn is_token(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Strictly parses `host[:port]` and returns the host; anything else (userinfo, path, query,
/// spaces) is `None`, so the host that is checked is exactly the host that is connected to.
fn parse_authority(authority: &str) -> Option<&str> {
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (inner, after) = v6.split_once(']')?;
            let port = match after {
                "" => None,
                _ => Some(after.strip_prefix(':')?),
            };
            if inner.is_empty() || !inner.chars().all(|c| c.is_ascii_hexdigit() || c == ':') {
                return None;
            }
            (inner, port)
        }
        None => {
            let (host, port) = match authority.split_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            };
            if host.is_empty()
                || !host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
            {
                return None;
            }
            (host, port)
        }
    };
    if let Some(port) = port
        && (port.is_empty() || port.len() > 5 || !port.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    Some(host)
}

/// Whether a host returned by [`parse_authority`] is a loopback host.
fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> OpenAiRealtimeConfig {
        OpenAiRealtimeConfig::new(Secret::new("k".to_owned()))
    }

    #[test]
    fn the_default_url_names_the_model_and_never_the_key() {
        let cfg = OpenAiRealtimeConfig::new(Secret::new("sk-secret".to_owned()));
        assert_eq!(
            cfg.url(),
            "wss://api.openai.com/v1/realtime?model=gpt-realtime-2.1"
        );
        assert!(!format!("{cfg:?}").contains("sk-secret"));
    }

    #[test]
    fn plain_ws_is_loopback_only() {
        let mut cfg = config();
        cfg.base_url = Some("ws://127.0.0.1:9000".to_owned());
        assert!(cfg.validate().is_ok());
        assert_eq!(
            cfg.url(),
            "ws://127.0.0.1:9000/v1/realtime?model=gpt-realtime-2.1"
        );
        cfg.base_url = Some("ws://example.com".to_owned());
        assert!(cfg.validate().is_err());
        cfg.base_url = Some("wss://user@example.com".to_owned());
        assert!(cfg.validate().is_err());
        cfg.base_url = Some("wss://example.com/path".to_owned());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn tokens_are_checked() {
        let mut cfg = config();
        cfg.model = "a b".to_owned();
        assert!(cfg.validate().is_err());
        let mut cfg = config();
        cfg.voice = String::new();
        assert!(cfg.validate().is_err());
        let mut cfg = config();
        cfg.vad = ServerVad::Silence {
            threshold: Some(1.5),
            prefix_padding_ms: None,
            silence_duration_ms: None,
        };
        assert!(cfg.validate().is_err());
    }
}
