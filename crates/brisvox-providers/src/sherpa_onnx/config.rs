//! Settings of the sherpa-onnx adapter and the construction of its WebSocket URL.

use std::time::Duration;

use brisvox_core::Secret;
use brisvox_core::frame::SampleRate;

use super::PROVIDER;
use crate::error::{Error, Result};
use crate::wsstt::{Settings, Target, check_base_url, is_token};

/// Where the sherpa-onnx example server listens by default (`--port` 6006).
pub const DEFAULT_URL: &str = "ws://127.0.0.1:6006";

/// Default for [`SherpaOnnxConfig::finalize_padding`].
pub const DEFAULT_FINALIZE_PADDING: Duration = Duration::from_millis(300);

/// Largest accepted [`SherpaOnnxConfig::finalize_padding`].
pub const MAX_FINALIZE_PADDING: Duration = Duration::from_secs(5);

/// Settings of the sherpa-onnx streaming WebSocket STT adapter.
///
/// Create it with [`SherpaOnnxConfig::new`] and override fields; the struct is
/// `#[non_exhaustive]` so that new settings are not a breaking change. `Debug` never prints the
/// bearer token ([`Secret`]).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SherpaOnnxConfig {
    /// Server address, `ws://host[:port]` or `wss://host[:port]` (scheme and authority only).
    /// Default [`DEFAULT_URL`]. Plain `ws://` is allowed for any host unless a `bearer` token
    /// is set (then loopback only, so the token never travels in the clear).
    pub base_url: String,
    /// Path of the WebSocket endpoint. Default `/` (the example servers serve it at the root;
    /// a reverse proxy may map another path).
    pub path: String,
    /// Optional `Authorization: Bearer <token>` for a reverse proxy in front of the server (the
    /// sherpa-onnx servers themselves have no authentication).
    pub bearer: Option<Secret<String>>,
    /// Rate of the audio sent to the server. For the Python server this MUST be the rate the
    /// model was trained at (`--sample-rate`, default 16000): it does not resample. The C++
    /// server resamples from `--input-sample-rate` itself. The engine's audio is resampled to
    /// this rate here when it differs. Default 16 kHz.
    pub sample_rate: SampleRate,
    /// Deadline of every network wait: the connect (TCP, TLS and WebSocket handshake together),
    /// each write, the hand-over of an event, and the sign of life owed by the server after
    /// audio was sent (half-way a ping asks for one; no sign within the deadline replaces the
    /// connection). Default 5 s.
    pub io_timeout: Duration,
    /// A ping goes out after this long without audio; the liveness wait is suspended until audio
    /// resumes. Default 4 s.
    pub keepalive_interval: Duration,
    /// How long to wait for the end of a `Done` exchange (the server decodes the tail, sends the
    /// last result and closes) before answering the `Finalize` with the latest hypothesis.
    /// Default 1.5 s.
    pub finalize_timeout: Duration,
    /// How long the close handshake may take on hang-up. Default 500 ms.
    pub close_timeout: Duration,
    /// Reconnect attempts per dropped connection (0 disables reconnecting); also the retries of
    /// the connection that replaces one spent by `Done`. The counter starts over once a new
    /// connection delivers a result. Default 2.
    pub max_reconnects: u32,
    /// Wait before the first reconnect attempt; doubles per attempt (cap 5 s). Default 100 ms.
    pub reconnect_backoff: Duration,
    /// How much audio is kept for replay after a reconnect, and the most that is held back while
    /// a `Done` exchange is under way (the oldest is dropped first). Default 8 s.
    pub replay_window: Duration,
    /// Largest server message accepted. Default 1 MiB (results are a few KB; this only bounds a
    /// hostile server).
    pub max_message_bytes: usize,
    /// Silence sent after the last audio of a turn and before `Done`. The streaming
    /// zipformer decodes in chunks with a right context; the Python server pads only 0.3 s
    /// before it ends the stream, which cut the last word of an utterance ("... ORTHODOX
    /// CHRIST", "HIE" for "Hi there") through the engine's real VAD-cut turns (live,
    /// 2026-10-14). 300 ms more settles it (the decoder saturates at about 0.6 s of trailing
    /// silence) for about 10 ms of server decoding. The C++ server pads 0.8 s by itself: set
    /// this to zero there. At most 5 s. Default 300 ms.
    pub finalize_padding: Duration,
}

impl Default for SherpaOnnxConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl SherpaOnnxConfig {
    /// Defaults: the Python example server on this machine, a 16 kHz model.
    pub fn new() -> Self {
        Self {
            base_url: DEFAULT_URL.to_owned(),
            path: "/".to_owned(),
            bearer: None,
            sample_rate: SampleRate::Hz16000,
            io_timeout: Duration::from_secs(5),
            keepalive_interval: Duration::from_secs(4),
            finalize_timeout: Duration::from_millis(1500),
            close_timeout: Duration::from_millis(500),
            max_reconnects: 2,
            reconnect_backoff: Duration::from_millis(100),
            replay_window: Duration::from_secs(8),
            max_message_bytes: 1 << 20,
            finalize_padding: DEFAULT_FINALIZE_PADDING,
        }
    }

    /// The silence [`finalize_padding`](Self::finalize_padding) as `float32` samples (zeros) of
    /// the wire format, built once per session.
    pub(crate) fn padding_bytes(&self) -> bytes::Bytes {
        let samples = self.finalize_padding.as_secs_f64() * f64::from(self.sample_rate.hz());
        let samples = if samples.is_finite() && samples > 0.0 {
            samples as usize
        } else {
            0
        };
        bytes::Bytes::from(vec![0_u8; samples.saturating_mul(4)])
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
        check_base_url(PROVIDER, &self.base_url, self.bearer.is_some())?;
        if !self.path.starts_with('/')
            || !self
                .path
                .split('/')
                .all(|part| part.is_empty() || is_token(part))
        {
            return bad("path must start with '/' and hold letters, digits, '.', '_' and '-'");
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
        if self.finalize_padding > MAX_FINALIZE_PADDING {
            return bad("finalize_padding must be at most 5 s");
        }
        Ok(())
    }

    /// The session core's view of these settings.
    pub(crate) fn settings(&self) -> Settings {
        Settings {
            provider: PROVIDER,
            connect_label: "sherpa_onnx connect",
            io_timeout: self.io_timeout,
            keepalive_interval: self.keepalive_interval,
            finalize_timeout: self.finalize_timeout,
            close_timeout: self.close_timeout,
            max_reconnects: self.max_reconnects,
            reconnect_backoff: self.reconnect_backoff,
            replay_window: self.replay_window,
            max_message_bytes: self.max_message_bytes,
            wire_rate: self.sample_rate,
        }
    }

    /// Where to connect (the URL has no query: the protocol has no parameters).
    ///
    /// # Errors
    /// [`Error::Protocol`] when the settings are invalid or TLS cannot be set up.
    pub(crate) fn target(&self) -> Result<Target> {
        self.validate()?;
        let (authority, tls) = check_base_url(PROVIDER, &self.base_url, self.bearer.is_some())?;
        let (scheme, tls_config) = if tls {
            ("wss", Some(crate::net::tls_config()?))
        } else {
            ("ws", None)
        };
        Ok(Target {
            url: format!("{scheme}://{authority}{}", self.path),
            bearer: self.bearer.clone(),
            tls: tls_config,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_url_is_the_example_servers_root() {
        let cfg = SherpaOnnxConfig::new();
        let target = cfg.target();
        assert!(target.is_ok_and(|t| t.url == "ws://127.0.0.1:6006/" && t.tls.is_none()));
    }

    #[test]
    fn a_remote_keyless_server_over_plain_ws_is_fine_but_a_token_needs_wss() {
        let mut cfg = SherpaOnnxConfig::new();
        cfg.base_url = "ws://gpu-box.lan:6006".to_owned();
        assert!(cfg.validate().is_ok());
        cfg.bearer = Some(Secret::new("t".to_owned()));
        assert!(cfg.validate().is_err());
        cfg.base_url = "wss://stt.example.com".to_owned();
        let target = cfg.target();
        assert!(target.is_ok_and(|t| t.tls.is_some() && t.url == "wss://stt.example.com/"));
    }

    #[test]
    fn the_padding_is_float32_zeros_of_the_wire_rate_and_bounded() {
        let mut cfg = SherpaOnnxConfig::new();
        assert_eq!(cfg.finalize_padding, DEFAULT_FINALIZE_PADDING);
        // 300 ms at 16 kHz, 4 bytes a sample.
        assert_eq!(cfg.padding_bytes().len(), 4_800 * 4);
        cfg.finalize_padding = Duration::ZERO;
        assert!(cfg.padding_bytes().is_empty());
        cfg.finalize_padding = Duration::from_secs(5);
        assert!(cfg.validate().is_ok());
        cfg.finalize_padding = Duration::from_millis(5_001);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn paths_and_timings_are_checked() {
        let mut cfg = SherpaOnnxConfig::new();
        cfg.path = "/asr/stream".to_owned();
        assert!(cfg.validate().is_ok());
        for bad in ["asr", "/a b", "/a?x=1", "/a#b"] {
            cfg.path = bad.to_owned();
            assert!(cfg.validate().is_err(), "{bad}");
        }
        let mut cfg = SherpaOnnxConfig::new();
        cfg.io_timeout = Duration::ZERO;
        assert!(cfg.validate().is_err());
        let mut cfg = SherpaOnnxConfig::new();
        cfg.replay_window = Duration::from_secs(61);
        assert!(cfg.validate().is_err());
    }
}
