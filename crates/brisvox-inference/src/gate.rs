//! Level gates in front of the speech model's verdict: a static level floor and a rolling
//! percentile gate (the idea behind Vapi's "Fourier denoising"), plus a running noise-floor
//! estimate for the barge-in guards. No model, no allocation after construction, a few hundred
//! nanoseconds per 32 ms window.
//!
//! [`Gated`] wraps any [`SpeechModel`]. The wrapped model always runs on the window (its recurrent
//! state must see all audio); the gate only overrides the probability it reports:
//!
//! - **static floor** ([`GateConfig::floor_db`]): a window whose RMS is below this level (dBFS) is
//!   not speech. Background voices and distant sound are usually quieter than the caller.
//! - **rolling gate** ([`RollingGate`]): the levels of the windows the model itself called speech are
//!   kept for a few seconds; their `percentile` (85th by default) is the caller's level. A window more
//!   than `offset_db` below it (-15 dB by default) is not speech. With less than `min_windows` of
//!   speech heard the gate stays open (fail open: the first words of a call are never gated).
//! - **noise floor** ([`NoiseFloor`]): the 10th percentile of the levels of the last few seconds of
//!   windows, published through a shared handle so that the turn driver can raise its guards when the
//!   line is noisy.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use crate::error::Result;
use crate::vad::SpeechModel;

/// Level of silence in dBFS (the floor of every measurement).
pub const SILENCE_DB: f32 = -100.0;

/// Settings of the rolling percentile gate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RollingGate {
    /// How many windows of speech are remembered (at 32 ms per window, 94 is about 3 s).
    pub windows: usize,
    /// Which level of the remembered windows is the caller's level, 0..=1.
    pub percentile: f32,
    /// A window this many dB below the caller's level is not speech (negative).
    pub offset_db: f32,
    /// Speech windows needed before the gate acts.
    pub min_windows: usize,
}

impl Default for RollingGate {
    /// Vapi's defaults: 3 s, the 85th percentile, 15 dB below.
    fn default() -> Self {
        Self {
            windows: 94,
            percentile: 0.85,
            offset_db: -15.0,
            min_windows: 16,
        }
    }
}

/// Which gates [`Gated`] applies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateConfig {
    /// Windows below this level (dBFS RMS) are not speech.
    pub floor_db: Option<f32>,
    /// Windows far below the caller's own level are not speech.
    pub rolling: Option<RollingGate>,
    /// The probability from which the model calls a window speech (the VAD's threshold): the
    /// windows that teach the caller's level, and the ones a gate may veto.
    pub speech_from: f32,
    /// Apply the gates to the *start* of speech only: once a window passed (the speaker is
    /// talking), quieter windows of the same stretch pass too, so the soft consonants and word
    /// endings of the caller are not cut out (a gate on every window fragments the caller's
    /// own utterance). A quiet voice still cannot start speech.
    pub onset_only: bool,
    /// Measure the noise floor ([`NoiseFloorHandle`]) even when no gate is on. With no gate and no
    /// tracking the wrapper passes every window straight to the model.
    pub track_floor: bool,
}

impl Default for GateConfig {
    /// No gate.
    fn default() -> Self {
        Self {
            floor_db: None,
            rolling: None,
            speech_from: 0.5,
            onset_only: false,
            track_floor: false,
        }
    }
}

/// The noise floor in dBFS (tenths of a dB in an atomic), shared between the VAD stream that
/// measures it and the turn driver that reads it.
#[derive(Debug, Clone)]
pub struct NoiseFloorHandle(Arc<AtomicI32>);

impl Default for NoiseFloorHandle {
    fn default() -> Self {
        Self(Arc::new(AtomicI32::new(-1000)))
    }
}

impl NoiseFloorHandle {
    /// The latest estimate in dBFS ([`SILENCE_DB`] before any audio).
    #[allow(clippy::cast_precision_loss)]
    pub fn db(&self) -> f32 {
        self.0.load(Ordering::Relaxed) as f32 / 10.0
    }

    #[allow(clippy::cast_possible_truncation)]
    fn set(&self, db: f32) {
        self.0.store((db * 10.0).round() as i32, Ordering::Relaxed);
    }
}

/// RMS level of `samples` (floats in -1..1) in dBFS.
pub fn level_db(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return SILENCE_DB;
    }
    #[allow(clippy::cast_precision_loss)]
    let mean = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
    if mean <= 1e-10 {
        SILENCE_DB
    } else {
        (10.0 * mean.log10()).max(SILENCE_DB)
    }
}

/// A ring of the last `capacity` levels with a percentile query on a scratch copy.
#[derive(Debug)]
struct LevelRing {
    levels: Vec<f32>,
    scratch: Vec<f32>,
    next: usize,
    filled: usize,
}

impl LevelRing {
    fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            levels: vec![SILENCE_DB; capacity],
            scratch: Vec::with_capacity(capacity),
            next: 0,
            filled: 0,
        }
    }

    fn push(&mut self, level: f32) {
        if let Some(slot) = self.levels.get_mut(self.next) {
            *slot = level;
        }
        self.next = (self.next + 1) % self.levels.len();
        self.filled = (self.filled + 1).min(self.levels.len());
    }

    /// The `q`-quantile (0..=1) of the levels held, nearest rank.
    fn percentile(&mut self, q: f32) -> f32 {
        self.scratch.clear();
        self.scratch
            .extend(self.levels.iter().take(self.filled).copied());
        if self.scratch.is_empty() {
            return SILENCE_DB;
        }
        self.scratch.sort_unstable_by(f32::total_cmp);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let rank =
            ((q.clamp(0.0, 1.0) * self.scratch.len() as f32).ceil() as usize).saturating_sub(1);
        self.scratch
            .get(rank.min(self.scratch.len() - 1))
            .copied()
            .unwrap_or(SILENCE_DB)
    }
}

/// A speech model with level gates on its verdict.
pub struct Gated<M: SpeechModel> {
    inner: M,
    config: GateConfig,
    /// Levels of the windows the model called speech (only with the rolling gate).
    speech: Option<LevelRing>,
    /// Levels of every window, for the noise floor (only with `track_floor`).
    all: Option<LevelRing>,
    handle: NoiseFloorHandle,
    /// Windows seen; the noise floor is refreshed every few windows.
    seen: u64,
    /// The caller's level, refreshed with the noise floor.
    baseline: f32,
    /// Windows the gate vetoed (for the tests and the benchmark).
    gated: u64,
    /// The last window was speech (after the gates).
    active: bool,
}

impl<M: SpeechModel> std::fmt::Debug for Gated<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gated")
            .field("config", &self.config)
            .field("gated", &self.gated)
            .finish_non_exhaustive()
    }
}

/// Windows between two refreshes of the percentiles.
const REFRESH_EVERY: u64 = 4;

/// Windows of history for the noise floor (about 5 s at 32 ms).
const NOISE_FLOOR_WINDOWS: usize = 156;

impl<M: SpeechModel> Gated<M> {
    /// `inner` with `config`; the noise floor is published through `handle`.
    pub fn new(inner: M, config: GateConfig, handle: NoiseFloorHandle) -> Self {
        // The rings are allocated here, once, and only for the gates that are on: a model without
        // gates or tracking allocates nothing.
        let speech = config.rolling.map(|r| LevelRing::new(r.windows));
        let all = config
            .track_floor
            .then(|| LevelRing::new(NOISE_FLOOR_WINDOWS));
        Self {
            inner,
            config,
            speech,
            all,
            handle,
            seen: 0,
            baseline: SILENCE_DB,
            gated: 0,
            active: false,
        }
    }

    /// Windows whose verdict the gate overrode.
    pub fn gated_windows(&self) -> u64 {
        self.gated
    }

    /// The caller's level the rolling gate holds now (dBFS), [`SILENCE_DB`] before enough speech.
    pub fn baseline_db(&self) -> f32 {
        self.baseline
    }
}

impl<M: SpeechModel> SpeechModel for Gated<M> {
    fn window(&self) -> usize {
        self.inner.window()
    }

    fn context(&self) -> usize {
        self.inner.context()
    }

    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }

    fn infer(&mut self, input: &[f32]) -> Result<f32> {
        let probability = self.inner.infer(input)?;
        if self.config.floor_db.is_none()
            && self.config.rolling.is_none()
            && !self.config.track_floor
        {
            return Ok(probability);
        }
        let new = input.get(self.inner.context()..).unwrap_or(input);
        let level = level_db(new);
        self.seen += 1;
        if let Some(all) = self.all.as_mut() {
            all.push(level);
            if self.seen.is_multiple_of(REFRESH_EVERY) {
                self.handle.set(all.percentile(0.10));
            }
        }
        let mut speech = probability >= self.config.speech_from;
        let applies = !(self.config.onset_only && self.active);
        if let Some(floor) = self.config.floor_db
            && level < floor
            && applies
        {
            speech = false;
        }
        if let (Some(rolling), Some(ring)) = (self.config.rolling, self.speech.as_mut()) {
            if self.seen.is_multiple_of(REFRESH_EVERY) || self.baseline <= SILENCE_DB {
                self.baseline = if ring.filled >= rolling.min_windows {
                    ring.percentile(rolling.percentile)
                } else {
                    SILENCE_DB
                };
            }
            if speech
                && applies
                && self.baseline > SILENCE_DB
                && level < self.baseline + rolling.offset_db
            {
                speech = false;
            } else if speech {
                // Only windows that passed teach the caller's level.
                ring.push(level);
            }
        }
        self.active = speech;
        if probability >= self.config.speech_from && !speech {
            self.gated += 1;
            // Report silence: the detector's hysteresis band sits under the negative threshold.
            return Ok(0.0);
        }
        Ok(probability)
    }

    /// Resets the model (a gap in the audio, a new stream). The gate starts again with no speech in
    /// progress, so the next onset is judged by the gates; what it learned about the caller's
    /// level and the noise floor is kept: it is the same caller on the same line.
    fn reset(&mut self) {
        self.inner.reset();
        self.active = false;
    }

    fn warm(&self) -> Result<()> {
        self.inner.warm()
    }

    fn warm_key(&self) -> Option<String> {
        self.inner.warm_key()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A model that says "speech" for every window.
    struct Always;

    impl SpeechModel for Always {
        fn window(&self) -> usize {
            4
        }
        fn context(&self) -> usize {
            1
        }
        fn sample_rate(&self) -> u32 {
            16_000
        }
        fn infer(&mut self, _: &[f32]) -> Result<f32> {
            Ok(0.9)
        }
        fn reset(&mut self) {}
    }

    /// One window at `amplitude` (square wave, so the RMS is the amplitude).
    fn window(amplitude: f32) -> Vec<f32> {
        vec![amplitude, amplitude, -amplitude, amplitude, -amplitude]
    }

    #[test]
    fn the_level_of_a_square_wave_is_its_amplitude() {
        assert!((level_db(&[0.1, -0.1, 0.1, -0.1]) + 20.0).abs() < 0.01);
        assert_eq!(level_db(&[]), SILENCE_DB);
        assert_eq!(level_db(&[0.0; 8]), SILENCE_DB);
    }

    #[test]
    fn the_static_floor_silences_quiet_windows_only() {
        let config = GateConfig {
            floor_db: Some(-40.0),
            ..GateConfig::default()
        };
        let mut gate = Gated::new(Always, config, NoiseFloorHandle::default());
        assert!(gate.infer(&window(0.1)).unwrap() > 0.5); // -20 dB
        assert!(gate.infer(&window(0.001)).unwrap() < 0.1); // -60 dB
        assert_eq!(gate.gated_windows(), 1);
    }

    #[test]
    fn the_rolling_gate_learns_the_callers_level_and_drops_a_quieter_voice() {
        let config = GateConfig {
            rolling: Some(RollingGate {
                min_windows: 8,
                ..RollingGate::default()
            }),
            ..GateConfig::default()
        };
        let mut gate = Gated::new(Always, config, NoiseFloorHandle::default());
        // Before it knows the caller, everything passes (fail open): a quiet voice first.
        assert!(gate.infer(&window(0.01)).unwrap() > 0.5);
        for _ in 0..20 {
            assert!(gate.infer(&window(0.2)).unwrap() > 0.5); // the caller, -14 dB
        }
        // A voice 25 dB below the caller is dropped, one 10 dB below passes.
        assert!(gate.infer(&window(0.011)).unwrap() < 0.1);
        assert!(gate.infer(&window(0.06)).unwrap() > 0.5);
        assert!(gate.baseline_db() > -20.0);
    }

    #[test]
    fn with_onset_only_a_quiet_window_inside_speech_passes_but_cannot_start_it() {
        let config = GateConfig {
            rolling: Some(RollingGate {
                min_windows: 8,
                ..RollingGate::default()
            }),
            onset_only: true,
            ..GateConfig::default()
        };
        let mut gate = Gated::new(Always, config, NoiseFloorHandle::default());
        for _ in 0..20 {
            gate.infer(&window(0.2)).unwrap();
        }
        // The caller keeps talking, a soft syllable 25 dB down is kept.
        assert!(gate.infer(&window(0.011)).unwrap() > 0.5);
    }

    #[test]
    fn a_reset_ends_the_speech_in_progress_but_keeps_what_was_learned() {
        let config = GateConfig {
            rolling: Some(RollingGate {
                min_windows: 8,
                ..RollingGate::default()
            }),
            onset_only: true,
            ..GateConfig::default()
        };
        let mut gate = Gated::new(Always, config, NoiseFloorHandle::default());
        for _ in 0..20 {
            gate.infer(&window(0.2)).unwrap();
        }
        assert!(gate.infer(&window(0.011)).unwrap() > 0.5); // inside speech
        gate.reset();
        // A gap: the next window is an onset again, and the caller's level is still known.
        assert!(gate.infer(&window(0.011)).unwrap() < 0.1);
        assert!(gate.baseline_db() > -20.0);
    }

    #[test]
    fn a_gate_that_is_off_keeps_no_history() {
        let gate = Gated::new(Always, GateConfig::default(), NoiseFloorHandle::default());
        assert!(gate.speech.is_none() && gate.all.is_none());
        let floor_only = Gated::new(
            Always,
            GateConfig {
                floor_db: Some(-40.0),
                ..GateConfig::default()
            },
            NoiseFloorHandle::default(),
        );
        assert!(floor_only.speech.is_none() && floor_only.all.is_none());
    }

    #[test]
    fn the_noise_floor_follows_the_quiet_windows() {
        let handle = NoiseFloorHandle::default();
        let config = GateConfig {
            track_floor: true,
            ..GateConfig::default()
        };
        let mut gate = Gated::new(Always, config, handle.clone());
        for i in 0..40 {
            // Mostly speech at -14 dB, with a quarter of the windows at -50 dB.
            let a = if i % 4 == 0 { 0.003 } else { 0.2 };
            gate.infer(&window(a)).unwrap();
        }
        assert!((handle.db() + 50.0).abs() < 1.5, "{}", handle.db());
    }
}
