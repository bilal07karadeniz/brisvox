//! Hush (weya-ai; Apache-2.0 code and weights) through the pure-Rust `hush-vani` crate: a
//! DeepFilterNet-3 derivative trained for voice agents, with competing speakers in about 60 % of its
//! training mixes. 16 kHz, 10 ms frames, 10 ms of delay, weights embedded (int8, 2.4 MB, shared by all
//! calls); each call has its own recurrent state.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use hush_vani::{Hush, HushStream};

use super::rate::RateConverter;
use super::{Denoiser, to_i16};
use crate::error::{Error, Result};

/// Sample rate Hush works at.
const NATIVE_HZ: u32 = Hush::SAMPLE_RATE;

/// Samples per Hush frame.
const FRAME: usize = Hush::FRAME_SAMPLES;

/// The weights, loaded once per process (about 10 ms).
fn model() -> Result<Arc<Hush>> {
    static MODEL: OnceLock<std::result::Result<Arc<Hush>, String>> = OnceLock::new();
    MODEL
        .get_or_init(|| Hush::new().map(Arc::new).map_err(|e| e.to_string()))
        .clone()
        .map_err(|e| Error::Model(format!("hush: {e}")))
}

/// One caller's Hush state.
pub struct HushDenoiser {
    rate: RateConverter,
    stream: HushStream,
    /// Model-rate input short of a whole frame (`f32`, -1..1).
    pending: Vec<f32>,
    frame_out: Vec<f32>,
    up: Vec<i16>,
    native_out: Vec<i16>,
}

impl std::fmt::Debug for HushDenoiser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HushDenoiser")
            .field("rate", &self.rate)
            .finish_non_exhaustive()
    }
}

impl HushDenoiser {
    /// A denoiser for audio at `rate_hz`.
    ///
    /// # Errors
    /// [`Error::Model`] for a rate the resampler does not handle.
    pub fn new(rate_hz: u32) -> Result<Self> {
        Ok(Self {
            rate: RateConverter::new(rate_hz, NATIVE_HZ)?,
            stream: HushStream::new(model()?),
            pending: Vec::with_capacity(2 * FRAME),
            frame_out: vec![0.0; FRAME],
            up: Vec::new(),
            native_out: Vec::new(),
        })
    }
}

impl Denoiser for HushDenoiser {
    fn sample_rate(&self) -> u32 {
        self.rate.outer_hz()
    }

    fn delay(&self) -> Duration {
        let lag = u64::try_from(Hush::LATENCY_SAMPLES).unwrap_or(0);
        self.rate.delay() + Duration::from_micros(lag * 1_000_000 / u64::from(NATIVE_HZ))
    }

    fn process(&mut self, input: &[i16], output: &mut Vec<i16>) -> Result<()> {
        self.up.clear();
        self.rate.to_native(input, &mut self.up)?;
        self.pending
            .extend(self.up.iter().map(|s| f32::from(*s) / 32_768.0));
        self.native_out.clear();
        let mut used = 0;
        while let Some(frame) = self.pending.get(used..used + FRAME) {
            self.stream.process_frame(frame, &mut self.frame_out);
            self.native_out
                .extend(self.frame_out.iter().map(|s| to_i16(*s * 32_768.0)));
            used += FRAME;
        }
        self.pending.drain(..used);
        self.rate.from_native(&self.native_out, output)
    }

    fn reset(&mut self) {
        self.rate.reset();
        self.stream.reset();
        self.pending.clear();
    }
}
