//! Settings of the OpenAI-Realtime-transcription adapter, the dialect table, and the
//! construction of the WebSocket URL.

use std::time::Duration;

use brisvox_core::Secret;
use brisvox_core::frame::SampleRate;

use super::PROVIDER;
use crate::error::{Error, Result};
use crate::stt::SttConfig;
use crate::wsstt::{Settings, Target, check_base_url, is_token, percent_encode};

/// A server that speaks (a dialect of) the OpenAI Realtime transcription protocol.
///
/// The dialects differ in the URL, the session message, the audio rate, whether interim text is
/// streamed, and the event names; `docs/providers/openai-realtime-stt.md` lists each fact and
/// where it was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Dialect {
    /// OpenAI's Realtime API in transcription mode: 24 kHz, `session.update` of type
    /// `transcription`, no server VAD, interim deltas.
    #[default]
    OpenAi,
    /// NVIDIA Speech NIM (`/v1/realtime?intent=transcription`): 16 kHz,
    /// `transcription_session.update`, interim deltas, server-side endpointing.
    Nim,
    /// Speaches (`/v1/realtime?intent=transcription&model=...`): 24 kHz in, Whisper on commit,
    /// `completed` events only (no interim text), server VAD that cannot be switched off.
    Speaches,
    /// vLLM's `/v1/realtime` for streaming ASR models (Voxtral Realtime, Qwen3-ASR): 16 kHz,
    /// its own event names (`transcription.delta` / `transcription.done`).
    Vllm,
}

impl Dialect {
    /// The name used in definition files and logs.
    pub const fn name(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Nim => "nim",
            Self::Speaches => "speaches",
            Self::Vllm => "vllm",
        }
    }

    /// Parses [`Dialect::name`].
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "openai" => Some(Self::OpenAi),
            "nim" => Some(Self::Nim),
            "speaches" => Some(Self::Speaches),
            "vllm" => Some(Self::Vllm),
            _ => None,
        }
    }

    /// Where the dialect's server usually listens (scheme and authority).
    pub const fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAi => "wss://api.openai.com",
            Self::Nim => "ws://127.0.0.1:9000",
            Self::Speaches | Self::Vllm => "ws://127.0.0.1:8000",
        }
    }

    /// The rate of the audio the server expects.
    pub const fn default_rate(self) -> SampleRate {
        match self {
            Self::OpenAi | Self::Speaches => SampleRate::Hz24000,
            Self::Nim | Self::Vllm => SampleRate::Hz16000,
        }
    }

    /// The model used when the config names none (`None`: the server's own default, or, for
    /// Speaches and vLLM, a model is required).
    pub const fn default_model(self) -> Option<&'static str> {
        match self {
            Self::OpenAi => Some("gpt-live-transcribe"),
            Self::Nim | Self::Speaches | Self::Vllm => None,
        }
    }

    /// Whether the server streams interim text before the final.
    pub const fn streams_interims(self) -> bool {
        !matches!(self, Self::Speaches)
    }

    /// Whether a delta replaces the hypothesis so far rather than extending it (a guess for
    /// NVIDIA NIM, whose page does not say: see [`OpenAiRealtimeSttConfig::delta_replaces`]).
    pub(crate) const fn delta_replaces(self) -> bool {
        false
    }

    /// The least audio a commit may hold; a shorter buffer is answered empty here instead of
    /// provoking an error (OpenAI and Speaches document 100 ms).
    pub(crate) const fn min_commit(self) -> Duration {
        match self {
            Self::OpenAi | Self::Speaches => Duration::from_millis(100),
            Self::Nim | Self::Vllm => Duration::ZERO,
        }
    }
}

/// Settings of the OpenAI-Realtime-transcription STT adapter.
///
/// Create it with [`OpenAiRealtimeSttConfig::new`] and override fields; the struct is
/// `#[non_exhaustive]`. `Debug` never prints the API key ([`Secret`]).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct OpenAiRealtimeSttConfig {
    /// The server's dialect. Default [`Dialect::OpenAi`].
    pub dialect: Dialect,
    /// API key, sent only as `Authorization: Bearer <key>`. Required for OpenAI; local servers
    /// usually run without one.
    pub api_key: Option<Secret<String>>,
    /// Replaces the dialect's default `ws[s]://host[:port]` (scheme and authority only).
    /// Plain `ws://` is accepted for any host unless an `api_key` is set (then loopback only).
    pub base_url: Option<String>,
    /// Replaces the dialect's path and query (`/v1/realtime?...`) entirely, for a server or
    /// proxy that differs from the documented URL. Starts with `/`; `?` allowed; no spaces.
    pub path_and_query: Option<String>,
    /// The transcription model. Default: the dialect's ([`Dialect::default_model`]). Required
    /// for Speaches and vLLM. Letters, digits, `.`, `_`, `-`, `/` and `:` only.
    pub model: Option<String>,
    /// Language used when the session has none, sent in the form the dialect wants (OpenAI and
    /// Speaches: ISO-639-1 such as `tr`; NIM: a locale such as `tr-TR`). `None` leaves the
    /// field out (the server detects, or defaults to English). A language from the session
    /// (BCP-47) is mapped to that form instead.
    pub language: Option<String>,
    /// The languages the speaker is expected to use, when the session and [`Self::language`]
    /// name none: OpenAI's `gpt-live-transcribe` takes them as `languages` ("add `languages`
    /// when more than one input language is expected"); every other model and dialect detects
    /// the language itself when none is sent. This provider reports no language on streaming
    /// transcripts, so the engine falls back to text detection. Empty by default.
    pub languages: Vec<String>,
    /// Transcription prompt (OpenAI, NIM). `None` sends none.
    pub prompt: Option<String>,
    /// NIM: `enable_automatic_punctuation`. Default `true`.
    pub punctuation: bool,
    /// Whether a delta replaces the hypothesis so far (`true`) or extends it (`false`). `None`
    /// uses the dialect's: extend for all of them (OpenAI and vLLM document incremental
    /// deltas; NIM's page does not say, see the provider notes). Only interim text depends on
    /// it; the final text always comes from the `completed` / `done` event.
    pub delta_replaces: Option<bool>,
    /// The least audio a commit may hold; a `Finalize` with less buffered is answered empty
    /// here instead of provoking the server's error (the audio stays in the server's buffer and
    /// leads the next commit). `None` uses the dialect's: 100 ms for OpenAI and Speaches (their
    /// documented minimum), none for NIM and vLLM.
    pub min_commit: Option<Duration>,
    /// Rate of the audio sent to the server. `None` uses the dialect's
    /// ([`Dialect::default_rate`]). The engine's audio is resampled to it when it differs.
    pub sample_rate: Option<SampleRate>,
    /// Deadline of every network wait: the connect (TCP, TLS and WebSocket handshake together),
    /// each write, the hand-over of an event, and the sign of life owed by the server after
    /// audio was sent (half-way a ping asks for one; no sign within the deadline replaces the
    /// connection). Default 10 s.
    pub io_timeout: Duration,
    /// A ping goes out after this long without audio; the liveness wait is suspended until audio
    /// resumes. Default 5 s.
    pub keepalive_interval: Duration,
    /// How long to wait for the answer to a commit before ending the utterance with a synthetic
    /// final (the latest hypothesis, or empty). Keep it below the engine's own finalize timeout
    /// (3 s by default). Default 2 s.
    pub finalize_timeout: Duration,
    /// How long the close handshake may take on hang-up. Default 500 ms.
    pub close_timeout: Duration,
    /// Reconnect attempts per dropped connection (0 disables reconnecting). The counter starts
    /// over once a new connection delivers a transcript. Default 1.
    pub max_reconnects: u32,
    /// Wait before the first reconnect attempt; doubles per attempt (cap 5 s). Default 100 ms.
    pub reconnect_backoff: Duration,
    /// How much audio is kept for replay after a reconnect, and the most that is held back
    /// while the server cannot take it (the oldest is dropped first). Default 8 s.
    pub replay_window: Duration,
    /// Largest server message accepted. Default 1 MiB.
    pub max_message_bytes: usize,
}

impl OpenAiRealtimeSttConfig {
    /// Defaults for `dialect`.
    pub fn new(dialect: Dialect) -> Self {
        Self {
            dialect,
            api_key: None,
            base_url: None,
            path_and_query: None,
            model: None,
            language: None,
            languages: Vec::new(),
            prompt: None,
            punctuation: true,
            delta_replaces: None,
            min_commit: None,
            sample_rate: None,
            io_timeout: Duration::from_secs(10),
            keepalive_interval: Duration::from_secs(5),
            finalize_timeout: Duration::from_secs(2),
            close_timeout: Duration::from_millis(500),
            max_reconnects: 1,
            reconnect_backoff: Duration::from_millis(100),
            replay_window: Duration::from_secs(8),
            max_message_bytes: 1 << 20,
        }
    }

    /// The rate sent to the server.
    pub(crate) fn wire_rate(&self) -> SampleRate {
        self.sample_rate
            .unwrap_or_else(|| self.dialect.default_rate())
    }

    /// The model in use, if any.
    pub(crate) fn model(&self) -> Option<&str> {
        self.model
            .as_deref()
            .or_else(|| self.dialect.default_model())
    }

    /// The least audio a commit may hold.
    pub(crate) fn min_commit(&self) -> Duration {
        self.min_commit.unwrap_or_else(|| self.dialect.min_commit())
    }

    /// Whether deltas replace the hypothesis.
    pub(crate) fn replaces(&self) -> bool {
        self.delta_replaces
            .unwrap_or_else(|| self.dialect.delta_replaces())
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
        if self.dialect == Dialect::OpenAi && self.api_key.is_none() {
            return bad("the openai dialect needs an api_key");
        }
        match self.model() {
            None if matches!(self.dialect, Dialect::Speaches | Dialect::Vllm) => {
                return bad("this dialect needs a model");
            }
            Some(model) if !model.split(['/', ':']).all(is_token) => {
                return bad("model must be letters, digits, '.', '_', '-', '/' and ':' only");
            }
            _ => {}
        }
        if let Some(language) = &self.language
            && !is_token(language)
        {
            return bad("language must be letters, digits, '.', '_' or '-'");
        }
        if self.languages.len() > 16 || self.languages.iter().any(|l| !is_token(l)) {
            return bad("languages: at most 16 codes of letters, digits, '.', '_' or '-'");
        }
        if let Some(path) = &self.path_and_query
            && (!path.starts_with('/')
                || path.contains("//")
                || path
                    .chars()
                    .any(|c| c.is_whitespace() || c == '#' || c.is_control()))
        {
            return bad("path_and_query must start with '/' and hold no spaces or '#'");
        }
        if self.prompt.as_deref().is_some_and(|p| p.len() > 4096) {
            return bad("prompt must be at most 4096 bytes");
        }
        if self.io_timeout.is_zero()
            || self.keepalive_interval.is_zero()
            || self.finalize_timeout.is_zero()
            || self.close_timeout.is_zero()
        {
            return bad(
                "io_timeout, keepalive_interval, finalize_timeout and close_timeout must be positive",
            );
        }
        if self.replay_window > Duration::from_secs(60) {
            return bad("replay_window must be at most 60 s");
        }
        if self.max_message_bytes == 0 {
            return bad("max_message_bytes must be positive");
        }
        let base = self
            .base_url
            .as_deref()
            .unwrap_or_else(|| self.dialect.default_base_url());
        check_base_url(PROVIDER, base, self.api_key.is_some())?;
        Ok(())
    }

    /// The session core's view of these settings.
    pub(crate) fn settings(&self) -> Settings {
        Settings {
            provider: PROVIDER,
            connect_label: "openai_realtime_stt connect",
            io_timeout: self.io_timeout,
            keepalive_interval: self.keepalive_interval,
            finalize_timeout: self.finalize_timeout,
            close_timeout: self.close_timeout,
            max_reconnects: self.max_reconnects,
            reconnect_backoff: self.reconnect_backoff,
            replay_window: self.replay_window,
            max_message_bytes: self.max_message_bytes,
            wire_rate: self.wire_rate(),
        }
    }

    /// The language to send for a session: the session's (BCP-47) mapped to the dialect's form,
    /// else the configured one, else none.
    pub(crate) fn language_for(&self, stt: &SttConfig) -> Option<String> {
        match stt.language.as_deref().filter(|l| !l.is_empty()) {
            Some(tag) => dialect_language(self.dialect, tag),
            None => self.language.clone(),
        }
    }

    /// The path and query of the URL.
    pub(crate) fn path_and_query(&self, language: Option<&str>) -> String {
        if let Some(custom) = &self.path_and_query {
            return custom.clone();
        }
        match self.dialect {
            Dialect::OpenAi => {
                let model = self.model().unwrap_or_default();
                format!("/v1/realtime?model={}", percent_encode(model))
            }
            Dialect::Nim => "/v1/realtime?intent=transcription".to_owned(),
            Dialect::Speaches => {
                let mut q = String::from("/v1/realtime?intent=transcription");
                if let Some(model) = self.model() {
                    q.push_str("&model=");
                    q.push_str(&percent_encode(model));
                }
                if let Some(language) = language {
                    q.push_str("&language=");
                    q.push_str(&percent_encode(language));
                }
                q
            }
            Dialect::Vllm => "/v1/realtime".to_owned(),
        }
    }

    /// Where to connect for a session.
    ///
    /// # Errors
    /// [`Error::Protocol`] when the settings are invalid or TLS cannot be set up.
    pub(crate) fn target(&self, language: Option<&str>) -> Result<Target> {
        self.validate()?;
        let base = self
            .base_url
            .as_deref()
            .unwrap_or_else(|| self.dialect.default_base_url());
        let (authority, tls) = check_base_url(PROVIDER, base, self.api_key.is_some())?;
        let (scheme, tls_config) = if tls {
            ("wss", Some(crate::net::tls_config()?))
        } else {
            ("ws", None)
        };
        Ok(Target {
            url: format!("{scheme}://{authority}{}", self.path_and_query(language)),
            bearer: self.api_key.clone(),
            tls: tls_config,
        })
    }
}

/// A session's BCP-47 tag in the form `dialect` wants.
fn dialect_language(dialect: Dialect, tag: &str) -> Option<String> {
    let primary = tag
        .split(['-', '_'])
        .next()
        .unwrap_or(tag)
        .to_ascii_lowercase();
    if !is_token(&primary) {
        return None;
    }
    match dialect {
        Dialect::OpenAi | Dialect::Speaches => Some(primary),
        Dialect::Vllm => None,
        Dialect::Nim => {
            if tag.contains(['-', '_']) {
                Some(tag.replace('_', "-"))
            } else {
                Some(match primary.as_str() {
                    "en" => "en-US".to_owned(),
                    "tr" => "tr-TR".to_owned(),
                    _ => primary,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stt(language: Option<&str>) -> SttConfig {
        SttConfig {
            sample_rate: SampleRate::Hz16000,
            language: language.map(Into::into),
        }
    }

    fn key() -> Option<Secret<String>> {
        Some(Secret::new("sk-test".to_owned()))
    }

    #[test]
    fn each_dialect_has_its_documented_url_rate_and_defaults() {
        let mut openai = OpenAiRealtimeSttConfig::new(Dialect::OpenAi);
        openai.api_key = key();
        let target = openai.target(None);
        assert!(target.is_ok_and(|t| {
            t.url == "wss://api.openai.com/v1/realtime?model=gpt-live-transcribe" && t.tls.is_some()
        }));
        assert_eq!(openai.wire_rate(), SampleRate::Hz24000);

        let nim = OpenAiRealtimeSttConfig::new(Dialect::Nim);
        assert!(
            nim.target(None)
                .is_ok_and(|t| t.url == "ws://127.0.0.1:9000/v1/realtime?intent=transcription")
        );
        assert_eq!(nim.wire_rate(), SampleRate::Hz16000);

        let mut speaches = OpenAiRealtimeSttConfig::new(Dialect::Speaches);
        speaches.model = Some("Systran/faster-whisper-small".to_owned());
        assert!(speaches.target(Some("tr")).is_ok_and(|t| {
            t.url
                == "ws://127.0.0.1:8000/v1/realtime?intent=transcription&model=Systran%2Ffaster-whisper-small&language=tr"
        }));
        assert_eq!(speaches.wire_rate(), SampleRate::Hz24000);

        let mut vllm = OpenAiRealtimeSttConfig::new(Dialect::Vllm);
        vllm.model = Some("mistralai/Voxtral-Mini-4B-Realtime-2602".to_owned());
        assert!(
            vllm.target(None)
                .is_ok_and(|t| t.url == "ws://127.0.0.1:8000/v1/realtime")
        );
        assert_eq!(vllm.wire_rate(), SampleRate::Hz16000);
    }

    #[test]
    fn required_fields_and_unsafe_settings_are_rejected() {
        // OpenAI needs a key; Speaches and vLLM need a model.
        assert!(
            OpenAiRealtimeSttConfig::new(Dialect::OpenAi)
                .validate()
                .is_err()
        );
        assert!(
            OpenAiRealtimeSttConfig::new(Dialect::Speaches)
                .validate()
                .is_err()
        );
        assert!(
            OpenAiRealtimeSttConfig::new(Dialect::Vllm)
                .validate()
                .is_err()
        );
        assert!(
            OpenAiRealtimeSttConfig::new(Dialect::Nim)
                .validate()
                .is_ok()
        );
        // A key is never sent in plain text to a remote host.
        let mut cfg = OpenAiRealtimeSttConfig::new(Dialect::Nim);
        cfg.api_key = key();
        cfg.base_url = Some("ws://10.1.2.3:9000".to_owned());
        assert!(cfg.validate().is_err());
        cfg.base_url = Some("ws://127.0.0.1:9000".to_owned());
        assert!(cfg.validate().is_ok());
        // Injection through model, language or path is refused.
        let mut cfg = OpenAiRealtimeSttConfig::new(Dialect::Nim);
        cfg.model = Some("a&b".to_owned());
        assert!(cfg.validate().is_err());
        cfg.model = None;
        cfg.language = Some("tr&x=1".to_owned());
        assert!(cfg.validate().is_err());
        cfg.language = None;
        cfg.path_and_query = Some("/v1/realtime ?x".to_owned());
        assert!(cfg.validate().is_err());
        cfg.path_and_query = Some("v1/realtime".to_owned());
        assert!(cfg.validate().is_err());
        cfg.path_and_query = Some("/custom/rt?intent=transcription".to_owned());
        assert!(cfg.validate().is_ok());
        assert!(
            cfg.target(None)
                .is_ok_and(|t| t.url == "ws://127.0.0.1:9000/custom/rt?intent=transcription")
        );
    }

    #[test]
    fn session_languages_map_to_each_dialects_form() {
        let mut cfg = OpenAiRealtimeSttConfig::new(Dialect::OpenAi);
        assert_eq!(cfg.language_for(&stt(Some("tr-TR"))).as_deref(), Some("tr"));
        assert_eq!(cfg.language_for(&stt(None)), None);
        cfg.language = Some("de".to_owned());
        assert_eq!(cfg.language_for(&stt(None)).as_deref(), Some("de"));
        let nim = OpenAiRealtimeSttConfig::new(Dialect::Nim);
        assert_eq!(nim.language_for(&stt(Some("tr"))).as_deref(), Some("tr-TR"));
        assert_eq!(
            nim.language_for(&stt(Some("en_GB"))).as_deref(),
            Some("en-GB")
        );
        assert_eq!(nim.language_for(&stt(Some("de"))).as_deref(), Some("de"));
        let vllm = OpenAiRealtimeSttConfig::new(Dialect::Vllm);
        assert_eq!(vllm.language_for(&stt(Some("tr"))), None);
    }

    #[test]
    fn dialect_names_round_trip() {
        for d in [
            Dialect::OpenAi,
            Dialect::Nim,
            Dialect::Speaches,
            Dialect::Vllm,
        ] {
            assert_eq!(Dialect::from_name(d.name()), Some(d));
        }
        assert_eq!(Dialect::from_name("whisper"), None);
        assert!(Dialect::Nim.streams_interims() && !Dialect::Speaches.streams_interims());
    }
}
