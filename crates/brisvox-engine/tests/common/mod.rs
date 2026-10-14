//! Helpers shared by the input-chain integration tests.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::chunks_exact_to_as_chunks
)]

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_core::frame::{AudioFrame, DataFrame, Frame, SampleRate, SystemFrame};
use brisvox_core::{Direction, EpochPolicy, Processor, ProcessorContext, TurnEpoch};
use bytes::{Bytes, BytesMut};
use tokio::time::Instant;

pub const MS: Duration = Duration::from_millis(1);

pub mod llm;
pub mod loopback;

/// A 20 ms frame at `rate`: a square wave of `amplitude` (RMS = `amplitude`), or silence for 0.
/// `index` is the frame's position (sequence number and time offset from `origin`).
pub fn square_frame(origin: Instant, index: u64, rate: SampleRate, amplitude: i16) -> AudioFrame {
    let samples = rate.hz() as usize / 50;
    let mut pcm = BytesMut::with_capacity(samples * 2);
    for i in 0..samples {
        let v = if (i / 4) % 2 == 0 {
            amplitude
        } else {
            -amplitude
        };
        pcm.extend_from_slice(&v.to_le_bytes());
    }
    AudioFrame::new(
        pcm.freeze(),
        rate,
        origin + 20 * MS * u32::try_from(index).unwrap(),
        index,
        TurnEpoch::ZERO,
    )
    .unwrap()
}

/// A 20 ms frame of a 440 Hz sine of `amplitude` at `rate`; `index` continues the phase.
pub fn sine_frame(origin: Instant, index: u64, rate: SampleRate, amplitude: f32) -> AudioFrame {
    let samples = rate.hz() as usize / 50;
    let mut pcm = BytesMut::with_capacity(samples * 2);
    for i in 0..samples {
        let n = index as usize * samples + i;
        let t = n as f32 / rate.hz() as f32;
        let v = (amplitude * (2.0 * std::f32::consts::PI * 440.0 * t).sin()) as i16;
        pcm.extend_from_slice(&v.to_le_bytes());
    }
    AudioFrame::new(
        pcm.freeze(),
        rate,
        origin + 20 * MS * u32::try_from(index).unwrap(),
        index,
        TurnEpoch::ZERO,
    )
    .unwrap()
}

/// PCM16 samples of `bytes`.
pub fn samples(bytes: &Bytes) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

/// Root-mean-square level of `pcm`.
pub fn rms(pcm: &[i16]) -> f64 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f64 = pcm.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
    (sum / pcm.len() as f64).sqrt()
}

/// Everything a [`Recorder`] saw.
#[derive(Clone, Default)]
pub struct Recorded {
    frames: Arc<Mutex<Vec<(Frame, Direction)>>>,
}

impl Recorded {
    fn lock(&self) -> MutexGuard<'_, Vec<(Frame, Direction)>> {
        self.frames.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn record(&self, frame: Frame, direction: Direction) {
        self.lock().push((frame, direction));
    }

    pub fn all(&self) -> Vec<(Frame, Direction)> {
        self.lock().clone()
    }

    pub fn audio(&self) -> Vec<AudioFrame> {
        self.lock()
            .iter()
            .filter_map(|(f, _)| match f {
                Frame::Data(DataFrame::Audio(a)) => Some(a.clone()),
                _ => None,
            })
            .collect()
    }

    /// `(is_start, at)` of the VAD frames, in arrival order.
    pub fn vad(&self) -> Vec<(bool, Instant)> {
        self.lock()
            .iter()
            .filter_map(|(f, _)| match f {
                Frame::System(SystemFrame::UserStartedSpeaking { at }) => Some((true, *at)),
                Frame::System(SystemFrame::UserStoppedSpeaking { at }) => Some((false, *at)),
                _ => None,
            })
            .collect()
    }

    pub fn system(&self) -> Vec<SystemFrame> {
        self.lock()
            .iter()
            .filter_map(|(f, _)| match f {
                Frame::System(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }
}

/// A tail stage that records every frame it receives (and swallows it).
pub struct Recorder(pub Recorded);

impl Processor for Recorder {
    fn name(&self) -> &'static str {
        "recorder"
    }

    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        frame: Frame,
        direction: Direction,
        _ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        self.0.lock().push((frame, direction));
        Ok(())
    }
}
