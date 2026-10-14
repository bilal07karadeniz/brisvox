//! A mock voice bot behind the real websocket transport, in process: the self-check of the
//! harness (`brisvox-bench v2v --sim --target mock-bot`).
//!
//! It serves the real [`WebSocketServer`] and [`TransportSession`] (framing, pacing, marks,
//! clears: the code a live deployment runs) with a scripted bot behind them instead of the voice
//! pipeline, so its result is the harness plus the transport against a known floor. It was the
//! sim-mode target until the engine assembly landed; sim mode now measures
//! the engine ([`super::engine_target`]) and this bot tells how much of that measurement the
//! harness and the transport account for.
//!
//! # Behaviour
//! Per call, on the injected clock:
//! 1. Input audio is classified frame by frame as speech (peak at or above the threshold) or
//!    silence.
//! 2. After speech, [`MockBotConfig::endpoint_silence`] of silence ends the turn (the end-of-turn
//!    policy of a real bot).
//! 3. [`MockBotConfig::response_delay`] plus a seeded random jitter later (the stand-in for STT
//!    final + LLM first token + TTS first byte), a [`MockBotConfig::reply`] long 440 Hz tone is
//!    sent as 20 ms frames; the transport's output pacer releases them at real time.
//! 4. Speech while a reply is queued or playing is a barge-in: the reply is cancelled and, if
//!    audio was already out, a `Clear` goes to the transport (epoch + 1).
//!
//! So the V2V of an answered turn is the floor `endpoint_silence + response_delay` (reported as
//! [`MockBotSettings::floor_ms`]) plus the energy gate's offset against the ground truth, the
//! 20 ms input and output frame granularity and the transport: that remainder is what the
//! harness can say about the transport and itself.
//!
//! Bounds: at most [`MockBotConfig::max_calls`] calls at once (the websocket server refuses more
//! with HTTP 503); per call one reply task and the transport session's own bounded queues.
//! Every task hangs off the server's cancellation token.

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::clock::{deadline_after, wait_for};
use brisvox_core::frame::SampleRate;
use brisvox_core::{AudioFrame, SharedClock, TurnEpoch};
use brisvox_transport::websocket::{ServerConfig, WebSocketServer};
use brisvox_transport::{
    CallLink, SessionConfig, Transport, TransportCommand, TransportEvent, TransportSession,
    UtteranceId,
};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::measure::{DEFAULT_SILENCE_PEAK, is_audible};
use crate::BenchError;

/// Amplitude of the reply tone (of 32768): well above any silence threshold.
const TONE_PEAK: f64 = 8_000.0;
/// Frequency of the reply tone.
const TONE_HZ: f64 = 440.0;

/// How the mock bot behaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockBotConfig {
    /// Silence after speech that ends the user's turn.
    pub endpoint_silence: Duration,
    /// Time between the end of the turn and the first reply frame handed to the transport.
    pub response_delay: Duration,
    /// Up to this much seeded random time is added to `response_delay` per turn.
    pub response_jitter: Duration,
    /// Length of the reply.
    pub reply: Duration,
    /// Peak amplitude at or above which an input frame is speech.
    pub speech_peak: u16,
    /// Seed of the jitter (each call derives its own).
    pub seed: u64,
    /// Most calls served at once.
    pub max_calls: usize,
}

impl Default for MockBotConfig {
    /// 500 ms end-of-turn silence, 300 ms response delay without jitter, a 600 ms reply.
    fn default() -> Self {
        Self {
            endpoint_silence: Duration::from_millis(500),
            response_delay: Duration::from_millis(300),
            response_jitter: Duration::ZERO,
            reply: Duration::from_millis(600),
            speech_peak: DEFAULT_SILENCE_PEAK,
            seed: 1,
            max_calls: 64,
        }
    }
}

impl MockBotConfig {
    /// The report form of the configuration.
    pub fn settings(&self) -> MockBotSettings {
        let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        MockBotSettings {
            endpoint_silence_ms: ms(self.endpoint_silence),
            response_delay_ms: ms(self.response_delay),
            response_jitter_ms: ms(self.response_jitter),
            reply_ms: ms(self.reply),
        }
    }
}

/// The mock bot's behaviour as recorded in a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MockBotSettings {
    /// Silence after speech that ends the turn, in milliseconds.
    pub endpoint_silence_ms: u64,
    /// Simulated provider time before the reply, in milliseconds.
    pub response_delay_ms: u64,
    /// Maximum random addition to the response delay, in milliseconds.
    pub response_jitter_ms: u64,
    /// Length of the reply, in milliseconds.
    pub reply_ms: u64,
}

impl MockBotSettings {
    /// The least V2V the bot can produce: end-of-turn silence plus response delay.
    pub const fn floor_ms(&self) -> u64 {
        self.endpoint_silence_ms
            .saturating_add(self.response_delay_ms)
    }
}

/// A running mock bot server.
pub struct MockBotServer {
    addr: std::net::SocketAddr,
    cancel: CancellationToken,
    tracker: TaskTracker,
    clock: SharedClock,
}

impl MockBotServer {
    /// Starts the bot on a free port of 127.0.0.1. It is a child of `parent`.
    ///
    /// # Errors
    /// [`BenchError::Config`] if the websocket server cannot be started.
    pub async fn start(
        config: MockBotConfig,
        clock: SharedClock,
        parent: &CancellationToken,
    ) -> Result<Self, BenchError> {
        let server_config = ServerConfig {
            bind_addr: ([127, 0, 0, 1], 0).into(),
            max_calls: config.max_calls.max(1),
            ..ServerConfig::default()
        };
        let cancel = parent.child_token();
        let server = WebSocketServer::bind(server_config, clock.clone(), &cancel)
            .await
            .map_err(|e| BenchError::Config(format!("cannot start the mock bot server: {e}")))?;
        let addr = server.local_addr();
        let tracker = TaskTracker::new();
        tracker.spawn(accept_loop(
            server,
            Arc::new(config),
            clock.clone(),
            cancel.clone(),
            tracker.clone(),
        ));
        Ok(Self {
            addr,
            cancel,
            tracker,
            clock,
        })
    }

    /// The websocket URL of the bot.
    pub fn url(&self) -> String {
        format!("ws://{}/call", self.addr)
    }

    /// Cancels every call and waits up to `timeout` for the tasks to end.
    ///
    /// # Errors
    /// [`BenchError::Call`] if tasks were still running after `timeout` (they stay cancelled).
    pub async fn shutdown(&self, timeout: Duration) -> Result<(), BenchError> {
        self.cancel.cancel();
        self.tracker.close();
        tokio::select! {
            biased;
            () = self.tracker.wait() => Ok(()),
            () = self.clock.sleep(timeout) => {
                Err(BenchError::Call("the mock bot did not stop in time".to_owned()))
            }
        }
    }
}

impl Drop for MockBotServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Takes calls from the transport until it is shut down or cancelled.
async fn accept_loop(
    mut server: WebSocketServer,
    config: Arc<MockBotConfig>,
    clock: SharedClock,
    cancel: CancellationToken,
    tracker: TaskTracker,
) {
    let mut call = 0u64;
    while let Ok(Some(link)) = server.accept(&cancel).await {
        tracker.spawn(serve_call(
            clock.clone(),
            link,
            Arc::clone(&config),
            call,
            cancel.child_token(),
        ));
        call = call.wrapping_add(1);
    }
    let _ = server.shutdown(Duration::from_secs(2)).await;
}

/// A reply the bot is working on.
struct Reply {
    cancel: CancellationToken,
    /// When its first frame is handed to the transport.
    starts: Instant,
    /// When its last frame has been played.
    ends: Instant,
}

/// SplitMix64: a small seeded generator for the jitter.
fn next_random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `duration` of a 440 Hz tone at `rate`, as PCM16 LE.
fn tone(rate: SampleRate, duration: Duration) -> Bytes {
    let hz = f64::from(rate.hz());
    let samples = (hz * duration.as_secs_f64()).round();
    let count = if samples.is_finite() && samples > 0.0 {
        samples as u64
    } else {
        0
    };
    let mut pcm = Vec::with_capacity(usize::try_from(count.saturating_mul(2)).unwrap_or(0));
    for i in 0..count {
        let phase = std::f64::consts::TAU * TONE_HZ * (i as f64) / hz;
        let sample = (TONE_PEAK * phase.sin()) as i16;
        pcm.extend_from_slice(&sample.to_le_bytes());
    }
    Bytes::from(pcm)
}

/// Sends `pcm` to the caller as 20 ms frames after `delay`, unless cancelled first.
#[allow(clippy::too_many_arguments)] // one task, all inputs are distinct
async fn play_reply(
    clock: SharedClock,
    commands: brisvox_transport::CommandSender,
    cancel: CancellationToken,
    pcm: Bytes,
    rate: SampleRate,
    epoch: TurnEpoch,
    utterance: UtteranceId,
    delay: Duration,
) {
    tokio::select! {
        biased;
        () = cancel.cancelled() => return,
        () = wait_for(&*clock, delay) => {}
    }
    let frame_bytes = usize::try_from(u64::from(rate.hz()) * 2 / 50)
        .unwrap_or(640)
        .max(2);
    for (seq, chunk) in pcm.chunks(frame_bytes).enumerate() {
        let Ok(frame) = AudioFrame::new(
            pcm.slice_ref(chunk),
            rate,
            clock.now(),
            u64::try_from(seq).unwrap_or(u64::MAX),
            epoch,
        ) else {
            return;
        };
        let command = TransportCommand::Audio {
            epoch,
            utterance,
            frame,
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            sent = commands.send(command) => if sent.is_err() { return },
        }
    }
}

/// Serves one call until it ends or `cancel` fires.
async fn serve_call(
    clock: SharedClock,
    link: CallLink,
    config: Arc<MockBotConfig>,
    call: u64,
    cancel: CancellationToken,
) {
    let session_config = SessionConfig {
        seed: call,
        ..SessionConfig::default()
    };
    let Ok(mut session) =
        TransportSession::open(clock.clone(), link, session_config, &cancel).await
    else {
        return;
    };
    let commands = session.commands();
    let tasks = TaskTracker::new();
    let mut rng = config.seed ^ call.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    let mut out_rate: Option<SampleRate> = None;
    let mut epoch = TurnEpoch::ZERO;
    let mut utterance = 0u64;
    let mut speaking = false;
    let mut silence = Duration::ZERO;
    let mut reply: Option<Reply> = None;
    // The reply tone, made once per call (at the output rate) and shared by every reply.
    let mut reply_pcm: Option<Bytes> = None;

    loop {
        let event = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            event = session.recv() => event,
        };
        match event {
            None | Some(TransportEvent::Stopped { .. }) => break,
            Some(TransportEvent::Started(info)) => {
                out_rate = info.sample_rate().ok().flatten().map(|rates| rates.output);
            }
            Some(TransportEvent::Audio(frame)) => {
                let now = clock.now();
                let rate = *out_rate.get_or_insert_with(|| frame.sample_rate());
                if is_audible(frame.data(), config.speech_peak) {
                    speaking = true;
                    silence = Duration::ZERO;
                    if let Some(active) = reply.take() {
                        // The caller talks over the bot: drop the reply, and cut what is out.
                        active.cancel.cancel();
                        if now >= active.starts && now < active.ends {
                            epoch = epoch.next();
                            let _ = commands.send(TransportCommand::Clear { epoch }).await;
                        }
                    }
                } else if speaking {
                    silence = silence.saturating_add(frame.duration());
                    if silence >= config.endpoint_silence {
                        speaking = false;
                        let jitter_us =
                            u64::try_from(config.response_jitter.as_micros()).unwrap_or(u64::MAX);
                        let extra = next_random(&mut rng)
                            .checked_rem(jitter_us.saturating_add(1))
                            .unwrap_or(0);
                        let delay = config
                            .response_delay
                            .saturating_add(Duration::from_micros(extra));
                        let token = cancel.child_token();
                        let starts = deadline_after(now, delay);
                        reply = Some(Reply {
                            cancel: token.clone(),
                            starts,
                            ends: deadline_after(starts, config.reply),
                        });
                        utterance += 1;
                        tasks.spawn(play_reply(
                            clock.clone(),
                            commands.clone(),
                            token,
                            reply_pcm
                                .get_or_insert_with(|| tone(rate, config.reply))
                                .clone(),
                            rate,
                            epoch,
                            UtteranceId::new(utterance),
                            delay,
                        ));
                    }
                }
            }
            Some(_) => {}
        }
    }
    cancel.cancel();
    tasks.close();
    tasks.wait().await;
    let _ = session.close(Duration::from_secs(2)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tone_is_audible_and_exactly_as_long_as_asked() {
        let pcm = tone(SampleRate::Hz16000, Duration::from_millis(600));
        assert_eq!(pcm.len(), 16_000 * 600 / 1000 * 2);
        assert!(is_audible(&pcm, 5_000));
        // 20 ms frames of 16 kHz audio are 640 bytes; each is audible on its own.
        assert!(
            pcm.chunks(640)
                .all(|frame| is_audible(frame, DEFAULT_SILENCE_PEAK))
        );
        assert!(tone(SampleRate::Hz8000, Duration::ZERO).is_empty());
    }

    #[test]
    fn jitter_is_seeded_and_bounded() {
        let (mut a, mut b) = (7u64, 7u64);
        let first: Vec<u64> = (0..5).map(|_| next_random(&mut a)).collect();
        let again: Vec<u64> = (0..5).map(|_| next_random(&mut b)).collect();
        assert_eq!(first, again);
        assert!(first.windows(2).all(|w| w[0] != w[1]));
        assert!(
            first
                .iter()
                .all(|r| r.checked_rem(51).is_some_and(|m| m <= 50))
        );
    }

    #[test]
    fn settings_report_the_floor() {
        let settings = MockBotConfig::default().settings();
        assert_eq!(settings.floor_ms(), 800);
        assert_eq!(settings.reply_ms, 600);
    }
}
