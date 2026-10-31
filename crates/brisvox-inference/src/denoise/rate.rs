//! Rate conversion around a denoiser model: the call's rate to the model's and back, with the
//! engine's own [`StreamResampler`] (one filter state per direction, scratch buffers reused).

use std::time::Duration;

use brisvox_audio::StreamResampler;
use brisvox_core::frame::SampleRate;

use crate::error::{Error, Result};

fn audio_error(e: impl std::fmt::Display) -> Error {
    Error::Model(format!("rate conversion: {e}"))
}

/// Converts between a call's sample rate (`outer`) and the rate a model works at (`native`).
/// Equal rates copy.
pub struct RateConverter {
    up: StreamResampler,
    down: StreamResampler,
    scratch: Vec<i16>,
}

impl std::fmt::Debug for RateConverter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateConverter")
            .field("outer", &self.up.from_rate())
            .field("native", &self.up.to_rate())
            .finish_non_exhaustive()
    }
}

impl RateConverter {
    /// A converter between `outer_hz` and `native_hz`.
    ///
    /// # Errors
    /// [`Error::Model`] for a rate the engine's resampler does not handle.
    pub fn new(outer_hz: u32, native_hz: u32) -> Result<Self> {
        let outer = SampleRate::try_from(outer_hz).map_err(audio_error)?;
        let native = SampleRate::try_from(native_hz).map_err(audio_error)?;
        Ok(Self {
            up: StreamResampler::new(outer, native).map_err(audio_error)?,
            down: StreamResampler::new(native, outer).map_err(audio_error)?,
            scratch: Vec::new(),
        })
    }

    /// The call's rate (Hz).
    pub fn outer_hz(&self) -> u32 {
        self.up.from_rate().hz()
    }

    /// The model's rate (Hz).
    pub fn native_hz(&self) -> u32 {
        self.up.to_rate().hz()
    }

    /// Delay both filters add to a round trip.
    pub fn delay(&self) -> Duration {
        self.up.delay() + self.down.delay()
    }

    /// Forgets the filter state of both directions.
    pub fn reset(&mut self) {
        self.up.reset();
        self.down.reset();
    }

    /// Converts `input` at the call's rate and appends the model-rate samples that are ready to
    /// `out` (resamplers emit whole blocks, so a call may add none).
    ///
    /// # Errors
    /// [`Error::Model`] if the resampler fails.
    pub fn to_native(&mut self, input: &[i16], out: &mut Vec<i16>) -> Result<()> {
        Self::run(&mut self.up, &mut self.scratch, input, out)
    }

    /// Converts model-rate `native` samples back to the call's rate, appending to `out`.
    ///
    /// # Errors
    /// [`Error::Model`] if the resampler fails.
    pub fn from_native(&mut self, native: &[i16], out: &mut Vec<i16>) -> Result<()> {
        Self::run(&mut self.down, &mut self.scratch, native, out)
    }

    fn run(
        resampler: &mut StreamResampler,
        scratch: &mut Vec<i16>,
        input: &[i16],
        out: &mut Vec<i16>,
    ) -> Result<()> {
        let needed = resampler.max_output_len(input.len());
        if scratch.len() < needed {
            scratch.resize(needed, 0);
        }
        let written = resampler.process(input, scratch).map_err(audio_error)?;
        out.extend_from_slice(scratch.get(..written).unwrap_or_default());
        Ok(())
    }
}
