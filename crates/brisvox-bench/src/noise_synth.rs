//! Synthetic background noises for the realistic barge-in calls (feature `denoise`).
//!
//! The committed real noise recordings (`fixtures/noise`, nine excerpts of the MUSAN corpus) are
//! steady or slowly changing. The sounds that cut a bot off in practice are often short events: a
//! cough, a keystroke, a door, a plate. This module makes stand-ins for them from a seeded
//! generator, so the same seed gives the same samples on every platform and nothing has to be
//! committed. They are **models, not recordings**: they have the broad shape of the sound (onset,
//! decay, spectral region, repetition), not its detail, and a VAD that reacts to them reacts to
//! that shape. The reports say so.
//!
//! Every clip is [`CLIP_SECONDS`] long, 16 kHz mono, peak-normalised; the caller of the module sets
//! the level against the caller's speech with [`active_rms`].

use crate::realistic::Rng;

const HZ: f64 = 16_000.0;
const TAU: f64 = std::f64::consts::TAU;

/// The length of every synthetic clip, seconds.
pub const CLIP_SECONDS: f64 = 6.0;

/// The kinds of synthetic sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    /// One to three cough bursts: a voiced start, noisy with two formant regions, a fast decay.
    Cough,
    /// Two bursts of key clicks: a sharp high click, a smaller release, a low thump.
    Keyboard,
    /// A door: a low damped thump with a latch click and a short rumble, once or twice.
    Door,
    /// Plates and glasses: a handful of high ringing clinks at irregular intervals.
    Dishes,
    /// Music: a chord progression of harmonic tones with a melody and a little tremolo.
    Music,
    /// Street: a low rumble with the swell of a passing vehicle.
    Street,
}

fn samples() -> usize {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let n = (CLIP_SECONDS * HZ) as usize;
    n
}

fn white(rng: &mut Rng) -> f64 {
    rng.range(-1.0, 1.0)
}

/// A two-pole resonator (centre `hz`, pole radius `r`, near 1 for a narrow peak) over `x`.
fn resonate(x: &[f64], hz: f64, r: f64) -> Vec<f64> {
    let a1 = 2.0 * r * (TAU * hz / HZ).cos();
    let a2 = -r * r;
    let g = 1.0 - r;
    let (mut y1, mut y2) = (0.0, 0.0);
    x.iter()
        .map(|v| {
            let y = g * v + a1 * y1 + a2 * y2;
            y2 = y1;
            y1 = y;
            y
        })
        .collect()
}

/// Adds `src` into `out` from `at_s`, scaled by `gain`, clipped at the end of `out`.
fn add(out: &mut [f64], at_s: f64, src: &[f64], gain: f64) {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let start = (at_s.max(0.0) * HZ) as usize;
    for (i, v) in src.iter().enumerate() {
        if let Some(slot) = out.get_mut(start + i) {
            *slot += v * gain;
        }
    }
}

fn exp_env(len: usize, attack_s: f64, tau_s: f64) -> Vec<f64> {
    (0..len)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f64 / HZ;
            let a = if attack_s > 0.0 {
                (t / attack_s).min(1.0)
            } else {
                1.0
            };
            a * (-t / tau_s).exp()
        })
        .collect()
}

fn noise_burst(rng: &mut Rng, len_s: f64) -> Vec<f64> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let n = (len_s * HZ) as usize;
    (0..n).map(|_| white(rng)).collect()
}

fn cough(rng: &mut Rng, out: &mut [f64]) {
    let bursts = 1 + (rng.next_u64() % 3) as usize;
    let mut at = rng.range(0.4, 1.6);
    let mut level = 1.0;
    let f1 = rng.range(550.0, 1_100.0);
    let f2 = rng.range(1_800.0, 3_200.0);
    let f0 = rng.range(110.0, 190.0);
    // How much of the cough is voice: a dry throat-clear has none, a bark or a laugh-cough has
    // as much as speech (log-uniform over a factor of sixteen).
    let voiced_weight = 0.5 * 16f64.powf(rng.unit());
    for _ in 0..bursts {
        let len_s = rng.range(0.22, 0.34);
        let n = noise_burst(rng, len_s);
        let a = resonate(&n, f1, 0.975);
        let b = resonate(&n, f2, 0.96);
        let env = exp_env(n.len(), 0.012, rng.range(0.07, 0.11));
        let voiced: Vec<f64> = (0..n.len())
            .map(|i| {
                #[allow(clippy::cast_precision_loss)]
                let t = i as f64 / HZ;
                // The voiced part of a cough lasts 80 to 200 ms, depending on how voiced it is.
                let span = 0.08 + 0.12 * (voiced_weight / 8.0).min(1.0);
                let w = if t < span { 1.0 - t / span } else { 0.0 };
                w * ((TAU * f0 * t).sin() + 0.5 * (2.0 * TAU * f0 * t).sin())
            })
            .collect();
        let burst: Vec<f64> = (0..n.len())
            .map(|i| {
                let e = env.get(i).copied().unwrap_or(0.0);
                e * (12.0 * a.get(i).copied().unwrap_or(0.0)
                    + 8.0 * b.get(i).copied().unwrap_or(0.0)
                    + voiced_weight * 4.0 * voiced.get(i).copied().unwrap_or(0.0))
            })
            .collect();
        add(out, at, &burst, level);
        at += rng.range(0.18, 0.34);
        level *= 0.8;
    }
}

fn keyboard(rng: &mut Rng, out: &mut [f64]) {
    for start in [rng.range(0.2, 1.0), rng.range(2.6, 3.6)] {
        let keys = 4 + (rng.next_u64() % 6) as usize;
        let mut at = start;
        for _ in 0..keys {
            let n = noise_burst(rng, 0.03);
            // High-passed: the white noise minus its own smoothing.
            let mut lp = 0.0;
            let hp: Vec<f64> = n
                .iter()
                .map(|v| {
                    lp += 0.25 * (v - lp);
                    v - lp
                })
                .collect();
            let env = exp_env(hp.len(), 0.0005, 0.004);
            let click: Vec<f64> = hp.iter().zip(&env).map(|(v, e)| v * e).collect();
            let amp = rng.range(0.6, 1.3);
            add(out, at, &click, amp);
            // The release, softer, a key's travel later.
            add(out, at + rng.range(0.06, 0.09), &click, 0.35 * amp);
            // The thump of the key bed.
            let thump_src = noise_burst(rng, 0.03);
            let thump = resonate(&thump_src, 180.0, 0.98);
            let tenv = exp_env(thump.len(), 0.001, 0.012);
            let thump: Vec<f64> = thump.iter().zip(&tenv).map(|(v, e)| 10.0 * v * e).collect();
            add(out, at, &thump, 0.3 * amp);
            at += rng.range(0.09, 0.22);
        }
    }
}

fn door(rng: &mut Rng, out: &mut [f64]) {
    let events = 1 + (rng.next_u64() % 2) as usize;
    let mut at = rng.range(0.5, 1.8);
    let mut level = 1.0;
    for _ in 0..events {
        let f = rng.range(65.0, 120.0);
        let tau = rng.range(0.09, 0.15);
        let n = noise_burst(rng, 0.7);
        let env = exp_env(n.len(), 0.002, tau);
        let thump: Vec<f64> = (0..n.len())
            .map(|i| {
                #[allow(clippy::cast_precision_loss)]
                let t = i as f64 / HZ;
                env.get(i).copied().unwrap_or(0.0) * (TAU * f * t).sin()
            })
            .collect();
        add(out, at, &thump, level);
        // A short rumble after it.
        let rumble = resonate(&n, 220.0, 0.97);
        let renv = exp_env(n.len(), 0.004, 0.14);
        let rumble: Vec<f64> = rumble.iter().zip(&renv).map(|(v, e)| 8.0 * v * e).collect();
        add(out, at, &rumble, 0.5 * level);
        // The latch: a very short click of mid noise.
        let click_src = noise_burst(rng, 0.012);
        let cenv = exp_env(click_src.len(), 0.0005, 0.004);
        let click: Vec<f64> = click_src.iter().zip(&cenv).map(|(v, e)| v * e).collect();
        add(out, at - 0.01, &click, 0.4 * level);
        at += rng.range(1.2, 2.0);
        level *= 0.45;
    }
}

fn dishes(rng: &mut Rng, out: &mut [f64]) {
    let clinks = 4 + (rng.next_u64() % 5) as usize;
    let mut at = rng.range(0.3, 1.0);
    for _ in 0..clinks {
        let len_s = 0.45;
        let parts = 3;
        let mut clink: Vec<f64> = Vec::new();
        for _ in 0..parts {
            let f = rng.range(2_200.0, 6_800.0);
            let tau = rng.range(0.04, 0.2);
            let amp = rng.range(0.4, 1.0);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let n = (len_s * HZ) as usize;
            let env = exp_env(n, 0.0004, tau);
            let tone: Vec<f64> = (0..n)
                .map(|i| {
                    #[allow(clippy::cast_precision_loss)]
                    let t = i as f64 / HZ;
                    amp * env.get(i).copied().unwrap_or(0.0) * (TAU * f * t).sin()
                })
                .collect();
            if clink.is_empty() {
                clink = tone;
            } else {
                for (a, b) in clink.iter_mut().zip(&tone) {
                    *a += b;
                }
            }
        }
        // The strike: a few milliseconds of noise.
        let strike_src = noise_burst(rng, 0.004);
        for (i, v) in strike_src.iter().enumerate() {
            if let Some(slot) = clink.get_mut(i) {
                *slot += 1.2 * v;
            }
        }
        add(out, at, &clink, rng.range(0.5, 1.0));
        at += rng.range(0.15, 0.6);
    }
}

fn midi_hz(note: f64) -> f64 {
    440.0 * 2f64.powf((note - 69.0) / 12.0)
}

fn music(rng: &mut Rng, out: &mut [f64]) {
    // A progression in a minor key (i, VI, III, VII), a note per quarter of a second on top.
    let root = 45.0 + (rng.next_u64() % 7) as f64;
    let chords: [(f64, [f64; 3]); 4] = [
        (0.0, [0.0, 3.0, 7.0]),
        (8.0, [0.0, 4.0, 7.0]),
        (3.0, [0.0, 4.0, 7.0]),
        (10.0, [0.0, 4.0, 7.0]),
    ];
    let scale = [0.0, 2.0, 3.0, 5.0, 7.0, 8.0, 10.0, 12.0];
    let tone = |note: f64, len_s: f64, amp: f64, attack: f64| -> Vec<f64> {
        let hz = midi_hz(note);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let n = (len_s * HZ) as usize;
        let env = exp_env(n, attack, len_s * 0.7);
        (0..n)
            .map(|i| {
                #[allow(clippy::cast_precision_loss)]
                let t = i as f64 / HZ;
                let mut v = 0.0;
                for (h, a) in [(1.0, 1.0), (2.0, 0.5), (3.0, 0.33), (4.0, 0.2)] {
                    v += a * (TAU * hz * h * t).sin();
                }
                amp * env.get(i).copied().unwrap_or(0.0) * v
            })
            .collect()
    };
    let mut at = 0.0;
    let mut chord_no = 0usize;
    while at < CLIP_SECONDS {
        let (shift, parts) = chords
            .get(chord_no % chords.len())
            .copied()
            .unwrap_or((0.0, [0.0, 3.0, 7.0]));
        for p in parts {
            add(out, at, &tone(root + shift + p, 1.6, 0.4, 0.03), 1.0);
        }
        for q in 0..4 {
            let idx = (rng.next_u64() % scale.len() as u64) as usize;
            let note = root + 24.0 + scale.get(idx).copied().unwrap_or(0.0);
            add(
                out,
                at + f64::from(q) * 0.4,
                &tone(note, 0.5, 0.5, 0.01),
                1.0,
            );
        }
        at += 1.6;
        chord_no += 1;
    }
    // A little tremolo, as speakers and rooms give.
    let rate = rng.range(4.0, 6.0);
    for (i, v) in out.iter_mut().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let t = i as f64 / HZ;
        *v *= 1.0 + 0.1 * (TAU * rate * t).sin();
    }
}

fn street(rng: &mut Rng, out: &mut [f64]) {
    let n = out.len();
    let raw: Vec<f64> = (0..n).map(|_| white(rng)).collect();
    // A low rumble: two one-pole low-passes in a row (about 100 Hz).
    let (mut a, mut b) = (0.0, 0.0);
    let rumble: Vec<f64> = raw
        .iter()
        .map(|v| {
            a += 0.04 * (v - a);
            b += 0.04 * (a - b);
            b
        })
        .collect();
    // The vehicle: noise around 300 to 900 Hz with a slow swell.
    let mid = resonate(&raw, rng.range(350.0, 800.0), 0.9);
    let centre = rng.range(2.0, 4.0);
    let sigma = rng.range(0.7, 1.2);
    for (i, slot) in out.iter_mut().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let t = i as f64 / HZ;
        let swell = (-((t - centre) / sigma).powi(2)).exp();
        *slot = 20.0 * rumble.get(i).copied().unwrap_or(0.0)
            + 6.0 * swell * mid.get(i).copied().unwrap_or(0.0);
    }
}

/// A clip of `sound` from `rng`, 16 kHz, peak-normalised to 20 000.
pub fn synthesize(sound: Sound, rng: &mut Rng) -> Vec<i16> {
    let mut x = vec![0.0f64; samples()];
    match sound {
        Sound::Cough => cough(rng, &mut x),
        Sound::Keyboard => keyboard(rng, &mut x),
        Sound::Door => door(rng, &mut x),
        Sound::Dishes => dishes(rng, &mut x),
        Sound::Music => music(rng, &mut x),
        Sound::Street => street(rng, &mut x),
    }
    let peak = x.iter().fold(1e-9f64, |m, v| m.max(v.abs()));
    x.iter()
        .map(|v| {
            #[allow(clippy::cast_possible_truncation)]
            {
                (v / peak * 20_000.0).round() as i16
            }
        })
        .collect()
}

/// The RMS of `pcm` over its active frames: the 20 ms frames whose RMS is within 20 dB of the
/// loudest frame. For a steady sound this is its RMS; for an event (a cough, a click) it is the
/// level while it sounds, which is what the caller's speech is compared with.
pub fn active_rms(pcm: &[i16]) -> f64 {
    let frames: Vec<f64> = pcm
        .chunks(320)
        .map(|c| {
            let e: f64 = c.iter().map(|s| f64::from(*s).powi(2)).sum();
            #[allow(clippy::cast_precision_loss)]
            let n = c.len().max(1) as f64;
            e / n
        })
        .collect();
    let max = frames.iter().fold(0.0f64, |m, v| m.max(*v));
    if max <= 0.0 {
        return 0.0;
    }
    let keep: Vec<f64> = frames.into_iter().filter(|e| *e >= max * 0.01).collect();
    #[allow(clippy::cast_precision_loss)]
    let mean = keep.iter().sum::<f64>() / keep.len().max(1) as f64;
    mean.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Sound; 6] = [
        Sound::Cough,
        Sound::Keyboard,
        Sound::Door,
        Sound::Dishes,
        Sound::Music,
        Sound::Street,
    ];

    #[test]
    fn clips_are_seeded_full_length_and_not_silent() {
        for s in ALL {
            let a = synthesize(s, &mut Rng::new(3));
            let b = synthesize(s, &mut Rng::new(3));
            let c = synthesize(s, &mut Rng::new(4));
            assert_eq!(a.len(), samples(), "{s:?}");
            assert_eq!(a, b, "{s:?} is not deterministic");
            assert_ne!(a, c, "{s:?} ignores the seed");
            assert!(a.iter().any(|v| v.abs() > 5_000), "{s:?} is silent");
        }
    }

    #[test]
    fn events_are_short_and_steady_sounds_are_not() {
        let share_active = |pcm: &[i16]| {
            let peak = f64::from(pcm.iter().map(|v| v.abs()).max().unwrap_or(1));
            let loud = pcm
                .chunks(320)
                .filter(|c| {
                    let rms = (c.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>()
                        / c.len() as f64)
                        .sqrt();
                    rms > peak * 0.1
                })
                .count();
            loud as f64 / pcm.len().div_ceil(320) as f64
        };
        for s in [Sound::Cough, Sound::Keyboard, Sound::Door] {
            let x = synthesize(s, &mut Rng::new(9));
            assert!(share_active(&x) < 0.45, "{s:?}: {}", share_active(&x));
        }
        let m = synthesize(Sound::Music, &mut Rng::new(9));
        assert!(share_active(&m) > 0.5, "{}", share_active(&m));
    }

    #[test]
    fn active_rms_follows_the_event_not_the_silence() {
        let mut x = vec![0i16; 16_000];
        for v in x.iter_mut().skip(8_000).take(1_600) {
            *v = 10_000;
        }
        let a = active_rms(&x);
        assert!((a - 10_000.0).abs() < 100.0, "{a}");
        assert_eq!(active_rms(&[0; 100]), 0.0);
    }
}
