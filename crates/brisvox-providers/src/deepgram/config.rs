//! Configuration of the Deepgram adapter and the construction of its `/v1/listen` URL.

use std::time::Duration;

use brisvox_core::Secret;

use super::PROVIDER;
use crate::error::{Error, Result};
use crate::stt::SttConfig;

/// How long Deepgram waits for audio before closing a connection (`NET-0001`).
const NO_AUDIO_CLOSE: Duration = Duration::from_secs(10);

/// Deepgram hosting region (same API keys and paths everywhere; strict routing, no fallback to
/// another region when one is down).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Region {
    /// `api.deepgram.com` (global).
    #[default]
    Global,
    /// `api.eu.deepgram.com` (requests never leave the EU).
    Eu,
    /// `api.au.deepgram.com`.
    Au,
    /// `api.in.deepgram.com`.
    India,
}

impl Region {
    /// The hostname of this region's API.
    pub const fn host(self) -> &'static str {
        match self {
            Self::Global => "api.deepgram.com",
            Self::Eu => "api.eu.deepgram.com",
            Self::Au => "api.au.deepgram.com",
            Self::India => "api.in.deepgram.com",
        }
    }
}

/// Audio encoding of the bytes sent in [`SttInput::Audio`](crate::stt::SttInput::Audio).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Encoding {
    /// Raw PCM16 little-endian mono (`encoding=linear16`).
    #[default]
    Linear16,
    /// G.711 mu-law mono (`encoding=mulaw`), e.g. telephony audio passed through unchanged. The
    /// sample rate defaults to the one in the [`SttConfig`] (8000 for telephony).
    Mulaw,
}

impl Encoding {
    /// The `encoding` query value.
    pub const fn param(self) -> &'static str {
        match self {
            Self::Linear16 => "linear16",
            Self::Mulaw => "mulaw",
        }
    }

    /// Bytes per sample.
    pub const fn bytes_per_sample(self) -> u64 {
        match self {
            Self::Linear16 => 2,
            Self::Mulaw => 1,
        }
    }

    /// The byte of a digital-silence sample.
    pub(crate) const fn silence_byte(self) -> u8 {
        match self {
            Self::Linear16 => 0x00,
            Self::Mulaw => 0xFF,
        }
    }
}

/// Provider-side endpointing (`endpointing` query parameter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Endpointing {
    /// `endpointing=false`: Deepgram never sets `speech_final` on its own; the utterance ends
    /// with [`SttInput::Finalize`](crate::stt::SttInput::Finalize) (or `UtteranceEnd`).
    Off,
    /// `endpointing=<ms>`: this much trailing silence finalizes the segment and sets
    /// `speech_final`.
    Ms(u32),
}

/// Settings of the Deepgram streaming STT adapter.
///
/// Create it with [`DeepgramConfig::new`] and override fields; the struct is
/// `#[non_exhaustive]` so that new settings are not a breaking change. `Debug` never prints the
/// API key ([`Secret`]).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DeepgramConfig {
    /// API key, sent only as `Authorization: Token <key>`.
    pub api_key: Secret<String>,
    /// Model (`model` parameter). Default `nova-3`. Letters, digits, `.`, `_` and `-` only.
    pub model: String,
    /// Language (`language` parameter, e.g. `tr`, `en`) used when the [`SttConfig`] of a session
    /// has none. `None` leaves the parameter out (Deepgram's default is `en`).
    pub language: Option<String>,
    /// Encoding of the audio bytes. Default [`Encoding::Linear16`].
    pub encoding: Encoding,
    /// Sample rate override in Hz. `None` uses the [`SttConfig`] sample rate.
    pub sample_rate_hz: Option<u32>,
    /// `interim_results`. Default `true`: interims are what lets the engine start thinking
    /// before the user stops talking.
    pub interim_results: bool,
    /// Provider endpointing. Default `Ms(300)` (documented as suitable for conversation). The
    /// engine's own VAD and [`SttInput::Finalize`](crate::stt::SttInput::Finalize) normally
    /// end the utterance first; this is the fallback.
    pub endpointing: Endpointing,
    /// `utterance_end_ms` (1000..=5000, needs `interim_results`). `None` leaves it out. When set,
    /// `UtteranceEnd` messages close an utterance that endpointing did not.
    pub utterance_end_ms: Option<u32>,
    /// `smart_format`. Default `true`.
    pub smart_format: bool,
    /// `punctuate`. Default `false` (`smart_format` already punctuates).
    pub punctuate: bool,
    /// Nova-3 keyterm prompting (`keyterm`, repeated). Empty by default.
    pub keyterms: Vec<String>,
    /// Languages the speaker is expected to use (`language_hint`, repeated), e.g. `["en", "tr"]`.
    /// Deepgram biases `language=multi` detection towards them ("hints bias detection; they do
    /// not restrict it"); with any other `language` the parameter is left out (Deepgram ignores
    /// it there). Empty by default.
    pub language_hints: Vec<String>,
    /// Hosting region. Default [`Region::Global`].
    pub region: Region,
    /// Replaces `wss://<region host>` (no path), e.g. `ws://127.0.0.1:9000` for tests. Plain
    /// `ws://` is accepted only for loopback hosts so the key can never travel in the clear.
    pub base_url: Option<String>,
    /// Deadline of every network wait: the connect (TCP, TLS and WebSocket handshake together),
    /// each write, and the silence of the server after audio was sent (the connection is then
    /// replaced like any lost one; with no reconnect left the session ends with
    /// `Core(Timeout)`). Default 10 s.
    pub io_timeout: Duration,
    /// Send `{"type":"KeepAlive"}` after this long without audio (documented: every 3-5 s).
    /// Also the delay after which a connection that has seen no audio at all gets 20 ms of
    /// digital silence (KeepAlive alone does not prevent the 10 s `NET-0001` close). Must be
    /// below 10 s (rejected otherwise); should be below `io_timeout`, or a pause in the audio
    /// looks like a silent server to the response watchdog (a warning is logged). Default 4 s.
    pub keepalive_interval: Duration,
    /// How long to wait for the answer to a `Finalize` before ending the utterance with a
    /// synthetic final (Deepgram does not promise an answer for little buffered audio).
    /// Default 800 ms.
    pub finalize_timeout: Duration,
    /// A `Finalize` is answered at once (empty `speech_final` transcript with `from_finalize`,
    /// no round trip, nothing sent) when nothing is in progress and the latest `Results` message
    /// of this connection already reaches to within this much of the end of the audio sent: the
    /// recognizer has seen everything and found nothing to flush, which is the usual state when
    /// endpointing fired before the engine's own end-of-speech detection. Choose it a little
    /// above `endpointing` plus the recognition latency; speech that is newer than this much
    /// audio would be reported as a late utterance instead (see the module docs). `ZERO`
    /// disables the shortcut. Default 500 ms.
    pub finalize_settle_gap: Duration,
    /// How long to wait for the server to finish after `CloseStream` on hang-up. Default 1 s.
    pub close_timeout: Duration,
    /// Reconnect attempts per dropped connection (0 disables reconnecting). The counter starts
    /// over once a new connection delivers a message. Default 1.
    pub max_reconnects: u32,
    /// Wait before the first reconnect attempt; doubles per attempt (cap 5 s). Default 100 ms.
    pub reconnect_backoff: Duration,
    /// How much unfinalized audio is kept for replay after a reconnect (the oldest is dropped
    /// first). Default 8 s.
    pub replay_window: Duration,
    /// Replayed audio is paced at this multiple of real time (Deepgram accepts at most 1.25x).
    /// A backlog of `B` seconds drains in `B / (speed - 1)` seconds while live audio keeps
    /// arriving. Default 1.25.
    pub replay_speed: f64,
}

impl DeepgramConfig {
    /// Defaults for `api_key`.
    pub fn new(api_key: Secret<String>) -> Self {
        Self {
            api_key,
            model: "nova-3".to_owned(),
            language: None,
            encoding: Encoding::Linear16,
            sample_rate_hz: None,
            interim_results: true,
            endpointing: Endpointing::Ms(300),
            utterance_end_ms: None,
            smart_format: true,
            punctuate: false,
            keyterms: Vec::new(),
            language_hints: Vec::new(),
            region: Region::Global,
            base_url: None,
            io_timeout: Duration::from_secs(10),
            keepalive_interval: Duration::from_secs(4),
            finalize_timeout: Duration::from_millis(800),
            finalize_settle_gap: Duration::from_millis(500),
            close_timeout: Duration::from_secs(1),
            max_reconnects: 1,
            reconnect_backoff: Duration::from_millis(100),
            replay_window: Duration::from_secs(8),
            replay_speed: 1.25,
        }
    }

    /// The sample rate sent to Deepgram for a session.
    pub(crate) fn sample_rate(&self, stt: &SttConfig) -> u32 {
        self.sample_rate_hz.unwrap_or_else(|| stt.sample_rate.hz())
    }

    /// Bytes of audio per second for a session.
    pub(crate) fn bytes_per_second(&self, stt: &SttConfig) -> u64 {
        u64::from(self.sample_rate(stt)).saturating_mul(self.encoding.bytes_per_sample())
    }

    /// Checks the settings that would otherwise produce a malformed URL or an unsafe
    /// connection.
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
        if self.language.as_deref().is_some_and(|l| !is_token(l)) {
            return bad("language must be letters, digits, '.', '_' or '-'");
        }
        if self.language_hints.len() > 16 || self.language_hints.iter().any(|l| !is_token(l)) {
            return bad(
                "language_hints: at most 16 language codes of letters, digits, '.', '_' or '-'",
            );
        }
        if let Some(ms) = self.utterance_end_ms {
            if !(1000..=5000).contains(&ms) {
                return bad("utterance_end_ms must be 1000..=5000");
            }
            if !self.interim_results {
                return bad("utterance_end_ms needs interim_results");
            }
        }
        if self
            .sample_rate_hz
            .is_some_and(|hz| !(1..=384_000).contains(&hz))
        {
            return bad("sample_rate_hz must be 1..=384000");
        }
        if self.replay_window > Duration::from_secs(60) {
            return bad("replay_window must be at most 60 s");
        }
        if self.io_timeout.is_zero() || self.keepalive_interval.is_zero() {
            return bad("io_timeout and keepalive_interval must be positive");
        }
        if self.keepalive_interval >= NO_AUDIO_CLOSE {
            return bad("keepalive_interval must be below 10 s (the NET-0001 close)");
        }
        if self.keepalive_interval >= self.io_timeout {
            tracing::warn!(
                "deepgram keepalive_interval >= io_timeout: a pause in the audio longer than                  io_timeout is treated as a silent server"
            );
        }
        if !self.replay_speed.is_finite() || self.replay_speed < 1.0 {
            return bad("replay_speed must be at least 1.0");
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

    /// The `host[:port]` and whether TLS is used, from `base_url` or the region.
    pub(crate) fn authority(&self) -> (String, bool) {
        match &self.base_url {
            Some(base) => {
                let (scheme, rest) = base.split_once("://").unwrap_or(("wss", base));
                (rest.to_owned(), scheme == "wss")
            }
            None => (self.region.host().to_owned(), true),
        }
    }

    /// The full WebSocket URL for a session, query included.
    pub(crate) fn url(&self, stt: &SttConfig) -> String {
        let (authority, tls) = self.authority();
        let scheme = if tls { "wss" } else { "ws" };
        format!("{scheme}://{authority}/v1/listen?{}", self.query(stt))
    }

    /// The query string, in a fixed order.
    pub(crate) fn query(&self, stt: &SttConfig) -> String {
        let mut q = String::with_capacity(128);
        let mut add = |key: &str, value: &str| {
            if !q.is_empty() {
                q.push('&');
            }
            q.push_str(key);
            q.push('=');
            q.push_str(&percent_encode(value));
        };
        add("model", &self.model);
        let language = stt.language.as_deref().or(self.language.as_deref());
        if let Some(lang) = language {
            add("language", lang);
        }
        add("encoding", self.encoding.param());
        add("sample_rate", &self.sample_rate(stt).to_string());
        add("interim_results", bool_param(self.interim_results));
        match self.endpointing {
            Endpointing::Off => add("endpointing", "false"),
            Endpointing::Ms(ms) => add("endpointing", &ms.to_string()),
        }
        if let Some(ms) = self.utterance_end_ms {
            add("utterance_end_ms", &ms.to_string());
        }
        add("smart_format", bool_param(self.smart_format));
        if self.punctuate {
            add("punctuate", "true");
        }
        for term in &self.keyterms {
            add("keyterm", term);
        }
        if language == Some("multi") {
            for hint in &self.language_hints {
                add("language_hint", hint);
            }
        }
        q
    }
}

fn bool_param(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn is_token(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Strictly parses `host[:port]` (a bracketed IPv6 literal, or a name / IPv4 address of
/// letters, digits, `-` and `.`; a digits-only port) and returns the host. Anything else
/// (userinfo `@`, backslash, path, query, spaces, a stray `:`) is `None`, so the host that is
/// checked is exactly the host that is connected to.
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

/// Percent-encodes everything but RFC 3986 unreserved characters.
pub(crate) fn percent_encode(value: &str) -> String {
    let hex = |nibble: u8| {
        char::from_digit(u32::from(nibble), 16).map_or('0', |c| c.to_ascii_uppercase())
    };
    let mut out = String::with_capacity(value.len());
    for &b in value.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(hex(b >> 4));
            out.push(hex(b & 0x0F));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use brisvox_core::frame::SampleRate;

    use super::*;

    fn stt(language: Option<&str>) -> SttConfig {
        SttConfig {
            sample_rate: SampleRate::Hz16000,
            language: language.map(Into::into),
        }
    }

    fn config() -> DeepgramConfig {
        DeepgramConfig::new(Secret::new("k".to_owned()))
    }

    #[test]
    fn the_default_url_is_the_documented_recommended_set() {
        assert_eq!(
            config().url(&stt(Some("tr"))),
            "wss://api.deepgram.com/v1/listen?model=nova-3&language=tr&encoding=linear16\
             &sample_rate=16000&interim_results=true&endpointing=300&smart_format=true"
        );
    }

    #[test]
    fn every_option_reaches_the_query_in_a_fixed_order() {
        let mut c = config();
        c.encoding = Encoding::Mulaw;
        c.sample_rate_hz = Some(8000);
        c.endpointing = Endpointing::Off;
        c.utterance_end_ms = Some(1000);
        c.punctuate = true;
        c.smart_format = false;
        c.language = Some("en".to_owned());
        c.keyterms = vec!["Brisvox".to_owned(), "a b&c".to_owned()];
        c.region = Region::Eu;
        assert_eq!(
            c.url(&stt(None)),
            "wss://api.eu.deepgram.com/v1/listen?model=nova-3&language=en&encoding=mulaw\
             &sample_rate=8000&interim_results=true&endpointing=false&utterance_end_ms=1000\
             &smart_format=false&punctuate=true&keyterm=Brisvox&keyterm=a%20b%26c"
        );
        // The session language wins over the configured default.
        assert!(c.query(&stt(Some("tr"))).contains("language=tr"));
    }

    #[test]
    fn multi_language_sends_the_hints_and_a_fixed_language_does_not() {
        let mut c = config();
        c.language = Some("multi".to_owned());
        c.language_hints = vec!["en".to_owned(), "tr".to_owned()];
        let q = c.query(&stt(None));
        assert!(q.contains("&language=multi&"), "{q}");
        assert!(q.ends_with("&language_hint=en&language_hint=tr"), "{q}");
        // Hints do nothing with another language and are left out.
        assert!(!c.query(&stt(Some("tr"))).contains("language_hint"));
        c.language_hints = vec!["en&x=1".to_owned()];
        assert!(c.validate().is_err());
    }

    #[test]
    fn regions_map_to_their_hosts_and_base_url_overrides_them() {
        assert_eq!(Region::Global.host(), "api.deepgram.com");
        assert_eq!(Region::Eu.host(), "api.eu.deepgram.com");
        assert_eq!(Region::Au.host(), "api.au.deepgram.com");
        assert_eq!(Region::India.host(), "api.in.deepgram.com");
        let mut c = config();
        c.base_url = Some("ws://127.0.0.1:4000".to_owned());
        assert_eq!(c.authority(), ("127.0.0.1:4000".to_owned(), false));
        assert!(
            c.url(&stt(None))
                .starts_with("ws://127.0.0.1:4000/v1/listen?")
        );
    }

    #[test]
    fn validation_rejects_unsafe_or_malformed_settings() {
        assert!(config().validate().is_ok());
        let mut c = config();
        c.model = "nova-3&evil=1".to_owned();
        assert!(c.validate().is_err());
        let mut c = config();
        c.utterance_end_ms = Some(500);
        assert!(c.validate().is_err());
        let mut c = config();
        c.utterance_end_ms = Some(1000);
        c.interim_results = false;
        assert!(c.validate().is_err());
        let mut c = config();
        c.base_url = Some("ws://evil.example.com".to_owned());
        assert!(
            c.validate().is_err(),
            "the key must not travel in the clear"
        );
        for ok in [
            "ws://127.0.0.1:1",
            "ws://localhost:9",
            "ws://[::1]:9",
            "wss://proxy.example",
        ] {
            let mut c = config();
            c.base_url = Some(ok.to_owned());
            assert!(c.validate().is_ok(), "{ok}");
        }
        // Anything that is not strictly host[:port] is refused, in particular userinfo, which
        // would make the part that looks like a loopback host a username and send the key to
        // whatever follows the `@`.
        for bad in [
            "ws://127.0.0.1:1/path",
            "ws://127.0.0.1:80@evil.example",
            "ws://[::1]@evil.example",
            "ws://[::1]:9@evil.example",
            "ws://localhost@evil.example",
            "ws://localhost:x",
            "ws://localhost:",
            r"ws://127.0.0.1\@evil.example",
            "ws://127.0.0.1:1:2",
            "ws://[::1",
            "ws://[::1]x",
            "ws://",
            "wss://user@proxy.example",
            "wss://proxy.example:99999999",
            "wss://proxy.example?x=1",
        ] {
            let mut c = config();
            c.base_url = Some(bad.to_owned());
            assert!(c.validate().is_err(), "{bad}");
        }
        // KeepAlive slower than the server's no-audio close can never work.
        let mut c = config();
        c.keepalive_interval = Duration::from_secs(10);
        assert!(c.validate().is_err());
        c.keepalive_interval = Duration::from_millis(9_999);
        assert!(c.validate().is_ok());
        let mut c = config();
        c.replay_speed = 0.5;
        assert!(c.validate().is_err());
        for hz in [0, 1_000_000] {
            let mut c = config();
            c.sample_rate_hz = Some(hz);
            assert!(c.validate().is_err(), "{hz}");
        }
        let mut c = config();
        c.replay_window = Duration::from_secs(120);
        assert!(c.validate().is_err());
    }

    #[test]
    fn percent_encoding_keeps_unreserved_characters_and_escapes_the_rest() {
        assert_eq!(percent_encode("aZ09-._~"), "aZ09-._~");
        assert_eq!(percent_encode("a b/ç"), "a%20b%2F%C3%A7");
    }

    #[test]
    fn debug_output_hides_the_key() {
        let c = DeepgramConfig::new(Secret::new("dg-secret-key-123".to_owned()));
        assert!(!format!("{c:?}").contains("dg-secret-key-123"));
    }
}
