//! Streaming noise suppression of the caller's audio.
//!
//! A [`Denoiser`] is one call's state: it takes mono PCM16 in chunks of any size and returns
//! the enhanced samples that became available, in order. The engine runs it on the inference
//! pool next to the VAD (the models cost 40 to 200 microseconds per 20 ms of audio, far above
//! the ~50 microsecond bar for work on a tokio worker) and feeds the enhanced audio to the VAD
//! and the turn detector. Whether the speech-to-text provider also gets it is an agent setting
//! (`audio.denoise_stt`): independent studies found that denoised audio often *raises* the
//! word error rate of Whisper-class recognizers (`docs/providers/noise-suppression.md`).
//!
//! # Stream contract
//! - Output comes in the models' own block sizes, so a call may return fewer samples than it was
//!   given (even none) and a later call more. Over a stream, `output samples = input samples -
//!   samples still buffered`. Nothing is dropped or invented.
//! - **Delay.** Output sample `k` is the enhanced version of input sample `k - delay()`: the
//!   model's algorithmic latency plus the filters of the rate conversion. The first
//!   [`Denoiser::delay`] samples of a stream are a fade-in (silence or ramp), not speech. A
//!   consumer that dates audio by position subtracts `delay()` from the position of the output.
//! - [`Denoiser::reset`] starts a new, unrelated stream (a hole in the audio).
//! - The rates in and out are the same; a backend converts internally to the rate its model was
//!   trained at ([`rate::RateConverter`]), so an 8 kHz telephone call works.
//!
//! # Allocation
//! A steady-state call allocates nothing: scratch buffers grow once to the largest chunk seen
//! and are reused. (The FastEnhancer backend allocates its small ONNX Runtime tensors per hop;
//! it is an evaluation backend.)

use std::time::Duration;

use crate::error::{Error, Result};

pub mod blend;
#[cfg(feature = "denoise-fastenhancer")]
pub mod fastenhancer;
#[cfg(feature = "denoise-hush")]
pub mod hush;
#[cfg(feature = "denoise")]
pub mod rate;
#[cfg(feature = "denoise-rnnoise")]
pub mod rnnoise;

/// A streaming noise suppressor for one caller (see the module docs for the contract).
pub trait Denoiser: Send {
    /// Sample rate of the audio in and out (Hz).
    fn sample_rate(&self) -> u32;

    /// How far the output lags the input: output sample `k` belongs to input sample
    /// `k - delay()`.
    fn delay(&self) -> Duration;

    /// Feeds `input` (mono PCM16 at [`Self::sample_rate`]) and appends the enhanced samples that
    /// are ready to `output`.
    ///
    /// # Errors
    /// [`Error::Model`] when the model fails (the stream continues; the samples of the failing
    /// step are not produced).
    fn process(&mut self, input: &[i16], output: &mut Vec<i16>) -> Result<()>;

    /// Forgets all state: a new, unrelated stream follows.
    fn reset(&mut self);
}

/// A boxed denoiser, as the engine holds it.
pub type BoxedDenoiser = Box<dyn Denoiser>;

/// Which denoiser backend an agent asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Backend {
    /// RNNoise (`nnnoiseless`, BSD-3-Clause); cargo feature `denoise-rnnoise`.
    Rnnoise,
    /// FastEnhancer on ONNX Runtime; cargo feature `denoise-fastenhancer`. Evaluation only.
    FastEnhancer,
    /// Hush (`hush-vani`, Apache-2.0); cargo feature `denoise-hush`.
    Hush,
}

impl Backend {
    /// Whether this build includes the backend.
    pub const fn compiled(self) -> bool {
        match self {
            Self::Rnnoise => cfg!(feature = "denoise-rnnoise"),
            Self::FastEnhancer => cfg!(feature = "denoise-fastenhancer"),
            Self::Hush => cfg!(feature = "denoise-hush"),
        }
    }

    /// The cargo feature that provides the backend.
    pub const fn feature(self) -> &'static str {
        match self {
            Self::Rnnoise => "denoise-rnnoise",
            Self::FastEnhancer => "denoise-fastenhancer",
            Self::Hush => "denoise-hush",
        }
    }

    /// The name used in the agent file and in reports.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Rnnoise => "rnnoise",
            Self::FastEnhancer => "fastenhancer",
            Self::Hush => "hush",
        }
    }
}

/// Builds a denoiser of `backend` for audio at `rate_hz` (8000, 16000, 22050, 24000, 44100 or
/// 48000). The FastEnhancer backend loads its model from `model` (the ONNX file; ignored by
/// RNNoise, whose model is embedded).
///
/// Building can touch the file system and takes up to ~100 ms (a FastEnhancer session): call it
/// where the engine builds a call's VAD stream, on the inference pool, never on a tokio worker.
///
/// # Errors
/// [`Error::Model`] when the backend is not compiled in, the rate is unsupported or the model
/// cannot be loaded.
pub fn build(
    backend: Backend,
    rate_hz: u32,
    model: Option<&std::path::Path>,
) -> Result<BoxedDenoiser> {
    let _ = (rate_hz, model); // unused when no backend is compiled in
    match backend {
        #[cfg(feature = "denoise-rnnoise")]
        Backend::Rnnoise => Ok(Box::new(rnnoise::RnnoiseDenoiser::new(rate_hz)?)),
        #[cfg(feature = "denoise-fastenhancer")]
        Backend::FastEnhancer => {
            let path =
                model.ok_or_else(|| Error::Model("fastenhancer needs a model file".into()))?;
            Ok(Box::new(fastenhancer::FastEnhancer::load(path, rate_hz)?))
        }
        #[cfg(feature = "denoise-hush")]
        Backend::Hush => Ok(Box::new(hush::HushDenoiser::new(rate_hz)?)),
        #[allow(unreachable_patterns)]
        other => Err(Error::Model(format!(
            "denoiser `{}` is not compiled in (cargo feature `{}`)",
            other.name(),
            other.feature()
        ))),
    }
}

/// Converts PCM16 to the `f32` the models take, in the range of the 16-bit integers.
#[cfg(feature = "denoise-rnnoise")]
pub(crate) fn push_f32(samples: &[i16], out: &mut Vec<f32>) {
    out.extend(samples.iter().map(|s| f32::from(*s)));
}

/// Rounds and clamps one model output sample to PCM16.
#[cfg(any(
    feature = "denoise-rnnoise",
    feature = "denoise-fastenhancer",
    feature = "denoise-hush"
))]
#[allow(clippy::cast_possible_truncation)] // clamped to the i16 range first
pub(crate) fn to_i16(sample: f32) -> i16 {
    sample.round().clamp(-32_768.0, 32_767.0) as i16
}
