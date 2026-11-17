//! Realistic barge-in calls (feature `denoise`): the sources of `brisvox-bench denoise realistic`.
//!
//! [`crate::denoise_calls`] mixes a dry close-microphone clip into the caller's line at a gain and
//! calls it a television. This module builds calls where the other sound on the line is more
//! varied, and where the pairing and the speech recognizer are less ideal:
//!
//! - **Second voices** ([`Interferer`]): LibriSpeech clips of 18 speakers, played "through a
//!   loudspeaker" (a band-limited response with a mid resonance and some saturation) and a measured
//!   room impulse response ([`Rir`], OpenSLR 28), both applied at run time to the committed clips
//!   and impulse responses; real far-field loudspeaker recordings (VOiCES); Turkish broadcast
//!   speech (MediaSpeech) through the same loudspeaker and rooms; and the old dry close-microphone
//!   clip for comparison.
//! - **Noise** ([`Kind::NOISES`]): the real MUSAN recordings of `fixtures/noise`, synthetic events
//!   (a cough, keystrokes, a door, dishes), synthetic music and street rumble
//!   ([`crate::noise_synth`]) and babble of several voices, at levels from very light to loud
//!   against the caller's speech. Noise has no words (the recognizer can be made to invent one).
//! - **Pairing** ([`scenarios`]): many distinct caller x second voice pairs, balanced between the
//!   same sex and the other sex where the sex is known.
//! - **Recognizer** ([`Recognizer`]): the oracle of the earlier measurements, or a model that
//!   leaves windows of a source unheard at assumed rates (a seed makes the draws repeat).
//! - **First turn** ([`FirstTurn`]): a clean first turn, a short one ("Alo"), and one with a
//!   television running behind it.
//! - **Barge-ins**: a whole sentence, a 0.9 s command, and a 0.5 s command word ("stop", "wait",
//!   "dur", "bekle") whose transcript is the word.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use realfft::RealFftPlanner;
use realfft::num_complex::Complex;
use serde::Deserialize;

use crate::BenchError;
use crate::denoise_calls::{
    Assembled, BARGE_AFTER_BOT, BOT_AFTER_FIRST, FIRST_START, HZ, QUIET_AFTER_BOT, TV_AFTER_BOT,
    Word, model_error, place, spread,
};
use crate::denoise_eval::{NoiseClip, Path16, Speech, load_speech, read_wav, rms};
use crate::noise_synth::{Sound, active_rms, synthesize};

// ---------------------------------------------------------------------------------------------
// Randomness

/// A small deterministic generator (splitmix64): the same seed gives the same numbers on every
/// platform.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator from `seed`.
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// A generator for the stream `parts` of `seed` (a scenario, a purpose).
    pub fn derive(seed: u64, parts: &[u64]) -> Self {
        let mut r = Self(seed ^ 0x9E37_79B9_7F4A_7C15);
        for p in parts {
            r.0 ^= p.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            let _ = r.next_u64();
        }
        r
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    #[allow(clippy::cast_precision_loss)]
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `[a, b)`.
    pub fn range(&mut self, a: f64, b: f64) -> f64 {
        a + (b - a) * self.unit()
    }
}

// ---------------------------------------------------------------------------------------------
// Sources

/// A speaker's sex, where it is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Sex {
    /// Female.
    Female,
    /// Male.
    Male,
}

impl Sex {
    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "F" | "f" => Some(Self::Female),
            "M" | "m" => Some(Self::Male),
            _ => None,
        }
    }
}

/// How a clip was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    /// A close-microphone recording (LibriSpeech).
    Dry,
    /// A real loudspeaker in a real room recorded from across it (VOiCES).
    FarField,
    /// Turkish broadcast speech (MediaSpeech), close recording.
    Turkish,
}

/// A clip of another speaker with its transcript.
#[derive(Debug, Clone)]
pub struct Interferer {
    /// File stem.
    pub name: String,
    /// The speaker (the corpus id).
    pub speaker: String,
    /// Sex, if known.
    pub sex: Option<Sex>,
    /// How it was recorded.
    pub source: Source,
    /// 16 kHz samples.
    pub pcm: Vec<i16>,
    /// Transcript, or placeholder tokens when the clip has none.
    pub text: String,
}

#[derive(Deserialize)]
struct ClipJson {
    file: String,
    #[serde(default)]
    speaker: String,
    #[serde(default)]
    sex: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    words: usize,
    #[serde(default)]
    kind: String,
}

/// Loads the clips listed in `dir/clips.json`.
///
/// # Errors
/// [`BenchError`] when the list or a clip cannot be read, or a clip is not 16 kHz.
pub fn load_interferers(dir: &Path) -> Result<Vec<Interferer>, BenchError> {
    let list: Vec<ClipJson> =
        serde_json::from_str(&std::fs::read_to_string(dir.join("clips.json"))?)
            .map_err(model_error)?;
    list.into_iter()
        .map(|c| {
            let (rate, pcm) = read_wav(&dir.join(&c.file))?;
            if rate != 16_000 {
                return Err(model_error(format!("{}: {rate} Hz", c.file)));
            }
            let source = match c.kind.as_str() {
                "farfield" => Source::FarField,
                "turkish" => Source::Turkish,
                _ => Source::Dry,
            };
            let text = if c.text.trim().is_empty() {
                vec!["~"; c.words.max(1)].join(" ")
            } else {
                c.text
            };
            Ok(Interferer {
                name: c.file.trim_end_matches(".wav").to_owned(),
                speaker: c.speaker,
                sex: Sex::parse(&c.sex),
                source,
                pcm,
                text,
            })
        })
        .collect()
}

/// A caller: two utterances of one voice.
#[derive(Debug, Clone)]
pub struct Caller {
    /// The fixture.
    pub speech: Speech,
    /// The speaker id (the fixture name without its prefix).
    pub speaker: String,
    /// Sex, if known.
    pub sex: Option<Sex>,
}

/// Loads the caller fixtures `names` from `dir` (`callers/libri_1993`, `tr_two_utterances`) with
/// the sexes of `dir/callers/sex.json`.
///
/// # Errors
/// [`BenchError`] when a fixture cannot be read.
pub fn load_callers(dir: &Path, names: &[String]) -> Result<Vec<Caller>, BenchError> {
    let sexes: HashMap<String, String> = std::fs::read_to_string(dir.join("callers/sex.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let speech = load_speech(Some(dir), names)?;
    Ok(speech
        .into_iter()
        .map(|speech| {
            let speaker = speech
                .name
                .rsplit('/')
                .next()
                .unwrap_or(&speech.name)
                .trim_start_matches("libri_")
                .to_owned();
            let sex = sexes.get(&speaker).and_then(|s| Sex::parse(s));
            Caller {
                speech,
                speaker,
                sex,
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// Loudspeaker and room

/// A measured room impulse response.
#[derive(Debug, Clone)]
pub struct Rir {
    /// Room name.
    pub name: String,
    /// Reverberation time of the whole response in seconds.
    pub rt60: f32,
    taps: Vec<f32>,
}

impl Rir {
    /// An impulse response from its taps (tests).
    pub fn from_taps(name: &str, taps: Vec<f32>) -> Self {
        Self {
            name: name.to_owned(),
            rt60: 0.0,
            taps,
        }
    }
}

#[derive(Deserialize)]
struct RirJson {
    file: String,
    room: String,
    rt60_s: f32,
}

/// Loads the impulse responses listed in `dir/rirs.json`.
///
/// # Errors
/// [`BenchError`] when the list or a response cannot be read.
pub fn load_rirs(dir: &Path) -> Result<Vec<Rir>, BenchError> {
    let list: Vec<RirJson> = serde_json::from_str(&std::fs::read_to_string(dir.join("rirs.json"))?)
        .map_err(model_error)?;
    list.into_iter()
        .map(|r| {
            let (rate, pcm) = read_wav(&dir.join(&r.file))?;
            if rate != 16_000 {
                return Err(model_error(format!("{}: {rate} Hz", r.file)));
            }
            Ok(Rir {
                name: r.room,
                rt60: r.rt60_s,
                taps: pcm.iter().map(|s| f32::from(*s) / 32_768.0).collect(),
            })
        })
        .collect()
}

/// A second-order section (direct form I).
#[derive(Debug, Clone, Copy)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
}

impl Biquad {
    fn from_raw(b: [f64; 3], a: [f64; 3]) -> Self {
        let a0 = if a[0].abs() < 1e-12 { 1.0 } else { a[0] };
        Self {
            b: [b[0] / a0, b[1] / a0, b[2] / a0],
            a: [a[1] / a0, a[2] / a0],
        }
    }

    fn rbj(kind: u8, f: f64, q: f64, gain_db: f64) -> Self {
        let w0 = 2.0 * std::f64::consts::PI * f / HZ;
        let (s, c) = (w0.sin(), w0.cos());
        let alpha = s / (2.0 * q);
        match kind {
            0 => Self::from_raw(
                [(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0],
                [1.0 + alpha, -2.0 * c, 1.0 - alpha],
            ),
            1 => Self::from_raw(
                [(1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0],
                [1.0 + alpha, -2.0 * c, 1.0 - alpha],
            ),
            _ => {
                let a = 10f64.powf(gain_db / 40.0);
                Self::from_raw(
                    [1.0 + alpha * a, -2.0 * c, 1.0 - alpha * a],
                    [1.0 + alpha / a, -2.0 * c, 1.0 - alpha / a],
                )
            }
        }
    }

    fn run(&self, x: &mut [f64]) {
        let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
        for v in x.iter_mut() {
            let y =
                self.b[0] * *v + self.b[1] * x1 + self.b[2] * x2 - self.a[0] * y1 - self.a[1] * y2;
            x2 = x1;
            x1 = *v;
            y2 = y1;
            y1 = y;
            *v = y;
        }
    }
}

/// The response of a small loudspeaker or a television: a high-pass at 90 to 350 Hz, a low-pass at
/// 4.5 to 7.6 kHz (one or two sections), a resonance in the mids and, in four cases of ten, a
/// soft saturation. The parameters come from `rng`.
pub fn loudspeaker(x: &mut [f64], rng: &mut Rng) {
    Biquad::rbj(
        0,
        rng.range(90.0, 350.0),
        std::f64::consts::FRAC_1_SQRT_2,
        0.0,
    )
    .run(x);
    let lp = rng.range(4_500.0, 7_600.0);
    Biquad::rbj(1, lp, std::f64::consts::FRAC_1_SQRT_2, 0.0).run(x);
    if rng.unit() < 0.5 {
        Biquad::rbj(1, lp, std::f64::consts::FRAC_1_SQRT_2, 0.0).run(x);
    }
    Biquad::rbj(
        2,
        rng.range(1_200.0, 3_500.0),
        rng.range(0.8, 2.0),
        rng.range(-3.0, 6.0),
    )
    .run(x);
    if rng.unit() < 0.4 {
        let peak = x.iter().fold(1e-9f64, |m, v| m.max(v.abs()));
        let d = rng.range(1.5, 3.0);
        for v in x.iter_mut() {
            *v = (d * *v / peak).tanh() * peak / d.tanh();
        }
    }
}

/// `x` convolved with `h`, the first `out_len` samples (FFT).
///
/// # Errors
/// [`BenchError`] when the transform fails.
pub fn convolve(x: &[f32], h: &[f32], out_len: usize) -> Result<Vec<f32>, BenchError> {
    if x.is_empty() || h.is_empty() {
        return Ok(vec![0.0; out_len]);
    }
    let n = (x.len() + h.len()).next_power_of_two();
    let mut planner = RealFftPlanner::<f32>::new();
    let (fwd, inv) = (planner.plan_fft_forward(n), planner.plan_fft_inverse(n));
    let mut a = vec![0.0f32; n];
    let mut b = vec![0.0f32; n];
    a.iter_mut().zip(x).for_each(|(d, s)| *d = *s);
    b.iter_mut().zip(h).for_each(|(d, s)| *d = *s);
    let mut fa: Vec<Complex<f32>> = fwd.make_output_vec();
    let mut fb: Vec<Complex<f32>> = fwd.make_output_vec();
    fwd.process(&mut a, &mut fa).map_err(model_error)?;
    fwd.process(&mut b, &mut fb).map_err(model_error)?;
    for (p, q) in fa.iter_mut().zip(&fb) {
        *p *= *q;
    }
    let mut out = vec![0.0f32; n];
    inv.process(&mut fa, &mut out).map_err(model_error)?;
    #[allow(clippy::cast_precision_loss)]
    let scale = 1.0 / n as f32;
    out.truncate(out_len);
    out.resize(out_len, 0.0);
    out.iter_mut().for_each(|v| *v *= scale);
    Ok(out)
}

/// `clip` played through a loudspeaker (parameters from `rng`) in the room of `rir`, with the
/// reverberation tail kept (up to 0.4 s beyond the clip).
///
/// # Errors
/// [`BenchError`] when the transform fails.
pub fn through_loudspeaker_and_room(
    clip: &[i16],
    rir: &Rir,
    rng: &mut Rng,
) -> Result<Vec<i16>, BenchError> {
    let mut x: Vec<f64> = clip.iter().map(|s| f64::from(*s) / 32_768.0).collect();
    loudspeaker(&mut x, rng);
    #[allow(clippy::cast_possible_truncation)]
    let xf: Vec<f32> = x.iter().map(|v| *v as f32).collect();
    let out_len = clip.len() + (0.4 * HZ) as usize;
    let y = convolve(&xf, &rir.taps, out_len)?;
    let peak = y.iter().fold(1e-9f32, |m, v| m.max(v.abs()));
    Ok(y.iter()
        .map(|v| {
            #[allow(clippy::cast_possible_truncation)]
            {
                (v / peak * 29_000.0).round() as i16
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// Recognizer

/// What else is on the line besides the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// Nothing: the caller on a clean line.
    None,
    /// A dry close-microphone clip (the old "television").
    Dry,
    /// A clip through a loudspeaker and a measured room.
    Room,
    /// A real loudspeaker recording from across a room (VOiCES).
    FarField,
    /// Turkish broadcast speech through a loudspeaker and a room.
    Turkish,
    /// Real noise recordings (MUSAN, `fixtures/noise`).
    Musan,
    /// A cough (synthetic).
    Cough,
    /// Keystrokes (synthetic).
    Keyboard,
    /// A door (synthetic).
    Door,
    /// Dishes (synthetic).
    Dishes,
    /// Music (synthetic).
    Music,
    /// A street: rumble and a passing vehicle (synthetic).
    Street,
    /// Several dry voices at once, none of them standing out.
    Babble,
}

impl Kind {
    /// Name in reports.
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "clean line",
            Self::Dry => "dry second voice",
            Self::Room => "loudspeaker in a room",
            Self::FarField => "far-field recording",
            Self::Turkish => "Turkish broadcast",
            Self::Musan => "real noise (MUSAN)",
            Self::Cough => "cough",
            Self::Keyboard => "keyboard",
            Self::Door => "door",
            Self::Dishes => "dishes",
            Self::Music => "music",
            Self::Street => "street",
            Self::Babble => "babble",
        }
    }

    /// The kinds with a second voice.
    pub const VOICES: [Self; 4] = [Self::Dry, Self::Room, Self::FarField, Self::Turkish];

    /// The noise kinds.
    pub const NOISES: [Self; 8] = [
        Self::Musan,
        Self::Cough,
        Self::Keyboard,
        Self::Door,
        Self::Dishes,
        Self::Music,
        Self::Street,
        Self::Babble,
    ];

    /// Whether this is background noise (rather than a voice or nothing).
    pub fn is_noise(self) -> bool {
        Self::NOISES.contains(&self)
    }

    /// Noise without speech in it: every noise kind but babble.
    pub fn is_speechless_noise(self) -> bool {
        self.is_noise() && self != Self::Babble
    }

    /// The synthetic sound, if this kind is one.
    fn sound(self) -> Option<Sound> {
        match self {
            Self::Cough => Some(Sound::Cough),
            Self::Keyboard => Some(Sound::Keyboard),
            Self::Door => Some(Sound::Door),
            Self::Dishes => Some(Sound::Dishes),
            Self::Music => Some(Sound::Music),
            Self::Street => Some(Sound::Street),
            _ => None,
        }
    }

    fn source(self) -> Option<Source> {
        match self {
            Self::None
            | Self::Musan
            | Self::Cough
            | Self::Keyboard
            | Self::Door
            | Self::Dishes
            | Self::Music
            | Self::Street => None,
            Self::Dry | Self::Room | Self::Babble => Some(Source::Dry),
            Self::FarField => Some(Source::FarField),
            Self::Turkish => Some(Source::Turkish),
        }
    }
}

/// How often a recognizer hears nothing of a source in a window of `window_s` seconds.
///
/// The defaults are assumed rates, in the range a small streaming recognizer shows on audio like
/// this (they are not a measurement of any provider, and not reproducible from this repository):
/// 0 words in the first 1.8 s for 2.6% of the real barge-ins on a wideband line and 9.7% on a
/// telephone line, 22% of a dry second voice, 33% of a television through a loudspeaker and a
/// room, 68% of far-field loudspeaker recordings, 62% of Turkish broadcast speech. Replace them
/// with your provider's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecognizerModel {
    /// Seeds the draws.
    pub seed: u64,
    /// Real barge-ins heard as nothing, wideband.
    pub miss_real_wideband: f64,
    /// Real barge-ins heard as nothing, telephone.
    pub miss_real_telephone: f64,
    /// A dry second voice heard as nothing.
    pub deaf_dry: f64,
    /// A loudspeaker in a room heard as nothing.
    pub deaf_room: f64,
    /// A far-field loudspeaker recording heard as nothing.
    pub deaf_farfield: f64,
    /// Turkish broadcast speech heard as nothing.
    pub deaf_turkish: f64,
    /// The source is cut into windows of this length from its start, each unheard on its own
    /// draw (a candidate of a television that talks on is a new draw), seconds.
    pub window_s: f64,
    /// Noise heard as a word: the chance that a window of a noise clip makes the recognizer
    /// write one word. Zero for the default set (it is an assumption either way); the sensitivity
    /// runs raise it.
    pub hallucinate_noise: f64,
    /// A real utterance of at most [`SHORT_UTTERANCE_S`] is missed this many times as often as
    /// the sentence rate (a recognizer has less to go on in one short word). The
    /// measurement behind `miss_real_*` was made on sentences; the factor is an assumption, and
    /// the sensitivity runs vary it.
    pub short_miss_factor: f64,
}

impl RecognizerModel {
    /// The assumed rates, with `seed`.
    pub const fn assumed(seed: u64) -> Self {
        Self {
            seed,
            miss_real_wideband: 0.026,
            miss_real_telephone: 0.097,
            deaf_dry: 0.22,
            deaf_room: 0.33,
            deaf_farfield: 0.68,
            deaf_turkish: 0.62,
            window_s: 1.8,
            hallucinate_noise: 0.0,
            short_miss_factor: DEFAULT_SHORT_MISS_FACTOR,
        }
    }

    /// The same with short utterances missed `factor` times as often as sentences.
    pub const fn with_short_miss_factor(self, factor: f64) -> Self {
        Self {
            short_miss_factor: factor,
            ..self
        }
    }

    /// The same with noise sometimes heard as a word.
    pub const fn with_hallucination(self, rate: f64) -> Self {
        Self {
            hallucinate_noise: rate,
            ..self
        }
    }
}

/// An utterance at most this long (seconds) is "short" for the recognizer's miss rate.
pub const SHORT_UTTERANCE_S: f64 = 1.0;
/// Short utterances are missed this many times as often as sentences unless a run says otherwise.
pub const DEFAULT_SHORT_MISS_FACTOR: f64 = 2.0;

/// The speech recognizer the call simulation uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Recognizer {
    /// Hears every source within 15 dB of the caller, words at a steady rate (the earlier
    /// measurements).
    Oracle,
    /// Leaves sources unheard at the model's rates.
    Modelled(RecognizerModel),
}

impl Recognizer {
    /// Name in reports.
    pub fn name(&self) -> String {
        match self {
            Self::Oracle => "oracle recognizer".to_owned(),
            Self::Modelled(m) => {
                let mut name = format!(
                    "modelled recognizer (short utterances missed x{})",
                    m.short_miss_factor
                );
                if m.hallucinate_noise > 0.0 {
                    let _ = std::fmt::Write::write_fmt(
                        &mut name,
                        format_args!(
                            ", a word in {:.0}% of noise windows",
                            100.0 * m.hallucinate_noise
                        ),
                    );
                }
                name
            }
        }
    }

    /// Whether the recognizer hears nothing of the `window`th window of this source in this
    /// scenario. `real`: the caller's barge-in; otherwise the interferer of the `kind`.
    pub fn unheard(
        &self,
        scenario: u64,
        real: bool,
        kind: Kind,
        path: Path16,
        window: u64,
    ) -> bool {
        self.unheard_utterance(scenario, real, false, kind, path, window)
    }

    /// As [`Self::unheard`]; `short`: the real utterance is at most [`SHORT_UTTERANCE_S`] long.
    pub fn unheard_utterance(
        &self,
        scenario: u64,
        real: bool,
        short: bool,
        kind: Kind,
        path: Path16,
        window: u64,
    ) -> bool {
        let Self::Modelled(m) = self else {
            return false;
        };
        let p = if real {
            let sentence = match path {
                Path16::Wideband => m.miss_real_wideband,
                Path16::Telephone => m.miss_real_telephone,
            };
            if short {
                (sentence * m.short_miss_factor).min(1.0)
            } else {
                sentence
            }
        } else {
            match kind {
                Kind::None => 0.0,
                Kind::Dry => m.deaf_dry,
                Kind::Room => m.deaf_room,
                Kind::FarField => m.deaf_farfield,
                Kind::Turkish => m.deaf_turkish,
                Kind::Babble => m.deaf_dry,
                _ => 0.0,
            }
        };
        Rng::derive(m.seed, &[scenario, u64::from(real), kind as u64, window]).unit() < p
    }

    /// The length of the recognizer's windows, seconds (none for the oracle).
    pub const fn window_s(&self) -> f64 {
        match self {
            Self::Oracle => 0.0,
            Self::Modelled(m) => m.window_s,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Scenarios

/// What the caller's first turn, the one the caller's profile is built from, looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FirstTurn {
    /// A whole utterance of 4 to 7 s on a clean line.
    Clean,
    /// 1 to 2 s ("Alo").
    Short,
    /// A whole utterance with a second voice of the scenario's kind running behind it.
    Behind,
}

impl FirstTurn {
    /// Name in reports.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Clean => "clean first turn",
            Self::Short => "short first turn (1 to 2 s)",
            Self::Behind => "television behind the first turn",
        }
    }

    /// All variants.
    pub const ALL: [Self; 3] = [Self::Clean, Self::Short, Self::Behind];
}

/// Whether the caller and the interferer have the same sex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SexRel {
    /// Same sex.
    Same,
    /// Different sex.
    Other,
    /// One of them is not known.
    Unknown,
}

impl SexRel {
    /// Name in reports.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Same => "same sex",
            Self::Other => "other sex",
            Self::Unknown => "sex unknown",
        }
    }
}

/// One call to simulate.
#[derive(Debug, Clone)]
pub struct Real {
    /// Numbers the scenario (seeds the draws).
    pub id: u64,
    /// The caller (index into [`World::callers`]).
    pub caller: usize,
    /// The caller's barge-in utterance (the first turn is the other one).
    pub utt: usize,
    /// The caller says nothing: every interruption is a false barge-in.
    pub quiet: bool,
    /// Cut the barge-in to this many seconds (a command rather than a sentence).
    pub trim_s: Option<f64>,
    /// The cut barge-in is a command word: its transcript is that word.
    pub word: bool,
    /// The first turn.
    pub first_turn: FirstTurn,
    /// What else is on the line.
    pub kind: Kind,
    /// Its level below the caller, dB (negative).
    pub level_db: f32,
    /// The interferer (index into [`World::interferers`]).
    pub interferer: Option<usize>,
    /// The second voice behind the first turn of [`FirstTurn::Behind`].
    pub tv_before: Option<usize>,
    /// The room (index into [`World::rirs`]).
    pub rir: usize,
    /// Caller against interferer.
    pub rel: SexRel,
}

/// Everything the scenarios draw on.
#[derive(Debug, Clone)]
pub struct World {
    /// The callers.
    pub callers: Vec<Caller>,
    /// The second voices.
    pub interferers: Vec<Interferer>,
    /// The rooms.
    pub rirs: Vec<Rir>,
    /// The real noise recordings.
    pub noises: Vec<NoiseClip>,
}

struct Pairer {
    turn: HashMap<(usize, Kind), usize>,
    seen: HashMap<(usize, Kind, bool), usize>,
}

impl Pairer {
    /// The interferer for the caller's next call of this kind: the same sex and the other sex in
    /// turn, each pool walked round robin from a caller-dependent start.
    fn pick(&mut self, world: &World, caller: usize, kind: Kind) -> Option<(usize, SexRel)> {
        let source = kind.source()?;
        let csex = world.callers.get(caller)?.sex;
        let t = self.turn.entry((caller, kind)).or_default();
        let want_same = (*t).is_multiple_of(2);
        *t += 1;
        let pool: Vec<usize> = world
            .interferers
            .iter()
            .enumerate()
            .filter(|(_, i)| i.source == source)
            .map(|(k, _)| k)
            .collect();
        let known = csex.is_some()
            && pool
                .iter()
                .any(|k| world.interferers.get(*k).is_some_and(|i| i.sex.is_some()));
        let subset: Vec<usize> = if known {
            pool.iter()
                .copied()
                .filter(|k| {
                    world
                        .interferers
                        .get(*k)
                        .is_some_and(|i| i.sex.is_some() && (i.sex == csex) == want_same)
                })
                .collect()
        } else {
            pool.clone()
        };
        let subset = if subset.is_empty() { pool } else { subset };
        let c = self.seen.entry((caller, kind, want_same)).or_default();
        let n = *c;
        *c += 1;
        let k = *subset.get((n + caller * 3) % subset.len().max(1))?;
        let rel = match (csex, world.interferers.get(k).and_then(|i| i.sex)) {
            (Some(a), Some(b)) if a == b => SexRel::Same,
            (Some(_), Some(_)) => SexRel::Other,
            _ => SexRel::Unknown,
        };
        Some((k, rel))
    }
}

/// Levels of a second voice below the caller, dB, in quiet calls and in barge-in calls.
pub const QUIET_VOICE_LEVELS: [f32; 3] = [-18.0, -10.0, -4.0];
/// See [`QUIET_VOICE_LEVELS`].
pub const BARGE_VOICE_LEVELS: [f32; 2] = [-14.0, -6.0];
/// Levels of noise below the caller's speech, dB, from very light to loud, in quiet calls ...
pub const QUIET_NOISE_LEVELS: [f32; 5] = [-30.0, -22.0, -14.0, -6.0, 0.0];
/// Draws of every noise kind and level in the quiet calls of one caller and first turn (each a
/// different seed: another cough, other keystrokes, another recording and time).
pub const NOISE_DRAWS: usize = 4;
/// ... and in barge-in calls.
pub const BARGE_NOISE_LEVELS: [f32; 2] = [-22.0, -14.0];

/// The command words the short barge-ins say.
pub const COMMAND_WORDS: [&str; 6] = ["stop", "wait", "dur", "bekle", "hold on", "yeter"];

/// The scenario list of the realistic set (the same for every path): per caller and first-turn
/// variant, quiet calls with each kind of second voice at three levels and each kind of noise at
/// five (each drawn several times), and barge-in calls (a whole sentence, a 0.9 s command and a 0.5 s command word) on a
/// clean line and with each kind of second voice and of noise at two levels.
pub fn scenarios(world: &World) -> Vec<Real> {
    let mut out = Vec::new();
    let mut pairer = Pairer {
        turn: HashMap::new(),
        seen: HashMap::new(),
    };
    let rooms = world.rirs.len().max(1);
    let mut id = 0u64;
    for caller in 0..world.callers.len() {
        let utts = world
            .callers
            .get(caller)
            .map_or(1, |c| c.speech.segments.len().max(1));
        for first_turn in FirstTurn::ALL {
            let mut push = |out: &mut Vec<Real>,
                            pairer: &mut Pairer,
                            quiet: bool,
                            trim_s: Option<f64>,
                            word: bool,
                            kind: Kind,
                            level_db: f32| {
                id += 1;
                let picked = pairer.pick(world, caller, kind);
                // The television behind the first turn is another clip of the same recording
                // (it does not take a turn of the same-sex / other-sex alternation).
                let tv_before =
                    picked
                        .filter(|_| first_turn == FirstTurn::Behind)
                        .and_then(|(k, _)| {
                            let source = kind.source()?;
                            let pool: Vec<usize> = world
                                .interferers
                                .iter()
                                .enumerate()
                                .filter(|(_, i)| i.source == source)
                                .map(|(j, _)| j)
                                .collect();
                            let at = pool.iter().position(|j| *j == k)?;
                            pool.get((at + 5) % pool.len()).copied()
                        });
                #[allow(clippy::cast_possible_truncation)]
                let rir = (Rng::derive(151, &[id]).next_u64() % rooms as u64) as usize;
                out.push(Real {
                    id,
                    caller,
                    utt: (id as usize) % utts,
                    quiet,
                    trim_s,
                    word,
                    first_turn,
                    kind,
                    level_db,
                    interferer: picked.map(|p| p.0),
                    tv_before,
                    rir,
                    rel: picked.map_or(SexRel::Unknown, |p| p.1),
                });
            };
            for kind in Kind::VOICES {
                for level in QUIET_VOICE_LEVELS {
                    push(&mut out, &mut pairer, true, None, false, kind, level);
                }
            }
            for kind in Kind::NOISES {
                for level in QUIET_NOISE_LEVELS {
                    for _ in 0..NOISE_DRAWS {
                        push(&mut out, &mut pairer, true, None, false, kind, level);
                    }
                }
            }
            for (trim, word) in [(None, false), (Some(0.9), false), (Some(0.5), true)] {
                push(&mut out, &mut pairer, false, trim, word, Kind::None, 0.0);
                for kind in Kind::VOICES {
                    for level in BARGE_VOICE_LEVELS {
                        push(&mut out, &mut pairer, false, trim, word, kind, level);
                    }
                }
                for kind in Kind::NOISES {
                    for level in BARGE_NOISE_LEVELS {
                        push(&mut out, &mut pairer, false, trim, word, kind, level);
                    }
                }
            }
        }
    }
    out
}

/// How many distinct pairs the scenarios make.
#[derive(Debug, Clone, Default)]
pub struct PairStats {
    /// Scenarios with a second voice.
    pub scenarios: usize,
    /// Distinct caller x interferer-speaker pairs.
    pub pairs: usize,
    /// The same, with the kind of recording and the room.
    pub conditions: usize,
    /// Distinct pairs of the same sex.
    pub same: usize,
    /// Distinct pairs of different sexes.
    pub other: usize,
    /// Distinct pairs where a sex is not known.
    pub unknown: usize,
    /// Scenarios by relation (same, other, unknown).
    pub scenarios_by_rel: [usize; 3],
    /// Distinct callers and distinct interferer speakers.
    pub callers: usize,
    /// Distinct interferer speakers.
    pub speakers: usize,
}

/// Counts the distinct pairs of `scs`.
pub fn pair_stats(world: &World, scs: &[Real]) -> PairStats {
    let mut s = PairStats::default();
    let mut pairs: BTreeSet<(usize, String, SexRel)> = BTreeSet::new();
    let mut conds: BTreeSet<(usize, String, Kind, usize)> = BTreeSet::new();
    let mut callers = BTreeSet::new();
    let mut speakers = BTreeSet::new();
    for sc in scs {
        if sc.kind.is_noise() {
            continue;
        }
        let Some(i) = sc.interferer.and_then(|i| world.interferers.get(i)) else {
            continue;
        };
        s.scenarios += 1;
        let k = match sc.rel {
            SexRel::Same => 0,
            SexRel::Other => 1,
            SexRel::Unknown => 2,
        };
        if let Some(c) = s.scenarios_by_rel.get_mut(k) {
            *c += 1;
        }
        pairs.insert((sc.caller, i.speaker.clone(), sc.rel));
        conds.insert((sc.caller, i.name.clone(), sc.kind, sc.rir));
        callers.insert(sc.caller);
        speakers.insert(i.speaker.clone());
    }
    s.pairs = pairs.len();
    s.conditions = conds.len();
    for (_, _, r) in &pairs {
        match r {
            SexRel::Same => s.same += 1,
            SexRel::Other => s.other += 1,
            SexRel::Unknown => s.unknown += 1,
        }
    }
    s.callers = callers.len();
    s.speakers = speakers.len();
    s
}

// ---------------------------------------------------------------------------------------------
// Audio

fn first_words(text: &str, n: usize) -> String {
    text.split_whitespace()
        .take(n)
        .collect::<Vec<_>>()
        .join(" ")
}

/// `text` spread over `[start, start + len_s]`, without the words in windows of `window_s` from
/// `start` for which `lost(window)` is true.
fn add_words(
    out: &mut Vec<Word>,
    text: &str,
    start: f64,
    len_s: f64,
    window_s: f64,
    lost: &dyn Fn(u64) -> bool,
) {
    let mut tmp = Vec::new();
    spread(text, start, len_s, &mut tmp);
    out.extend(tmp.into_iter().filter(|w| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let k = if window_s > 0.0 {
            ((w.at - start).max(0.0) / window_s) as u64
        } else {
            0
        };
        window_s <= 0.0 || !lost(k)
    }));
}

/// The first sample of speech in `pcm`: the start of the first 20 ms frame whose RMS is within
/// 20 dB of the loudest frame (an utterance cut from a recording starts with lead-in silence).
pub fn speech_onset(pcm: &[i16]) -> usize {
    let rms_of = |c: &[i16]| -> f64 {
        let e: f64 = c.iter().map(|s| f64::from(*s).powi(2)).sum();
        #[allow(clippy::cast_precision_loss)]
        let n = c.len().max(1) as f64;
        (e / n).sqrt()
    };
    let frames: Vec<f64> = pcm.chunks(320).map(rms_of).collect();
    let max = frames.iter().fold(0.0f64, |m, v| m.max(*v));
    frames
        .iter()
        .position(|v| *v >= max * 0.1)
        .map_or(0, |i| i * 320)
}

/// The interferer's samples as they sound on the line.
fn rendered(world: &World, sc: &Real, which: usize, salt: u64) -> Result<Vec<i16>, BenchError> {
    let clip = world
        .interferers
        .get(which)
        .ok_or_else(|| model_error("no interferer"))?;
    match sc.kind {
        Kind::Room | Kind::Turkish => {
            let rir = world
                .rirs
                .get(sc.rir)
                .ok_or_else(|| model_error("no room impulse response"))?;
            through_loudspeaker_and_room(&clip.pcm, rir, &mut Rng::derive(151, &[sc.id, salt]))
        }
        _ => Ok(clip.pcm.clone()),
    }
}

/// The noise of `sc` on the line from `start`, at `sc.level_db` below the caller's speech
/// (`level` is the caller's RMS), and the words the recognizer writes for it, if any.
#[allow(clippy::too_many_arguments)]
fn place_noise(
    world: &World,
    sc: &Real,
    pcm: &mut [i16],
    start: f64,
    level: f64,
    rec: &Recognizer,
    path: Path16,
    words: &mut Vec<Word>,
) -> Result<(), BenchError> {
    let mut rng = Rng::derive(153, &[sc.id, 7]);
    let clip: Vec<i16> = match sc.kind {
        Kind::Musan => {
            let n = world.noises.len().max(1);
            world
                .noises
                .get((sc.id as usize + sc.caller) % n)
                .map(|c| c.pcm.clone())
                .ok_or_else(|| model_error("no noise recording"))?
        }
        Kind::Babble => {
            // Four dry voices at once, each a little apart in time, none louder than the others.
            let dry: Vec<usize> = world
                .interferers
                .iter()
                .enumerate()
                .filter(|(_, i)| i.source == Source::Dry)
                .map(|(k, _)| k)
                .collect();
            let first = sc
                .interferer
                .and_then(|w| dry.iter().position(|k| *k == w))
                .unwrap_or(0);
            let mut mix = vec![0i16; (8.0 * HZ) as usize];
            for (j, offset) in [0.0, 0.6, 1.3, 2.1].into_iter().enumerate() {
                let Some(clip) = dry
                    .get((first + 3 * j + 1) % dry.len().max(1))
                    .and_then(|k| world.interferers.get(*k))
                else {
                    continue;
                };
                let g = level / rms(&clip.pcm).max(1.0);
                place(&mut mix, offset, &clip.pcm, g);
                let from = start + offset;
                let len_s = clip.pcm.len() as f64 / HZ;
                let id = sc.id.wrapping_add(10 + j as u64);
                match rec {
                    Recognizer::Oracle => {
                        // Babble this loud is heard; this quiet is not.
                        if sc.level_db >= crate::denoise_calls::STT_HEARS_DOWN_TO_DB + 6.0 {
                            add_words(words, &clip.text, from, len_s, 0.0, &|_| false);
                        }
                    }
                    Recognizer::Modelled(_) => {
                        add_words(words, &clip.text, from, len_s, rec.window_s(), &|k| {
                            rec.unheard(id, false, Kind::Babble, path, k)
                        });
                    }
                }
            }
            mix
        }
        kind => match kind.sound() {
            Some(sound) => synthesize(sound, &mut rng),
            None => Vec::new(),
        },
    };
    let g = level * 10f64.powf(f64::from(sc.level_db) / 20.0) / active_rms(&clip).max(1.0);
    // Babble is mixed at the caller's level already; the level shifts the whole mix.
    let g = if sc.kind == Kind::Babble {
        10f64.powf(f64::from(sc.level_db) / 20.0) * 0.5
    } else {
        g
    };
    place(pcm, start, &clip, g);
    // A recognizer that writes a word for noise now and then (a sensitivity setting).
    if let Recognizer::Modelled(m) = rec
        && m.hallucinate_noise > 0.0
        && sc.kind != Kind::Babble
    {
        let seconds = clip.len() as f64 / HZ;
        let mut w = 0u64;
        let mut t = 0.0;
        while t < seconds {
            if Rng::derive(m.seed, &[sc.id, 9, w]).unit() < m.hallucinate_noise {
                words.push(Word {
                    at: start + t + 0.4,
                    text: "the".to_owned(),
                });
            }
            w += 1;
            t += m.window_s.max(0.5);
        }
    }
    Ok(())
}

/// A scenario as audio on `path`, with the transcripts the recognizer `rec` would produce.
///
/// # Errors
/// [`BenchError`] when a clip or a transform is missing or fails.
pub(crate) fn assemble(
    world: &World,
    sc: &Real,
    path: Path16,
    rec: &Recognizer,
) -> Result<Assembled, BenchError> {
    #[allow(clippy::cast_precision_loss)]
    let seconds = |samples: usize| samples as f64 / HZ;
    let caller = world
        .callers
        .get(sc.caller)
        .ok_or_else(|| model_error("no caller"))?;
    let sp = &caller.speech;
    let n_seg = sp.segments.len().max(1);
    let utt = |s: usize| -> Option<(&[i16], &str)> {
        let (a, b) = *sp.segments.get(s)?;
        Some((sp.pcm.get(a..b)?, sp.texts.get(s)?.as_str()))
    };
    let (barge_full, barge_text) =
        utt(sc.utt % n_seg).ok_or_else(|| model_error("no barge-in utterance"))?;
    let (first_full, first_text_full) =
        utt((sc.utt + 1) % n_seg).ok_or_else(|| model_error("no first utterance"))?;
    // The level every disturbance is set against: the caller's whole first utterance.
    let level = rms(first_full);
    // The first turn.
    let mut first = first_full;
    let mut first_text = first_text_full.to_owned();
    if sc.first_turn == FirstTurn::Short {
        let secs = 1.0 + 0.5 * (Rng::derive(151, &[sc.id, 1]).next_u64() % 3) as f64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let n = ((secs * HZ) as usize).min(first_full.len());
        first = first_full.get(..n).unwrap_or(first_full);
        let words = first_text_full.split_whitespace().count();
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let keep = (((words as f64) * secs / seconds(first_full.len())).round() as usize).max(1);
        first_text = first_words(first_text_full, keep);
    }
    // The barge-in.
    let mut barge = barge_full;
    let mut barge_words_text = barge_text.to_owned();
    let command_word = COMMAND_WORDS
        .get((sc.id as usize) % COMMAND_WORDS.len())
        .copied()
        .unwrap_or("stop");
    if let Some(t) = sc.trim_s {
        // The cut starts where the speech starts: a command of `t` seconds is `t` seconds of
        // speech, not of lead-in silence.
        let from = speech_onset(barge_full);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let n = (from + (t * HZ) as usize).min(barge_full.len());
        barge = barge_full.get(from..n).unwrap_or(barge_full);
        let words = barge_text.split_whitespace().count();
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let keep = (((words as f64) * seconds(barge.len()) / seconds(barge_full.len())).round()
            as usize)
            .max(1);
        barge_words_text = first_words(barge_text, keep);
        if sc.word {
            command_word.clone_into(&mut barge_words_text);
        }
    }
    let short_barge = seconds(barge.len()) <= SHORT_UTTERANCE_S;
    // Where everything starts. A television behind the first turn starts before it.
    let first_start = FIRST_START;
    let bot_start = first_start + seconds(first.len()) + BOT_AFTER_FIRST;
    let barge_start = bot_start + BARGE_AFTER_BOT;
    let len_s = if sc.quiet {
        bot_start + QUIET_AFTER_BOT
    } else {
        barge_start + seconds(barge.len()) + 1.6
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let total = (len_s * HZ) as usize;
    let mut pcm = vec![0i16; total];
    let mut caller_only = vec![0i16; total];
    let mut words: Vec<Word> = Vec::new();
    let mut bot_words: Vec<Word> = Vec::new();
    place(&mut pcm, first_start, first, 1.0);
    place(&mut caller_only, first_start, first, 1.0);
    let window = rec.window_s();
    add_words(
        &mut words,
        &first_text,
        first_start,
        seconds(first.len()),
        0.0,
        &|_| false,
    );
    let mut caller_span = None;
    if !sc.quiet {
        place(&mut pcm, barge_start, barge, 1.0);
        place(&mut caller_only, barge_start, barge, 1.0);
        add_words(
            &mut words,
            &barge_words_text,
            barge_start,
            seconds(barge.len()),
            window,
            &|k| rec.unheard_utterance(sc.id, true, short_barge, sc.kind, path, k),
        );
        caller_span = Some((barge_start, barge_start + seconds(barge.len())));
    }
    let gain = |clip_rms: f64, rel_db: f32| {
        level * 10f64.powf(f64::from(rel_db) / 20.0) / clip_rms.max(1.0)
    };
    // A second voice from the bot's start (plus the television's delay), and behind the first turn.
    let hears = |start: f64, len_s: f64, text: &str, rel: f32, out: &mut Vec<Word>, salt: u64| {
        let id = sc.id.wrapping_add(salt);
        match rec {
            Recognizer::Oracle => {
                if rel >= crate::denoise_calls::STT_HEARS_DOWN_TO_DB {
                    add_words(out, text, start, len_s, 0.0, &|_| false);
                }
            }
            Recognizer::Modelled(_) => {
                add_words(out, text, start, len_s, window, &|k| {
                    rec.unheard(id, false, sc.kind, path, k)
                });
            }
        }
    };
    if let (Some(which), true) = (sc.interferer, sc.kind != Kind::None) {
        let voice = rendered(world, sc, which, 2)?;
        let g = gain(rms(&voice), sc.level_db);
        let start = bot_start + TV_AFTER_BOT;
        place(&mut pcm, start, &voice, g);
        let text = world.interferers.get(which).map_or("", |i| i.text.as_str());
        hears(
            start,
            seconds(voice.len()),
            text,
            sc.level_db,
            &mut words,
            0,
        );
        if let (FirstTurn::Behind, Some(before)) = (sc.first_turn, sc.tv_before) {
            let tv = rendered(world, sc, before, 3)?;
            let rel = -6.0 - 8.0 * (Rng::derive(151, &[sc.id, 4]).unit() as f32);
            place(&mut pcm, 0.0, &tv, gain(rms(&tv), rel));
            let text = world
                .interferers
                .get(before)
                .map_or("", |i| i.text.as_str());
            hears(0.0, seconds(tv.len()), text, rel, &mut words, 1);
        }
    }
    if sc.kind.is_noise() {
        place_noise(
            world,
            sc,
            &mut pcm,
            bot_start + TV_AFTER_BOT,
            level,
            rec,
            path,
            &mut words,
        )?;
    }
    // What the bot is saying (the echo text match looks at it).
    let dry: Vec<&Interferer> = world
        .interferers
        .iter()
        .filter(|i| i.source == Source::Dry)
        .collect();
    #[allow(clippy::cast_possible_truncation)]
    if let Some(v) = dry.get((sc.id as usize + 3) % dry.len().max(1)) {
        spread(&v.text, bot_start, seconds(v.pcm.len()), &mut bot_words);
    }
    words.sort_by(|a, b| a.at.total_cmp(&b.at));
    Ok(Assembled {
        pcm: path.apply(&pcm)?,
        caller_only: Some(path.apply(&caller_only)?),
        words,
        bot_words,
        bot_start,
        caller_span,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interferer(name: &str, speaker: &str, sex: Option<Sex>, source: Source) -> Interferer {
        Interferer {
            name: name.to_owned(),
            speaker: speaker.to_owned(),
            sex,
            source,
            pcm: vec![1000; 16_000],
            text: "a b c d".to_owned(),
        }
    }

    fn caller(name: &str, sex: Option<Sex>) -> Caller {
        let pcm: Vec<i16> = (0..64_000)
            .map(|i| ((f64::from(i) * 0.3).sin() * 8000.0) as i16)
            .collect();
        Caller {
            speech: Speech {
                name: name.to_owned(),
                pcm,
                segments: vec![(0, 24_000), (32_000, 64_000)],
                texts: vec![
                    "one two three four".to_owned(),
                    "five six seven eight".to_owned(),
                ],
                speech_rms: 5000.0,
            },
            speaker: name.to_owned(),
            sex,
        }
    }

    fn world() -> World {
        let mut interferers = Vec::new();
        for k in 0..12 {
            let sex = if k % 2 == 0 { Sex::Female } else { Sex::Male };
            interferers.push(interferer(
                &format!("d{k}"),
                &format!("{k}"),
                Some(sex),
                Source::Dry,
            ));
        }
        for k in 0..3 {
            interferers.push(interferer(
                &format!("t{k}"),
                &format!("tr{k}"),
                None,
                Source::Turkish,
            ));
            interferers.push(interferer(
                &format!("f{k}"),
                &format!("vo{k}"),
                Some(Sex::Female),
                Source::FarField,
            ));
        }
        World {
            callers: vec![
                caller("c0", Some(Sex::Female)),
                caller("c1", Some(Sex::Male)),
                caller("c2", None),
            ],
            interferers,
            rirs: vec![Rir::from_taps("r", vec![1.0, 0.0, 0.5, 0.0, 0.25])],
            noises: vec![NoiseClip {
                name: "n".to_owned(),
                pcm: (0..64_000)
                    .map(|i| ((i * 37) % 2_001 - 1_000) as i16)
                    .collect(),
            }],
        }
    }

    #[test]
    fn convolution_matches_direct() {
        let x: Vec<f32> = (0..300).map(|i| ((i * 7) % 11) as f32 - 5.0).collect();
        let h = [1.0f32, -0.5, 0.25, 0.0, 0.1];
        let y = convolve(&x, &h, x.len() + 4).unwrap();
        for (n, v) in y.iter().enumerate() {
            let mut d = 0.0f32;
            for (k, hk) in h.iter().enumerate() {
                if n >= k && n - k < x.len() {
                    d += hk * x[n - k];
                }
            }
            assert!((d - v).abs() < 1e-3, "{n}: {d} vs {v}");
        }
    }

    #[test]
    fn loudspeaker_band_limits_and_is_seeded() {
        let tone = |hz: f64| -> Vec<f64> {
            (0..16_000)
                .map(|i| (2.0 * std::f64::consts::PI * hz * f64::from(i) / HZ).sin())
                .collect()
        };
        let power = |x: &[f64]| x.iter().skip(4000).map(|v| v * v).sum::<f64>();
        let mut low = tone(40.0);
        let mut mid = tone(1000.0);
        loudspeaker(&mut low, &mut Rng::new(5));
        loudspeaker(&mut mid, &mut Rng::new(5));
        assert!(power(&low) < 0.2 * power(&mid), "40 Hz must be cut");
        let (mut a, mut b) = (tone(500.0), tone(500.0));
        loudspeaker(&mut a, &mut Rng::new(9));
        loudspeaker(&mut b, &mut Rng::new(9));
        assert_eq!(a, b);
    }

    #[test]
    fn pairing_is_balanced_and_diverse() {
        let w = world();
        let scs = scenarios(&w);
        assert!(!scs.is_empty());
        let s = pair_stats(&w, &scs);
        // 3 callers x 12 dry speakers at least reached, and both sexes used for known callers.
        assert!(s.pairs >= 30, "{s:?}");
        assert!(s.same > 5 && s.other > 5, "{s:?}");
        let by = s.scenarios_by_rel;
        let (same, other) = (by[0] as f64, by[1] as f64);
        assert!((same / other - 1.0).abs() < 0.3, "{s:?}");
        // The known-sex callers never get an "unknown" relation for a dry clip.
        assert!(
            scs.iter()
                .all(|sc| sc.kind != Kind::Dry || sc.caller == 2 || sc.rel != SexRel::Unknown)
        );
    }

    #[test]
    fn recognizer_is_deterministic_and_matches_rates() {
        let rec = Recognizer::Modelled(RecognizerModel::assumed(7));
        let n = 20_000u64;
        let rate = |real: bool, kind: Kind, path: Path16| {
            (0..n)
                .filter(|i| rec.unheard(*i, real, kind, path, 0))
                .count() as f64
                / n as f64
        };
        assert!((rate(true, Kind::None, Path16::Telephone) - 0.097).abs() < 0.01);
        assert!((rate(true, Kind::None, Path16::Wideband) - 0.026).abs() < 0.01);
        assert!((rate(false, Kind::FarField, Path16::Wideband) - 0.68).abs() < 0.02);
        assert_eq!(
            rec.unheard(5, false, Kind::Room, Path16::Wideband, 0),
            rec.unheard(5, false, Kind::Room, Path16::Wideband, 0)
        );
        assert!(!Recognizer::Oracle.unheard(5, true, Kind::Dry, Path16::Telephone, 0));
    }

    #[test]
    fn assemble_builds_every_variant() {
        let w = world();
        let scs = scenarios(&w);
        for sc in scs.iter().filter(|s| s.caller == 0).take(40) {
            let a = assemble(&w, sc, Path16::Wideband, &Recognizer::Oracle).unwrap();
            assert!(a.pcm.len() > 16_000);
            assert_eq!(a.caller_span.is_some(), !sc.quiet);
        }
        let clean = scs
            .iter()
            .find(|s| s.first_turn == FirstTurn::Clean && s.caller == 0)
            .unwrap();
        let short = Real {
            first_turn: FirstTurn::Short,
            ..clean.clone()
        };
        let a = assemble(&w, &short, Path16::Wideband, &Recognizer::Oracle).unwrap();
        let b = assemble(&w, clean, Path16::Wideband, &Recognizer::Oracle).unwrap();
        assert!(a.bot_start < b.bot_start);
    }

    #[test]
    fn noise_and_command_word_scenarios_exist_at_every_level() {
        let w = world();
        let scs = scenarios(&w);
        for kind in Kind::NOISES {
            for level in QUIET_NOISE_LEVELS {
                assert!(
                    scs.iter()
                        .any(|s| s.quiet && s.kind == kind && (s.level_db - level).abs() < 0.01),
                    "{kind:?} {level}"
                );
            }
            assert!(scs.iter().any(|s| !s.quiet && s.kind == kind), "{kind:?}");
        }
        assert!(scs.iter().any(|s| s.word && s.trim_s == Some(0.5)));
        // Noise is no second voice: it does not count as a pair.
        let stats = pair_stats(&w, &scs);
        let voices = scs
            .iter()
            .filter(|s| s.interferer.is_some() && !s.kind.is_noise())
            .count();
        assert_eq!(stats.scenarios, voices);
    }

    #[test]
    fn noise_calls_have_no_words_unless_the_recognizer_invents_one() {
        let w = world();
        let scs = scenarios(&w);
        let sc = scs
            .iter()
            .find(|s| s.quiet && s.kind == Kind::Cough && s.caller == 0)
            .unwrap();
        let a = assemble(&w, sc, Path16::Wideband, &Recognizer::Oracle).unwrap();
        assert!(
            a.words.iter().all(|x| x.at < a.bot_start),
            "no words after the bot starts"
        );
        let halluc = Recognizer::Modelled(RecognizerModel::assumed(3).with_hallucination(1.0));
        let b = assemble(&w, sc, Path16::Wideband, &halluc).unwrap();
        assert!(b.words.iter().any(|x| x.at > b.bot_start));
        // The noise is on the line, at the level asked for.
        assert!(a.pcm.iter().any(|v| v.abs() > 100));
    }

    #[test]
    fn a_command_word_barge_in_says_the_word() {
        let w = world();
        let scs = scenarios(&w);
        let sc = scs.iter().find(|s| s.word && s.kind == Kind::None).unwrap();
        let a = assemble(&w, sc, Path16::Wideband, &Recognizer::Oracle).unwrap();
        let (from, to) = a.caller_span.unwrap();
        assert!(to - from < 0.6, "{from} {to}");
        let said: Vec<&str> = a
            .words
            .iter()
            .filter(|x| x.at >= from - 0.01)
            .map(|x| x.text.as_str())
            .collect();
        assert!(
            COMMAND_WORDS
                .iter()
                .any(|c| c.split(' ').all(|p| said.contains(&p))),
            "{said:?}"
        );
    }

    #[test]
    fn a_command_slice_starts_at_the_speech_not_at_the_lead_in() {
        let mut pcm = vec![0i16; 16_000];
        for v in pcm.iter_mut().skip(6_400) {
            *v = 8_000;
        }
        assert_eq!(speech_onset(&pcm), 6_400);
        assert_eq!(speech_onset(&[0; 1000]), 0);
        // The cut barge-in of a scenario is all speech.
        let w = world();
        let scs = scenarios(&w);
        let sc = scs.iter().find(|s| s.word && s.kind == Kind::None).unwrap();
        let a = assemble(&w, sc, Path16::Wideband, &Recognizer::Oracle).unwrap();
        let (from, to) = a.caller_span.unwrap();
        assert!((to - from - 0.5).abs() < 0.05, "{from} {to}");
    }

    #[test]
    fn short_utterances_are_missed_more_often_by_the_factor() {
        let model = RecognizerModel::assumed(11);
        let n = 20_000u64;
        let rate = |rec: Recognizer, short: bool| {
            (0..n)
                .filter(|i| {
                    rec.unheard_utterance(*i, true, short, Kind::None, Path16::Telephone, 0)
                })
                .count() as f64
                / n as f64
        };
        let x2 = Recognizer::Modelled(model);
        let x1 = Recognizer::Modelled(model.with_short_miss_factor(1.0));
        assert!((rate(x2, false) - 0.097).abs() < 0.01);
        assert!((rate(x2, true) - 0.194).abs() < 0.015, "{}", rate(x2, true));
        assert!((rate(x1, true) - 0.097).abs() < 0.01);
        assert!(x2.name().contains("x2"), "{}", x2.name());
    }
}
