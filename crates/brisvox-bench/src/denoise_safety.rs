//! Does a denoiser leave clean speech alone? (`brisvox-bench denoise safety`, feature `denoise`.)
//!
//! A filter that works on noise can still destroy speech: Hush attenuated clean 8 kHz telephone
//! speech by 12 dB and distorted it until a recognizer lost most of the words, and nothing in the
//! noise tests showed it. This check runs the speech fixtures with **no noise at all**, on both
//! paths and at three caller levels (as recorded, 10 and 20 dB quieter: a soft-spoken caller), and
//! compares every backend with the unfiltered audio:
//!
//! - **gain**: the median level change of the 20 ms frames inside utterances (dB). Below
//!   [`WARN_GAIN_DB`] it is reported as attenuating, below [`MIN_GAIN_DB`] it fails.
//! - **VAD recall**: the share of the utterances' duration that the VAD marks as speech, against the
//!   same measure without the filter. Fails when it drops by more than [`MAX_RECALL_LOSS`].
//! - **missed utterances**: utterances with no speech start, against none without the filter.
//!
//! A backend that fails a path is one the engine must not use there; the run exits with an error so
//! that a new backend, a new crate version or a changed resampler cannot slip through.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{Clock, SystemClock};
use brisvox_inference::silero_tract::SileroTractModel;
use brisvox_inference::{VadEvent, VadParams, VadStream};

use crate::BenchError;
use crate::denoise_eval::{Path16, Speech, Variant, enhance, rms};

/// Below this median gain on clean speech a backend attenuates it noticeably (dB): reported, not
/// a failure (Hush on wideband audio: -8 dB, and the same word error rate).
pub const WARN_GAIN_DB: f64 = -6.0;
/// The lowest median gain on clean speech a backend may have (dB).
pub const MIN_GAIN_DB: f64 = -10.0;
/// The most VAD recall a backend may lose against no filter.
pub const MAX_RECALL_LOSS: f64 = 0.05;

/// Caller levels relative to the fixtures (dB).
pub const LEVELS_DB: [f32; 3] = [0.0, -10.0, -20.0];

/// One backend on one path at one level.
#[derive(Debug, Clone)]
pub struct SafetyRow {
    /// Backend.
    pub variant: &'static str,
    /// Path.
    pub path: Path16,
    /// Caller level against the fixture (dB).
    pub level_db: f32,
    /// The engine does not use the backend on this path (below its minimum rate): not measured.
    pub refused: bool,
    /// Median gain of speech frames (dB).
    pub gain_db: f64,
    /// Share of the utterances' duration the VAD marked as speech.
    pub recall: f64,
    /// Same without the filter.
    pub recall_without: f64,
    /// Utterances without a speech start.
    pub missed: u32,
    /// Same without the filter.
    pub missed_without: u32,
    /// Utterances.
    pub utterances: u32,
}

impl SafetyRow {
    /// Whether the backend preserves speech here.
    pub fn passes(&self) -> bool {
        self.refused
            || (self.gain_db >= MIN_GAIN_DB
                && self.recall >= self.recall_without - MAX_RECALL_LOSS
                && self.missed <= self.missed_without)
    }
}

fn gain(
    d: &mut dyn brisvox_inference::Denoiser,
    pcm: &[i16],
    segments: &[(usize, usize)],
) -> Result<f64, BenchError> {
    let out = enhance(d, pcm)?;
    let mut gains = Vec::new();
    for &(a, b) in segments {
        let (Some(input), Some(output)) = (pcm.get(a..b), out.get(a..b)) else {
            continue;
        };
        for (i, o) in input
            .as_chunks::<320>()
            .0
            .iter()
            .zip(output.as_chunks::<320>().0.iter())
        {
            let (ri, ro) = (rms(&i[..]), rms(&o[..]));
            if ri > 100.0 {
                gains.push(20.0 * (ro.max(1.0) / ri).log10());
            }
        }
    }
    gains.sort_by(f64::total_cmp);
    Ok(gains.get(gains.len() / 2).copied().unwrap_or(0.0))
}

/// (share of the utterances' duration marked as speech, utterances missed).
fn vad_recall(
    pcm: &[i16],
    segments: &[(usize, usize)],
    silero: &Arc<SileroTractModel>,
    variant: &Variant,
) -> Result<(f64, u32), BenchError> {
    let mut stream = VadStream::new(
        silero
            .stream()
            .map_err(|e| BenchError::Config(e.to_string()))?,
        VadParams::default(),
    );
    if let Some(d) = variant.build(16_000)? {
        stream = stream
            .with_denoiser(d)
            .map_err(|e| BenchError::Config(e.to_string()))?;
    }
    let origin = SystemClock.now();
    let mut events = Vec::new();
    for (i, frame) in pcm.chunks(320).enumerate() {
        let at = origin + Duration::from_millis(20) * u32::try_from(i).unwrap_or(u32::MAX);
        stream
            .push(frame, at, &mut events)
            .map_err(|e| BenchError::Config(e.to_string()))?;
    }
    events.extend(stream.finish());
    // Active intervals in samples.
    let to_sample = |t: tokio::time::Instant| {
        usize::try_from(t.saturating_duration_since(origin).as_micros() * 16 / 1000).unwrap_or(0)
    };
    let mut active: Vec<(usize, usize)> = Vec::new();
    let mut open = None;
    for e in &events {
        match e {
            VadEvent::Start { at } => open = Some(to_sample(*at)),
            VadEvent::Stop { at } => {
                if let Some(start) = open.take() {
                    active.push((start, to_sample(*at)));
                }
            }
        }
    }
    if let Some(start) = open {
        active.push((start, pcm.len()));
    }
    let (mut covered, mut total, mut missed) = (0usize, 0usize, 0u32);
    for &(a, b) in segments {
        total += b.saturating_sub(a);
        let overlap: usize = active
            .iter()
            .map(|&(s, e)| e.min(b).saturating_sub(s.max(a)))
            .sum();
        covered += overlap.min(b.saturating_sub(a));
        let hit = active.iter().any(|&(s, _)| s + 4800 >= a && s <= b);
        missed += u32::from(!hit);
    }
    #[allow(clippy::cast_precision_loss)]
    Ok((covered as f64 / total.max(1) as f64, missed))
}

fn scaled(speech: &Speech, level_db: f32) -> Vec<i16> {
    let g = 10f64.powf(f64::from(level_db) / 20.0);
    #[allow(clippy::cast_possible_truncation)]
    speech
        .pcm
        .iter()
        .map(|s| (f64::from(*s) * g).round() as i16)
        .collect()
}

/// Runs the check. With `force`, a backend below its minimum rate is measured anyway (to show what
/// the engine's refusal avoids).
///
/// # Errors
/// [`BenchError`] when a model or a resampler fails.
pub fn safety(
    variants: &[Variant],
    paths: &[Path16],
    speech: &[Speech],
    silero: &Arc<SileroTractModel>,
    force: bool,
) -> Result<Vec<SafetyRow>, BenchError> {
    let mut rows = Vec::new();
    for &path in paths {
        for &level in &LEVELS_DB {
            // The line as the engine receives it, per fixture.
            let mut lines = Vec::new();
            for sp in speech {
                lines.push((path.apply(&scaled(sp, level))?, &sp.segments));
            }
            let mut baseline = (0.0f64, 0u32, 0u32);
            let mut utterances = 0u32;
            for (pcm, segments) in &lines {
                let (r, m) = vad_recall(pcm, segments, silero, &Variant::None)?;
                baseline.0 += r;
                baseline.1 += m;
                utterances += u32::try_from(segments.len()).unwrap_or(0);
            }
            #[allow(clippy::cast_precision_loss)]
            let n = lines.len().max(1) as f64;
            for variant in variants {
                if *variant == Variant::None {
                    continue;
                }
                let refused = !force && variant.min_call_hz() > path.call_hz();
                let (mut g, mut recall, mut missed) = (Vec::new(), 0.0f64, 0u32);
                if !refused {
                    for (pcm, segments) in &lines {
                        if let Some(mut d) = variant.build(16_000)? {
                            g.push(gain(d.as_mut(), pcm, segments)?);
                        }
                        let (r, m) = vad_recall(pcm, segments, silero, variant)?;
                        recall += r;
                        missed += m;
                    }
                }
                g.sort_by(f64::total_cmp);
                rows.push(SafetyRow {
                    variant: variant.name(),
                    path,
                    level_db: level,
                    refused,
                    gain_db: g.get(g.len() / 2).copied().unwrap_or(0.0),
                    recall: recall / n,
                    recall_without: baseline.0 / n,
                    missed,
                    missed_without: baseline.1,
                    utterances,
                });
            }
        }
    }
    Ok(rows)
}

/// The rows as markdown; the problems are the failing rows.
pub fn safety_markdown(rows: &[SafetyRow], environment: &[String]) -> (String, Vec<String>) {
    let mut out = String::from("# Noise suppression: clean speech must survive\n\n");
    for line in environment {
        let _ = writeln!(out, "- {line}");
    }
    let _ = write!(
        out,
        "\nSpeech fixtures with no noise, on both paths, as recorded and 10 and 20 dB quieter (a \
         soft-spoken caller). *Gain*: median level change of the frames inside utterances; fails \
         below {MIN_GAIN_DB} dB (a warning from {WARN_GAIN_DB} dB). *VAD recall*: share of the utterances' duration the VAD marks as \
         speech, with the filter against without; fails on a loss of more than {:.0} points. *Missed*: \
         utterances with no speech start, with the filter against without. *refused*: the engine does \
         not use the backend on this path (below its minimum sample rate), nothing measured.\n\n",
        MAX_RECALL_LOSS * 100.0
    );
    out.push_str(
        "| backend | path | caller level | gain | VAD recall (without) | missed (without) | result |\n\
         |---|---|---|---|---|---|---|\n",
    );
    let mut problems = Vec::new();
    for r in rows {
        if r.refused {
            let _ = writeln!(
                out,
                "| {} | {} | {:+} dB | - | - | - | refused by the engine on this path |",
                r.variant,
                r.path.name(),
                r.level_db
            );
            continue;
        }
        let ok = r.passes();
        if !ok {
            problems.push(format!(
                "{} on the {} path at {:+} dB: gain {:.1} dB, VAD recall {:.2} (without {:.2}), missed {} (without {})",
                r.variant,
                r.path.name(),
                r.level_db,
                r.gain_db,
                r.recall,
                r.recall_without,
                r.missed,
                r.missed_without
            ));
        }
        let _ = writeln!(
            out,
            "| {} | {} | {:+} dB | {:.1} dB | {:.2} ({:.2}) | {}/{} ({}) | {} |",
            r.variant,
            r.path.name(),
            r.level_db,
            r.gain_db,
            r.recall,
            r.recall_without,
            r.missed,
            r.utterances,
            r.missed_without,
            if !ok {
                "FAILS"
            } else if r.gain_db < WARN_GAIN_DB {
                "attenuates"
            } else {
                "ok"
            }
        );
    }
    (out, problems)
}
