//! The configurations `brisvox-bench denoise calls --set NAME` compares.

use std::time::Duration;

use brisvox_agent::NoiseAdapt;
use brisvox_inference::{RollingGate, VadParams};

use crate::BenchError;
use crate::denoise_calls::{Config, PolicyChoice};
use crate::denoise_eval::Variant;

const MS: Duration = Duration::from_millis(1);

fn named(name: &str, edit: impl FnOnce(&mut Config)) -> Config {
    let mut config = Config::baseline();
    name.clone_into(&mut config.name);
    edit(&mut config);
    config
}

/// A VAD with this threshold (the negative threshold 0.15 below it) and hangover; the policy's
/// `vad_hangover` follows the VAD's `min_silence`.
fn vad(config: &mut Config, threshold: f32, min_silence_ms: u64) {
    config.vad = VadParams {
        threshold,
        neg_threshold: (threshold - 0.15).max(0.1),
        min_silence: Duration::from_millis(min_silence_ms),
        ..VadParams::default()
    };
    config.gate.speech_from = threshold;
    config.policy.base.vad_hangover = Duration::from_millis(min_silence_ms);
}

fn rolling(offset_db: f32) -> Option<RollingGate> {
    Some(RollingGate {
        offset_db,
        ..RollingGate::default()
    })
}

/// The configurations of a named set.
///
/// # Errors
/// [`BenchError`] for an unknown set.
pub fn catalog(set: &str) -> Result<Vec<Config>, BenchError> {
    let baseline = Config::baseline();
    let mut out = vec![baseline];
    match set {
        "guards" => {
            out.push(named("min_words 1", |c| {
                c.policy = PolicyChoice::recommended(1)
            }));
            out.push(named("min_words 2", |c| {
                c.policy = PolicyChoice::recommended(2)
            }));
            out.push(named("noise-adapted guards", |c| {
                c.policy.noise_adapt = Some(NoiseAdapt::default());
            }));
            out.push(named("echo text match", |c| c.policy.echo_text = true));
            out.push(named("min_words 1 + echo text match", |c| {
                c.policy = PolicyChoice::recommended(1);
                c.policy.echo_text = true;
            }));
            out.push(named("min_words 1 + adapted guards", |c| {
                c.policy = PolicyChoice::recommended(1);
                c.policy.noise_adapt = Some(NoiseAdapt::default());
            }));
            out.push(named("min_words 1 + adapted + echo match", |c| {
                c.policy = PolicyChoice::recommended(1);
                c.policy.noise_adapt = Some(NoiseAdapt::default());
                c.policy.echo_text = true;
            }));
        }
        "gates" => {
            for db in [-50.0, -45.0, -40.0, -35.0] {
                out.push(named(&format!("level floor {db} dBFS"), |c| {
                    c.gate.floor_db = Some(db)
                }));
            }
            for offset in [-10.0, -15.0, -20.0] {
                out.push(named(&format!("rolling gate {offset} dB"), |c| {
                    c.gate.rolling = rolling(offset);
                }));
            }
            out.push(named("floor -45 + rolling -15", |c| {
                c.gate.floor_db = Some(-45.0);
                c.gate.rolling = rolling(-15.0);
            }));
            for offset in [-10.0, -15.0, -20.0] {
                out.push(named(
                    &format!("rolling gate {offset} dB, onset only"),
                    |c| {
                        c.gate.rolling = rolling(offset);
                        c.gate.onset_only = true;
                    },
                ));
            }
            for db in [-40.0, -35.0] {
                out.push(named(&format!("level floor {db} dBFS, onset only"), |c| {
                    c.gate.floor_db = Some(db);
                    c.gate.onset_only = true;
                }));
            }
        }
        "vad" => {
            for (threshold, silence) in [(0.6, 100), (0.7, 100), (0.5, 200), (0.5, 300), (0.6, 200)]
            {
                out.push(named(
                    &format!("vad threshold {threshold}, silence {silence} ms"),
                    |c| {
                        vad(c, threshold, silence);
                    },
                ));
            }
            out.push(named("vad min_speech 128 ms", |c| {
                c.vad.min_speech = 128 * MS;
            }));
        }
        "denoisers" => {
            out.push(named("rnnoise", |c| c.variant = Variant::Rnnoise));
            out.push(named("rnnoise, 25 % dry", |c| {
                c.variant = Variant::Rnnoise;
                c.dry = 0.25;
            }));
            out.push(named("hush", |c| c.variant = Variant::Hush));
            out.push(named("hush, 25 % dry", |c| {
                c.variant = Variant::Hush;
                c.dry = 0.25;
            }));
            out.push(named("hush, 50 % dry", |c| {
                c.variant = Variant::Hush;
                c.dry = 0.5;
            }));
            // What the engine's refusal on 8 kHz audio avoids.
            out.push(named("hush, forced on 8 kHz audio", |c| {
                c.variant = Variant::Hush;
                c.force_narrowband = true;
            }));
        }
        "combined" => {
            out.push(named("echo text match", |c| c.policy.echo_text = true));
            out.push(named("hush", |c| c.variant = Variant::Hush));
            out.push(named("hush + echo text match", |c| {
                c.variant = Variant::Hush;
                c.policy.echo_text = true;
            }));
            out.push(named(
                "hush + rolling gate -15 dB (onset) + echo match",
                |c| {
                    c.variant = Variant::Hush;
                    c.gate.rolling = rolling(-15.0);
                    c.gate.onset_only = true;
                    c.policy.echo_text = true;
                },
            ));
            out.push(named("rolling gate -15 dB (onset) + echo match", |c| {
                c.gate.rolling = rolling(-15.0);
                c.gate.onset_only = true;
                c.policy.echo_text = true;
            }));
            out.push(named("hush + echo match + min_words 1", |c| {
                c.variant = Variant::Hush;
                c.policy = PolicyChoice::recommended(1);
                c.policy.echo_text = true;
            }));
        }
        "all" => {
            out.clear();
            for part in ["guards", "gates", "vad", "denoisers", "combined"] {
                for config in catalog(part)? {
                    if !out.iter().any(|c: &Config| c.name == config.name) {
                        out.push(config);
                    }
                }
            }
        }
        other => {
            return Err(BenchError::Config(format!(
                "--set: guards, gates, vad, denoisers, combined or all, not {other}"
            )));
        }
    }
    Ok(out)
}
