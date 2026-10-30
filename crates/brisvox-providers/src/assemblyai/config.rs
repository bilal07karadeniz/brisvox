//! Configuration of the AssemblyAI adapter and the construction of its `/v3/ws` URL.

use std::time::Duration;

use brisvox_core::Secret;

use super::PROVIDER;
use crate::error::{Error, Result};
use crate::stt::SttConfig;

/// Shortest audio chunk the server accepts (`docs/providers/assemblyai.md`, "Audio chunk rule").
pub(crate) const MIN_CHUNK: Duration = Duration::from_millis(50);

/// Longest audio chunk the adapter sends. The server accepts up to 1000 ms; half of that leaves
/// a wide margin for rounding.
pub(crate) const MAX_CHUNK: Duration = Duration::from_millis(500);

/// How [`AssemblyAiConfig::bytes_in`] rounds to whole samples.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Round {
    /// Towards more audio (lower bounds).
    Up,
    /// Towards less audio (upper bounds).
    Down,
}

/// Interval of the server's `Heartbeat` messages when `session_heartbeat` is on.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);

/// AssemblyAI hosting zone (same API keys everywhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Region {
    /// `streaming.assemblyai.com`: edge routing to the nearest region (US or EU).
    #[default]
    Edge,
    /// `streaming.us.assemblyai.com`: US regions only.
    Us,
    /// `streaming.eu.assemblyai.com`: EU regions only.
    Eu,
}

impl Region {
    /// The hostname of this zone's streaming API.
    pub const fn host(self) -> &'static str {
        match self {
            Self::Edge => "streaming.assemblyai.com",
            Self::Us => "streaming.us.assemblyai.com",
            Self::Eu => "streaming.eu.assemblyai.com",
        }
    }
}

/// Audio encoding of the bytes sent in [`SttInput::Audio`](crate::stt::SttInput::Audio).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Encoding {
    /// Raw PCM16 little-endian mono (`encoding=pcm_s16le`).
    #[default]
    Pcm16,
    /// G.711 mu-law mono (`encoding=pcm_mulaw`), e.g. telephony audio passed through unchanged.
    /// The sample rate is the one in the [`SttConfig`] (8000 for telephony; AssemblyAI does not
    /// state a rate for this encoding: UNVERIFIED).
    Mulaw,
}

impl Encoding {
    /// The `encoding` query value.
    pub const fn param(self) -> &'static str {
        match self {
            Self::Pcm16 => "pcm_s16le",
            Self::Mulaw => "pcm_mulaw",
        }
    }

    /// Bytes per sample.
    pub const fn bytes_per_sample(self) -> u64 {
        match self {
            Self::Pcm16 => 2,
            Self::Mulaw => 1,
        }
    }

    /// The byte of a digital-silence sample.
    pub(crate) const fn silence_byte(self) -> u8 {
        match self {
            Self::Pcm16 => 0x00,
            Self::Mulaw => 0xFF,
        }
    }
}

/// Settings of the AssemblyAI streaming STT adapter.
///
/// Create it with [`AssemblyAiConfig::new`] and override fields; the struct is
/// `#[non_exhaustive]` so that new settings are not a breaking change. `Debug` never prints the
/// API key ([`Secret`]).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AssemblyAiConfig {
    /// API key, sent only as `Authorization: <key>` (no `Bearer`).
    pub api_key: Secret<String>,
    /// `speech_model`. Default `universal-3-6-pro` (supports Turkish). Always sent: the server's
    /// own default differs between documentation pages. Letters, digits, `.`, `_` and `-` only.
    pub speech_model: String,
    /// Language (`language_codes`, e.g. `tr`) used when the [`SttConfig`] of a session has none.
    /// `None` leaves the parameter out (the model then switches languages freely). A single code
    /// makes a monolingual session. The value is sent as a JSON array string
    /// (`language_codes=["tr"]`); that serialisation is UNVERIFIED (see `extra_params`).
    pub language: Option<String>,
    /// Languages the speaker is expected to use (`language_codes`, e.g. `["en", "tr"]`): the
    /// model is steered towards them while it may still switch between them mid-session
    /// (Universal-3.6 Pro code-switches by default). Used when the session has no language;
    /// takes the place of [`Self::language`]. Empty by default.
    pub language_codes: Vec<String>,
    /// `language_detection`: each final turn then reports `language_code` and
    /// `language_confidence`, which the adapter hands on as the transcript language. Reporting
    /// only: it does not change how the model transcribes. Default `false`.
    pub language_detection: bool,
    /// Encoding of the audio bytes. Default [`Encoding::Pcm16`].
    pub encoding: Encoding,
    /// Sample rate override in Hz (8000..=96000). `None` uses the [`SttConfig`] sample rate.
    pub sample_rate_hz: Option<u32>,
    /// `format_turns=true`: Universal Streaming models then send an unformatted final turn followed
    /// by a formatted one; the adapter waits for the formatted one. The Pro models always format
    /// and the parameter is not sent for them. Default `false`.
    pub format_turns: bool,
    /// `min_turn_silence` in ms (50..=10000): silence after which a punctuated turn ends.
    /// `None` leaves the model default.
    pub min_turn_silence_ms: Option<u32>,
    /// `max_turn_silence` in ms (50..=10000): silence that ends a turn whatever it reads like.
    /// `None` leaves the model default. Raising it makes the engine's own end-of-speech detection
    /// (`SttInput::Finalize`) the normal way a turn ends.
    pub max_turn_silence_ms: Option<u32>,
    /// `keyterms_prompt`: terms to bias recognition towards (at most 100, each at most 50
    /// characters). Sent as a JSON array string. Empty by default.
    pub keyterms: Vec<String>,
    /// `session_heartbeat=true`: the server sends a `Heartbeat` every 5 s. This is the only
    /// liveness signal during silence, so it is what lets the adapter tell a quiet caller from a
    /// dead connection. With it off the adapter probes a connection that has been silent for
    /// `io_timeout` after audio with a WebSocket `Ping` and treats a further `io_timeout` without
    /// any frame as lost (the server answering pings is standard WebSocket behaviour but
    /// UNVERIFIED for AssemblyAI). Default `true`.
    pub heartbeat: bool,
    /// `inactivity_timeout` (5..=3600 s): the server closes a session that receives no message
    /// for this long. `None` (default) sends nothing and no `KeepAlive` is needed. When set, the
    /// adapter sends `{"type":"KeepAlive"}` after `keepalive_interval` without audio.
    pub inactivity_timeout: Option<Duration>,
    /// Hosting zone. Default [`Region::Edge`].
    pub region: Region,
    /// Replaces `wss://<region host>` (no path), e.g. `ws://127.0.0.1:9000` for tests. Plain
    /// `ws://` is accepted only for loopback hosts so the key can never travel in the clear.
    pub base_url: Option<String>,
    /// Extra query parameters appended verbatim (name, value; the value is percent-encoded), an
    /// escape hatch for parameters the adapter does not model (`mode`, `vad_threshold`,
    /// `prompt`...) and for the UNVERIFIED serialisation of `language_codes`. Names: letters,
    /// digits, `.`, `_`, `-`.
    pub extra_params: Vec<(String, String)>,
    /// Deadline of every network wait: the connect (TCP, TLS, WebSocket handshake and the `Begin`
    /// message together), each write, and the silence of the server after audio was sent (the
    /// connection is then replaced like any lost one; with no reconnect left the session ends
    /// with `Core(Timeout)`). With `heartbeat` off the silence is first probed with a `Ping` and
    /// the connection is declared lost after a second `io_timeout`. Default 10 s.
    pub io_timeout: Duration,
    /// After this long without audio the response watchdog pauses (a quiet line gives the server
    /// nothing to say) and, with `inactivity_timeout` set, a `KeepAlive` is sent. Must be below
    /// `inactivity_timeout`. Default 4 s.
    pub keepalive_interval: Duration,
    /// How long to wait for the answer to a `ForceEndpoint` while a turn is in progress before
    /// ending the utterance with a synthetic final (the docs promise an immediate answer but do
    /// not say what happens in edge cases). Default 800 ms.
    pub finalize_timeout: Duration,
    /// The same wait when nothing is open on the server: no audio since the last final or
    /// `Finalize`, or the last thing the server reported was a final turn (it ended the turn by
    /// itself and the caller's end-of-speech detection is merely late). `ForceEndpoint` is still
    /// sent, but the caller is not held up for long for an answer that may never come. Speech that shows up later is delivered as a late
    /// utterance. `ZERO` answers at once. Default 200 ms.
    pub finalize_idle_timeout: Duration,
    /// Audio shorter than 50 ms cannot be sent (the server closes the session), so audio is
    /// batched until 50 ms are there. `None` (default) holds a shorter remainder until the next
    /// audio arrives (or a `Finalize` pads it): padding on a timer would put fake silence into
    /// the middle of a word whenever the network jitters, and sends audio ahead of real time.
    /// `Some(gap)` pads the held remainder with silence and sends it once the caller has sent
    /// nothing for `gap`, for sources that go quiet for good (the provider contract suite sends a
    /// single 20 ms frame and expects an interim). `gap` must be at least 50 ms: the padding is
    /// always shorter than that, so the audio the server has been given never runs ahead of the
    /// wall clock because of it.
    pub batch_flush: Option<Duration>,
    /// How long to wait for the server to finish after `Terminate` on hang-up. Default 1 s.
    pub close_timeout: Duration,
    /// Reconnect attempts per dropped connection (0 disables reconnecting). The counter starts
    /// over once a new connection delivers a `Turn` or `Heartbeat`. Default 1.
    pub max_reconnects: u32,
    /// Wait before the first reconnect attempt; doubles per attempt (cap 5 s). Default 100 ms.
    pub reconnect_backoff: Duration,
    /// How much audio that was not yet turned into a final is kept for replay after a reconnect
    /// (the oldest is dropped first). Default 8 s. A replay does not resend all of it: it starts
    /// 2 s before the turn in progress, or 1 s before the old connection's send position when no
    /// turn is open (and always includes audio the old connection never received).
    pub replay_window: Duration,
    /// Replayed audio is paced at this multiple of real time (at least 1.0). Sending audio faster
    /// than real time is a documented cause of close code 3007 ("Audio Transmission Rate
    /// Exceeded") and the tolerated factor is not documented (UNVERIFIED), so the default is 1.0:
    /// a replay then finishes after as long as the audio it holds, and a backlog never drains
    /// while live audio keeps arriving, but the call cannot be killed by its own recovery. Raise
    /// it only after checking against the live service.
    pub replay_speed: f64,
}

impl AssemblyAiConfig {
    /// Defaults for `api_key`.
    pub fn new(api_key: Secret<String>) -> Self {
        Self {
            api_key,
            speech_model: "universal-3-6-pro".to_owned(),
            language: None,
            language_codes: Vec::new(),
            language_detection: false,
            encoding: Encoding::Pcm16,
            sample_rate_hz: None,
            format_turns: false,
            min_turn_silence_ms: None,
            max_turn_silence_ms: None,
            keyterms: Vec::new(),
            heartbeat: true,
            inactivity_timeout: None,
            region: Region::Edge,
            base_url: None,
            extra_params: Vec::new(),
            io_timeout: Duration::from_secs(10),
            keepalive_interval: Duration::from_secs(4),
            finalize_timeout: Duration::from_millis(800),
            finalize_idle_timeout: Duration::from_millis(200),
            batch_flush: None,
            close_timeout: Duration::from_secs(1),
            max_reconnects: 1,
            reconnect_backoff: Duration::from_millis(100),
            replay_window: Duration::from_secs(8),
            replay_speed: 1.0,
        }
    }

    /// The sample rate sent to AssemblyAI for a session.
    pub(crate) fn sample_rate(&self, stt: &SttConfig) -> u32 {
        self.sample_rate_hz.unwrap_or_else(|| stt.sample_rate.hz())
    }

    /// Bytes of audio per second for a session.
    pub(crate) fn bytes_per_second(&self, stt: &SttConfig) -> u64 {
        u64::from(self.sample_rate(stt)).saturating_mul(self.encoding.bytes_per_sample())
    }

    /// Bytes in `span` of audio, rounded to whole samples: **up** for a lower bound (the minimum
    /// chunk must not come out below 50 ms: at 22.05 kHz 50 ms is 1102.5 samples), **down** for
    /// an upper bound.
    pub(crate) fn bytes_in(&self, stt: &SttConfig, span: Duration, round: Round) -> usize {
        let align = u128::from(self.encoding.bytes_per_sample().max(1));
        let micros = span.as_micros();
        let scaled = micros.saturating_mul(u128::from(self.bytes_per_second(stt)));
        let raw = match round {
            Round::Up => scaled.div_ceil(1_000_000),
            Round::Down => scaled / 1_000_000,
        };
        let aligned = match round {
            Round::Up => raw.div_ceil(align) * align,
            Round::Down => raw / align * align,
        };
        usize::try_from(aligned).unwrap_or(usize::MAX)
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
        if !is_token(&self.speech_model) {
            return bad("speech_model must be letters, digits, '.', '_' or '-'");
        }
        if self.language.as_deref().is_some_and(|l| !is_token(l)) {
            return bad("language must be letters, digits, '.', '_' or '-'");
        }
        if self.language_codes.len() > 16 || self.language_codes.iter().any(|l| !is_token(l)) {
            return bad(
                "language_codes: at most 16 language codes of letters, digits, '.', '_' or '-'",
            );
        }
        if self
            .sample_rate_hz
            .is_some_and(|hz| !(8_000..=96_000).contains(&hz))
        {
            return bad("sample_rate_hz must be 8000..=96000");
        }
        for ms in [self.min_turn_silence_ms, self.max_turn_silence_ms]
            .into_iter()
            .flatten()
        {
            if !(50..=10_000).contains(&ms) {
                return bad("min_turn_silence_ms and max_turn_silence_ms must be 50..=10000");
            }
        }
        if self.keyterms.len() > 100 {
            return bad("at most 100 keyterms");
        }
        if self.keyterms.iter().any(|t| t.chars().count() > 50) {
            return bad("a keyterm is longer than 50 characters");
        }
        if let Some(timeout) = self.inactivity_timeout {
            if !(5..=3600).contains(&timeout.as_secs()) {
                return bad("inactivity_timeout must be 5..=3600 s");
            }
            if self.keepalive_interval >= timeout {
                return bad("keepalive_interval must be below inactivity_timeout");
            }
        }
        for (name, _) in &self.extra_params {
            if !is_token(name) {
                return bad("extra_params names must be letters, digits, '.', '_' or '-'");
            }
        }
        if self.replay_window > Duration::from_secs(60) {
            return bad("replay_window must be at most 60 s");
        }
        if self.io_timeout.is_zero() || self.keepalive_interval.is_zero() {
            return bad("io_timeout and keepalive_interval must be positive");
        }
        if self.batch_flush.is_some_and(|gap| gap < MIN_CHUNK) {
            return bad("batch_flush must be at least 50 ms");
        }
        if self.heartbeat && self.io_timeout <= HEARTBEAT_EVERY {
            tracing::warn!(
                "assemblyai io_timeout <= 5 s, the server's heartbeat interval: a quiet line may \
                 look like a silent server to the response watchdog"
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
        format!("{scheme}://{authority}/v3/ws?{}", self.query(stt))
    }

    /// Whether the model is one of the Universal Streaming models (the ones that know
    /// `format_turns`).
    fn is_universal_streaming(&self) -> bool {
        self.speech_model.starts_with("universal-streaming")
    }

    /// The query string, in a fixed order.
    pub(crate) fn query(&self, stt: &SttConfig) -> String {
        let mut q = String::with_capacity(160);
        let mut add = |key: &str, value: &str| {
            if !q.is_empty() {
                q.push('&');
            }
            q.push_str(key);
            q.push('=');
            q.push_str(&percent_encode(value));
        };
        add("speech_model", &self.speech_model);
        add("encoding", self.encoding.param());
        add("sample_rate", &self.sample_rate(stt).to_string());
        let language = stt
            .language
            .as_deref()
            .or(if self.language_codes.is_empty() {
                self.language.as_deref()
            } else {
                None
            })
            .map(primary_language);
        if let Some(language) = language.filter(|l| !l.is_empty()) {
            add("language_codes", &json_array(&[language]));
        } else if stt.language.is_none() && !self.language_codes.is_empty() {
            let mut codes: Vec<String> = Vec::new();
            for code in self.language_codes.iter().map(|c| primary_language(c)) {
                if !code.is_empty() && !codes.contains(&code) {
                    codes.push(code);
                }
            }
            add("language_codes", &json_array(&codes));
        }
        if self.language_detection {
            add("language_detection", "true");
        }
        if self.waits_for_formatted() {
            add("format_turns", "true");
        }
        if let Some(ms) = self.min_turn_silence_ms {
            add("min_turn_silence", &ms.to_string());
        }
        if let Some(ms) = self.max_turn_silence_ms {
            add("max_turn_silence", &ms.to_string());
        }
        if !self.keyterms.is_empty() {
            add("keyterms_prompt", &json_array(&self.keyterms));
        }
        if self.heartbeat {
            add("session_heartbeat", "true");
        }
        if let Some(timeout) = self.inactivity_timeout {
            add("inactivity_timeout", &timeout.as_secs().to_string());
        }
        for (name, value) in &self.extra_params {
            add(name, value);
        }
        q
    }

    /// Whether the adapter waits for the formatted twin of an unformatted final turn: only the
    /// Universal Streaming models send one, and only when asked to with `format_turns`.
    pub(crate) fn waits_for_formatted(&self) -> bool {
        self.format_turns && self.is_universal_streaming()
    }
}

/// `tr-TR` / `TR` -> `tr`: AssemblyAI takes bare language codes.
fn primary_language(tag: &str) -> String {
    tag.split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// A JSON array of strings (`["a","b"]`), the form of `keyterms_prompt` in the raw-socket
/// examples of the docs.
fn json_array<S: AsRef<str>>(items: &[S]) -> String {
    serde_json::to_string(&items.iter().map(AsRef::as_ref).collect::<Vec<_>>())
        .unwrap_or_else(|_| "[]".to_owned())
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

    fn config() -> AssemblyAiConfig {
        AssemblyAiConfig::new(Secret::new("k".to_owned()))
    }

    #[test]
    fn the_default_url_is_the_documented_recommended_set() {
        assert_eq!(
            config().url(&stt(Some("tr"))),
            "wss://streaming.assemblyai.com/v3/ws?speech_model=universal-3-6-pro\
             &encoding=pcm_s16le&sample_rate=16000&language_codes=%5B%22tr%22%5D\
             &session_heartbeat=true"
        );
    }

    #[test]
    fn every_option_reaches_the_query_in_a_fixed_order() {
        let mut c = config();
        c.speech_model = "universal-streaming-english".to_owned();
        c.encoding = Encoding::Mulaw;
        c.sample_rate_hz = Some(8000);
        c.format_turns = true;
        c.min_turn_silence_ms = Some(200);
        c.max_turn_silence_ms = Some(2000);
        c.keyterms = vec!["Brisvox".to_owned(), "a b".to_owned()];
        c.heartbeat = false;
        c.inactivity_timeout = Some(Duration::from_secs(30));
        c.region = Region::Eu;
        c.extra_params = vec![("mode".to_owned(), "min latency".to_owned())];
        assert_eq!(
            c.url(&stt(None)),
            "wss://streaming.eu.assemblyai.com/v3/ws?speech_model=universal-streaming-english\
             &encoding=pcm_mulaw&sample_rate=8000&format_turns=true&min_turn_silence=200\
             &max_turn_silence=2000&keyterms_prompt=%5B%22Brisvox%22%2C%22a%20b%22%5D\
             &inactivity_timeout=30&mode=min%20latency"
        );
    }

    #[test]
    fn format_turns_is_only_sent_for_universal_streaming_models() {
        let mut c = config();
        c.format_turns = true;
        assert!(!c.query(&stt(None)).contains("format_turns"));
        assert!(!c.waits_for_formatted());
        c.speech_model = "universal-streaming-multilingual".to_owned();
        assert!(c.query(&stt(None)).contains("format_turns=true"));
        assert!(c.waits_for_formatted());
    }

    #[test]
    fn language_tags_are_reduced_to_the_bare_code_and_the_session_wins() {
        let mut c = config();
        c.language = Some("en".to_owned());
        assert!(c.query(&stt(Some("tr-TR"))).contains("%5B%22tr%22%5D"));
        assert!(c.query(&stt(None)).contains("%5B%22en%22%5D"));
        c.language = None;
        assert!(!c.query(&stt(None)).contains("language_codes"));
    }

    #[test]
    fn several_language_codes_steer_the_model_and_detection_is_reported() {
        let mut c = config();
        c.language_codes = vec!["en".to_owned(), "tr-TR".to_owned(), "EN".to_owned()];
        c.language_detection = true;
        let q = c.query(&stt(None));
        // `["en","tr"]` percent-encoded, de-duplicated and reduced to bare codes.
        assert!(
            q.contains("language_codes=%5B%22en%22%2C%22tr%22%5D"),
            "{q}"
        );
        assert!(q.contains("&language_detection=true"), "{q}");
        // A language the session names wins; the list is for sessions without one.
        assert!(
            c.query(&stt(Some("tr")))
                .contains("language_codes=%5B%22tr%22%5D")
        );
        c.language_codes = vec!["en&x=1".to_owned()];
        assert!(c.validate().is_err());
    }

    #[test]
    fn regions_map_to_their_hosts_and_base_url_overrides_them() {
        assert_eq!(Region::Edge.host(), "streaming.assemblyai.com");
        assert_eq!(Region::Us.host(), "streaming.us.assemblyai.com");
        assert_eq!(Region::Eu.host(), "streaming.eu.assemblyai.com");
        let mut c = config();
        c.base_url = Some("ws://127.0.0.1:4000".to_owned());
        assert_eq!(c.authority(), ("127.0.0.1:4000".to_owned(), false));
        assert!(c.url(&stt(None)).starts_with("ws://127.0.0.1:4000/v3/ws?"));
    }

    #[test]
    fn chunk_sizes_follow_the_encoding_and_rate() {
        let c = config();
        assert_eq!(c.bytes_in(&stt(None), MIN_CHUNK, Round::Up), 1600);
        assert_eq!(c.bytes_in(&stt(None), MAX_CHUNK, Round::Down), 16_000);
        let mut m = config();
        m.encoding = Encoding::Mulaw;
        let telephony = SttConfig {
            sample_rate: SampleRate::Hz8000,
            language: None,
        };
        assert_eq!(m.bytes_in(&telephony, MIN_CHUNK, Round::Up), 400);
        assert_eq!(m.bytes_per_second(&telephony), 8000);
    }

    #[test]
    fn the_minimum_chunk_is_never_below_fifty_milliseconds() {
        // 22.05 kHz: 50 ms is 1102.5 samples; 11.025 kHz: 551.25 samples.
        for hz in [22_050_u32, 11_025, 44_100, 12_345] {
            for encoding in [Encoding::Pcm16, Encoding::Mulaw] {
                let mut c = config();
                c.encoding = encoding;
                c.sample_rate_hz = Some(hz);
                let s = stt(None);
                let min = c.bytes_in(&s, MIN_CHUNK, Round::Up) as u64;
                let max = c.bytes_in(&s, MAX_CHUNK, Round::Down) as u64;
                let bps = c.bytes_per_second(&s);
                assert!(
                    min * 1000 >= 50 * bps,
                    "{hz} {encoding:?}: {min} B is under 50 ms"
                );
                assert!(
                    (min - 1) * 1000 < 50 * bps + 1000 * encoding.bytes_per_sample(),
                    "{hz} {encoding:?}: {min} B is more than one sample over 50 ms"
                );
                assert!(min.is_multiple_of(encoding.bytes_per_sample()));
                assert!(max * 1000 <= 500 * bps && max.is_multiple_of(encoding.bytes_per_sample()));
            }
        }
        let mut c = config();
        c.sample_rate_hz = Some(22_050);
        assert_eq!(c.bytes_in(&stt(None), MIN_CHUNK, Round::Up), 2206);
        c.encoding = Encoding::Mulaw;
        assert_eq!(c.bytes_in(&stt(None), MIN_CHUNK, Round::Up), 1103);
    }

    #[test]
    fn replay_is_paced_at_real_time_by_default() {
        // Faster-than-real-time input is a documented cause of close code 3007.
        let c = AssemblyAiConfig::new(Secret::new("k".to_owned()));
        assert!((c.replay_speed - 1.0).abs() < f64::EPSILON);
        assert!(c.batch_flush.is_none());
    }

    #[test]
    fn the_timing_defaults_are_the_documented_ones() {
        // docs/providers/assemblyai.md quotes these; every integration test overrides them, so
        // this is the only place that notices a default drifting (a 5 s `finalize_timeout`
        // would be 5 s of dead air on every unanswered `Finalize`).
        let c = AssemblyAiConfig::new(Secret::new("k".to_owned()));
        assert_eq!(c.io_timeout, Duration::from_secs(10));
        assert_eq!(c.keepalive_interval, Duration::from_secs(4));
        assert_eq!(c.finalize_timeout, Duration::from_millis(800));
        assert_eq!(c.finalize_idle_timeout, Duration::from_millis(200));
        assert_eq!(c.close_timeout, Duration::from_secs(1));
        assert_eq!(c.max_reconnects, 1);
        assert_eq!(c.reconnect_backoff, Duration::from_millis(100));
        assert_eq!(c.replay_window, Duration::from_secs(8));
        assert!((c.replay_speed - 1.0).abs() < f64::EPSILON);
        assert!(c.batch_flush.is_none());
        assert!(c.heartbeat);
        assert!(c.inactivity_timeout.is_none());
        assert!(c.validate().is_ok(), "the defaults must validate");
    }

    #[test]
    fn validation_rejects_unsafe_or_malformed_settings() {
        assert!(config().validate().is_ok());
        let mut c = config();
        c.speech_model = "x&evil=1".to_owned();
        assert!(c.validate().is_err());
        let mut c = config();
        c.language = Some("tr&x=1".to_owned());
        assert!(c.validate().is_err());
        let mut c = config();
        c.min_turn_silence_ms = Some(10);
        assert!(c.validate().is_err());
        let mut c = config();
        c.keyterms = vec!["x".repeat(51)];
        assert!(c.validate().is_err());
        let mut c = config();
        c.keyterms = vec!["x".to_owned(); 101];
        assert!(c.validate().is_err());
        let mut c = config();
        c.inactivity_timeout = Some(Duration::from_secs(2));
        assert!(c.validate().is_err());
        c.inactivity_timeout = Some(Duration::from_secs(5));
        c.keepalive_interval = Duration::from_secs(5);
        assert!(c.validate().is_err(), "keepalive must be below the timeout");
        let mut c = config();
        c.extra_params = vec![("a=b".to_owned(), "x".to_owned())];
        assert!(c.validate().is_err());
        let mut c = config();
        c.batch_flush = Some(Duration::from_millis(49));
        assert!(c.validate().is_err());
        let mut c = config();
        c.replay_speed = 0.5;
        assert!(c.validate().is_err());
        let mut c = config();
        c.replay_window = Duration::from_secs(120);
        assert!(c.validate().is_err());
        for hz in [100, 200_000] {
            let mut c = config();
            c.sample_rate_hz = Some(hz);
            assert!(c.validate().is_err(), "{hz}");
        }
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
        for bad in [
            "ws://127.0.0.1:1/path",
            "ws://127.0.0.1:80@evil.example",
            "ws://[::1]@evil.example",
            "ws://localhost@evil.example",
            "ws://localhost:x",
            r"ws://127.0.0.1\@evil.example",
            "ws://127.0.0.1:1:2",
            "ws://",
            "wss://user@proxy.example",
            "wss://proxy.example?x=1",
        ] {
            let mut c = config();
            c.base_url = Some(bad.to_owned());
            assert!(c.validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn percent_encoding_keeps_unreserved_characters_and_escapes_the_rest() {
        assert_eq!(percent_encode("aZ09-._~"), "aZ09-._~");
        assert_eq!(percent_encode("a b/ç"), "a%20b%2F%C3%A7");
    }

    #[test]
    fn debug_output_hides_the_key() {
        let c = AssemblyAiConfig::new(Secret::new("aai-secret-key-123".to_owned()));
        assert!(!format!("{c:?}").contains("aai-secret-key-123"));
    }
}
