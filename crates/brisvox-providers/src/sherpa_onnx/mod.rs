//! sherpa-onnx streaming speech-to-text over its WebSocket server, feature
//! `provider-sherpa-onnx`.
//!
//! [`SherpaOnnxStt`] implements [`SttProvider`] for the servers of
//! [k2-fsa/sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) (the Python
//! `streaming_server.py` and the C++ `sherpa-onnx-online-websocket-server`): CPU-first, true
//! streaming transducers, English and Turkish models among others. The protocol facts are in
//! `docs/providers/sherpa-onnx.md` (read from the server source on 2026-10-14).
//!
//! # Behaviour in one page
//! - **Audio.** One binary message per frame: little-endian `float32` samples at the model's
//!   rate ([`SherpaOnnxConfig::sample_rate`]). The Python server does not resample, so the
//!   engine's PCM16 is converted (and resampled when the rates differ) here, without a
//!   per-frame allocation in the steady state.
//! - **Results.** JSON per decode step: the cumulative hypothesis of the open segment. A changed
//!   hypothesis is an interim transcript. The server's own endpoint (Python: a higher
//!   `segment` number; C++: `is_final`) ends the segment with a final transcript flagged
//!   `speech_final`.
//! - **Finalize.** The protocol has no mid-stream finalize. [`SttInput::Finalize`](crate::stt::SttInput::Finalize) sends the
//!   text `Done`: the server pads 0.3 s (Python) / 0.8 s (C++) of silence, decodes the rest,
//!   sends the last result and closes. That result is the answer (`from_finalize`,
//!   `speech_final`); the spent connection is replaced at once by a new one, which gets the
//!   audio that arrived meanwhile. A `Finalize` for a connection that heard no audio is
//!   answered empty without a round trip. An unanswered `Done` ends after
//!   [`SherpaOnnxConfig::finalize_timeout`] with the latest hypothesis. The alternative of
//!   appending silence and waiting for the server's endpoint was rejected: it needs the
//!   server's endpointing on and tuned (`--use-endpoint`, rule2 trailing silence 1.2 s), which
//!   this adapter cannot see; `Done` works with any server settings.
//! - **Errors.** Handshake HTTP status (503 when `--max-active-connections` is reached, 401 and
//!   403 behind a proxy) is `Rejected { status }`; a `1008` close is `Rejected { status: 1008 }`;
//!   a message that is not JSON is `Protocol`; a connection lost for good is `Disconnected`; a
//!   slow connect, or a server that stays silent after the connection was replaced as often as
//!   allowed, is `Core(Timeout)`.
//! - **Reconnect.** An unexpected drop opens a new connection and replays the audio kept in
//!   the bounded ring (the server keeps no state across connections). See `wsstt` for the
//!   shared mechanics.
//!
//! The session language is ignored: a sherpa-onnx model is trained for its languages.

mod config;
mod wire;

use std::sync::Arc;

use brisvox_core::SharedClock;

pub use config::{DEFAULT_FINALIZE_PADDING, DEFAULT_URL, MAX_FINALIZE_PADDING, SherpaOnnxConfig};

use crate::error::Result;
use crate::stt::{SttConfig, SttProvider, SttSession};
use crate::wsstt;

/// Provider name used in errors and logs.
pub(crate) const PROVIDER: &str = "sherpa_onnx";

/// sherpa-onnx streaming STT.
#[derive(Debug, Clone)]
pub struct SherpaOnnxStt {
    cfg: Arc<SherpaOnnxConfig>,
    clock: SharedClock,
}

impl SherpaOnnxStt {
    /// An adapter with `config`; all deadlines and backoffs run on `clock`.
    pub fn new(config: SherpaOnnxConfig, clock: SharedClock) -> Self {
        Self {
            cfg: Arc::new(config),
            clock,
        }
    }

    /// The settings in use.
    pub fn config(&self) -> &SherpaOnnxConfig {
        &self.cfg
    }
}

impl SttProvider for SherpaOnnxStt {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    async fn connect(&self, config: SttConfig) -> Result<SttSession> {
        let target = self.cfg.target()?;
        wsstt::start(
            Arc::new(self.cfg.settings()),
            Arc::clone(&self.clock),
            target,
            wire::SherpaWire::new(self.cfg.padding_bytes()),
            config.sample_rate,
        )
        .await
    }

    async fn warm(&self) -> Result<()> {
        let target = self.cfg.target()?;
        wsstt::warm(&self.cfg.settings(), &*self.clock, &target).await
    }
}
