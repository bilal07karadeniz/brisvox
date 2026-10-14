//! Whisper log-mel features for Smart Turn v3.
//!
//! A port of Pipecat's vendored numpy extractor (`_whisper_features.py` at Pipecat commit
//! 3b2dccd8, itself `transformers.WhisperFeatureExtractor(chunk_length=8)`); see
//! `docs/providers/smart-turn.md`. On exactly 8 s of 16 kHz audio (128 000 samples):
//!
//! 1. waveform normalization `(x - mean) / sqrt(var + 1e-7)`;
//! 2. reflect pad 200 samples on both sides, frames of 400 samples every 160, periodic Hann;
//! 3. power spectrum `|rFFT|^2` (201 bins), Slaney mel filterbank (80 mels, 0-8 kHz,
//!    area-normalized), floor 1e-10, `log10`;
//! 4. the last of the 801 frames is dropped, values are floored at `max - 8`, then
//!    `(x + 4) / 4`: an `[80, 800]` row-major matrix (mel-major).
//!
//! The spectrum and filterbank run in `f64` like the reference. Matches the reference to
//! ~1e-5 on the fixtures (test `features_match_the_reference_extractor`). About 2-4 ms per
//! call: run it on the inference pool, never on a tokio worker. No allocation after
//! [`WhisperLogMel::new`].

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

use crate::error::{Error, Result};

/// Sample rate the features are defined for.
pub const SAMPLE_RATE: u32 = 16_000;
/// Samples per call: 8 s at 16 kHz.
pub const WINDOW_SAMPLES: usize = 128_000;
/// FFT size (25 ms).
pub const N_FFT: usize = 400;
/// Hop (10 ms).
pub const HOP: usize = 160;
/// Mel bands.
pub const N_MELS: usize = 80;
/// Frames kept (801 analysed, the last dropped).
pub const FRAMES: usize = 800;
/// Values per feature matrix (`N_MELS * FRAMES`).
pub const FEATURES: usize = N_MELS * FRAMES;

const BINS: usize = N_FFT / 2 + 1;
const PAD: usize = N_FFT / 2;
const MEL_FLOOR: f64 = 1e-10;
const NORM_EPS: f64 = 1e-7;
const DYNAMIC_RANGE: f64 = 8.0;

/// One triangular mel filter: its non-zero weights from bin `start`.
#[derive(Debug, Clone)]
struct Filter {
    start: usize,
    weights: Vec<f64>,
}

/// The extractor with its FFT plan and scratch buffers (one per worker or per session).
pub struct WhisperLogMel {
    fft: Arc<dyn RealToComplex<f64>>,
    window: Vec<f64>,
    filters: Vec<Filter>,
    padded: Vec<f64>,
    frame: Vec<f64>,
    spectrum: Vec<Complex<f64>>,
    scratch: Vec<Complex<f64>>,
    power: Vec<f64>,
    /// `x - mean`, then its squares (f32, like the reference's normalization).
    centered: Vec<f32>,
    squares: Vec<f32>,
    /// log10 mel energies before the final scaling, kept in f64 like the reference.
    log_spec: Vec<f64>,
}

impl std::fmt::Debug for WhisperLogMel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhisperLogMel").finish_non_exhaustive()
    }
}

impl Default for WhisperLogMel {
    fn default() -> Self {
        Self::new()
    }
}

impl WhisperLogMel {
    /// Plans the FFT and builds the window and filterbank (allocates ~1 MB of scratch).
    #[must_use]
    pub fn new() -> Self {
        let fft = RealFftPlanner::<f64>::new().plan_fft_forward(N_FFT);
        let spectrum = fft.make_output_vec();
        let scratch = fft.make_scratch_vec();
        Self {
            fft,
            window: periodic_hann(N_FFT),
            filters: slaney_filters(),
            padded: vec![0.0; WINDOW_SAMPLES + 2 * PAD],
            frame: vec![0.0; N_FFT],
            spectrum,
            scratch,
            power: vec![0.0; BINS],
            centered: vec![0.0; WINDOW_SAMPLES],
            squares: vec![0.0; WINDOW_SAMPLES],
            log_spec: vec![0.0; FEATURES],
        }
    }

    /// Computes the `[80, 800]` features (mel-major) of exactly [`WINDOW_SAMPLES`] samples in
    /// [-1, 1] into `out` ([`FEATURES`] values).
    ///
    /// # Errors
    /// [`Error::Model`] if `audio` or `out` has the wrong length.
    pub fn compute(&mut self, audio: &[f32], out: &mut [f32]) -> Result<()> {
        if audio.len() != WINDOW_SAMPLES || out.len() != FEATURES {
            return Err(Error::Model(format!(
                "log-mel: {} samples into {} values, expected {WINDOW_SAMPLES} into {FEATURES}",
                audio.len(),
                out.len()
            )));
        }
        self.normalize_and_pad(audio)?;
        let mut max = f64::NEG_INFINITY;
        for (t, frame) in self
            .padded
            .windows(N_FFT)
            .step_by(HOP)
            .take(FRAMES)
            .enumerate()
        {
            for ((dst, x), w) in self.frame.iter_mut().zip(frame).zip(&self.window) {
                *dst = x * w;
            }
            self.fft
                .process_with_scratch(&mut self.frame, &mut self.spectrum, &mut self.scratch)
                .map_err(|e| Error::Model(format!("log-mel FFT: {e}")))?;
            for (p, c) in self.power.iter_mut().zip(&self.spectrum) {
                // np.abs(spec) ** 2: hypot, then square.
                *p = c.re.hypot(c.im).powi(2);
            }
            let rows = self.log_spec.as_chunks_mut::<FRAMES>().0;
            for (filter, row) in self.filters.iter().zip(rows) {
                let bins = self
                    .power
                    .get(filter.start..filter.start + filter.weights.len())
                    .unwrap_or(&[]);
                let energy: f64 = bins.iter().zip(&filter.weights).map(|(p, w)| p * w).sum();
                let log = energy.max(MEL_FLOOR).log10();
                max = max.max(log);
                if let Some(slot) = row.get_mut(t) {
                    *slot = log;
                }
            }
        }
        let floor = max - DYNAMIC_RANGE;
        for (value, log) in out.iter_mut().zip(&self.log_spec) {
            // The reference casts to f32 only at the end.
            #[allow(clippy::cast_possible_truncation)]
            {
                *value = ((log.max(floor) + 4.0) / 4.0) as f32;
            }
        }
        Ok(())
    }

    /// Normalizes `audio` into the middle of `padded` and reflects the edges (numpy `reflect`:
    /// the edge sample itself is not repeated).
    ///
    /// The normalization reproduces numpy's float32 `(x - x.mean()) / np.sqrt(x.var() + 1e-7)`
    /// bit for bit (pairwise f32 sums, f32 division): the int8 model moves by up to 0.1 for
    /// feature changes of 1e-6, so "close" is not enough (docs/providers/smart-turn.md).
    fn normalize_and_pad(&mut self, audio: &[f32]) -> Result<()> {
        // 128 000 is exact in f32.
        #[allow(clippy::cast_precision_loss)]
        let n = WINDOW_SAMPLES as f32;
        let mean = (0.0 + pairwise_sum(audio)) / n;
        for ((c, s), &x) in self.centered.iter_mut().zip(&mut self.squares).zip(audio) {
            *c = x - mean;
            *s = *c * *c;
        }
        let var = (0.0 + pairwise_sum(&self.squares)) / n;
        #[allow(clippy::cast_possible_truncation)] // 1e-7 as the reference's f32 scalar
        let std = (var + NORM_EPS as f32).sqrt();
        let middle = self
            .padded
            .get_mut(PAD..PAD + WINDOW_SAMPLES)
            .ok_or_else(|| Error::Model("log-mel: pad buffer".into()))?;
        for (dst, &c) in middle.iter_mut().zip(&self.centered) {
            *dst = f64::from(c / std);
        }
        let (head, rest) = self.padded.split_at_mut(PAD);
        // padded[i] = x[PAD - i] for i < PAD, x[k] = rest[k].
        let source = rest
            .get(1..=PAD)
            .ok_or_else(|| Error::Model("log-mel: pad head".into()))?;
        for (dst, src) in head.iter_mut().zip(source.iter().rev()) {
            *dst = *src;
        }
        let (body, tail) = self.padded.split_at_mut(PAD + WINDOW_SAMPLES);
        // padded[PAD + n + j] = x[n - 2 - j].
        let source = body
            .get(PAD + WINDOW_SAMPLES - 1 - PAD..PAD + WINDOW_SAMPLES - 1)
            .ok_or_else(|| Error::Model("log-mel: pad tail".into()))?;
        for (dst, src) in tail.iter_mut().zip(source.iter().rev()) {
            *dst = *src;
        }
        Ok(())
    }
}

/// `np.hanning(n + 1)[:-1]` (torch's periodic Hann), with numpy's own formula:
/// `0.5 + 0.5 * cos(pi * k / (M - 1))` for `k = 1 - M, 3 - M, ...` and `M = n + 1`.
fn periodic_hann(n: usize) -> Vec<f64> {
    #[allow(clippy::cast_precision_loss)] // n = 400
    let span = n as f64;
    (0..n)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let k = (2 * i) as f64 - span;
            0.5 + 0.5 * (std::f64::consts::PI * k / span).cos()
        })
        .collect()
}

/// numpy's pairwise float32 summation (`pairwise_sum_FLOAT`, `PW_BLOCKSIZE` 128, 8 partial
/// sums), so sums match `np.add.reduce` on float32 bit for bit.
fn pairwise_sum(a: &[f32]) -> f32 {
    let n = a.len();
    if n < 8 {
        a.iter().fold(0.0, |acc, &v| acc + v)
    } else if n <= 128 {
        let (blocks, tail) = a.as_chunks::<8>();
        let mut blocks = blocks.iter();
        let mut r = blocks.next().copied().unwrap_or([0.0; 8]);
        for block in blocks {
            for (acc, v) in r.iter_mut().zip(block) {
                *acc += v;
            }
        }
        let [r0, r1, r2, r3, r4, r5, r6, r7] = r;
        let head = ((r0 + r1) + (r2 + r3)) + ((r4 + r5) + (r6 + r7));
        tail.iter().fold(head, |acc, &v| acc + v)
    } else {
        let half = n / 2;
        let (left, right) = a.split_at(half - half % 8);
        pairwise_sum(left) + pairwise_sum(right)
    }
}

fn hz_to_mel(hz: f64) -> f64 {
    if hz >= 1_000.0 {
        15.0 + (hz / 1_000.0).ln() * (27.0 / 6.4_f64.ln())
    } else {
        3.0 * hz / 200.0
    }
}

fn mel_to_hz(mel: f64) -> f64 {
    if mel >= 15.0 {
        1_000.0 * ((6.4_f64.ln() / 27.0) * (mel - 15.0)).exp()
    } else {
        200.0 * mel / 3.0
    }
}

/// The reference `_build_mel_filterbank(201, 80, 0, 8000, 16000)`, kept sparse.
fn slaney_filters() -> Vec<Filter> {
    let mel_max = hz_to_mel(f64::from(SAMPLE_RATE) / 2.0);
    let points = N_MELS + 2;
    // np.linspace: start + i * step, the last point exactly `stop`.
    #[allow(clippy::cast_precision_loss)]
    let step = mel_max / (points - 1) as f64;
    let edges: Vec<f64> = (0..points)
        .map(|i| {
            if i + 1 == points {
                mel_to_hz(mel_max)
            } else {
                #[allow(clippy::cast_precision_loss)]
                let i = i as f64;
                mel_to_hz(i * step)
            }
        })
        .collect();
    #[allow(clippy::cast_precision_loss)]
    let bin_hz = f64::from(SAMPLE_RATE / 2) / (BINS - 1) as f64;
    edges
        .windows(3)
        .map(|edge| {
            let (lower, center, upper) = match *edge {
                [a, b, c] => (a, b, c),
                _ => (0.0, 0.0, 0.0),
            };
            let enorm = 2.0 / (upper - lower);
            let dense = (0..BINS).map(|k| {
                #[allow(clippy::cast_precision_loss)]
                let hz = k as f64 * bin_hz;
                let down = (hz - lower) / (center - lower);
                let up = (upper - hz) / (upper - center);
                down.min(up).max(0.0) * enorm
            });
            let dense: Vec<f64> = dense.collect();
            let start = dense.iter().position(|&w| w > 0.0).unwrap_or(0);
            let end = dense
                .iter()
                .rposition(|&w| w > 0.0)
                .map_or(start, |e| e + 1);
            Filter {
                start,
                weights: dense
                    .get(start..end)
                    .map(<[f64]>::to_vec)
                    .unwrap_or_default(),
            }
        })
        .collect()
}
