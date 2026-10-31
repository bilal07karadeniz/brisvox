//! FastEnhancer (ICASSP 2026, MIT code) on ONNX Runtime: the waveform-to-waveform streaming graphs
//! of the project's releases (`fastenhancer_<size>.onnx`).
//!
//! **Evaluation backend.** The release files carry no license statement of their own and were
//! trained on DNS Challenge 3 data (`docs/providers/noise-suppression.md`), so the agent file
//! does not offer this backend; the benchmark tooling uses it to size what a better model would
//! cost and gain.
//!
//! The graph (measured on `onnx-dns-v1.0.0/fastenhancer_t.onnx`): input `wav_in` `[1, hop]`
//! (`hop` = 256 samples = 16 ms at 16 kHz, floats in -1..1), the caches `cache_in_0` `[1, 256]`
//! and `cache_in_1` `[1, 256]` (the overlap of the STFT input and of the overlap-add output) and
//! two recurrent states `cache_in_2/3` `[1, 16, 20]`; output `wav_out` `[1, hop]` and the same
//! caches as `cache_out_*`. All inputs start at zero; each step feeds the caches back. The output
//! lags the input by `n_fft - hop` = 256 samples (the length of `cache_in_0`). Other sizes read
//! their `hop` and cache shapes from the file (M uses 160, L 100).
//!
//! ONNX Runtime does not share a session between instances here: every call builds its own
//! (about 100 ms for the tiny model).

use std::borrow::Cow;
use std::path::Path;
use std::time::Duration;

use ort::session::{Session, SessionInputValue};
use ort::value::Tensor;

use super::rate::RateConverter;
use super::{Denoiser, to_i16};
use crate::error::{Error, Result};

/// Sample rate the 16 kHz models work at.
const NATIVE_HZ: u32 = 16_000;

fn model_error(e: impl std::fmt::Display) -> Error {
    Error::Model(format!("fastenhancer: {e}"))
}

/// One recurrent cache: its input name, shape and current contents.
struct Cache {
    input: String,
    output: String,
    shape: Vec<i64>,
    data: Vec<f32>,
}

/// One caller's FastEnhancer stream.
pub struct FastEnhancer {
    session: Session,
    rate: RateConverter,
    hop: usize,
    /// Samples the output lags the input (the STFT overlap).
    lag: usize,
    caches: Vec<Cache>,
    /// Model-rate samples (-1..1) waiting for a whole hop.
    pending: Vec<f32>,
    up: Vec<i16>,
    native_out: Vec<i16>,
}

impl std::fmt::Debug for FastEnhancer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FastEnhancer")
            .field("hop", &self.hop)
            .field("caches", &self.caches.len())
            .finish_non_exhaustive()
    }
}

fn dims(dtype: &ort::value::ValueType) -> Vec<i64> {
    dtype
        .tensor_shape()
        .map(|s| s.iter().map(|d| (*d).max(1)).collect())
        .unwrap_or_default()
}

impl FastEnhancer {
    /// Loads the waveform graph at `path` for audio at `rate_hz`.
    ///
    /// # Errors
    /// [`Error::Model`] when the file cannot be loaded or is not a streaming waveform graph with
    /// matching `wav_in`/`wav_out` chunks.
    pub fn load(path: &Path, rate_hz: u32) -> Result<Self> {
        let session = Session::builder()
            .map_err(model_error)?
            .with_intra_threads(1)
            .map_err(model_error)?
            .with_inter_threads(1)
            .map_err(model_error)?
            .with_parallel_execution(false)
            .map_err(model_error)?
            .with_independent_thread_pool()
            .map_err(model_error)?
            .commit_from_file(path)
            .map_err(|e| model_error(format!("{}: {e}", path.display())))?;
        let wav_in = session
            .inputs()
            .iter()
            .find(|i| i.name() == "wav_in")
            .map(|i| dims(i.dtype()))
            .ok_or_else(|| model_error("no wav_in input"))?;
        let wav_out = session
            .outputs()
            .iter()
            .find(|o| o.name() == "wav_out")
            .map(|o| dims(o.dtype()))
            .ok_or_else(|| model_error("no wav_out output"))?;
        let hop = usize::try_from(wav_in.last().copied().unwrap_or(0)).unwrap_or(0);
        if hop == 0 || wav_in != wav_out {
            return Err(model_error(format!(
                "not a streaming waveform graph (wav_in {wav_in:?}, wav_out {wav_out:?})"
            )));
        }
        let mut caches = Vec::new();
        for input in session.inputs().iter().filter(|i| i.name() != "wav_in") {
            let shape = dims(input.dtype());
            let len = shape.iter().product::<i64>();
            let output = input.name().replace("cache_in", "cache_out");
            caches.push(Cache {
                input: input.name().to_owned(),
                output,
                data: vec![0.0; usize::try_from(len).unwrap_or(0)],
                shape,
            });
        }
        let lag = caches.first().map_or(0, |c| {
            usize::try_from(c.shape.last().copied().unwrap_or(0)).unwrap_or(0)
        });
        Ok(Self {
            session,
            rate: RateConverter::new(rate_hz, NATIVE_HZ)?,
            hop,
            lag,
            caches,
            pending: Vec::with_capacity(2 * hop),
            up: Vec::new(),
            native_out: Vec::new(),
        })
    }

    /// One hop through the graph; appends `hop` samples to `native_out`.
    fn step(&mut self, start: usize) -> Result<()> {
        let hop = self.hop;
        let chunk = self
            .pending
            .get(start..start + hop)
            .ok_or_else(|| model_error("short hop"))?
            .to_vec();
        let mut inputs: Vec<(Cow<'static, str>, SessionInputValue<'static>)> =
            Vec::with_capacity(1 + self.caches.len());
        let wav = Tensor::from_array((vec![1_i64, i64::try_from(hop).unwrap_or(0)], chunk))
            .map_err(model_error)?;
        inputs.push(("wav_in".into(), SessionInputValue::from(wav.into_dyn())));
        for cache in &self.caches {
            let tensor = Tensor::from_array((cache.shape.clone(), cache.data.clone()))
                .map_err(model_error)?;
            inputs.push((
                cache.input.clone().into(),
                SessionInputValue::from(tensor.into_dyn()),
            ));
        }
        let outputs = self.session.run(inputs).map_err(model_error)?;
        let (_, wav_out) = outputs
            .get("wav_out")
            .ok_or_else(|| model_error("no wav_out"))?
            .try_extract_tensor::<f32>()
            .map_err(model_error)?;
        self.native_out
            .extend(wav_out.iter().map(|s| to_i16(*s * 32_768.0)));
        for cache in &mut self.caches {
            let (_, data) = outputs
                .get(cache.output.as_str())
                .ok_or_else(|| model_error("missing cache output"))?
                .try_extract_tensor::<f32>()
                .map_err(model_error)?;
            cache.data.clear();
            cache.data.extend_from_slice(data);
        }
        Ok(())
    }
}

impl Denoiser for FastEnhancer {
    fn sample_rate(&self) -> u32 {
        self.rate.outer_hz()
    }

    fn delay(&self) -> Duration {
        let samples = u64::try_from(self.lag).unwrap_or(0);
        self.rate.delay() + Duration::from_micros(samples * 1_000_000 / u64::from(NATIVE_HZ))
    }

    fn process(&mut self, input: &[i16], output: &mut Vec<i16>) -> Result<()> {
        self.up.clear();
        self.rate.to_native(input, &mut self.up)?;
        self.pending
            .extend(self.up.iter().map(|s| f32::from(*s) / 32_768.0));
        self.native_out.clear();
        let mut used = 0;
        let mut failure = None;
        while used + self.hop <= self.pending.len() {
            if let Err(e) = self.step(used) {
                // The hop is lost, but the stream goes on: the caches are untouched.
                failure.get_or_insert(e);
            }
            used += self.hop;
        }
        self.pending.drain(..used);
        self.rate.from_native(&self.native_out, output)?;
        failure.map_or(Ok(()), Err)
    }

    fn reset(&mut self) {
        self.rate.reset();
        self.pending.clear();
        for cache in &mut self.caches {
            cache.data.fill(0.0);
        }
    }
}
