//! Speech-to-text over the OpenAI Realtime *transcription* WebSocket protocol and its dialects,
//! feature `provider-openai-realtime-stt`.
//!
//! [`OpenAiRealtimeStt`] implements [`SttProvider`] for OpenAI itself and for the local servers
//! that copied the protocol: NVIDIA Speech NIM, Speaches and vLLM ([`Dialect`]). One client, a
//! small dialect table; the protocol facts and where each was read (documentation and server
//! source, 2026-10-14) are in `docs/providers/openai-realtime-stt.md`.
//!
//! # Behaviour in one page
//! - **Audio.** `input_audio_buffer.append` with base64 PCM16 at the dialect's rate (24 kHz for
//!   OpenAI and Speaches, 16 kHz for NIM and vLLM); the engine's audio is resampled when its
//!   rate differs. No per-frame allocation in the steady state.
//! - **Turns are ours.** The engine's VAD ends the turn. OpenAI is told `turn_detection: null`;
//!   NIM and vLLM have no server VAD to disable; Speaches' server VAD cannot be switched off
//!   (its `session.update` does not accept `null`, source), so its own turn ends arrive as
//!   additional `speech_final` transcripts that did not answer a `Finalize`.
//! - **Interims.** `...input_audio_transcription.delta` (OpenAI, NIM) and `transcription.delta`
//!   (vLLM) extend the hypothesis, which is forwarded as an interim transcript whenever it
//!   changes. Speaches sends none.
//! - **Finalize.** [`SttInput::Finalize`](crate::stt::SttInput::Finalize) sends `input_audio_buffer.commit` (vLLM: the
//!   `final: true` commit, then a new generation is started). The `completed` / `done` event of
//!   the committed item answers it (`from_finalize`, `speech_final`). Nothing, or less than
//!   the server accepts (100 ms for OpenAI and Speaches), buffered since the last commit is
//!   answered empty without a round trip. An error event while a commit is open (an empty
//!   buffer, a failed transcription) answers it empty too. A commit that is not answered within
//!   [`OpenAiRealtimeSttConfig::finalize_timeout`] is answered with the latest hypothesis, and
//!   its late transcript is recognised by the item id and dropped.
//! - **Errors.** Handshake HTTP status is `Rejected { status }`; `invalid_api_key`,
//!   `model_not_found` and quota errors are `Rejected` too (401 / 404 / 429); a `1008` close is
//!   `Rejected { status: 1008 }`; a message that is not JSON is `Protocol`; other `error`
//!   events are logged and do not end the session; a connection lost for good is
//!   `Disconnected`; a slow connect or a silent server is `Core(Timeout)`.
//! - **Reconnect.** An unexpected drop opens a new connection (session message first) and
//!   replays the audio kept in the bounded ring. See `wsstt` for the shared mechanics.

mod config;
mod wire;

use std::sync::Arc;

use brisvox_core::SharedClock;

pub use config::{Dialect, OpenAiRealtimeSttConfig};

use crate::error::Result;
use crate::stt::{SttConfig, SttProvider, SttSession};
use crate::wsstt;

/// Provider name used in errors and logs.
pub(crate) const PROVIDER: &str = "openai_realtime_stt";

/// Streaming STT over the OpenAI Realtime transcription protocol.
#[derive(Debug, Clone)]
pub struct OpenAiRealtimeStt {
    cfg: Arc<OpenAiRealtimeSttConfig>,
    clock: SharedClock,
}

impl OpenAiRealtimeStt {
    /// An adapter with `config`; all deadlines and backoffs run on `clock`.
    pub fn new(config: OpenAiRealtimeSttConfig, clock: SharedClock) -> Self {
        Self {
            cfg: Arc::new(config),
            clock,
        }
    }

    /// The settings in use.
    pub fn config(&self) -> &OpenAiRealtimeSttConfig {
        &self.cfg
    }
}

impl SttProvider for OpenAiRealtimeStt {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    async fn connect(&self, config: SttConfig) -> Result<SttSession> {
        let language = self.cfg.language_for(&config);
        let target = self.cfg.target(language.as_deref())?;
        let params = wire::SessionParams {
            dialect: self.cfg.dialect,
            model: self.cfg.model().map(str::to_owned),
            language,
            prompt: self.cfg.prompt.clone(),
            punctuation: self.cfg.punctuation,
            replaces: self.cfg.replaces(),
            min_commit: self.cfg.min_commit(),
            rate: self.cfg.wire_rate(),
        };
        wsstt::start(
            Arc::new(self.cfg.settings()),
            Arc::clone(&self.clock),
            target,
            wire::RealtimeWire::new(params),
            config.sample_rate,
        )
        .await
    }

    async fn warm(&self) -> Result<()> {
        let target = self.cfg.target(None)?;
        wsstt::warm(&self.cfg.settings(), &*self.clock, &target).await
    }
}
