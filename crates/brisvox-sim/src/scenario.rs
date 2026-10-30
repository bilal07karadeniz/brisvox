//! Scripted user side of a simulated call: what the caller says and when, rendered into the
//! input audio a transport would deliver, plus a recorder for what the bot sends back.
//!
//! The user-input side is epoch-agnostic: input audio is tagged `TurnEpoch::ZERO`, and the
//! stages that consume it (VAD, STT, turn manager) use `EpochPolicy::KeepAll`, so barge-in audio
//! is never purged. Epochs begin at the turn manager, which re-stamps what it forwards; no `DropStale` stage may sit between the input transport and the turn manager. The
//! output side is epoch-strict.
//!
//! Scripts should end with at least the VAD hangover (300 ms by default) of silence, or the last
//! utterance never gets a `UserStoppedSpeaking` ([`Scenario::with_tail`] adds it).

use std::time::Duration;

use brisvox_core::clock::wait_until;
use brisvox_core::frame::{AudioFrame, Frame, SampleRate};
use brisvox_core::{
    Direction, Processor, ProcessorContext, SharedClock, TimelineRecorder, TurnEpoch,
};
use bytes::Bytes;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::input::InputSink;
use crate::mock_stt::SttUtterance;
use crate::output::OutputLog;

pub use crate::output::{DEFAULT_RECORD_LIMIT, OutputRecord};

/// Input frame length.
pub const FRAME: Duration = Duration::from_millis(20);

/// Amplitude of the synthetic user voice (a square wave; silence is all zeros).
pub const SPEECH_AMPLITUDE: i16 = 3_000;

/// One step of the caller's script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The caller speaks `text` for `duration` (rounded down to whole frames).
    Speak {
        /// What the mock STT will recognise.
        text: String,
        /// How long the speech lasts.
        duration: Duration,
    },
    /// The caller is silent (silent frames keep flowing, like a real call).
    Silence(Duration),
}

/// A caller's script, e.g. `Scenario::new().silence(ms(300)).speak("hi", ms(800))`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scenario {
    steps: Vec<Step>,
    /// The language the mock STT reports for each `Speak` step, in order (`None`: none).
    languages: Vec<Option<String>>,
}

impl Scenario {
    /// An empty script.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends speech.
    #[must_use]
    pub fn speak(mut self, text: &str, duration: Duration) -> Self {
        self.steps.push(Step::Speak {
            text: text.to_owned(),
            duration,
        });
        self.languages.push(None);
        self
    }

    /// Appends speech that the mock STT reports as spoken in `language` (`"tr"`), as a
    /// provider with language detection does.
    #[must_use]
    pub fn speak_in(mut self, language: &str, text: &str, duration: Duration) -> Self {
        self = self.speak(text, duration);
        if let Some(last) = self.languages.last_mut() {
            *last = Some(language.to_owned());
        }
        self
    }

    /// Appends silence.
    #[must_use]
    pub fn silence(mut self, duration: Duration) -> Self {
        self.steps.push(Step::Silence(duration));
        self
    }

    /// Appends `tail` of silence (use at least the VAD hangover, so the last utterance ends).
    #[must_use]
    pub fn with_tail(self, tail: Duration) -> Self {
        self.silence(tail)
    }

    /// The steps.
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// What the mock STT should recognise, one utterance per `Speak`, in order.
    pub fn utterances(&self) -> Vec<SttUtterance> {
        let mut languages = self.languages.iter();
        self.steps
            .iter()
            .filter_map(|step| match step {
                Step::Speak { text, duration } => {
                    let language = languages.next().cloned().flatten();
                    // Steps shorter than one frame produce no audio, so no utterance either.
                    (frames(*duration) > 0).then(|| {
                        let utterance = SttUtterance::words(text);
                        match language {
                            Some(language) => utterance.in_language(&language),
                            None => utterance,
                        }
                    })
                }
                Step::Silence(_) => None,
            })
            .collect()
    }

    /// Speech segments as (start, end) offsets from the start of the call (ground truth).
    pub fn speech_segments(&self) -> Vec<(Duration, Duration)> {
        let mut at = Duration::ZERO;
        let mut out = Vec::new();
        for step in &self.steps {
            let length = frames(step_duration(step)) * FRAME;
            if matches!(step, Step::Speak { .. }) && !length.is_zero() {
                out.push((at, at + length));
            }
            at += length;
        }
        out
    }

    /// Total scripted length (whole frames).
    pub fn duration(&self) -> Duration {
        self.steps
            .iter()
            .map(|s| frames(step_duration(s)) * FRAME)
            .sum()
    }

    /// For each 20 ms frame of the call: whether it is speech.
    fn frame_plan(&self) -> Vec<bool> {
        self.steps
            .iter()
            .flat_map(|step| {
                let speech = matches!(step, Step::Speak { .. });
                std::iter::repeat_n(
                    speech,
                    usize::try_from(frames(step_duration(step))).unwrap_or(0),
                )
            })
            .collect()
    }
}

fn step_duration(step: &Step) -> Duration {
    match step {
        Step::Speak { duration, .. } | Step::Silence(duration) => *duration,
    }
}

/// Whole frames in `d`.
fn frames(d: Duration) -> u32 {
    u32::try_from(d.as_millis() / FRAME.as_millis()).unwrap_or(u32::MAX)
}

/// The simulated input transport: sends one 20 ms frame per frame period into `input`, starting
/// at `start`, paced on `clock` (absolute schedule, no drift). Returns when the script ends,
/// `cancel` fires, or the pipeline is gone (`Ok` in all three cases).
///
/// `input` is an [`InputSink`]; a pipeline input ([`brisvox_core::FrameSender`]) converts into
/// one, so existing callers pass `running.input().clone()` unchanged.
///
/// # Errors
/// Only if a frame cannot be built (never for supported sample rates).
pub async fn play_input(
    scenario: &Scenario,
    clock: SharedClock,
    start: Instant,
    sample_rate: SampleRate,
    input: impl Into<InputSink>,
    cancel: &CancellationToken,
) -> Result<()> {
    let input: InputSink = input.into();
    // Every supported rate divides into whole 20 ms frames (441 samples at 22.05 kHz); u32 ->
    // usize cannot fail on supported targets.
    let samples = usize::try_from(sample_rate.hz() / 50).unwrap_or(320);
    let silence = Bytes::from(vec![0u8; samples * 2]);
    let speech = square_wave(samples, SPEECH_AMPLITUDE);
    for (seq, is_speech) in scenario.frame_plan().into_iter().enumerate() {
        let index = u32::try_from(seq).unwrap_or(u32::MAX);
        let at = start + FRAME * index;
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(()),
            () = wait_until(&*clock, at) => {}
        }
        let pcm = if is_speech { &speech } else { &silence };
        let frame = AudioFrame::new(
            pcm.clone(),
            sample_rate,
            at,
            u64::from(index),
            TurnEpoch::ZERO,
        )
        .map_err(Error::Core)?;
        if !input.send_bounded(&*clock, frame, cancel).await? {
            return Ok(());
        }
    }
    Ok(())
}

/// `samples` PCM16 samples of a square wave (period 40 samples).
fn square_wave(samples: usize, amplitude: i16) -> Bytes {
    let mut out = Vec::with_capacity(samples * 2);
    for i in 0..samples {
        let value = if (i / 20) % 2 == 0 {
            amplitude
        } else {
            -amplitude
        };
        out.extend_from_slice(&value.to_le_bytes());
    }
    Bytes::from(out)
}

/// The output transport of a simulated call: records every frame it is handed (epoch-strict:
/// stale frames never reach it) into an [`OutputLog`], marks `FirstAudioOut` per turn and, when
/// an interrupt arrives, `AudioOutStopped` on the turn whose audio it was playing.
///
/// Without a consume delay it is also the recording tap a production pipeline puts in front of
/// its real output transport ([`OutputLog::tap`]).
#[derive(Clone)]
pub struct SimTransportOut {
    clock: SharedClock,
    log: OutputLog,
    consume_delay: Duration,
    name: &'static str,
}

impl SimTransportOut {
    /// A recorder measuring times from `origin`.
    pub fn new(clock: SharedClock, origin: Instant, timeline: Option<TimelineRecorder>) -> Self {
        Self::from_log(clock, OutputLog::new(origin, timeline))
    }

    /// A recorder writing into an existing `log`.
    pub fn from_log(clock: SharedClock, log: OutputLog) -> Self {
        Self {
            clock,
            log,
            consume_delay: Duration::ZERO,
            name: "transport_out",
        }
    }

    /// Takes `delay` per frame before accepting the next one (a slow consumer: frames back up
    /// in front of the transport).
    #[must_use]
    pub fn with_consume_delay(mut self, delay: Duration) -> Self {
        self.consume_delay = delay;
        self
    }

    /// Keeps at most `limit` records (load tests); later frames are only counted.
    #[must_use]
    pub fn with_record_limit(mut self, limit: usize) -> Self {
        self.log = self.log.with_record_limit(limit);
        self
    }

    /// The processor's name in traces and errors.
    #[must_use]
    pub const fn named(mut self, name: &'static str) -> Self {
        self.name = name;
        self
    }

    /// The log this recorder writes into.
    pub fn log(&self) -> OutputLog {
        self.log.clone()
    }

    /// Everything recorded so far, in order.
    pub fn records(&self) -> Vec<OutputRecord> {
        self.log.records()
    }

    /// Frames received after the record limit was reached.
    pub fn dropped_records(&self) -> usize {
        self.log.dropped_records()
    }
}

impl std::fmt::Debug for SimTransportOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimTransportOut")
            .field("log", &self.log)
            .finish_non_exhaustive()
    }
}

impl Processor for SimTransportOut {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn process(
        &mut self,
        frame: Frame,
        direction: Direction,
        ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        if direction == Direction::Downstream {
            brisvox_core::clock::wait_for(&*self.clock, self.consume_delay).await;
            self.log.record_frame(&frame, self.clock.now());
        }
        ctx.forward(frame, direction).await
    }
}
