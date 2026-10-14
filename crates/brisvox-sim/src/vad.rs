//! A simple energy VAD for simulations (real VADs run on the inference pool).
//!
//! It emits VAD system frames only; the turn manager turns them into timeline marks, because
//! only it knows the epoch of the turn being started. Durations are summed per frame
//! (a lost frame does not count towards the hangover). `KeepAll` is defensive: in a linear
//! pipeline no `Interrupt` reaches the input side, but if one did, user audio must survive it.

use std::time::Duration;

use brisvox_core::frame::{AudioFrame, DataFrame, Frame, SystemFrame};
use brisvox_core::{Direction, EpochPolicy, Processor, ProcessorContext};
use tokio::time::Instant;

/// When the VAD decides speech started and stopped.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VadConfig {
    /// RMS level (PCM16 units) above which a frame is speech.
    pub threshold: f64,
    /// Consecutive speech needed to report a start.
    pub start_after: Duration,
    /// Consecutive silence needed to report a stop (hangover).
    pub stop_after: Duration,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            threshold: 500.0,
            start_after: Duration::from_millis(40),
            stop_after: Duration::from_millis(300),
        }
    }
}

/// Energy-threshold VAD. Forwards all audio and emits `UserStartedSpeaking` /
/// `UserStoppedSpeaking` system frames, timestamped (backdated) to the first speech frame and to
/// the end of the last speech frame.
#[derive(Debug)]
pub struct EnergyVad {
    config: VadConfig,
    speaking: bool,
    /// Start of the current run of speech frames, and its length.
    speech_run: Option<(Instant, Duration)>,
    /// End of the last speech frame, and the silence since then.
    last_speech_end: Option<Instant>,
    silence: Duration,
}

impl EnergyVad {
    /// A VAD with `config`.
    pub fn new(config: VadConfig) -> Self {
        Self {
            config,
            speaking: false,
            speech_run: None,
            last_speech_end: None,
            silence: Duration::ZERO,
        }
    }

    /// Feeds one frame; returns a VAD event to emit, if any.
    fn observe(&mut self, audio: &AudioFrame) -> Option<SystemFrame> {
        let length = audio.duration();
        let start = audio.timestamp();
        if rms(audio) >= self.config.threshold {
            self.silence = Duration::ZERO;
            self.last_speech_end = Some(start + length);
            let (run_start, run) = self
                .speech_run
                .map_or((start, length), |(s, d)| (s, d + length));
            self.speech_run = Some((run_start, run));
            if !self.speaking && run >= self.config.start_after {
                self.speaking = true;
                return Some(SystemFrame::UserStartedSpeaking { at: run_start });
            }
        } else {
            self.speech_run = None;
            self.silence += length;
            if self.speaking && self.silence >= self.config.stop_after {
                self.speaking = false;
                let at = self.last_speech_end.unwrap_or(start);
                return Some(SystemFrame::UserStoppedSpeaking { at });
            }
        }
        None
    }
}

/// Root-mean-square level of a PCM16 frame.
fn rms(audio: &AudioFrame) -> f64 {
    let data = audio.data();
    let samples = data.len() / 2;
    if samples == 0 {
        return 0.0;
    }
    let sum: f64 = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let v = f64::from(i16::from_le_bytes(*pair));
            v * v
        })
        .sum();
    #[allow(clippy::cast_precision_loss)] // sample counts are far below 2^52
    let mean = sum / samples as f64;
    mean.sqrt()
}

impl Processor for EnergyVad {
    fn name(&self) -> &'static str {
        "vad"
    }

    /// The user-input side is epoch-agnostic (see `scenario`).
    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        frame: Frame,
        direction: Direction,
        ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        let event = match (&frame, direction) {
            (Frame::Data(DataFrame::Audio(audio)), Direction::Downstream) => self.observe(audio),
            _ => None,
        };
        // The VAD event goes first: it overtakes the audio anyway (system lane).
        if let Some(event) = event {
            ctx.push(event.into(), Direction::Downstream).await?;
        }
        ctx.forward(frame, direction).await
    }
}
