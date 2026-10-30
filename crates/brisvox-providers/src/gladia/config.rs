//! Configuration of the Gladia adapter and the body of its `POST /v2/live` init request.

use std::time::Duration;

use brisvox_core::Secret;
use serde::Serialize;

use super::PROVIDER;
use crate::error::{Error, Result};
use crate::stt::SttConfig;

/// Gladia closes a socket that gets no audio for about 30 s (close code 4408, support article in
/// `docs/providers/gladia.md`); the silence keepalive must come well before.
const NO_AUDIO_CLOSE: Duration = Duration::from_secs(30);

/// Sample rates the init endpoint accepts.
const SAMPLE_RATES: [u32; 5] = [8_000, 16_000, 32_000, 44_100, 48_000];

/// Where Gladia processes the audio (`region` query parameter of the init call).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Region {
    /// `us-west`.
    UsWest,
    /// `eu-west`.
    EuWest,
}

impl Region {
    /// The `region` query value.
    pub const fn param(self) -> &'static str {
        match self {
            Self::UsWest => "us-west",
            Self::EuWest => "eu-west",
        }
    }
}

/// Audio encoding of the bytes sent in [`SttInput::Audio`](crate::stt::SttInput::Audio).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Encoding {
    /// Raw PCM16 little-endian mono (`wav/pcm`, 16 bit), the engine's native format.
    #[default]
    Pcm16,
    /// G.711 mu-law mono (`wav/ulaw`, 8 bit), e.g. telephony audio passed through unchanged.
    Ulaw,
    /// G.711 A-law mono (`wav/alaw`, 8 bit).
    Alaw,
}

impl Encoding {
    /// The `encoding` value of the init body.
    pub const fn param(self) -> &'static str {
        match self {
            Self::Pcm16 => "wav/pcm",
            Self::Ulaw => "wav/ulaw",
            Self::Alaw => "wav/alaw",
        }
    }

    /// The `bit_depth` of the init body.
    pub const fn bit_depth(self) -> u8 {
        match self {
            Self::Pcm16 => 16,
            Self::Ulaw | Self::Alaw => 8,
        }
    }

    /// Bytes per sample.
    pub const fn bytes_per_sample(self) -> u64 {
        match self {
            Self::Pcm16 => 2,
            Self::Ulaw | Self::Alaw => 1,
        }
    }

    /// The byte of a digital-silence sample.
    pub(crate) const fn silence_byte(self) -> u8 {
        match self {
            Self::Pcm16 => 0x00,
            Self::Ulaw => 0xFF,
            Self::Alaw => 0xD5,
        }
    }
}

/// Settings of the Gladia live STT adapter.
///
/// Create it with [`GladiaConfig::new`] and override fields; the struct is `#[non_exhaustive]` so
/// that new settings are not a breaking change. `Debug` never prints the API key ([`Secret`]).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GladiaConfig {
    /// API key, sent only as the `x-gladia-key` header of the init call. The WebSocket carries a
    /// temporary token instead (which the adapter never logs).
    pub api_key: Secret<String>,
    /// Model (`model` field). Default `solaria-1`, the only live model. Letters, digits, `.`, `_`
    /// and `-` only.
    pub model: String,
    /// Language used when the [`SttConfig`] of a session has none (`tr`, `en`, ...; a regional
    /// suffix such as `tr-TR` is cut off). `None` lets Gladia detect the language, which is less
    /// accurate for partials than naming it.
    pub language: Option<String>,
    /// The languages the speaker is expected to use (`language_config.languages`, e.g.
    /// `["en", "tr"]`), when the session names no language of its own: Gladia then detects the
    /// language among them (with [`Self::code_switching`] on each utterance) and reports it on
    /// every transcript. Takes the place of [`Self::language`]. Gladia advises a small set;
    /// empty by default.
    pub languages: Vec<String>,
    /// `language_config.code_switching`. Default `false`; only meaningful with a language set.
    /// With `false` Gladia detects the language on the first utterance and keeps it for the
    /// whole session.
    pub code_switching: bool,
    /// Encoding of the audio bytes. Default [`Encoding::Pcm16`].
    pub encoding: Encoding,
    /// Sample rate assertion in Hz (8000, 16000, 32000, 44100 or 48000). The adapter does not
    /// resample: the rate declared to Gladia is always the rate of the audio the caller sends,
    /// the [`SttConfig`] sample rate. `None` (the default) uses that rate. `Some(hz)` is accepted
    /// only when it equals the session's [`SttConfig`] sample rate; any other value makes the
    /// connect fail with a config error instead of declaring audio at a rate it is not in.
    pub sample_rate_hz: Option<u32>,
    /// `endpointing`: seconds of trailing silence that close an utterance (0.01..=10). Default
    /// 0.1, the voice-agent value of the recommended-parameters page. This is the only lever on
    /// how fast a final arrives; [`SttInput::Finalize`](crate::stt::SttInput::Finalize) cannot
    /// force one (see the module docs).
    pub endpointing: f64,
    /// `maximum_duration_without_endpointing`: seconds after which a still-running utterance is
    /// cut (5..=60). Default 15.
    pub maximum_duration_without_endpointing: f64,
    /// Custom vocabulary terms (`realtime_processing.custom_vocabulary`). Empty by default.
    pub vocabulary: Vec<String>,
    /// Processing region. `None` leaves the choice to Gladia.
    pub region: Option<Region>,
    /// Replaces `https://api.gladia.io` (scheme, host and optional port only), e.g.
    /// `http://127.0.0.1:9000` for tests. Plain `http://` is accepted only for loopback hosts so
    /// the key can never travel in the clear.
    pub base_url: Option<String>,
    /// `messages_config.receive_acknowledgments`. Default `false`. Gladia then sends nothing
    /// while there is no speech, so application messages are no proof of life for an idle line
    /// (see [`Self::liveness_ping`] for the proof that does not depend on them). With `true` the
    /// server acknowledges every audio chunk (about 50 small messages per second and call) and
    /// the adapter also treats `io_timeout` without any server message, while audio is flowing,
    /// as a lost connection. Whether Gladia acknowledges
    /// binary audio frames the same way as JSON chunks is UNVERIFIED (see
    /// `docs/providers/gladia.md`), which is why this is opt-in.
    pub acknowledgments: bool,
    /// Deadline of every network wait: the whole connect (init call, TCP, TLS and WebSocket
    /// handshake together), each write, the delivery of a final to the caller, and, with
    /// [`Self::acknowledgments`], the silence of the server after audio was sent, and the
    /// answer to a [`Self::liveness_ping`]. Default 10 s.
    pub io_timeout: Duration,
    /// Detect a dead link (NAT or route loss without FIN/RST) with WebSocket pings: after
    /// `keepalive_interval` without any frame from the server the adapter sends a Ping, and no
    /// frame at all within `io_timeout` counts as a lost session (replaced like any drop,
    /// `Core(Timeout)` when no reconnect is left). A WebSocket endpoint must answer a Ping
    /// with a Pong (RFC 6455), so this works with acknowledgments off; but Gladia's handling of
    /// pings is not documented (UNVERIFIED): set `false` if a deployment shows healthy calls
    /// being replaced every `keepalive_interval + io_timeout`. Default `true`.
    pub liveness_ping: bool,
    /// Gladia has no keepalive message; it wants audio ("keep sending audio, even digital
    /// silence"). After this long without audio the adapter sends 20 ms of digital silence. Must
    /// be below 30 s (the no-audio close). With [`Self::acknowledgments`] the effective interval
    /// is at most half of `io_timeout`, so the acknowledgment of the silence keeps the response
    /// watchdog quiet while the caller is idle. Default 5 s.
    pub keepalive_interval: Duration,
    /// How long a [`Finalize`](crate::stt::SttInput::Finalize) waits for Gladia's own final of
    /// the open utterance before the adapter ends the utterance with the last partial. Default
    /// 800 ms.
    pub finalize_timeout: Duration,
    /// How long to wait for the server to close after `stop_recording` on hang-up. Default 1 s.
    pub close_timeout: Duration,
    /// Reconnect attempts per dropped connection (0 disables reconnecting). Each attempt is a new
    /// init call and so a new session. The counter starts over once a new connection delivers a
    /// transcript or an acknowledgment, or after a connection that lived 30 s. Default 1.
    pub max_reconnects: u32,
    /// Wait before the first reconnect attempt; doubles per attempt (cap 5 s). Default 100 ms.
    pub reconnect_backoff: Duration,
    /// How much audio that has not become a final transcript yet is kept for replay into the new
    /// session after a reconnect (the oldest is dropped first). Default 8 s.
    pub replay_window: Duration,
    /// Replayed audio is paced at this multiple of real time. Gladia documents no limit on how
    /// fast audio may be sent (UNVERIFIED); 2.0 is a cautious choice. A backlog of `B` seconds
    /// drains in `B / (speed - 1)` seconds while live audio keeps arriving. Default 2.0.
    pub replay_speed: f64,
    /// How long an idle init connection stays in the HTTP client's pool. Default 60 s.
    pub pool_idle_timeout: Duration,
}

impl GladiaConfig {
    /// Defaults for `api_key`.
    pub fn new(api_key: Secret<String>) -> Self {
        Self {
            api_key,
            model: "solaria-1".to_owned(),
            language: None,
            languages: Vec::new(),
            code_switching: false,
            encoding: Encoding::Pcm16,
            sample_rate_hz: None,
            endpointing: 0.1,
            maximum_duration_without_endpointing: 15.0,
            vocabulary: Vec::new(),
            region: None,
            base_url: None,
            acknowledgments: false,
            io_timeout: Duration::from_secs(10),
            liveness_ping: true,
            keepalive_interval: Duration::from_secs(5),
            finalize_timeout: Duration::from_millis(800),
            close_timeout: Duration::from_secs(1),
            max_reconnects: 1,
            reconnect_backoff: Duration::from_millis(100),
            replay_window: Duration::from_secs(8),
            replay_speed: 2.0,
            pool_idle_timeout: Duration::from_secs(60),
        }
    }

    /// The sample rate sent to Gladia for a session.
    pub(crate) fn sample_rate(&self, stt: &SttConfig) -> u32 {
        self.sample_rate_hz.unwrap_or_else(|| stt.sample_rate.hz())
    }

    /// Bytes of audio per second for a session.
    pub(crate) fn bytes_per_second(&self, stt: &SttConfig) -> u64 {
        u64::from(self.sample_rate(stt)).saturating_mul(self.encoding.bytes_per_sample())
    }

    /// The language of a session: the session's own, else the configured one, reduced to the
    /// primary subtag in lower case (`tr-TR` is `tr`).
    pub(crate) fn language_for(&self, stt: &SttConfig) -> Option<String> {
        let raw = stt.language.as_deref().or(self.language.as_deref())?;
        let primary = raw.split(['-', '_']).next().unwrap_or(raw);
        Some(primary.to_ascii_lowercase())
    }

    /// The silence interval in effect (see [`Self::keepalive_interval`]).
    pub(crate) fn effective_keepalive(&self) -> Duration {
        if self.acknowledgments {
            self.keepalive_interval.min(self.io_timeout / 2)
        } else {
            self.keepalive_interval
        }
    }

    /// Checks the settings that would otherwise produce a malformed request or an unsafe
    /// connection. Session-independent; [`Self::validate_session`] checks the rest.
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
        if self.languages.len() > 8 || self.languages.iter().any(|l| !is_language(l)) {
            return bad("languages: at most 8 codes such as 'tr' or 'tr-TR'");
        }
        if let Some(language) = &self.language
            && !is_language(language)
        {
            return bad("language must be a code such as 'tr' or 'tr-TR'");
        }
        if self.vocabulary.iter().any(|t| t.trim().is_empty()) {
            return bad("vocabulary terms must not be blank");
        }
        if !(0.01..=10.0).contains(&self.endpointing) {
            return bad("endpointing must be 0.01..=10 seconds");
        }
        if !(5.0..=60.0).contains(&self.maximum_duration_without_endpointing) {
            return bad("maximum_duration_without_endpointing must be 5..=60 seconds");
        }
        if let Some(hz) = self.sample_rate_hz
            && !SAMPLE_RATES.contains(&hz)
        {
            return bad("sample_rate_hz must be 8000, 16000, 32000, 44100 or 48000");
        }
        if self.replay_window > Duration::from_secs(60) {
            return bad("replay_window must be at most 60 s");
        }
        if self.io_timeout.is_zero() || self.keepalive_interval.is_zero() {
            return bad("io_timeout and keepalive_interval must be positive");
        }
        if self.keepalive_interval >= NO_AUDIO_CLOSE {
            return bad("keepalive_interval must be below 30 s (the no-audio close)");
        }
        if !self.replay_speed.is_finite() || self.replay_speed < 1.0 {
            return bad("replay_speed must be at least 1.0");
        }
        if let Some(base) = &self.base_url {
            let Ok(url) = reqwest::Url::parse(base) else {
                return bad("base_url must be an http:// or https:// URL");
            };
            if !matches!(url.scheme(), "http" | "https") {
                return bad("base_url must be an http:// or https:// URL");
            }
            if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
                return bad("base_url must be scheme and host[:port] only");
            }
            if !url.username().is_empty() || url.password().is_some() {
                return bad("base_url must not carry credentials");
            }
            if url.scheme() == "http" && !url.host_str().is_some_and(is_loopback_host) {
                return bad("plain http:// is only allowed for loopback hosts");
            }
        }
        Ok(())
    }

    /// Checks what depends on the session: the sample rate must be one Gladia accepts, and an
    /// explicit [`Self::sample_rate_hz`] must be the rate of the audio that will be sent.
    pub(crate) fn validate_session(&self, stt: &SttConfig) -> Result<()> {
        self.validate()?;
        if self
            .sample_rate_hz
            .is_some_and(|hz| hz != stt.sample_rate.hz())
        {
            return Err(Error::Protocol {
                provider: PROVIDER,
                detail: "invalid config: sample_rate_hz differs from the session's audio sample                          rate (the adapter does not resample)"
                    .to_owned(),
            });
        }
        if SAMPLE_RATES.contains(&self.sample_rate(stt)) {
            Ok(())
        } else {
            Err(Error::Protocol {
                provider: PROVIDER,
                detail: "invalid config: Gladia accepts only 8000, 16000, 32000, 44100 and \
                         48000 Hz audio"
                    .to_owned(),
            })
        }
    }

    /// Scheme and host (and port) of the REST API, without a trailing slash.
    fn api_base(&self) -> &str {
        self.base_url
            .as_deref()
            .unwrap_or("https://api.gladia.io")
            .trim_end_matches('/')
    }

    /// `{base}/v2/live/`: append a session id for `DELETE /v2/live/{id}`. Holds no credentials.
    pub(crate) fn session_url_prefix(&self) -> String {
        format!("{}/v2/live/", self.api_base())
    }

    /// The URL of the init call (`POST {base}/v2/live[?region=...]`). Holds no credentials.
    pub(crate) fn init_url(&self) -> String {
        let base = self.api_base();
        match self.region {
            Some(region) => format!("{base}/v2/live?region={}", region.param()),
            None => format!("{base}/v2/live"),
        }
    }

    /// The JSON body of the init call for a session.
    ///
    /// # Errors
    /// [`Error::Protocol`] if serialisation fails (not expected: the body holds only strings,
    /// numbers and booleans).
    pub(crate) fn init_body(&self, stt: &SttConfig) -> Result<Vec<u8>> {
        let single = self.language_for(stt);
        let several: Vec<String> = if single.is_none() {
            let mut codes: Vec<String> = Vec::new();
            for code in &self.languages {
                let primary = code
                    .split(['-', '_'])
                    .next()
                    .unwrap_or(code)
                    .to_ascii_lowercase();
                if !primary.is_empty() && !codes.contains(&primary) {
                    codes.push(primary);
                }
            }
            codes
        } else {
            Vec::new()
        };
        let languages: Vec<&str> = match &single {
            Some(language) => vec![language.as_str()],
            None => several.iter().map(String::as_str).collect(),
        };
        let body = InitBody {
            encoding: self.encoding.param(),
            bit_depth: self.encoding.bit_depth(),
            sample_rate: self.sample_rate(stt),
            channels: 1,
            model: &self.model,
            endpointing: self.endpointing,
            maximum_duration_without_endpointing: self.maximum_duration_without_endpointing,
            language_config: (!languages.is_empty()).then_some(LanguageConfig {
                languages,
                code_switching: self.code_switching,
            }),
            realtime_processing: (!self.vocabulary.is_empty()).then(|| RealtimeProcessing {
                custom_vocabulary: true,
                custom_vocabulary_config: VocabularyConfig {
                    vocabulary: &self.vocabulary,
                },
            }),
            messages_config: MessagesConfig {
                receive_partial_transcripts: true,
                receive_final_transcripts: true,
                receive_speech_events: true,
                receive_pre_processing_events: false,
                receive_realtime_processing_events: false,
                receive_post_processing_events: false,
                receive_acknowledgments: self.acknowledgments,
                receive_errors: true,
                receive_lifecycle_events: false,
            },
        };
        serde_json::to_vec(&body).map_err(|_| Error::Protocol {
            provider: PROVIDER,
            detail: "cannot serialize the init request".to_owned(),
        })
    }
}

#[derive(Serialize)]
struct InitBody<'a> {
    encoding: &'static str,
    bit_depth: u8,
    sample_rate: u32,
    channels: u8,
    model: &'a str,
    endpointing: f64,
    maximum_duration_without_endpointing: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    language_config: Option<LanguageConfig<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    realtime_processing: Option<RealtimeProcessing<'a>>,
    messages_config: MessagesConfig,
}

#[derive(Serialize)]
struct LanguageConfig<'a> {
    languages: Vec<&'a str>,
    code_switching: bool,
}

#[derive(Serialize)]
struct RealtimeProcessing<'a> {
    custom_vocabulary: bool,
    custom_vocabulary_config: VocabularyConfig<'a>,
}

#[derive(Serialize)]
struct VocabularyConfig<'a> {
    vocabulary: &'a [String],
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Serialize)]
struct MessagesConfig {
    receive_partial_transcripts: bool,
    receive_final_transcripts: bool,
    receive_speech_events: bool,
    receive_pre_processing_events: bool,
    receive_realtime_processing_events: bool,
    receive_post_processing_events: bool,
    receive_acknowledgments: bool,
    receive_errors: bool,
    receive_lifecycle_events: bool,
}

/// Whether `host` (a URL host, possibly an IPv6 literal in brackets) is a loopback address.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn is_language(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 16
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        && value
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
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

    fn cfg() -> GladiaConfig {
        GladiaConfig::new(Secret::new("key".to_owned()))
    }

    fn body(c: &GladiaConfig, s: &SttConfig) -> serde_json::Value {
        serde_json::from_slice(&c.init_body(s).unwrap()).unwrap()
    }

    #[test]
    fn a_language_list_is_sent_when_the_session_names_none() {
        let mut c = cfg();
        c.languages = vec!["en".to_owned(), "TR-tr".to_owned(), "en_US".to_owned()];
        c.code_switching = true;
        assert_eq!(
            body(&c, &stt(None))["language_config"],
            serde_json::json!({"languages": ["en", "tr"], "code_switching": true})
        );
        // The session's own language wins: one language, no list.
        assert_eq!(
            body(&c, &stt(Some("tr")))["language_config"]["languages"],
            serde_json::json!(["tr"])
        );
        c.languages = vec!["tr&x".to_owned()];
        assert!(c.validate().is_err());
    }

    #[test]
    fn the_init_body_for_turkish_matches_the_recommended_one() {
        let c = cfg();
        let expected: serde_json::Value = serde_json::from_str(
            r#"{"encoding":"wav/pcm","bit_depth":16,"sample_rate":16000,"channels":1,
                "model":"solaria-1","endpointing":0.1,"maximum_duration_without_endpointing":15.0,
                "language_config":{"languages":["tr"],"code_switching":false},
                "messages_config":{"receive_partial_transcripts":true,
                  "receive_final_transcripts":true,"receive_speech_events":true,
                  "receive_pre_processing_events":false,
                  "receive_realtime_processing_events":false,
                  "receive_post_processing_events":false,
                  "receive_acknowledgments":false,"receive_errors":true,
                  "receive_lifecycle_events":false}}"#,
        )
        .unwrap();
        assert_eq!(body(&c, &stt(Some("tr-TR"))), expected);
        assert_eq!(body(&c, &stt(Some("tr"))), expected);
    }

    #[test]
    fn language_precedence_and_normalisation() {
        let mut c = cfg();
        assert_eq!(c.language_for(&stt(None)), None);
        assert!(body(&c, &stt(None)).get("language_config").is_none());
        c.language = Some("EN_us".to_owned());
        assert_eq!(c.language_for(&stt(None)).as_deref(), Some("en"));
        assert_eq!(c.language_for(&stt(Some("tr-TR"))).as_deref(), Some("tr"));
    }

    #[test]
    fn telephony_and_vocabulary_are_in_the_body() {
        let mut c = cfg();
        c.encoding = Encoding::Ulaw;
        c.acknowledgments = true;
        c.vocabulary = vec!["Brisvox".to_owned(), "Beşiktaş".to_owned()];
        let s = SttConfig {
            sample_rate: SampleRate::Hz8000,
            language: Some("tr".into()),
        };
        let b = body(&c, &s);
        assert_eq!(b["encoding"], "wav/ulaw");
        assert_eq!(b["bit_depth"], 8);
        assert_eq!(b["sample_rate"], 8000);
        assert_eq!(b["messages_config"]["receive_acknowledgments"], true);
        assert_eq!(b["realtime_processing"]["custom_vocabulary"], true);
        assert_eq!(
            b["realtime_processing"]["custom_vocabulary_config"]["vocabulary"],
            serde_json::json!(["Brisvox", "Beşiktaş"])
        );
        assert_eq!(c.bytes_per_second(&s), 8000);
    }

    #[test]
    fn the_init_url_has_no_credentials_and_honours_region_and_base() {
        let mut c = cfg();
        assert_eq!(c.init_url(), "https://api.gladia.io/v2/live");
        c.region = Some(Region::EuWest);
        assert_eq!(c.init_url(), "https://api.gladia.io/v2/live?region=eu-west");
        c.base_url = Some("http://127.0.0.1:9/".to_owned());
        c.region = None;
        assert_eq!(c.init_url(), "http://127.0.0.1:9/v2/live");
    }

    #[test]
    fn unsafe_or_malformed_settings_are_rejected() {
        let bad = |tweak: &dyn Fn(&mut GladiaConfig)| {
            let mut c = cfg();
            tweak(&mut c);
            assert!(c.validate().is_err());
        };
        assert!(cfg().validate().is_ok());
        bad(&|c| c.model = "a b".to_owned());
        bad(&|c| c.language = Some("tr&x=1".to_owned()));
        bad(&|c| c.vocabulary = vec![" ".to_owned()]);
        bad(&|c| c.endpointing = 0.0);
        bad(&|c| c.endpointing = 11.0);
        bad(&|c| c.maximum_duration_without_endpointing = 4.0);
        bad(&|c| c.sample_rate_hz = Some(22_050));
        bad(&|c| c.io_timeout = Duration::ZERO);
        bad(&|c| c.keepalive_interval = Duration::from_secs(30));
        bad(&|c| c.replay_speed = 0.5);
        bad(&|c| c.replay_window = Duration::from_secs(61));
        bad(&|c| c.base_url = Some("http://api.gladia.io".to_owned()));
        bad(&|c| c.base_url = Some("ws://127.0.0.1".to_owned()));
        bad(&|c| c.base_url = Some("https://api.gladia.io/v2".to_owned()));
        bad(&|c| c.base_url = Some("https://user:pw@api.gladia.io".to_owned()));
        let mut ok = cfg();
        ok.base_url = Some("http://localhost:1234".to_owned());
        assert!(ok.validate().is_ok());
        ok.base_url = Some("https://eu.example.com".to_owned());
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn the_session_sample_rate_must_be_one_gladia_accepts() {
        let c = cfg();
        let s = SttConfig {
            sample_rate: SampleRate::Hz24000,
            language: None,
        };
        assert!(c.validate_session(&s).is_err());
        // An override cannot make 24 kHz audio acceptable: the bytes stay 24 kHz.
        let mut c = cfg();
        c.sample_rate_hz = Some(16_000);
        assert!(c.validate_session(&s).is_err());
    }

    #[test]
    fn a_sample_rate_override_must_equal_the_rate_of_the_audio() {
        let s16 = SttConfig {
            sample_rate: SampleRate::Hz16000,
            language: None,
        };
        let mut c = cfg();
        c.sample_rate_hz = Some(48_000);
        assert!(c.validate_session(&s16).is_err());
        c.sample_rate_hz = Some(16_000);
        assert!(c.validate_session(&s16).is_ok());
        assert_eq!(c.sample_rate(&s16), 16_000);
    }

    #[test]
    fn silence_bytes_and_the_effective_keepalive() {
        assert_eq!(Encoding::Pcm16.silence_byte(), 0x00);
        assert_eq!(Encoding::Ulaw.silence_byte(), 0xFF);
        assert_eq!(Encoding::Alaw.silence_byte(), 0xD5);
        let mut c = cfg();
        assert_eq!(c.effective_keepalive(), Duration::from_secs(5));
        c.acknowledgments = true;
        assert_eq!(c.effective_keepalive(), Duration::from_secs(5));
        c.io_timeout = Duration::from_secs(2);
        assert_eq!(c.effective_keepalive(), Duration::from_secs(1));
    }

    #[test]
    fn debug_never_prints_the_key() {
        let c = GladiaConfig::new(Secret::new("super-secret-key".to_owned()));
        assert!(!format!("{c:?}").contains("super-secret-key"));
    }
}
