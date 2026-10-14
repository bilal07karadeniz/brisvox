//! Settings of the Gemini Live adapter and the construction of its WebSocket URL.

use std::time::Duration;

use brisvox_core::Secret;

use super::PROVIDER;
use crate::error::{Error, Result};

/// How readily the automatic activity detection reacts (`startOfSpeechSensitivity` and
/// `endOfSpeechSensitivity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Sensitivity {
    /// Detects more, sooner (`*_SENSITIVITY_HIGH`).
    High,
    /// Detects less (`*_SENSITIVITY_LOW`).
    Low,
}

impl Sensitivity {
    /// The wire value for the start-of-speech field.
    pub(crate) const fn start_wire(self) -> &'static str {
        match self {
            Self::High => "START_SENSITIVITY_HIGH",
            Self::Low => "START_SENSITIVITY_LOW",
        }
    }

    /// The wire value for the end-of-speech field.
    pub(crate) const fn end_wire(self) -> &'static str {
        match self {
            Self::High => "END_SENSITIVITY_HIGH",
            Self::Low => "END_SENSITIVITY_LOW",
        }
    }
}

/// Settings of the Gemini Live adapter.
///
/// Create it with [`GeminiLiveConfig::new`] and override fields; the struct is
/// `#[non_exhaustive]`. `Debug` never prints the API key ([`Secret`]).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GeminiLiveConfig {
    /// API key. By default it travels as the `key` query parameter of the WebSocket URL (the only
    /// way the documentation shows), see [`Self::key_in_header`].
    pub api_key: Secret<String>,
    /// Model, without the `models/` prefix. Default `gemini-3.8-live`. Letters, digits, `.`, `_`
    /// and `-` only.
    pub model: String,
    /// Voice used when the session config has none (a prebuilt voice name). Default `Kore`.
    pub voice: String,
    /// Replaces `wss://generativelanguage.googleapis.com` (scheme and `host[:port]` only), e.g.
    /// `ws://127.0.0.1:9000` for tests. Plain `ws://` is accepted only for loopback hosts so the
    /// key can never travel in the clear.
    pub base_url: Option<String>,
    /// Send the key as an `x-goog-api-key` header instead of the URL query (keeps it out of
    /// proxy and access logs). UNVERIFIED for the WebSocket endpoint (see
    /// `docs/providers/gemini-live.md`), so it is opt-in.
    pub key_in_header: bool,
    /// `startOfSpeechSensitivity` of the provider-side activity detection. `None` leaves the
    /// server default.
    pub start_sensitivity: Option<Sensitivity>,
    /// `endOfSpeechSensitivity`. `None` leaves the server default.
    pub end_sensitivity: Option<Sensitivity>,
    /// `prefixPaddingMs`: audio kept from before speech was detected. `None` leaves the default.
    pub prefix_padding_ms: Option<u32>,
    /// `silenceDurationMs`: silence that ends the caller's turn. `None` leaves the default
    /// (about 800 ms; the docs suggest 500 to 800).
    pub silence_duration_ms: Option<u32>,
    /// Deadline of every network wait: the connect (TCP, TLS and the WebSocket handshake
    /// together), the `setupComplete` answer, each write and the hand-over of an event to the
    /// engine. Default 10 s.
    pub io_timeout: Duration,
    /// A WebSocket ping goes out after this long without any message RECEIVED; no sign of life
    /// within `io_timeout` after it ends the session with a timeout error. Default 20 s.
    pub keepalive_interval: Duration,
    /// Largest server message accepted. Default 4 MiB.
    pub max_message_bytes: usize,
    /// How long one connection lives (reported by
    /// [`S2sProvider::session_limit`](crate::s2s::S2sProvider::session_limit) so the engine
    /// rotates before it). Default 10 minutes, the documented connection lifetime ("around 10
    /// minutes"); the server also sends a `goAway` message before it.
    pub connection_lifetime: Duration,
}

impl GeminiLiveConfig {
    /// Defaults for `api_key`.
    pub fn new(api_key: Secret<String>) -> Self {
        Self {
            api_key,
            model: "gemini-3.8-live".to_owned(),
            voice: "Kore".to_owned(),
            base_url: None,
            key_in_header: false,
            start_sensitivity: None,
            end_sensitivity: None,
            prefix_padding_ms: None,
            silence_duration_ms: None,
            io_timeout: Duration::from_secs(10),
            keepalive_interval: Duration::from_secs(20),
            max_message_bytes: 4 << 20,
            connection_lifetime: Duration::from_secs(10 * 60),
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
        // RFC 3986 unreserved characters only: they need no escaping in the `key=` query and
        // cannot add a parameter or a header. Google's newer keys contain a '.' ("AQ.…").
        let key = self.api_key.expose();
        if key.is_empty() || !is_token(key) {
            return bad("the API key must be letters, digits, '.', '_' or '-'");
        }
        if self.io_timeout.is_zero()
            || self.keepalive_interval.is_zero()
            || self.connection_lifetime.is_zero()
        {
            return bad("io_timeout, keepalive_interval and connection_lifetime must be positive");
        }
        if self.max_message_bytes < 1024 {
            return bad("max_message_bytes must be at least 1024");
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
            None => ("generativelanguage.googleapis.com".to_owned(), true),
        }
    }

    /// The WebSocket URL of a session, WITHOUT the key.
    pub(crate) fn url(&self) -> String {
        let (authority, tls) = self.authority();
        let scheme = if tls { "wss" } else { "ws" };
        format!(
            "{scheme}://{authority}/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent"
        )
    }

    /// The URL to connect to: [`Self::url`] plus the `key` query parameter unless the key goes
    /// in a header. Contains the key: never log it.
    pub(crate) fn url_with_key(&self) -> String {
        if self.key_in_header {
            self.url()
        } else {
            format!("{}?key={}", self.url(), self.api_key.expose())
        }
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

    fn config() -> GeminiLiveConfig {
        GeminiLiveConfig::new(Secret::new("AIza-key_1".to_owned()))
    }

    #[test]
    fn the_key_is_in_the_connect_url_only_and_never_in_debug() {
        let cfg = config();
        assert!(!cfg.url().contains("AIza"));
        assert!(
            cfg.url_with_key()
                .ends_with("BidiGenerateContent?key=AIza-key_1")
        );
        assert!(!format!("{cfg:?}").contains("AIza"));
        let mut cfg = config();
        cfg.key_in_header = true;
        assert_eq!(cfg.url_with_key(), cfg.url());
    }

    #[test]
    fn plain_ws_is_loopback_only() {
        let mut cfg = config();
        cfg.base_url = Some("ws://127.0.0.1:9000".to_owned());
        assert!(cfg.validate().is_ok());
        assert!(cfg.url().starts_with("ws://127.0.0.1:9000/ws/"));
        cfg.base_url = Some("ws://example.com".to_owned());
        assert!(cfg.validate().is_err());
        cfg.base_url = Some("wss://user@example.com".to_owned());
        assert!(cfg.validate().is_err());
        cfg.base_url = Some("wss://example.com/path".to_owned());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn tokens_and_keys_are_checked() {
        let mut cfg = config();
        cfg.model = "a b".to_owned();
        assert!(cfg.validate().is_err());
        let mut cfg = config();
        cfg.voice = String::new();
        assert!(cfg.validate().is_err());
        // A key that could add a query parameter or header is refused.
        let mut cfg = config();
        cfg.api_key = Secret::new("k&x=1".to_owned());
        assert!(cfg.validate().is_err());
        // Google's newer key format ("AQ." prefix) is accepted.
        let mut cfg = config();
        cfg.api_key = Secret::new("AQ.Ab8_x-Y9".to_owned());
        assert!(cfg.validate().is_ok());
        let mut cfg = config();
        cfg.connection_lifetime = Duration::ZERO;
        assert!(cfg.validate().is_err());
    }
}
