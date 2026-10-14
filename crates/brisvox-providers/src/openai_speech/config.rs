//! Configuration and per-server presets of the OpenAI-compatible speech adapter.

use std::fmt;
use std::time::Duration;

use brisvox_core::{Secret, SharedClock};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

/// Provider name used in errors and logs.
pub const PROVIDER: &str = "openai-speech";

/// Path appended to the base URL (the base URL carries the `/v1` prefix, like the base URL of
/// the OpenAI SDKs).
pub const DEFAULT_SPEECH_PATH: &str = "/audio/speech";

/// Default for [`OpenAiSpeechConfig::connect_timeout`].
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Default for [`OpenAiSpeechConfig::first_audio_timeout`]: a local CPU Kokoro needs seconds
/// for the first chunk (README: about 3.5 s on an older i7), and a server that answers only
/// when the whole segment is synthesized (Chatterbox-TTS-Server) needs the whole synthesis.
pub const DEFAULT_FIRST_AUDIO_TIMEOUT: Duration = Duration::from_secs(15);

/// Default for [`OpenAiSpeechConfig::idle_timeout`].
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(8);

/// Default for [`OpenAiSpeechConfig::max_request_duration`].
pub const DEFAULT_MAX_REQUEST_DURATION: Duration = Duration::from_secs(120);

/// Default for [`OpenAiSpeechConfig::pool_idle_timeout`]: 10 minutes.
pub const DEFAULT_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// Default for [`OpenAiSpeechConfig::coalesce_chars`].
pub const DEFAULT_COALESCE_CHARS: usize = 300;

/// Default for [`OpenAiSpeechConfig::warm_interval`]: a GPU server that idles for a minute or
/// more has gone cold (Kokoro-FastAPI measured 2.8 s for the first synthesis after idle).
pub const DEFAULT_WARM_INTERVAL: Duration = Duration::from_secs(60);

/// Default for [`OpenAiSpeechConfig::warm_timeout`].
pub const DEFAULT_WARM_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest [`OpenAiSpeechConfig::warm_text`] `OpenAiSpeechTts::new` accepts, in bytes.
pub const MAX_WARM_TEXT_BYTES: usize = 200;

/// Request fields the adapter owns: [`OpenAiSpeechConfig::extra_body`] must not set them.
pub(crate) const RESERVED_BODY_KEYS: [&str; 7] = [
    "model",
    "input",
    "voice",
    "response_format",
    "speed",
    "stream",
    "sample_rate",
];

/// The `response_format` requested from the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum AudioFormat {
    /// `pcm`: headerless signed 16-bit little-endian mono. OpenAI defines it as 24 kHz;
    /// Kokoro-FastAPI, Speaches and vLLM-Omni serve it (not Chatterbox-TTS-Server).
    #[default]
    Pcm,
    /// `wav`: the adapter reads the RIFF header (rate, channels and sample format are checked
    /// against the session) and forwards the samples after it. For servers without `pcm`
    /// (Chatterbox-TTS-Server accepts `wav`, `opus`, `mp3`).
    Wav,
}

impl AudioFormat {
    /// The wire value.
    pub(crate) const fn wire_name(self) -> &'static str {
        match self {
            Self::Pcm => "pcm",
            Self::Wav => "wav",
        }
    }
}

/// A local server whose quirks the adapter knows (`docs/providers/openai-speech.md`, fetched
/// 2026-10-14). A preset fills in the model id, the audio format and the request fields that
/// server wants; every field can be overridden afterwards. None of them has a default base URL:
/// where the server runs is the operator's to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Preset {
    /// Any server speaking `POST {base}/audio/speech`: model and voice must be set, `pcm`, no
    /// extra fields.
    Generic,
    /// Kokoro-FastAPI (`ghcr.io/remsky/kokoro-fastapi-*`, port 8880): model `kokoro`, `pcm`
    /// at 24 kHz, `stream: true`.
    Kokoro,
    /// Chatterbox-TTS-Server (`devnen`, port 8004): model `chatterbox` (required by the
    /// request model, unused by the handler), `wav` (the route has no `pcm`), a whole
    /// segment per response.
    Chatterbox,
    /// Speaches: model `speaches-ai/Kokoro-82M-v1.0-ONNX`, `pcm` at the session's rate (the
    /// adapter sends `sample_rate`).
    Speaches,
    /// vLLM-Omni serving a TTS model (Voxtral TTS): `pcm` at 24 kHz. The model id is the one
    /// the server was started with; the weights of Voxtral TTS are non-commercial.
    VllmOmni,
}

impl Preset {
    /// Every preset.
    pub const ALL: [Self; 5] = [
        Self::Generic,
        Self::Kokoro,
        Self::Chatterbox,
        Self::Speaches,
        Self::VllmOmni,
    ];

    /// The model id the preset sends unless overridden (`None`: the operator must set one).
    pub const fn model(self) -> Option<&'static str> {
        match self {
            Self::Generic => None,
            Self::Kokoro => Some("kokoro"),
            Self::Chatterbox => Some("chatterbox"),
            Self::Speaches => Some("speaches-ai/Kokoro-82M-v1.0-ONNX"),
            Self::VllmOmni => Some("mistralai/Voxtral-4B-TTS-2603"),
        }
    }

    /// A voice id that server is documented to have, for examples and the live test (`None`:
    /// depends on the deployment). The Chatterbox name is a file of the server's voices
    /// directory and UNVERIFIED: list the real ones with `GET /v1/audio/voices`.
    pub const fn example_voice(self) -> Option<&'static str> {
        match self {
            Self::Kokoro | Self::Speaches => Some("af_heart"),
            Self::Chatterbox => Some("Emily.wav"),
            Self::Generic | Self::VllmOmni => None,
        }
    }

    /// The name used in definition files (`preset = "kokoro"`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Kokoro => "kokoro",
            Self::Chatterbox => "chatterbox",
            Self::Speaches => "speaches",
            Self::VllmOmni => "vllm_omni",
        }
    }
}

/// How the OpenAI-compatible speech adapter is configured. Build with
/// [`OpenAiSpeechConfig::new`] or [`OpenAiSpeechConfig::preset`], then set fields.
#[derive(Clone)]
#[non_exhaustive]
pub struct OpenAiSpeechConfig {
    /// The server's API base including its version prefix, e.g. `http://localhost:8880/v1`.
    /// `https://` anywhere; `http://` for loopback hosts, and for any host while no
    /// [`api_key`](Self::api_key) is set (nothing secret is sent), or with
    /// [`allow_insecure_http`](Self::allow_insecure_http).
    pub base_url: String,
    /// Path appended to the base URL's path.
    pub speech_path: String,
    /// Bearer token, if the server wants one (local servers mostly do not).
    pub api_key: Option<Secret<String>>,
    /// The `model` field.
    pub model: String,
    /// The `response_format` requested.
    pub format: AudioFormat,
    /// The `speed` field, or `None` for the server's default.
    pub speed: Option<f32>,
    /// The `stream` field, or `None` to leave it out (Kokoro-FastAPI streams by default; its
    /// documented field is sent by the preset anyway).
    pub stream: Option<bool>,
    /// Send `sample_rate` (the session's rate) so the server resamples (Speaches). Otherwise
    /// the server delivers 24 kHz and `connect` refuses any other session rate.
    pub send_sample_rate: bool,
    /// Further top-level body fields (`lang_code`, ...), values included in the request
    /// verbatim. Keys the adapter owns are refused by `OpenAiSpeechTts::new`.
    pub extra_body: Map<String, Value>,
    /// Allow `http://` to a non-loopback host although an API key is set.
    pub allow_insecure_http: bool,
    /// TCP connect deadline.
    pub connect_timeout: Duration,
    /// From sending a request to its first audio byte (connect, headers and the server's
    /// synthesis time of the first chunk).
    pub first_audio_timeout: Duration,
    /// Silence tolerated between two body chunks after the first audio.
    pub idle_timeout: Duration,
    /// Cap on one request, whatever the server sends.
    pub max_request_duration: Duration,
    /// How long an idle pooled connection is kept.
    pub pool_idle_timeout: Option<Duration>,
    /// Pending text pieces of one context are joined into one request while the server is
    /// busy, up to this many characters (fewer requests when the server is slower than the
    /// language model; 0 turns joining off).
    pub coalesce_chars: usize,
    /// Text of the model warm-up request that [`warm`](crate::TtsProvider::warm) starts in the
    /// background (audio discarded), so the first real synthesis after an idle period does not
    /// pay the server's cold start (GPU clocks, caches, lazily loaded weights). Empty: off.
    /// At most [`MAX_WARM_TEXT_BYTES`] bytes. The [`Preset::Kokoro`] default is `"Hi."`; the
    /// other presets leave it off.
    pub warm_text: String,
    /// At most one warm-up request per this interval (on the injected clock), however many
    /// sessions start; the first call after construction always warms.
    pub warm_interval: Duration,
    /// Cap on one warm-up request (send to the last audio byte).
    pub warm_timeout: Duration,
    /// Voice of the warm-up request while no session has connected yet (afterwards the voice
    /// of the latest session is used). `None`: no warm-up until a session has connected. The
    /// [`Preset::Kokoro`] default is its example voice.
    pub warm_voice: Option<String>,
    /// Time source for every deadline.
    pub clock: SharedClock,
    /// Cancelling this ends every session of the provider.
    pub shutdown: CancellationToken,
}

impl OpenAiSpeechConfig {
    /// A configuration for a server without a preset: `model` is sent verbatim, audio is `pcm`.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>, clock: SharedClock) -> Self {
        Self {
            base_url: base_url.into(),
            speech_path: DEFAULT_SPEECH_PATH.to_owned(),
            api_key: None,
            model: model.into(),
            format: AudioFormat::Pcm,
            speed: None,
            stream: None,
            send_sample_rate: false,
            extra_body: Map::new(),
            allow_insecure_http: false,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            first_audio_timeout: DEFAULT_FIRST_AUDIO_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            max_request_duration: DEFAULT_MAX_REQUEST_DURATION,
            pool_idle_timeout: Some(DEFAULT_POOL_IDLE_TIMEOUT),
            coalesce_chars: DEFAULT_COALESCE_CHARS,
            warm_text: String::new(),
            warm_interval: DEFAULT_WARM_INTERVAL,
            warm_timeout: DEFAULT_WARM_TIMEOUT,
            warm_voice: None,
            clock,
            shutdown: CancellationToken::new(),
        }
    }

    /// The configuration `preset` describes for the server at `base_url`. The model is empty for
    /// [`Preset::Generic`] (set it).
    pub fn preset(preset: Preset, base_url: impl Into<String>, clock: SharedClock) -> Self {
        let mut config = Self::new(base_url, preset.model().unwrap_or_default(), clock);
        match preset {
            Preset::Generic | Preset::VllmOmni => {}
            Preset::Kokoro => {
                config.stream = Some(true);
                // Measured 2026-10-14: the first synthesis after idle took 2.78 s, 52-104 ms
                // once warm. A tiny request hides that; it is cheap on a streaming GPU server.
                "Hi.".clone_into(&mut config.warm_text);
                config.warm_voice = preset.example_voice().map(str::to_owned);
            }
            Preset::Chatterbox => config.format = AudioFormat::Wav,
            Preset::Speaches => config.send_sample_rate = true,
        }
        config
    }
}

impl fmt::Debug for OpenAiSpeechConfig {
    /// Shows the origin only (the base URL may carry credentials), the key redacted.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let body_keys: Vec<&String> = self.extra_body.keys().collect();
        f.debug_struct("OpenAiSpeechConfig")
            .field("base_url", &super::origin_of(&self.base_url))
            .field("speech_path", &self.speech_path.split(['?', '#']).next())
            .field("api_key", &self.api_key)
            .field("model", &self.model)
            .field("format", &self.format)
            .field("speed", &self.speed)
            .field("stream", &self.stream)
            .field("send_sample_rate", &self.send_sample_rate)
            .field("extra_body", &body_keys)
            .field("allow_insecure_http", &self.allow_insecure_http)
            .field("connect_timeout", &self.connect_timeout)
            .field("first_audio_timeout", &self.first_audio_timeout)
            .field("idle_timeout", &self.idle_timeout)
            .field("max_request_duration", &self.max_request_duration)
            .field("pool_idle_timeout", &self.pool_idle_timeout)
            .field("coalesce_chars", &self.coalesce_chars)
            .field("warm_text", &self.warm_text)
            .field("warm_interval", &self.warm_interval)
            .field("warm_timeout", &self.warm_timeout)
            .field("warm_voice", &self.warm_voice)
            .finish_non_exhaustive()
    }
}
