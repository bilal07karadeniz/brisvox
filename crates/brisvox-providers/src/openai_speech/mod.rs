//! Text-to-speech over the OpenAI-compatible `POST {base_url}/audio/speech` protocol, as served
//! by local servers (feature `provider-openai-speech`; protocol facts in
//! `docs/providers/openai-speech.md`, fetched 2026-10-14): Kokoro-FastAPI, Speaches,
//! Chatterbox-TTS-Server, vLLM-Omni, or any server with that route.
//!
//! # Shape
//! [`OpenAiSpeechTts`] implements [`TtsProvider`]. `connect` opens nothing: it spawns the task
//! that owns the session and returns at once (the HTTP connection comes from the process-wide
//! pool, `net::shared_client`, and is opened by [`warm`](TtsProvider::warm) or by the first
//! request). Consequently a refusal (401, 429, 5xx), an unreachable server or a stalled one is
//! reported as a [`TtsEvent::Error`] after the first text, not as a `connect` error, and the
//! session ends with it; the engine reconnects at its next round.
//!
//! # One request per text piece
//! The protocol takes the whole text in one request, so every [`TtsInput::Text`] the engine
//! sends (the segmenter already cuts the language model's output at sentence and clause
//! boundaries) is one `POST`: `{model, input, voice, response_format: "pcm", ..}` (see
//! [`OpenAiSpeechConfig`] for the optional fields). Requests run one after the other, in the
//! order the text arrived, across contexts: audio of one request is never interleaved with
//! another's. While a request is in flight, further pieces of the same context pile up and are
//! joined into one request (up to [`OpenAiSpeechConfig::coalesce_chars`]), which keeps a slow
//! server from falling behind with a request per clause. [`TtsInput::Flush`] answers with
//! [`TtsEvent::Flushed`] once every earlier piece has been fully received; it triggers no request
//! of its own (nothing is buffered waiting for it, unlike the Deepgram adapter).
//!
//! # Audio
//! Chunks of the response body become [`TtsEvent::Audio`] as they arrive, cut to whole 16-bit
//! samples (a sample that straddles two chunks is joined; that case costs a copy, the usual one
//! none). `pcm` is 24 kHz mono per the OpenAI documentation, so the session rate must be 24 kHz
//! (the engine resamples to the call): `connect` refuses another rate unless the config sends
//! `sample_rate` (Speaches). The `wav` format (Chatterbox-TTS-Server) is read through a small
//! RIFF parser whose declared rate must equal the session rate. A `200` with a JSON, text or XML
//! content type is a [`Error::Protocol`](crate::Error::Protocol) (an error page, or the wrong route).
//!
//! # Words
//! No word timestamps are requested, so this adapter NEVER emits [`TtsEvent::Word`] and the
//! consumer estimates word times from the text and the audio length. Kokoro-FastAPI has
//! `/dev/captioned_speech` (JSON items with base64 audio and word timestamps), but it is a
//! development route whose stream framing and `WordTimestamp` fields could not be confirmed from
//! the sources read, so it is left to a follow-up that has a live server to check against.
//!
//! # Cancel (barge-in)
//! A cancel is served before everything else, also while a request is being sent or its body
//! read: the context's queued text and unsent events are dropped, [`TtsEvent::Cancelled`] is
//! queued at once, and the response in flight is dropped, which resets the HTTP/2 stream or
//! closes the HTTP/1.1 connection. Kokoro-FastAPI checks for the disconnect before each chunk
//! and stops generating; other servers may finish the segment unobserved. Input that arrives for
//! a cancelled context afterwards is ignored (the last 64 cancels are remembered; do not reuse
//! ids). A cancel of an unknown or finished context still yields `Cancelled`.
//!
//! # Errors
//! Non-2xx is [`Error::Rejected`](crate::Error::Rejected) with the status; the body is not read (it can echo the
//! request) and nothing is retried. The exception: `400` and `422` refuse one *text piece*
//! (Kokoro-FastAPI answers 400 to a segment without speakable characters). That piece is skipped
//! with a warning; two refusals in a row without audio in between end the session with the
//! status (a wrong model or voice looks like that). A body cut off mid-stream, a reset or a
//! refused connection is [`Error::Disconnected`](crate::Error::Disconnected); a response that ends inside the WAV header, or a
//! WAV that is not 16-bit mono PCM, is a protocol error. Every error ends the session, after
//! the audio received before it.
//!
//! # Timeouts, backpressure, bounds
//! Connect ([`connect_timeout`](OpenAiSpeechConfig::connect_timeout)), request send to first
//! audio byte ([`first_audio_timeout`](OpenAiSpeechConfig::first_audio_timeout)), the silence
//! between body chunks afterwards ([`idle_timeout`](OpenAiSpeechConfig::idle_timeout)) and the
//! whole request ([`max_request_duration`](OpenAiSpeechConfig::max_request_duration)) are all
//! bounded on the injected clock; a miss is `Core(Timeout)`. The body is not read while 8 events
//! wait for the consumer (TCP backpressure, nothing dropped, and the idle deadline is not
//! charged for that time); the input channel is not read while 128 pieces queue (the sender
//! waits). The cancel channel is never gated.
//!
//! # Model warm-up
//! A GPU server that has idled goes cold: Kokoro-FastAPI took 2.78 s for the first synthesis
//! after idle, against 52-104 ms warm. `warm()` therefore also spawns (never awaits: the
//! engine's warm budget is `min(connect_timeout, 400 ms)`) one tiny request with
//! [`warm_text`](OpenAiSpeechConfig::warm_text) (audio drained and dropped), at most one at a
//! time and at most one per [`warm_interval`](OpenAiSpeechConfig::warm_interval) on the
//! injected clock across all sessions of the provider, bounded by
//! [`warm_timeout`](OpenAiSpeechConfig::warm_timeout) and by the provider's shutdown token. The
//! voice is the latest session's, or [`warm_voice`](OpenAiSpeechConfig::warm_voice) before any;
//! `connect` starts the same warm-up too (the first call of a process has no voice at `warm()`
//! time), which the interval makes a no-op when `warm()` just did.
//! A failed warm-up is only logged and may be retried by the next `warm()`. On by default for
//! the Kokoro preset only; empty `warm_text` turns it off.
//!
//! # Secrets and plain HTTP
//! The API key (optional) is a [`Secret`], sent only as `Authorization: Bearer <key>` (marked
//! sensitive), never in a URL, log, error or `Debug` output. `http://` base URLs are accepted
//! for loopback hosts; for other hosts only without a key, or with
//! [`OpenAiSpeechConfig::allow_insecure_http`].

mod config;
mod session;
mod warm;
mod wav;

use std::sync::Arc;

use bytes::Bytes;
use reqwest::header::{AUTHORIZATION, HeaderMap};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use self::session::Params;
pub use brisvox_core::Secret;
pub use config::{
    AudioFormat, DEFAULT_COALESCE_CHARS, DEFAULT_CONNECT_TIMEOUT, DEFAULT_FIRST_AUDIO_TIMEOUT,
    DEFAULT_IDLE_TIMEOUT, DEFAULT_MAX_REQUEST_DURATION, DEFAULT_POOL_IDLE_TIMEOUT,
    DEFAULT_SPEECH_PATH, DEFAULT_WARM_INTERVAL, DEFAULT_WARM_TIMEOUT, MAX_WARM_TEXT_BYTES,
    OpenAiSpeechConfig, PROVIDER, Preset,
};

use crate::error::Result;
use crate::http_llm::{self, protocol, within};
use crate::tts::{TTS_CANCEL_CAPACITY, TTS_CHANNEL_CAPACITY, TtsConfig, TtsProvider, TtsSession};
#[cfg(doc)]
use crate::tts::{TtsEvent, TtsInput};

/// The sample rate OpenAI's `pcm` format has, and so what servers without a `sample_rate`
/// field deliver.
const SERVER_RATE_HZ: u32 = 24_000;

/// Streaming-ish text-to-speech on any OpenAI-compatible speech server. See the
/// [module docs](self). Cheap to share: wrap it in an `Arc`. The HTTP client (and its
/// connection pool) comes from the process-wide pool, keyed by origin.
#[derive(Debug)]
pub struct OpenAiSpeechTts {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) config: OpenAiSpeechConfig,
    pub(crate) client: reqwest::Client,
    pub(crate) url: String,
    /// The auth header, sent with every request.
    pub(crate) headers: HeaderMap,
    /// `extra_body` plus the fixed fields, as a template for every request body.
    template: Map<String, Value>,
    /// State of the background model warm-up (see `warm`).
    pub(crate) warm: warm::WarmState,
}

impl std::fmt::Debug for Inner {
    /// Omits the URL and the headers: they may carry credentials.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Inner {
    /// The JSON body for one text piece.
    pub(crate) fn request_body(&self, params: &Params, text: &str) -> Result<Bytes> {
        let mut body = self.template.clone();
        body.insert("input".to_owned(), Value::String(text.to_owned()));
        body.insert("voice".to_owned(), Value::String(params.voice.to_string()));
        if self.config.send_sample_rate {
            body.insert("sample_rate".to_owned(), json!(params.rate.hz()));
        }
        serde_json::to_vec(&body)
            .map(Bytes::from)
            .map_err(|_| protocol(PROVIDER, "the request body cannot be serialised"))
    }
}

/// `scheme://host[:port]` of `url` for logs and `Debug`, never the path or credentials.
pub(crate) fn origin_of(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(parsed) => {
            let host = parsed.host_str().unwrap_or("?");
            match parsed.port() {
                Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
                None => format!("{}://{host}", parsed.scheme()),
            }
        }
        Err(_) => "<invalid url>".to_owned(),
    }
}

/// The speech URL: `path` appended to the path of `base_url`; the base URL's query stays after
/// it, the fragment is dropped.
fn speech_url(base_url: &str, path: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(base_url)
        .map_err(|_| protocol(PROVIDER, "base_url is not a valid absolute URL"))?;
    if url.cannot_be_a_base() {
        return Err(protocol(PROVIDER, "base_url cannot carry a path"));
    }
    if !path.starts_with('/') || path.contains(['?', '#']) {
        return Err(protocol(
            PROVIDER,
            "speech_path must start with '/' and carry no query",
        ));
    }
    let joined = format!("{}{path}", url.path().trim_end_matches('/'));
    url.set_path(&joined);
    url.set_fragment(None);
    Ok(url.into())
}

impl OpenAiSpeechTts {
    /// Builds the adapter and its HTTP client (rustls with the shared config, no system proxy,
    /// no redirects).
    ///
    /// # Errors
    /// [`Error::Protocol`](crate::Error::Protocol) if the base URL is invalid or is plain `http://` to a remote host with
    /// an API key and without `allow_insecure_http`, the model is empty, the speech path is
    /// malformed, the API key is not a valid header value (it is never echoed), `extra_body`
    /// sets a field the adapter owns, or the HTTP client cannot be built.
    pub fn new(config: OpenAiSpeechConfig) -> Result<Self> {
        if config.model.trim().is_empty() {
            return Err(protocol(PROVIDER, "the model id is empty"));
        }
        if let Some(speed) = config.speed
            && !(speed.is_finite() && speed > 0.0)
        {
            return Err(protocol(PROVIDER, "speed must be a positive number"));
        }
        if config.warm_text.len() > config::MAX_WARM_TEXT_BYTES {
            return Err(protocol(
                PROVIDER,
                format!(
                    "warm_text is longer than {} bytes",
                    config::MAX_WARM_TEXT_BYTES
                ),
            ));
        }
        http_llm::check_base_url(
            PROVIDER,
            &config.base_url,
            config.allow_insecure_http || config.api_key.is_none(),
        )?;
        let url = speech_url(&config.base_url, &config.speech_path)?;
        let mut template = Map::new();
        for (key, value) in &config.extra_body {
            if config::RESERVED_BODY_KEYS.contains(&key.as_str()) {
                return Err(protocol(
                    PROVIDER,
                    format!(
                        "extra_body sets {}, which the adapter owns",
                        http_llm::sanitize(key)
                    ),
                ));
            }
            template.insert(key.clone(), value.clone());
        }
        template.insert("model".to_owned(), Value::String(config.model.clone()));
        template.insert(
            "response_format".to_owned(),
            Value::String(config.format.wire_name().to_owned()),
        );
        if let Some(speed) = config.speed {
            template.insert("speed".to_owned(), json!(speed));
        }
        if let Some(stream) = config.stream {
            template.insert("stream".to_owned(), Value::Bool(stream));
        }
        let client = crate::net::shared_client(&crate::net::HttpClientSpec {
            provider: PROVIDER,
            base_url: &config.base_url,
            connect_timeout: config.connect_timeout,
            pool_idle_timeout: config.pool_idle_timeout,
            http2_prior_knowledge: false,
        })?;
        let mut headers = HeaderMap::new();
        if let Some(key) = &config.api_key {
            let value = format!("Bearer {}", key.expose());
            headers.insert(
                AUTHORIZATION,
                http_llm::sensitive_header_value(PROVIDER, &value, "the API key")?,
            );
        }
        let warm = warm::WarmState::new(config.clock.now());
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                client,
                url,
                headers,
                template,
                warm,
            }),
        })
    }

    /// The configuration in use (the API key prints as `[REDACTED]`).
    pub fn config(&self) -> &OpenAiSpeechConfig {
        &self.inner.config
    }
}

impl TtsProvider for OpenAiSpeechTts {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    async fn connect(&self, config: TtsConfig) -> Result<TtsSession> {
        let inner = &self.inner;
        if config.voice.trim().is_empty() {
            return Err(protocol(PROVIDER, "the voice is empty"));
        }
        if !inner.config.send_sample_rate && config.sample_rate.hz() != SERVER_RATE_HZ {
            return Err(protocol(
                PROVIDER,
                format!(
                    "the server delivers {SERVER_RATE_HZ} Hz audio but the session asks for {} Hz \
                     (set the TTS sample rate to 24000; the engine resamples to the call)",
                    config.sample_rate.hz()
                ),
            ));
        }
        let (input_tx, input_rx) = mpsc::channel(TTS_CHANNEL_CAPACITY);
        let (cancel_tx, cancel_rx) = mpsc::channel(TTS_CANCEL_CAPACITY);
        let (events_tx, events_rx) = mpsc::channel(TTS_CHANNEL_CAPACITY);
        inner.warm.remember_voice(&config.voice);
        // The first call of a process has no voice at `warm()` time: start the model warm-up here
        // (a no-op while one ran or runs within the interval).
        warm::start(inner);
        let params = Params {
            voice: config.voice,
            rate: config.sample_rate,
        };
        tokio::spawn(session::run(
            Arc::clone(inner),
            params,
            input_rx,
            cancel_rx,
            events_tx,
        ));
        Ok(TtsSession {
            input: input_tx,
            cancel: cancel_tx,
            events: events_rx,
        })
    }

    /// Opens a pooled connection with an unauthenticated `HEAD` to the speech URL: any HTTP
    /// answer (401, 404, 405 ...) proves the path works and leaves the connection in the pool.
    ///
    /// Also starts, in the background and not awaited, a tiny synthesis request that warms the
    /// model itself when [`OpenAiSpeechConfig::warm_text`] is set and none ran within
    /// [`OpenAiSpeechConfig::warm_interval`] (see the module docs, "Model warm-up"). Cold GPU
    /// servers need seconds for it, so the caller's short warm budget must not wait for it.
    async fn warm(&self) -> Result<()> {
        let inner = &self.inner;
        warm::start(inner);
        let config = &inner.config;
        let head = inner.client.head(&inner.url).send();
        within(&config.clock, config.connect_timeout, "tts warm", head)
            .await?
            .map(drop)
            .map_err(|e| {
                http_llm::transport_error(PROVIDER, &e, config.connect_timeout, "tts warm")
            })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use brisvox_core::SystemClock;

    use super::*;

    fn config(base: &str) -> OpenAiSpeechConfig {
        OpenAiSpeechConfig::preset(Preset::Kokoro, base, SystemClock::shared())
    }

    #[test]
    fn urls_join_the_path_and_keep_the_prefix() {
        assert_eq!(
            speech_url("http://localhost:8880/v1", "/audio/speech").unwrap(),
            "http://localhost:8880/v1/audio/speech"
        );
        assert_eq!(
            speech_url("http://localhost:8880/v1/", "/audio/speech").unwrap(),
            "http://localhost:8880/v1/audio/speech"
        );
        assert!(speech_url("not a url", "/audio/speech").is_err());
        assert!(speech_url("http://h/v1", "audio/speech").is_err());
        assert!(speech_url("http://h/v1", "/a?b=c").is_err());
    }

    #[tokio::test]
    async fn construction_rules() {
        // No key: plain http to a LAN host is fine.
        assert!(OpenAiSpeechTts::new(config("http://192.168.1.20:8880/v1")).is_ok());
        // A key over plain http to a remote host is not.
        let mut with_key = config("http://192.168.1.20:8880/v1");
        with_key.api_key = Some(Secret::new("k".to_owned()));
        assert!(OpenAiSpeechTts::new(with_key.clone()).is_err());
        with_key.allow_insecure_http = true;
        assert!(OpenAiSpeechTts::new(with_key).is_ok());
        // Loopback with a key is fine.
        let mut local = config("http://localhost:8880/v1");
        local.api_key = Some(Secret::new("k".to_owned()));
        assert!(OpenAiSpeechTts::new(local).is_ok());
        // The generic preset has no model.
        let generic = OpenAiSpeechConfig::preset(
            Preset::Generic,
            "http://localhost:1/v1",
            SystemClock::shared(),
        );
        assert!(OpenAiSpeechTts::new(generic).is_err());
        // Owned body fields.
        let mut owned = config("http://localhost:8880/v1");
        owned.extra_body.insert("input".to_owned(), json!("x"));
        assert!(OpenAiSpeechTts::new(owned).is_err());
    }

    #[tokio::test]
    async fn request_bodies_follow_the_preset() {
        let params = Params {
            voice: "af_heart".into(),
            rate: brisvox_core::frame::SampleRate::Hz24000,
        };
        let kokoro = OpenAiSpeechTts::new(config("http://localhost:8880/v1")).unwrap();
        let body: Value =
            serde_json::from_slice(&kokoro.inner.request_body(&params, "Hello.").unwrap()).unwrap();
        assert_eq!(
            body,
            json!({"model":"kokoro","input":"Hello.","voice":"af_heart",
                   "response_format":"pcm","stream":true})
        );
        let speaches = OpenAiSpeechTts::new(OpenAiSpeechConfig::preset(
            Preset::Speaches,
            "http://localhost:8000/v1",
            SystemClock::shared(),
        ))
        .unwrap();
        let body: Value =
            serde_json::from_slice(&speaches.inner.request_body(&params, "Hi").unwrap()).unwrap();
        assert_eq!(body["sample_rate"], json!(24_000));
        assert!(body.get("stream").is_none());
        let chatterbox = OpenAiSpeechTts::new(OpenAiSpeechConfig::preset(
            Preset::Chatterbox,
            "http://localhost:8004/v1",
            SystemClock::shared(),
        ))
        .unwrap();
        let body: Value =
            serde_json::from_slice(&chatterbox.inner.request_body(&params, "Hi").unwrap()).unwrap();
        assert_eq!(body["response_format"], json!("wav"));
    }

    #[tokio::test]
    async fn debug_never_shows_the_key_or_the_path() {
        let mut c = config("http://user:secret@localhost:8880/v1");
        c.api_key = Some(Secret::new("sk-very-secret".to_owned()));
        let shown = format!("{c:?}");
        assert!(!shown.contains("sk-very-secret"), "{shown}");
        assert!(!shown.contains("user:secret"), "{shown}");
        assert!(shown.contains("[REDACTED]"));
    }
}
