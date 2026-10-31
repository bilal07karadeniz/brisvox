//! Noise suppression of the caller's audio, measured (feature `denoise`; `brisvox-bench denoise`).
//!
//! Three questions, answered with the code the engine runs:
//!
//! - **CPU per stream** ([`cpu`]): the wall time of one `process` call per 20 ms frame, and the
//!   process' CPU time per second of audio, for each backend at the call's rate (16 kHz, 8 kHz).
//! - **False barge-ins and turn verdicts under real noise** ([`evaluate`]): the speech fixtures
//!   (`fixtures/*.wav`, ground truth to the sample) mixed with the licensed noise recordings
//!   (`fixtures/noise`) at 20, 10, 5 and 0 dB signal-to-noise ratio, wideband and through the
//!   telephone path (8 kHz, mu-law, back to 16 kHz). For every backend (and "none") the Silero
//!   VAD runs on the audio exactly as the engine's VAD worker runs it
//!   ([`brisvox_inference::VadStream::with_denoiser`], production `VadParams`) and Smart Turn on
//!   the enhanced audio at the end of each utterance. Counted per cell:
//!   speech starts outside any utterance (false barge-ins: a speech start while the caller says
//!   nothing makes the bot stop talking), utterances with no start (missed), extra starts inside
//!   an utterance (fragments), the onset error, and the Smart Turn verdicts that differ from the
//!   verdict on the clean audio of the same path. The same noise alone, at the level it has in
//!   each mix, gives the false starts per minute.
//! - **Word error rate hook** ([`crate::denoise_stt`]): the same mixes (original and enhanced) to
//!   a speech-to-text server.
//!
//! The mixes are made in memory. The signal-to-noise ratio is the speech' RMS over the ground
//! truth speech segments of the fixture against the RMS of the noise as mixed.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use brisvox_audio::StreamResampler;
use brisvox_audio::codec::{ulaw_decode, ulaw_encode};
use brisvox_core::frame::SampleRate;
use brisvox_core::{Clock, SystemClock};
use brisvox_inference::denoise::{self, Backend, BoxedDenoiser};
use brisvox_inference::silero_tract::SileroTractModel;
use brisvox_inference::smart_turn::{SmartTurn, THRESHOLD, TurnModel};
use brisvox_inference::{VadEvent, VadParams, VadStream};
use brisvox_sim::fixture::{Fixture, fixtures_dir};

use crate::BenchError;
use crate::resources::ProcessProbe;
use crate::stats::{Summary, ms};

/// Samples per 20 ms frame at 16 kHz.
const FRAME: usize = 320;

/// How far before an utterance' start a speech start still counts as its onset, and how far after
/// its end (the VAD hangover) a start is still not a new one.
const ONSET_BEFORE: Duration = Duration::from_millis(300);
const ONSET_AFTER: Duration = Duration::from_millis(300);

/// The end of an utterance as the turn detector sees it: the VAD stop is dated to the last speech
/// plus the pad, and the analysis follows the hangover.
const SMART_TURN_AFTER_END: Duration = Duration::from_millis(300);

fn invalid(what: impl Into<String>) -> BenchError {
    BenchError::Config(what.into())
}

fn audio_error(e: impl std::fmt::Display) -> BenchError {
    BenchError::Config(format!("audio: {e}"))
}

fn model_error(e: impl std::fmt::Display) -> BenchError {
    BenchError::Config(format!("model: {e}"))
}

// ---------------------------------------------------------------------------------------------
// WAV and signal helpers

/// Reads a mono PCM16 WAV file: the sample rate and the samples.
///
/// # Errors
/// [`BenchError`] for any other format.
pub fn read_wav(path: &Path) -> Result<(u32, Vec<i16>), BenchError> {
    let bytes = std::fs::read(path)?;
    let bad = || {
        invalid(format!(
            "{}: not a mono 16-bit PCM WAV file",
            path.display()
        ))
    };
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err(bad());
    }
    let (mut pos, mut rate, mut data) = (12usize, 0u32, None);
    while let Some(header) = bytes.get(pos..pos + 8) {
        let size = le_u32(header, 4)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(bad)?;
        let body = bytes.get(pos + 8..pos + 8 + size).ok_or_else(bad)?;
        match header.get(..4) {
            Some(b"fmt ") => {
                let (format, channels, bits) = (le_u16(body, 0), le_u16(body, 2), le_u16(body, 14));
                if (format, channels, bits) != (Some(1), Some(1), Some(16)) {
                    return Err(bad());
                }
                rate = le_u32(body, 4).ok_or_else(bad)?;
            }
            Some(b"data") => data = Some(body),
            _ => {}
        }
        pos += 8 + size + (size & 1);
    }
    let data = data.ok_or_else(bad)?;
    Ok((rate, pcm_from_le(data)))
}

fn le_u16(bytes: &[u8], at: usize) -> Option<u16> {
    let pair: [u8; 2] = bytes.get(at..at + 2)?.try_into().ok()?;
    Some(u16::from_le_bytes(pair))
}

fn le_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let quad: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(quad))
}

/// Little-endian PCM16 bytes as samples.
pub fn pcm_from_le(bytes: &[u8]) -> Vec<i16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| i16::from_le_bytes(*p))
        .collect()
}

/// Writes mono PCM16 samples as a WAV file.
///
/// # Errors
/// [`BenchError::Io`].
pub fn write_wav(path: &Path, rate: u32, pcm: &[i16]) -> Result<(), BenchError> {
    let data_len = u32::try_from(pcm.len() * 2).map_err(|_| invalid("WAV too long"))?;
    let mut out = Vec::with_capacity(44 + pcm.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in pcm {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    std::fs::write(path, out)?;
    Ok(())
}

pub(crate) fn rms(pcm: &[i16]) -> f64 {
    if pcm.is_empty() {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let mean = pcm.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / pcm.len() as f64;
    mean.sqrt()
}

/// A noise recording.
#[derive(Debug, Clone)]
pub struct NoiseClip {
    /// File stem.
    pub name: String,
    /// 16 kHz samples.
    pub pcm: Vec<i16>,
}

/// Loads every `*.wav` in `dir` (16 kHz mono), in name order.
///
/// # Errors
/// [`BenchError`] when none is found or one is not 16 kHz mono PCM16.
pub fn load_noise(dir: &Path) -> Result<Vec<NoiseClip>, BenchError> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "wav"))
        .collect();
    paths.sort();
    let mut clips = Vec::new();
    for path in paths {
        let (rate, pcm) = read_wav(&path)?;
        if rate != 16_000 {
            return Err(invalid(format!(
                "{}: {rate} Hz, expected 16000",
                path.display()
            )));
        }
        let name = path
            .file_stem()
            .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        clips.push(NoiseClip { name, pcm });
    }
    if clips.is_empty() {
        return Err(invalid(format!("no .wav noise in {}", dir.display())));
    }
    Ok(clips)
}

/// A fixture with its audio as samples.
#[derive(Debug, Clone)]
pub struct Speech {
    /// Fixture name.
    pub name: String,
    /// 16 kHz samples.
    pub pcm: Vec<i16>,
    /// Utterances as sample ranges.
    pub segments: Vec<(usize, usize)>,
    /// The transcripts of the utterances, in order.
    pub texts: Vec<String>,
    /// RMS over the utterances.
    pub speech_rms: f64,
}

/// Loads the named fixtures from `dir` (default: the repository's `fixtures/`).
///
/// # Errors
/// [`BenchError`] when a fixture cannot be read.
pub fn load_speech(dir: Option<&Path>, names: &[String]) -> Result<Vec<Speech>, BenchError> {
    let dir = dir.map_or_else(fixtures_dir, Path::to_path_buf);
    names
        .iter()
        .map(|name| {
            let fixture = Fixture::load(&dir, name).map_err(|e| invalid(format!("{name}: {e}")))?;
            let pcm = pcm_from_le(&fixture.pcm);
            let segments: Vec<(usize, usize)> = fixture
                .segments
                .iter()
                .map(|s| {
                    (
                        usize::try_from(s.start).unwrap_or(0),
                        usize::try_from(s.end).unwrap_or(0),
                    )
                })
                .collect();
            let voiced: Vec<i16> = segments
                .iter()
                .filter_map(|(a, b)| pcm.get(*a..*b))
                .flatten()
                .copied()
                .collect();
            Ok(Speech {
                name: fixture.name,
                speech_rms: rms(&voiced),
                texts: fixture.segments.iter().map(|s| s.text.clone()).collect(),
                pcm,
                segments,
            })
        })
        .collect()
}

/// `speech` plus `noise` (looped from `offset`, cross-faded at the seam) scaled to `snr_db`
/// against the speech' RMS. `None`: the speech alone.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub fn mix(speech: &Speech, noise: Option<(&NoiseClip, usize)>, snr_db: f32) -> Vec<i16> {
    let Some((clip, offset)) = noise else {
        return speech.pcm.clone();
    };
    let noise_rms = rms(&clip.pcm).max(1.0);
    let gain = speech.speech_rms / (noise_rms * 10f64.powf(f64::from(snr_db) / 20.0));
    let n = clip.pcm.len().max(1);
    speech
        .pcm
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let noise = clip.pcm.get((offset + i) % n).copied().unwrap_or(0);
            (f64::from(*s) + f64::from(noise) * gain)
                .round()
                .clamp(-32_768.0, 32_767.0) as i16
        })
        .collect()
}

/// Noise alone, `len` samples, at the level it has in a mix at `snr_db` against `reference_rms`.
#[allow(clippy::cast_possible_truncation)]
pub fn noise_alone(clip: &NoiseClip, reference_rms: f64, snr_db: f32, len: usize) -> Vec<i16> {
    let noise_rms = rms(&clip.pcm).max(1.0);
    let gain = reference_rms / (noise_rms * 10f64.powf(f64::from(snr_db) / 20.0));
    let n = clip.pcm.len().max(1);
    (0..len)
        .map(|i| {
            let s = clip.pcm.get(i % n).copied().unwrap_or(0);
            (f64::from(s) * gain).round().clamp(-32_768.0, 32_767.0) as i16
        })
        .collect()
}

/// Resamples a whole signal (chunks of 20 ms, the filter delay removed, the length kept).
fn resample_all(from: SampleRate, to: SampleRate, pcm: &[i16]) -> Result<Vec<i16>, BenchError> {
    let mut r = StreamResampler::new(from, to).map_err(audio_error)?;
    let target = pcm.len() * to.hz() as usize / from.hz() as usize;
    let delay = r.filter_delay_frames();
    let mut out = Vec::with_capacity(target + 2 * delay + 1024);
    let mut scratch = vec![0i16; r.max_output_len(pcm.len() + 2048)];
    let chunk = from.hz() as usize / 50;
    let silence = vec![0i16; chunk];
    for piece in pcm
        .chunks(chunk)
        .chain(std::iter::repeat_n(silence.as_slice(), 6))
    {
        let n = r.process(piece, &mut scratch).map_err(audio_error)?;
        out.extend_from_slice(scratch.get(..n).unwrap_or_default());
    }
    Ok(out.get(delay..delay + target).unwrap_or(&out).to_vec())
}

/// The telephone path: 8 kHz, G.711 mu-law, and back up to the 16 kHz the engine works at.
///
/// # Errors
/// [`BenchError`] if the resamplers fail.
pub fn telephone(pcm: &[i16]) -> Result<Vec<i16>, BenchError> {
    let narrow = resample_all(SampleRate::Hz16000, SampleRate::Hz8000, pcm)?;
    let companded: Vec<i16> = narrow
        .iter()
        .map(|s| ulaw_decode(ulaw_encode(*s)))
        .collect();
    resample_all(SampleRate::Hz8000, SampleRate::Hz16000, &companded)
}

/// How the audio reaches the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Path16 {
    /// 16 kHz as recorded.
    Wideband,
    /// Through 8 kHz mu-law, as on a phone call.
    Telephone,
}

impl Path16 {
    /// Name in reports.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Wideband => "wideband",
            Self::Telephone => "telephone",
        }
    }

    /// The sample rate the transport delivers on this path.
    pub const fn call_hz(self) -> u32 {
        match self {
            Self::Wideband => 16_000,
            Self::Telephone => 8_000,
        }
    }

    /// `pcm` as the engine receives it on this path.
    ///
    /// # Errors
    /// [`BenchError`] if a resampler fails.
    pub fn apply(self, pcm: &[i16]) -> Result<Vec<i16>, BenchError> {
        match self {
            Self::Wideband => Ok(pcm.to_vec()),
            Self::Telephone => telephone(pcm),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Backends

/// A denoiser backend to test, or none.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Variant {
    /// The audio as it is.
    None,
    /// RNNoise.
    Rnnoise,
    /// Hush (`hush-vani`).
    Hush,
    /// FastEnhancer from this ONNX file (evaluation only).
    FastEnhancer(PathBuf),
}

impl Variant {
    /// Name in reports.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Rnnoise => "rnnoise",
            Self::Hush => "hush",
            Self::FastEnhancer(_) => "fastenhancer-t",
        }
    }

    /// A fresh denoiser for 16 kHz audio (`None` for [`Variant::None`]).
    ///
    /// # Errors
    /// [`BenchError`] when the backend is not compiled in or its model cannot be loaded.
    pub fn build(&self, rate_hz: u32) -> Result<Option<BoxedDenoiser>, BenchError> {
        let (backend, model) = match self {
            Self::None => return Ok(None),
            Self::Rnnoise => (Backend::Rnnoise, None),
            Self::Hush => (Backend::Hush, None),
            Self::FastEnhancer(path) => (Backend::FastEnhancer, Some(path.as_path())),
        };
        denoise::build(backend, rate_hz, model)
            .map(Some)
            .map_err(model_error)
    }
}

impl Variant {
    /// The lowest call sample rate the engine uses the backend at (0: any); the server sets the
    /// same minimum when it builds the setup (`DenoiseSetup::with_min_call_rate`).
    pub const fn min_call_hz(&self) -> u32 {
        match self {
            Self::Hush => 16_000,
            _ => 0,
        }
    }

    /// [`Self::build`] with `dry` of the input blended back into the output (0: none).
    ///
    /// # Errors
    /// As [`Self::build`].
    pub fn build_blended(
        &self,
        rate_hz: u32,
        dry: f32,
    ) -> Result<Option<BoxedDenoiser>, BenchError> {
        Ok(self.build(rate_hz)?.map(|d| {
            if dry > 0.0 {
                Box::new(denoise::blend::Blended::new(d, dry)) as BoxedDenoiser
            } else {
                d
            }
        }))
    }
}

/// Runs `pcm` through a denoiser and returns the enhanced audio aligned with the input (the
/// fade-in dropped, the tail flushed with silence, the length kept).
///
/// # Errors
/// [`BenchError`] when the denoiser fails.
pub fn enhance(d: &mut dyn denoise::Denoiser, pcm: &[i16]) -> Result<Vec<i16>, BenchError> {
    let rate = u64::from(d.sample_rate());
    let preroll =
        usize::try_from(d.delay().as_micros() * u128::from(rate) / 1_000_000).unwrap_or(0);
    let mut out = Vec::with_capacity(pcm.len() + preroll + 4096);
    let silence = vec![0i16; FRAME];
    let tail = std::iter::repeat_n(silence.as_slice(), 25);
    for piece in pcm.chunks(FRAME).chain(tail) {
        d.process(piece, &mut out).map_err(model_error)?;
    }
    Ok(out
        .get(preroll..preroll + pcm.len())
        .unwrap_or(&out)
        .to_vec())
}

// ---------------------------------------------------------------------------------------------
// CPU

/// One row of the CPU table.
#[derive(Debug, Clone)]
pub struct CpuRow {
    /// Backend.
    pub variant: &'static str,
    /// Rate of the audio the call carries.
    pub input_hz: u32,
    /// Algorithmic delay the backend adds.
    pub delay: Duration,
    /// Time to build one denoiser (the first VAD batch of a call pays it, on the pool).
    pub build: Duration,
    /// Wall time of one `process` call (20 ms of audio), single thread.
    pub per_frame: Summary,
    /// Process CPU time per second of audio, in percent of one core.
    pub cpu_percent: f64,
    /// Seconds of audio measured.
    pub audio_s: f64,
}

/// Measures the CPU of each backend on `seconds` of the fixtures' speech (cycled), at 16 kHz and
/// at 8 kHz.
///
/// # Errors
/// [`BenchError`] when a backend cannot be built.
pub fn cpu(
    variants: &[Variant],
    speech: &[Speech],
    seconds: usize,
) -> Result<Vec<CpuRow>, BenchError> {
    let mut wide: Vec<i16> = Vec::new();
    while wide.len() < seconds * 16_000 {
        for s in speech {
            wide.extend_from_slice(&s.pcm);
        }
    }
    wide.truncate(seconds * 16_000);
    let narrow = resample_all(SampleRate::Hz16000, SampleRate::Hz8000, &wide)?;
    let clock = SystemClock;
    let mut probe = ProcessProbe::new();
    let mut rows = Vec::new();
    for variant in variants {
        for (hz, audio) in [(16_000u32, &wide), (8_000, &narrow)] {
            let built = clock.now();
            let Some(mut d) = variant.build(hz)? else {
                continue;
            };
            let build = clock.now().saturating_duration_since(built);
            let chunk = hz as usize / 50;
            let mut out = Vec::with_capacity(chunk * 4);
            // Warm-up: first allocations, caches, the ONNX session's first run.
            for piece in audio.chunks(chunk).take(250) {
                out.clear();
                d.process(piece, &mut out).map_err(model_error)?;
            }
            d.reset();
            let mut times = Vec::with_capacity(audio.len() / chunk + 1);
            let cpu_before = probe.read().map(|r| r.cpu_time);
            let wall_before = clock.now();
            for piece in audio.chunks(chunk) {
                out.clear();
                let t0 = clock.now();
                d.process(piece, &mut out).map_err(model_error)?;
                times.push(clock.now().saturating_duration_since(t0));
            }
            let wall = clock.now().saturating_duration_since(wall_before);
            let cpu_after = probe.read().map(|r| r.cpu_time);
            let audio_s = audio.len() as f64 / f64::from(hz);
            let cpu_s = match (cpu_before, cpu_after) {
                (Some(a), Some(b)) => b.saturating_sub(a).as_secs_f64(),
                _ => wall.as_secs_f64(),
            };
            rows.push(CpuRow {
                variant: variant.name(),
                input_hz: hz,
                delay: d.delay(),
                build,
                per_frame: Summary::of(times),
                cpu_percent: 100.0 * cpu_s / audio_s,
                audio_s,
            });
        }
    }
    Ok(rows)
}

/// The CPU table as markdown.
pub fn cpu_markdown(rows: &[CpuRow], environment: &[String]) -> String {
    let mut out = String::from("# Noise suppression: CPU per stream\n\n");
    for line in environment {
        let _ = writeln!(out, "- {line}");
    }
    out.push_str(
        "\nOne call, one thread, 20 ms frames of the speech fixtures, steady state (250 frames of \
         warm-up, then the model state reset). `per frame` is the wall time of one `process` \
         call; `CPU` is the process' CPU time over the run per second of audio, in percent of one \
         core (it includes the harness, which does nothing else). `delay` is the algorithmic delay \
         the backend adds to the audio the VAD and the turn detector hear.\n\n\
         | backend | call rate | delay (ms) | build (ms) | per frame p50 (ms) | p95 | p99 | max | CPU, % of one core | 100 calls need |\n\
         |---|---|---|---|---|---|---|---|---|---|\n",
    );
    for r in rows {
        let _ = writeln!(
            out,
            "| {} | {} Hz | {} | {} | {} | {} | {} | {} | {:.2} | {:.1} cores |",
            r.variant,
            r.input_hz,
            ms(r.delay),
            ms(r.build),
            ms(r.per_frame.p50),
            ms(r.per_frame.p95),
            ms(r.per_frame.p99),
            ms(r.per_frame.max),
            r.cpu_percent,
            r.cpu_percent,
        );
    }
    out
}

// ---------------------------------------------------------------------------------------------
// False barge-ins and Smart Turn

/// Counters of one cell (backend, path, SNR).
#[derive(Debug, Clone, Default)]
pub struct Cell {
    /// Utterances in the mixes.
    pub utterances: u32,
    /// Utterances with a speech start within the onset window.
    pub detected: u32,
    /// Extra starts inside an utterance (the VAD lost speech and found it again).
    pub fragments: u32,
    /// Starts outside every utterance, in the mixes.
    pub false_starts: u32,
    /// Seconds of the mixes outside the utterances (their onset and hangover windows excluded).
    pub quiet_s: f64,
    /// Sum of (first start - utterance start) in ms over detected utterances.
    pub onset_error_ms: f64,
    /// Starts in the noise alone.
    pub noise_starts: u32,
    /// Seconds of noise alone.
    pub noise_s: f64,
    /// Smart Turn analyses (one per utterance end).
    pub verdicts: u32,
    /// Verdicts that differ from the verdict on the clean audio of the same path.
    pub flips: u32,
    /// Sum of |probability - probability on the clean audio|.
    pub delta_p: f64,
    /// Verdicts of the clean audio that were "complete".
    pub clean_complete: u32,
    /// Verdicts that were "complete" in this cell.
    pub complete: u32,
}

/// What to run.
pub struct EvalConfig {
    /// Variants to compare; the first should be [`Variant::None`].
    pub variants: Vec<Variant>,
    /// Paths.
    pub paths: Vec<Path16>,
    /// Signal-to-noise ratios in dB.
    pub snrs: Vec<f32>,
    /// Speech.
    pub speech: Vec<Speech>,
    /// Noise.
    pub noise: Vec<NoiseClip>,
    /// The Smart Turn model.
    pub turn: Arc<dyn TurnModel>,
    /// Seconds of each noise recording run alone (looped when shorter).
    pub noise_alone_s: usize,
}

/// The results, by (variant, path, snr).
pub struct EvalReport {
    /// Cells keyed by (variant name, path, SNR in tenths of a dB).
    pub cells: BTreeMap<(&'static str, Path16, i32), Cell>,
    /// Clean-audio figures per path: (utterances, verdicts "complete", missed starts, false starts).
    pub clean: BTreeMap<(&'static str, Path16), Cell>,
    /// Cells counted per variant (runs of speech x noise).
    pub runs_per_cell: usize,
    /// Seconds of each noise recording run alone.
    pub noise_alone_s: usize,
    /// Where the noise came from, for the report (a path or a name).
    pub noise_source: String,
    /// The variants in order.
    pub variants: Vec<&'static str>,
    /// Paths in order.
    pub paths: Vec<Path16>,
    /// SNRs in order.
    pub snrs: Vec<f32>,
    /// Noise clips used.
    pub noise_names: Vec<String>,
    /// Fixtures used.
    pub speech_names: Vec<String>,
    /// The delay each variant adds.
    pub delays: BTreeMap<&'static str, Duration>,
}

#[allow(clippy::cast_possible_truncation)]
fn snr_key(snr: f32) -> i32 {
    (snr * 10.0).round() as i32
}

/// Speech start and stop events of `pcm` through the production VAD stream, times from the start
/// of the audio.
fn vad_events(
    pcm: &[i16],
    silero: &Arc<SileroTractModel>,
    variant: &Variant,
) -> Result<Vec<(Duration, bool)>, BenchError> {
    let mut stream = VadStream::new(silero.stream().map_err(model_error)?, VadParams::default());
    if let Some(d) = variant.build(16_000)? {
        stream = stream.with_denoiser(d).map_err(model_error)?;
    }
    let origin = SystemClock.now();
    let mut events = Vec::new();
    for (i, frame) in pcm.chunks(FRAME).enumerate() {
        let at = origin + Duration::from_millis(20) * u32::try_from(i).unwrap_or(u32::MAX);
        stream.push(frame, at, &mut events).map_err(model_error)?;
    }
    events.extend(stream.finish());
    Ok(events
        .into_iter()
        .map(|e| match e {
            VadEvent::Start { at } => (at.saturating_duration_since(origin), true),
            VadEvent::Stop { at } => (at.saturating_duration_since(origin), false),
        })
        .collect())
}

pub(crate) fn samples_to_duration(samples: usize) -> Duration {
    Duration::from_micros(u64::try_from(samples).unwrap_or(0) * 1_000_000 / 16_000)
}

/// Scores the starts of one run against the utterances.
fn score_starts(cell: &mut Cell, starts: &[Duration], segments: &[(usize, usize)], total: usize) {
    let windows: Vec<(Duration, Duration, Duration)> = segments
        .iter()
        .map(|(a, b)| {
            let (a, b) = (samples_to_duration(*a), samples_to_duration(*b));
            (a, a.saturating_sub(ONSET_BEFORE), b + ONSET_AFTER)
        })
        .collect();
    let mut claimed = vec![0u32; windows.len()];
    for start in starts {
        match windows
            .iter()
            .position(|(_, from, to)| start >= from && start <= to)
        {
            Some(i) => {
                if let Some(count) = claimed.get_mut(i) {
                    *count += 1;
                    if *count == 1
                        && let Some((onset, _, _)) = windows.get(i)
                    {
                        #[allow(clippy::cast_precision_loss)]
                        let error = start.as_secs_f64() * 1e3 - onset.as_secs_f64() * 1e3;
                        cell.onset_error_ms += error;
                    }
                }
            }
            None => cell.false_starts += 1,
        }
    }
    cell.utterances += u32::try_from(windows.len()).unwrap_or(0);
    cell.detected += u32::try_from(claimed.iter().filter(|c| **c > 0).count()).unwrap_or(0);
    cell.fragments += claimed.iter().map(|c| c.saturating_sub(1)).sum::<u32>();
    let covered: Duration = windows.iter().map(|(_, from, to)| *to - *from).sum();
    cell.quiet_s += (samples_to_duration(total).saturating_sub(covered)).as_secs_f64();
}

/// Runs the whole evaluation.
///
/// # Errors
/// [`BenchError`] when a model or a mix fails.
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
pub fn evaluate(
    config: &EvalConfig,
    silero: &Arc<SileroTractModel>,
) -> Result<EvalReport, BenchError> {
    let mut report = EvalReport {
        cells: BTreeMap::new(),
        clean: BTreeMap::new(),
        runs_per_cell: config.speech.len() * config.noise.len(),
        noise_alone_s: config.noise_alone_s,
        noise_source: "fixtures/noise".to_owned(),
        variants: config.variants.iter().map(Variant::name).collect(),
        paths: config.paths.clone(),
        snrs: config.snrs.clone(),
        noise_names: config.noise.iter().map(|n| n.name.clone()).collect(),
        speech_names: config.speech.iter().map(|s| s.name.clone()).collect(),
        delays: BTreeMap::new(),
    };
    for variant in &config.variants {
        if let Some(d) = variant.build(16_000)? {
            report.delays.insert(variant.name(), d.delay());
        }
    }
    let reference_rms =
        config.speech.iter().map(|s| s.speech_rms).sum::<f64>() / config.speech.len().max(1) as f64;

    for path in &config.paths {
        // The clean audio of each fixture on this path: the verdicts everything is compared to.
        let mut clean_p: Vec<Vec<f32>> = Vec::new();
        for speech in &config.speech {
            let audio = path.apply(&speech.pcm)?;
            clean_p.push(verdicts(&audio, &speech.segments, config.turn.as_ref())?);
        }
        for variant in &config.variants {
            // Clean speech, as a control for the cost of the denoiser on audio without noise.
            let mut control = Cell::default();
            for (speech, clean) in config.speech.iter().zip(&clean_p) {
                let audio = path.apply(&speech.pcm)?;
                let events = vad_events(&audio, silero, variant)?;
                let starts: Vec<Duration> = events.iter().filter(|e| e.1).map(|e| e.0).collect();
                score_starts(&mut control, &starts, &speech.segments, audio.len());
                let heard = heard_by_turn_detector(&audio, variant)?;
                let p = verdicts(&heard, &speech.segments, config.turn.as_ref())?;
                tally_verdicts(&mut control, &p, clean);
            }
            report.clean.insert((variant.name(), *path), control);
        }
        for &snr in &config.snrs {
            for variant in &config.variants {
                let mut cell = Cell::default();
                for (si, (speech, clean)) in config.speech.iter().zip(&clean_p).enumerate() {
                    for (ni, clip) in config.noise.iter().enumerate() {
                        let offset = (si * 3 + ni) * 12_345;
                        let mixed = path.apply(&mix(speech, Some((clip, offset)), snr))?;
                        let events = vad_events(&mixed, silero, variant)?;
                        let starts: Vec<Duration> =
                            events.iter().filter(|e| e.1).map(|e| e.0).collect();
                        score_starts(&mut cell, &starts, &speech.segments, mixed.len());
                        let heard = heard_by_turn_detector(&mixed, variant)?;
                        let p = verdicts(&heard, &speech.segments, config.turn.as_ref())?;
                        tally_verdicts(&mut cell, &p, clean);
                    }
                }
                // The noise alone, at the level it has in the mixes.
                for clip in &config.noise {
                    let alone = path.apply(&noise_alone(
                        clip,
                        reference_rms,
                        snr,
                        config.noise_alone_s * 16_000,
                    ))?;
                    let events = vad_events(&alone, silero, variant)?;
                    cell.noise_starts +=
                        u32::try_from(events.iter().filter(|e| e.1).count()).unwrap_or(u32::MAX);
                    cell.noise_s += alone.len() as f64 / 16_000.0;
                }
                report
                    .cells
                    .insert((variant.name(), *path, snr_key(snr)), cell);
            }
        }
    }
    Ok(report)
}

/// What the turn detector hears: the audio through the variant's denoiser (aligned).
fn heard_by_turn_detector(pcm: &[i16], variant: &Variant) -> Result<Vec<i16>, BenchError> {
    match variant.build(16_000)? {
        None => Ok(pcm.to_vec()),
        Some(mut d) => enhance(d.as_mut(), pcm),
    }
}

/// Smart Turn probabilities at the end of each utterance (+ the VAD hangover).
pub(crate) fn verdicts(
    audio: &[i16],
    segments: &[(usize, usize)],
    model: &dyn TurnModel,
) -> Result<Vec<f32>, BenchError> {
    segments
        .iter()
        .map(|(_, end)| {
            let upto =
                (end + (SMART_TURN_AFTER_END.as_micros() as usize * 16 / 1000)).min(audio.len());
            let from = upto.saturating_sub(8 * 16_000);
            let snapshot = audio.get(from..upto).unwrap_or_default();
            SmartTurn::analyze_snapshot_blocking(model, snapshot).map_err(model_error)
        })
        .collect()
}

fn tally_verdicts(cell: &mut Cell, p: &[f32], clean: &[f32]) {
    for (p, clean) in p.iter().zip(clean) {
        cell.verdicts += 1;
        cell.delta_p += f64::from((p - clean).abs());
        cell.flips += u32::from((*p > THRESHOLD) != (*clean > THRESHOLD));
        cell.clean_complete += u32::from(*clean > THRESHOLD);
        cell.complete += u32::from(*p > THRESHOLD);
    }
}

/// The evaluation as markdown.
pub fn eval_markdown(report: &EvalReport, environment: &[String]) -> String {
    let mut out = String::from("# Noise suppression: false barge-ins and turn verdicts\n\n");
    for line in environment {
        let _ = writeln!(out, "- {line}");
    }
    let _ = write!(
        out,
        "\nSpeech: {} ({} utterances in all). Noise: {} recordings from `{}` (MUSAN \
         free-sound, public domain). Every speech fixture is mixed with every noise recording at \
         each signal-to-noise ratio: {} mixes per cell, wideband and through the telephone path (8 kHz \
         mu-law, back to 16 kHz). The VAD is the engine's Silero stream with the production \
         parameters, fed 20 ms frames, with the backend in front of it exactly as the engine's VAD \
         worker runs it; the turn detector is Smart Turn v3.2 on the last 8 s of the enhanced audio, \
         {} ms after the end of each utterance.\n\n",
        report.speech_names.join(", "),
        report.clean.values().next().map_or(0, |c| c.utterances),
        report.noise_names.len(),
        report.noise_source,
        report.runs_per_cell,
        SMART_TURN_AFTER_END.as_millis(),
    );
    let _ = write!(
        out,
        "Terms. *False start*: a speech start outside every utterance (from 300 ms before its \
         first sample to 300 ms after its last): while the bot talks, this is a false barge-in. \
         *Missed*: an utterance with no start in that window. *Fragment*: a second start inside \
         one utterance. *Flip*: a Smart Turn verdict that differs from the verdict on the clean \
         audio of the same path; |dp| is the mean change of its probability. *Noise alone*: the \
         recordings alone ({} s of each, looped) at the level they have in the mix, starts per \
         minute.\n\n",
        report.noise_alone_s,
    );
    for path in &report.paths {
        let _ = writeln!(out, "## {}\n", path.name());
        out.push_str(
            "| SNR | backend | false starts in mixes | per quiet minute | missed | fragments | onset error | noise alone: starts/min | flips | mean abs dp |\n\
             |---|---|---|---|---|---|---|---|---|---|\n",
        );
        for variant in &report.variants {
            if let Some(c) = report.clean.get(&(*variant, *path)) {
                out.push_str(&row("clean", variant, c));
            }
        }
        for snr in &report.snrs {
            for variant in &report.variants {
                if let Some(c) = report.cells.get(&(*variant, *path, snr_key(*snr))) {
                    out.push_str(&row(&format!("{snr} dB"), variant, c));
                }
            }
        }
        out.push('\n');
    }
    out
}

fn row(snr: &str, variant: &str, c: &Cell) -> String {
    let per_min = |n: u32, s: f64| {
        if s > 0.0 {
            f64::from(n) * 60.0 / s
        } else {
            0.0
        }
    };
    let missed = c.utterances.saturating_sub(c.detected);
    let onset = if c.detected > 0 {
        format!("{:+.0} ms", c.onset_error_ms / f64::from(c.detected))
    } else {
        "n/a".to_owned()
    };
    let verdict = if c.verdicts > 0 {
        format!("{} of {}", c.flips, c.verdicts)
    } else {
        "n/a".to_owned()
    };
    format!(
        "| {snr} | {variant} | {} | {:.1} | {}/{} | {} | {onset} | {:.1} | {verdict} | {:.3} |\n",
        c.false_starts,
        per_min(c.false_starts, c.quiet_s),
        missed,
        c.utterances,
        c.fragments,
        per_min(c.noise_starts, c.noise_s),
        if c.verdicts > 0 {
            c.delta_p / f64::from(c.verdicts)
        } else {
            0.0
        },
    )
}
