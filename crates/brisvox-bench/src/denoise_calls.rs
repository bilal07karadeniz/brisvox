//! False barge-ins measured after the engine's real interruption guards (feature `denoise`;
//! `brisvox-bench denoise calls`).
//!
//! [`crate::denoise_eval`] counts speech starts of the VAD, which is where the engine's decision
//! begins, not where it ends: a VAD start while the bot talks is only a candidate, and the guards
//! (`min_speech`, the echo window, `min_words`, the backchannel lists; `docs/ARCHITECTURE.md`,
//! "Interruption policy") decide whether the bot is cut off. This module runs the real
//! [`brisvox_agent::TurnMachine`] with the real [`brisvox_agent::InterruptionPolicy`] on the events
//! of the real VAD stream, in calls where the bot is speaking:
//!
//! - **quiet calls**: the caller says nothing; a second voice (a television, a person nearby), the
//!   bot's own voice coming back (echo) or plain noise is on the line. Every interruption is a false
//!   barge-in.
//! - **barge-in calls**: the caller speaks over the bot, with the same disturbances on the line.
//!   Counted: interruptions that happened before the caller spoke (false), calls where the caller
//!   was never allowed to interrupt (missed), and the time from the first word to the interruption.
//!
//! What stands in for the speech-to-text provider is an *oracle*: words of the sources that are
//! audible enough (within 15 dB of the caller) appear in interim transcripts a fixed time after they
//! are spoken (3 words a second spread over each source's length, a 300 ms lag, an interim every
//! 250 ms), the text being the sources' real transcripts. Noise has no words. That makes the
//! min-words, echo and backchannel guards behave as they would with a recognizer that hears
//! everything it can; a real recognizer misses some and invents some.
//!
//! Each [`Config`] is a complete front end: denoiser (and its dry/wet blend), VAD parameters, level
//! gates ([`brisvox_inference::Gated`]) and the interruption policy with its optional line-dependent
//! parts (noise-adapted guards, bot-echo text match).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use brisvox_agent::{
    InterruptionPolicy, NoiseAdapt, TurnAction, TurnInput, TurnMachine, TurnPolicy, is_bot_echo,
};
use brisvox_core::{Clock, SystemClock};
use brisvox_engine::InterruptionConfig;
use brisvox_inference::silero_tract::SileroTractModel;
use brisvox_inference::smart_turn::TurnModel;
use brisvox_inference::{GateConfig, Gated, NoiseFloorHandle, VadEvent, VadParams, VadStream};
use serde::Deserialize;
use tokio::time::Instant;

use crate::BenchError;
use crate::denoise_eval::{NoiseClip, Path16, Speech, Variant, enhance, rms, verdicts};

const FRAME: usize = 320;
const HZ: f64 = 16_000.0;

/// The caller's first utterance starts this long into the call.
const FIRST_START: f64 = 0.3;
/// The bot's audio starts this long after the caller's first utterance ends (the turn-end silence
/// and the response latency).
const BOT_AFTER_FIRST: f64 = 1.5;
/// A television clip starts this long after the bot.
const TV_AFTER_BOT: f64 = 0.7;
/// The caller's barge-in utterance starts this long after the bot.
const BARGE_AFTER_BOT: f64 = 2.6;
/// A quiet call ends this long after the bot began.
const QUIET_AFTER_BOT: f64 = 7.0;
/// A source is transcribed by the oracle when it is no more than this far below the caller.
const STT_HEARS_DOWN_TO_DB: f32 = -15.0;
/// The oracle's recognizer lag and interim period.
const STT_LAG: f64 = 0.30;
const FIRST_INTERIM: f64 = 0.35;
const INTERIM_EVERY: f64 = 0.25;
/// How much of the bot's recent speech the echo-text match looks at.
const BOT_RECENT_S: f64 = 6.0;

fn model_error(e: impl std::fmt::Display) -> BenchError {
    BenchError::Config(format!("{e}"))
}

// ---------------------------------------------------------------------------------------------
// Sources

/// A clip of another speaker with its transcript.
#[derive(Debug, Clone)]
pub struct Voice {
    /// File stem.
    pub name: String,
    /// 16 kHz samples.
    pub pcm: Vec<i16>,
    /// Transcript.
    pub text: String,
}

#[derive(Deserialize)]
struct ClipEntry {
    file: String,
    text: String,
}

/// Loads the clips of `fixtures/interferers` (or `dir`).
///
/// # Errors
/// [`BenchError`] when `clips.json` or a clip cannot be read.
pub fn load_voices(dir: &Path) -> Result<Vec<Voice>, BenchError> {
    let entries: Vec<ClipEntry> =
        serde_json::from_str(&std::fs::read_to_string(dir.join("clips.json"))?)
            .map_err(model_error)?;
    entries
        .into_iter()
        .map(|e| {
            let (rate, pcm) = crate::denoise_eval::read_wav(&dir.join(&e.file))?;
            if rate != 16_000 {
                return Err(model_error(format!("{}: {rate} Hz", e.file)));
            }
            Ok(Voice {
                name: e.file.trim_end_matches(".wav").to_owned(),
                pcm,
                text: e.text,
            })
        })
        .collect()
}

/// What disturbs the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Disturbance {
    /// Another speaker, `rel_db` below the caller (television, a person nearby).
    Speech,
    /// The bot's own voice coming back, `rel_db` below the caller.
    Echo,
    /// Recorded noise at this signal-to-noise ratio against the caller.
    Noise,
}

impl Disturbance {
    /// Name in reports.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Speech => "background speech",
            Self::Echo => "bot echo",
            Self::Noise => "noise",
        }
    }
}

/// One call to simulate.
#[derive(Debug, Clone)]
pub struct Scenario {
    /// What is on the line.
    pub disturbance: Disturbance,
    /// Level of the disturbance in dB: below the caller for speech and echo (negative), the
    /// signal-to-noise ratio for noise (positive).
    pub level_db: f32,
    /// Index of the clip (a voice, or a noise recording).
    pub clip: usize,
    /// The caller's utterance (index into [`Suite::speech`] segments), or `None` for a quiet call.
    pub caller: Option<(usize, usize)>,
    /// The path the line takes.
    pub path: Path16,
}

/// Everything the scenarios draw on.
pub struct Suite {
    /// The caller's fixtures.
    pub speech: Vec<Speech>,
    /// Other speakers.
    pub voices: Vec<Voice>,
    /// Noise recordings.
    pub noise: Vec<NoiseClip>,
}

/// A word of the oracle's transcripts.
#[derive(Debug, Clone)]
struct Word {
    at: f64,
    text: String,
}

fn spread(text: &str, start: f64, len: f64, out: &mut Vec<Word>) {
    let words: Vec<&str> = text.split_whitespace().collect();
    #[allow(clippy::cast_precision_loss)]
    let n = words.len() as f64;
    for (i, w) in words.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let at = start + (i as f64 + 0.5) * len / n.max(1.0);
        out.push(Word {
            at,
            text: (*w).to_owned(),
        });
    }
}

/// A scenario turned into audio and the oracle's words.
///
/// Layout of a call: the caller's first utterance (which the machine takes as the first turn),
/// the bot's answer starting a second and a half after it (the fixed-silence turn end plus the
/// response latency), and from there the disturbance. In barge-in calls the caller speaks again
/// over the bot.
struct Assembled {
    /// The line as the engine receives it (path applied).
    pcm: Vec<i16>,
    /// The caller's audio alone, path applied (the clean reference of the verdicts).
    caller_only: Option<Vec<i16>>,
    /// Words the recognizer would hear, in time order.
    words: Vec<Word>,
    /// What the bot said, with times.
    bot_words: Vec<Word>,
    /// When the bot's audio starts, seconds.
    bot_start: f64,
    /// The caller's barge-in utterance in seconds, if the call has one.
    caller_span: Option<(f64, f64)>,
    /// The path the line took.
    path: Path16,
}

fn place(out: &mut [i16], at_s: f64, clip: &[i16], gain: f64) {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let start = (at_s * HZ) as usize;
    for (i, s) in clip.iter().enumerate() {
        if let Some(slot) = out.get_mut(start + i) {
            let v = (f64::from(*slot) + f64::from(*s) * gain)
                .round()
                .clamp(-32_768.0, 32_767.0);
            #[allow(clippy::cast_possible_truncation)]
            {
                *slot = v as i16;
            }
        }
    }
}

/// The caller's utterance `(fixture, segment)`: its samples, transcript.
fn utterance(suite: &Suite, f: usize, s: usize) -> Option<(&[i16], &str)> {
    let sp = suite.speech.get(f)?;
    let (a, b) = *sp.segments.get(s)?;
    Some((sp.pcm.get(a..b)?, sp.texts.get(s)?.as_str()))
}

fn assemble(suite: &Suite, sc: &Scenario) -> Result<Assembled, BenchError> {
    #[allow(clippy::cast_precision_loss)]
    let seconds = |samples: usize| samples as f64 / HZ;
    // The first utterance: another utterance of the barge-in's fixture, or one picked by the clip.
    let (f1, s1) = match sc.caller {
        Some((f, s)) => (
            f,
            (s + 1) % suite.speech.get(f).map_or(1, |sp| sp.segments.len().max(1)),
        ),
        None => (sc.clip % suite.speech.len().max(1), 0),
    };
    let (first, first_text) =
        utterance(suite, f1, s1).ok_or_else(|| model_error("no caller utterance"))?;
    let level = rms(first);
    let first_start = FIRST_START;
    let bot_start = first_start + seconds(first.len()) + BOT_AFTER_FIRST;
    let barge = match sc.caller {
        Some((f, s)) => {
            Some(utterance(suite, f, s).ok_or_else(|| model_error("no barge-in utterance"))?)
        }
        None => None,
    };
    let barge_start = bot_start + BARGE_AFTER_BOT;
    let len_s = match &barge {
        Some((seg, _)) => barge_start + seconds(seg.len()) + 1.6,
        None => bot_start + QUIET_AFTER_BOT,
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let total = (len_s * HZ) as usize;
    let mut pcm = vec![0i16; total];
    let mut caller_only = vec![0i16; total];
    let mut words = Vec::new();
    let mut bot_words = Vec::new();
    place(&mut pcm, first_start, first, 1.0);
    place(&mut caller_only, first_start, first, 1.0);
    spread(first_text, first_start, seconds(first.len()), &mut words);
    let mut caller_span = None;
    if let Some((seg, text)) = &barge {
        place(&mut pcm, barge_start, seg, 1.0);
        place(&mut caller_only, barge_start, seg, 1.0);
        spread(text, barge_start, seconds(seg.len()), &mut words);
        caller_span = Some((barge_start, barge_start + seconds(seg.len())));
    }
    let gain = |clip_rms: f64, rel_db: f32| {
        level * 10f64.powf(f64::from(rel_db) / 20.0) / clip_rms.max(1.0)
    };
    match sc.disturbance {
        Disturbance::Speech | Disturbance::Echo => {
            let voice = suite
                .voices
                .get(sc.clip % suite.voices.len().max(1))
                .ok_or_else(|| model_error("no interferer clips"))?;
            let g = gain(rms(&voice.pcm), sc.level_db);
            let start = if sc.disturbance == Disturbance::Echo {
                bot_start + 0.1
            } else {
                bot_start + TV_AFTER_BOT
            };
            place(&mut pcm, start, &voice.pcm, g);
            if sc.level_db >= STT_HEARS_DOWN_TO_DB {
                spread(&voice.text, start, seconds(voice.pcm.len()), &mut words);
            }
            if sc.disturbance == Disturbance::Echo {
                spread(
                    &voice.text,
                    bot_start,
                    seconds(voice.pcm.len()),
                    &mut bot_words,
                );
            }
        }
        Disturbance::Noise => {
            let clip = suite
                .noise
                .get(sc.clip % suite.noise.len().max(1))
                .ok_or_else(|| model_error("no noise clips"))?;
            let g = gain(rms(&clip.pcm), -sc.level_db);
            let n = clip.pcm.len().max(1);
            for (i, slot) in pcm.iter_mut().enumerate() {
                let v = f64::from(*slot) + f64::from(clip.pcm.get(i % n).copied().unwrap_or(0)) * g;
                #[allow(clippy::cast_possible_truncation)]
                {
                    *slot = v.round().clamp(-32_768.0, 32_767.0) as i16;
                }
            }
        }
    }
    // Whatever the disturbance, the bot is saying something (the match looks at it).
    if bot_words.is_empty()
        && let Some(voice) = suite.voices.get((sc.clip + 3) % suite.voices.len().max(1))
    {
        spread(
            &voice.text,
            bot_start,
            seconds(voice.pcm.len()),
            &mut bot_words,
        );
    }
    words.sort_by(|a, b| a.at.total_cmp(&b.at));
    Ok(Assembled {
        pcm: sc.path.apply(&pcm)?,
        caller_only: Some(sc.path.apply(&caller_only)?),
        words,
        bot_words,
        bot_start,
        caller_span,
        path: sc.path,
    })
}

// ---------------------------------------------------------------------------------------------
// Configurations

/// The interruption policy of a configuration and its line-dependent parts.
#[derive(Debug, Clone)]
pub struct PolicyChoice {
    /// The base policy.
    pub base: InterruptionPolicy,
    /// Tighten the guards on a noisy line.
    pub noise_adapt: Option<NoiseAdapt>,
    /// Hold barge-ins whose interims repeat the bot's recent words (`brisvox_agent::is_bot_echo`).
    pub echo_text: bool,
}

impl PolicyChoice {
    /// The agent file's default (`InterruptionConfig::RECOMMENDED`), optionally with `min_words`.
    pub fn recommended(min_words: u32) -> Self {
        let mut config = InterruptionConfig::RECOMMENDED;
        config.min_words = min_words;
        Self {
            base: config.policy(),
            noise_adapt: None,
            echo_text: false,
        }
    }
}

/// A complete front end under test.
#[derive(Debug, Clone)]
pub struct Config {
    /// Name in the reports.
    pub name: String,
    /// The denoiser.
    pub variant: Variant,
    /// Share of the input kept in the output (0: the plain denoiser).
    pub dry: f32,
    /// VAD parameters.
    pub vad: VadParams,
    /// Level gates.
    pub gate: GateConfig,
    /// The interruption policy.
    pub policy: PolicyChoice,
    /// Use the denoiser even on a path below its minimum rate (the engine does not).
    pub force_narrowband: bool,
}

impl Config {
    /// The engine as shipped: no denoiser, no gates, the production VAD parameters and the
    /// recommended guards.
    pub fn baseline() -> Self {
        Self {
            name: "baseline".to_owned(),
            variant: Variant::None,
            dry: 0.0,
            vad: VadParams::default(),
            gate: GateConfig::default(),
            policy: PolicyChoice::recommended(0),
            force_narrowband: false,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// One call

/// What happened in one call.
#[derive(Debug, Clone, Default)]
struct CallResult {
    /// VAD starts after the bot began to speak (before the guards).
    vad_starts: u32,
    /// Interruptions of the bot, as seconds into the call.
    interrupts: Vec<f64>,
    /// Smart Turn verdict at the end of the barge-in utterance flipped against the caller alone.
    flip: Option<bool>,
    /// Starts of the VAD inside the barge-in utterance, minus one.
    fragments: u32,
}

/// An input for the turn machine, in arrival order.
struct Timed {
    at: f64,
    kind: Kind,
}

enum Kind {
    BotStart,
    Start(Instant),
    Stop(Instant),
    Interim(usize),
    Final(usize),
}

fn run_call(
    asm: &Assembled,
    config: &Config,
    silero: &Arc<SileroTractModel>,
    turn: Option<&dyn TurnModel>,
) -> Result<CallResult, BenchError> {
    let origin = SystemClock.now();
    let at = |s: f64| origin + Duration::from_secs_f64(s.max(0.0));
    let seconds_of = |t: Instant| t.saturating_duration_since(origin).as_secs_f64();

    // The front end: denoiser -> level gates -> VAD, as the engine's VAD worker runs it.
    let handle = NoiseFloorHandle::default();
    let gate = GateConfig {
        track_floor: true,
        ..config.gate
    };
    let model = Gated::new(silero.stream().map_err(model_error)?, gate, handle.clone());
    let mut stream = VadStream::new(model, config.vad);
    // The engine does not run a backend on a call below its minimum rate (`with_min_call_rate`).
    let used = config.force_narrowband || config.variant.min_call_hz() <= asm.path.call_hz();
    if used && let Some(d) = config.variant.build_blended(16_000, config.dry)? {
        stream = stream.with_denoiser(d).map_err(model_error)?;
    }
    let mut events: Vec<(f64, VadEvent)> = Vec::new();
    let mut floors: Vec<f32> = Vec::new();
    let mut scratch = Vec::new();
    for (i, frame) in asm.pcm.chunks(FRAME).enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let t = i as f64 * 0.02;
        scratch.clear();
        stream
            .push(frame, at(t), &mut scratch)
            .map_err(model_error)?;
        events.extend(scratch.drain(..).map(|e| (t + 0.02, e)));
        floors.push(handle.db());
    }
    #[allow(clippy::cast_precision_loss)]
    let end = asm.pcm.len() as f64 / HZ;
    events.extend(stream.finish().map(|e| (end, e)));

    // The oracle's transcripts: interims while a VAD segment is open, a final at its stop.
    let words_in = |from: f64, to: f64| -> String {
        asm.words
            .iter()
            .filter(|w| w.at >= from && w.at <= to)
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut texts: Vec<String> = Vec::new();
    let mut timeline = vec![Timed {
        at: asm.bot_start,
        kind: Kind::BotStart,
    }];
    let interims = |texts: &mut Vec<String>,
                    from_arrival: f64,
                    speech_start: f64,
                    until: f64,
                    timeline: &mut Vec<Timed>| {
        let mut k = 0.0;
        loop {
            let when = from_arrival + FIRST_INTERIM + k * INTERIM_EVERY;
            if when >= until {
                break;
            }
            let text = words_in(speech_start - 0.25, when - STT_LAG);
            if !text.is_empty() {
                texts.push(text);
                timeline.push(Timed {
                    at: when,
                    kind: Kind::Interim(texts.len() - 1),
                });
            }
            k += 1.0;
        }
    };
    let mut vad_starts = 0u32;
    let mut open: Option<(f64, f64)> = None; // (arrival, speech start)
    for (arrival, e) in &events {
        match e {
            VadEvent::Start { at: t } => {
                if *arrival >= asm.bot_start {
                    vad_starts += 1;
                }
                open = Some((*arrival, seconds_of(*t)));
                timeline.push(Timed {
                    at: *arrival,
                    kind: Kind::Start(*t),
                });
            }
            VadEvent::Stop { at: t } => {
                if let Some((from_arrival, speech_start)) = open.take() {
                    interims(
                        &mut texts,
                        from_arrival,
                        speech_start,
                        *arrival,
                        &mut timeline,
                    );
                    let text = words_in(speech_start - 0.25, *arrival);
                    timeline.push(Timed {
                        at: *arrival,
                        kind: Kind::Stop(*t),
                    });
                    texts.push(text);
                    timeline.push(Timed {
                        at: *arrival + 0.2,
                        kind: Kind::Final(texts.len() - 1),
                    });
                }
            }
        }
    }
    if let Some((from_arrival, speech_start)) = open {
        interims(
            &mut texts,
            from_arrival,
            speech_start,
            end + 3.0,
            &mut timeline,
        );
    }
    timeline.sort_by(|a, b| a.at.total_cmp(&b.at));

    // The turn machine. The first utterance is the first turn; the bot answers.
    let mut machine = TurnMachine::with_interruption(
        TurnPolicy::FixedSilence {
            silence: Duration::from_millis(700),
        },
        config.policy.base.clone(),
    );
    let mut actions: Vec<TurnAction> = Vec::new();
    let mut timer: Option<Instant> = None;
    let mut interrupts: Vec<f64> = Vec::new();
    let mut feed = |machine: &mut TurnMachine,
                    input: TurnInput,
                    now: Instant,
                    timer: &mut Option<Instant>,
                    interrupts: &mut Vec<f64>| {
        actions.clear();
        machine.handle(input, &mut actions);
        for a in &actions {
            match a {
                TurnAction::ArmTimer { deadline } => *timer = Some(*deadline),
                TurnAction::CancelTimer => *timer = None,
                TurnAction::Interrupt { .. } => interrupts.push(seconds_of(now)),
                _ => {}
            }
        }
    };
    let bot_recent = |t: f64| -> String {
        asm.bot_words
            .iter()
            .filter(|w| w.at <= t && w.at >= t - BOT_RECENT_S)
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let floor_at = |t: f64| -> f32 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let i = ((t / 0.02) as usize).min(floors.len().saturating_sub(1));
        floors.get(i).copied().unwrap_or(-100.0)
    };
    let mut last = 0.0f64;
    let mut candidate_arrival = 0.0f64;
    for p in &timeline {
        while let Some(d) = timer {
            if seconds_of(d) > p.at {
                break;
            }
            timer = None;
            feed(
                &mut machine,
                TurnInput::Tick { now: d },
                d,
                &mut timer,
                &mut interrupts,
            );
        }
        if let Some(adapt) = &config.policy.noise_adapt {
            machine.set_interruption(config.policy.base.adapted_to_noise(floor_at(p.at), adapt));
        }
        let now = at(p.at);
        last = p.at;
        let input = match &p.kind {
            Kind::BotStart => TurnInput::BotAudioStarted {
                epoch: machine.epoch(),
                at: now,
            },
            Kind::Start(t) => {
                candidate_arrival = p.at;
                TurnInput::VadStart { at: *t }
            }
            Kind::Stop(t) => TurnInput::VadStop { at: *t },
            Kind::Interim(i) => {
                let text = texts.get(*i).cloned().unwrap_or_default();
                if config.policy.echo_text {
                    // The engine's own rule, on the same inputs.
                    let age = Duration::from_secs_f64((p.at - candidate_arrival).max(0.0));
                    machine.set_echo(is_bot_echo(&text, &bot_recent(p.at), age));
                }
                TurnInput::SttInterim { text, at: now }
            }
            Kind::Final(i) => TurnInput::SttFinal {
                text: texts.get(*i).cloned().unwrap_or_default(),
                at: now,
            },
        };
        feed(&mut machine, input, now, &mut timer, &mut interrupts);
    }
    let mut guard = 0;
    while let Some(d) = timer {
        guard += 1;
        if guard > 64 || seconds_of(d) > last + 10.0 {
            break;
        }
        timer = None;
        feed(
            &mut machine,
            TurnInput::Tick { now: d },
            d,
            &mut timer,
            &mut interrupts,
        );
    }
    // Interruptions of the bot: those after the bot began to speak (the first turn ends by its own
    // silence, with no interruption).
    interrupts.retain(|t| *t >= asm.bot_start - 0.5);

    // Fragments and the Smart Turn verdict in barge-in calls.
    let mut fragments = 0;
    let mut flip = None;
    if let Some((a, b)) = asm.caller_span {
        let inside = events
            .iter()
            .filter(|(_, e)| match e {
                VadEvent::Start { at: t } => {
                    let s = seconds_of(*t);
                    s >= a - 0.3 && s <= b + 0.3
                }
                VadEvent::Stop { .. } => false,
            })
            .count();
        fragments = u32::try_from(inside.saturating_sub(1)).unwrap_or(0);
        if let (Some(model), Some(clean)) = (turn, asm.caller_only.as_ref()) {
            let heard = match config.variant.build_blended(16_000, config.dry)? {
                Some(mut d) if used => enhance(d.as_mut(), &asm.pcm)?,
                _ => asm.pcm.clone(),
            };
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let seg = [((a * HZ) as usize, (b * HZ) as usize)];
            let p = verdicts(&heard, &seg, model)?;
            let reference = verdicts(clean, &seg, model)?;
            flip = Some(p.first().map(|x| *x > 0.5) != reference.first().map(|x| *x > 0.5));
        }
    }
    Ok(CallResult {
        vad_starts,
        interrupts,
        flip,
        fragments,
    })
}

// ---------------------------------------------------------------------------------------------
// Aggregation

/// Counters of one cell (configuration x path x disturbance x level).
#[derive(Debug, Clone, Default)]
pub struct Tally {
    /// Quiet calls.
    pub quiet: u32,
    /// Quiet calls in which the VAD started at least once (before the guards).
    pub quiet_vad: u32,
    /// Quiet calls in which the bot was interrupted (after the guards): false barge-ins.
    pub quiet_interrupted: u32,
    /// Barge-in calls.
    pub barge: u32,
    /// Barge-in calls interrupted before the caller spoke.
    pub premature: u32,
    /// Barge-in calls in which the caller never got through.
    pub missed: u32,
    /// Time from the caller's first word to the interruption, ms.
    pub latency_ms: Vec<f64>,
    /// Fragments in barge-in calls.
    pub fragments: u32,
    /// Smart Turn verdicts and how many flipped.
    pub verdicts: u32,
    /// Flipped verdicts.
    pub flips: u32,
}

impl Tally {
    fn add(&mut self, other: &Self) {
        self.quiet += other.quiet;
        self.quiet_vad += other.quiet_vad;
        self.quiet_interrupted += other.quiet_interrupted;
        self.barge += other.barge;
        self.premature += other.premature;
        self.missed += other.missed;
        self.latency_ms.extend_from_slice(&other.latency_ms);
        self.fragments += other.fragments;
        self.verdicts += other.verdicts;
        self.flips += other.flips;
    }

    fn median_ms(&self) -> Option<f64> {
        let mut v = self.latency_ms.clone();
        if v.is_empty() {
            return None;
        }
        v.sort_by(f64::total_cmp);
        v.get(v.len() / 2).copied()
    }
}

fn tally_of(sc: &Scenario, r: &CallResult, caller_start: Option<f64>) -> Tally {
    let mut t = Tally::default();
    match caller_start {
        None => {
            t.quiet = 1;
            t.quiet_vad = u32::from(r.vad_starts > 0);
            t.quiet_interrupted = u32::from(!r.interrupts.is_empty());
        }
        Some(start) => {
            t.barge = 1;
            match r.interrupts.first() {
                Some(first) if *first < start - 0.1 => t.premature = 1,
                Some(first) => t.latency_ms.push((first - start) * 1000.0),
                None => t.missed = 1,
            }
            t.fragments = r.fragments;
            if let Some(flip) = r.flip {
                t.verdicts = 1;
                t.flips = u32::from(flip);
            }
        }
    }
    let _ = sc;
    t
}

/// Key of a cell.
pub type CellKey = (String, Path16, Disturbance, i32);

/// The scenarios of the standard suite.
pub fn standard_scenarios(suite: &Suite, paths: &[Path16]) -> Vec<Scenario> {
    let mut out = Vec::new();
    let voices = suite.voices.len().max(1);
    let noises = suite.noise.len().max(1);
    for &path in paths {
        // Quiet calls.
        for clip in 0..noises {
            for snr in [10.0, 5.0, 0.0] {
                out.push(Scenario {
                    disturbance: Disturbance::Noise,
                    level_db: snr,
                    clip,
                    caller: None,
                    path,
                });
            }
        }
        for clip in 0..voices {
            for rel in [-5.0, -10.0, -15.0, -20.0] {
                out.push(Scenario {
                    disturbance: Disturbance::Speech,
                    level_db: rel,
                    clip,
                    caller: None,
                    path,
                });
            }
            for rel in [-15.0, -25.0] {
                out.push(Scenario {
                    disturbance: Disturbance::Echo,
                    level_db: rel,
                    clip,
                    caller: None,
                    path,
                });
            }
        }
        // Barge-in calls: every utterance of every fixture, a few disturbances each.
        let mut n = 0usize;
        for (f, sp) in suite.speech.iter().enumerate() {
            for s in 0..sp.segments.len() {
                for rel in [-5.0, -10.0, -15.0, -20.0] {
                    out.push(Scenario {
                        disturbance: Disturbance::Speech,
                        level_db: rel,
                        clip: n,
                        caller: Some((f, s)),
                        path,
                    });
                    n += 1;
                }
                for rel in [-15.0, -25.0] {
                    out.push(Scenario {
                        disturbance: Disturbance::Echo,
                        level_db: rel,
                        clip: n,
                        caller: Some((f, s)),
                        path,
                    });
                    n += 1;
                }
                for snr in [10.0, 0.0] {
                    out.push(Scenario {
                        disturbance: Disturbance::Noise,
                        level_db: snr,
                        clip: n,
                        caller: Some((f, s)),
                        path,
                    });
                    n += 1;
                }
            }
        }
    }
    out
}

/// Results of a run: tallies per cell and the CPU spent on the gates.
pub struct CallsReport {
    /// Cells.
    pub cells: BTreeMap<CellKey, Tally>,
    /// Configurations in order.
    pub configs: Vec<String>,
    /// Paths in order.
    pub paths: Vec<Path16>,
    /// Scenarios per configuration.
    pub scenarios: usize,
}

#[allow(clippy::cast_possible_truncation)]
fn level_key(db: f32) -> i32 {
    (db * 10.0).round() as i32
}

/// Runs every configuration over every scenario, on `threads` threads.
///
/// # Errors
/// [`BenchError`] when a model or a mix fails.
pub fn run_suite(
    suite: &Suite,
    configs: &[Config],
    scenarios: &[Scenario],
    silero: &Arc<SileroTractModel>,
    turn: Option<Arc<dyn TurnModel>>,
    threads: usize,
) -> Result<CallsReport, BenchError> {
    // The audio of each scenario once; the configurations share it.
    let assembled: Vec<Assembled> = scenarios
        .iter()
        .map(|sc| assemble(suite, sc))
        .collect::<Result<_, _>>()?;
    let jobs: Vec<(usize, usize)> = (0..configs.len())
        .flat_map(|c| (0..scenarios.len()).map(move |s| (c, s)))
        .collect();
    let next = AtomicUsize::new(0);
    let results: Vec<Result<Vec<(usize, Tally)>, BenchError>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads.max(1))
            .map(|_| {
                scope.spawn(|| -> Result<Vec<(usize, Tally)>, BenchError> {
                    let mut local = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(c, s)) = jobs.get(i) else { break };
                        let (Some(config), Some(sc), Some(asm)) =
                            (configs.get(c), scenarios.get(s), assembled.get(s))
                        else {
                            continue;
                        };
                        let result = run_call(asm, config, silero, turn.as_deref())?;
                        let tally = tally_of(sc, &result, asm.caller_span.map(|(a, _)| a));
                        local.push((i, tally));
                    }
                    Ok(local)
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| {
                w.join()
                    .unwrap_or_else(|_| Err(model_error("a worker panicked")))
            })
            .collect()
    });
    let mut cells: BTreeMap<CellKey, Tally> = BTreeMap::new();
    for r in results {
        for (i, tally) in r? {
            let Some(&(c, s)) = jobs.get(i) else { continue };
            let (Some(config), Some(sc)) = (configs.get(c), scenarios.get(s)) else {
                continue;
            };
            cells
                .entry((
                    config.name.clone(),
                    sc.path,
                    sc.disturbance,
                    level_key(sc.level_db),
                ))
                .or_default()
                .add(&tally);
        }
    }
    Ok(CallsReport {
        cells,
        configs: configs.iter().map(|c| c.name.clone()).collect(),
        paths: {
            let mut p: Vec<Path16> = Vec::new();
            for sc in scenarios {
                if !p.contains(&sc.path) {
                    p.push(sc.path);
                }
            }
            p
        },
        scenarios: scenarios.len(),
    })
}

fn sum<'a>(
    report: &'a CallsReport,
    config: &str,
    path: Path16,
    filter: impl Fn(Disturbance, i32) -> bool + 'a,
) -> Tally {
    let mut total = Tally::default();
    for ((c, p, d, l), t) in &report.cells {
        if c == config && *p == path && filter(*d, *l) {
            total.add(t);
        }
    }
    total
}

/// One table per path: a row per configuration.
pub fn calls_markdown(
    report: &CallsReport,
    environment: &[String],
    title: &str,
    notes: &str,
) -> String {
    let mut out = format!("# {title}\n\n");
    for line in environment {
        let _ = writeln!(out, "- {line}");
    }
    let _ = write!(out, "\n{notes}\n\n");
    for &path in &report.paths {
        let _ = writeln!(out, "## {}\n", path.name());
        out.push_str(
            "| configuration | quiet calls interrupted: speech | echo | noise | (VAD starts, all quiet) | real barge-ins missed | interrupted too early | time to interrupt, median | fragments | Smart Turn flips |\n\
             |---|---|---|---|---|---|---|---|---|---|\n",
        );
        for config in &report.configs {
            let speech = sum(report, config, path, |d, _| d == Disturbance::Speech);
            let echo = sum(report, config, path, |d, _| d == Disturbance::Echo);
            let noise = sum(report, config, path, |d, _| d == Disturbance::Noise);
            let all = sum(report, config, path, |_, _| true);
            let _ = writeln!(
                out,
                "| {config} | {}/{} | {}/{} | {}/{} | {}/{} | {}/{} | {} | {} | {} | {}/{} |",
                speech.quiet_interrupted,
                speech.quiet,
                echo.quiet_interrupted,
                echo.quiet,
                noise.quiet_interrupted,
                noise.quiet,
                all.quiet_vad,
                all.quiet,
                all.missed,
                all.barge,
                all.premature,
                all.median_ms()
                    .map_or("-".to_owned(), |m| format!("{m:.0} ms")),
                all.fragments,
                all.flips,
                all.verdicts,
            );
        }
        out.push('\n');
    }
    out
}

/// Per-level rows for the background speech cells of one configuration set.
pub fn levels_markdown(report: &CallsReport, disturbance: Disturbance, unit: &str) -> String {
    let mut levels: Vec<i32> = report
        .cells
        .keys()
        .filter(|k| k.2 == disturbance)
        .map(|k| k.3)
        .collect();
    levels.sort_unstable();
    levels.dedup();
    let mut out = String::new();
    for &path in &report.paths {
        let _ = writeln!(
            out,
            "### {}: {} by level ({unit})\n",
            path.name(),
            disturbance.name()
        );
        out.push_str("| configuration |");
        for l in &levels {
            let _ = write!(out, " {:+} dB |", f64::from(*l) / 10.0);
        }
        out.push_str("\n|---|");
        for _ in &levels {
            out.push_str("---|");
        }
        out.push('\n');
        for config in &report.configs {
            let _ = write!(out, "| {config} |");
            for &l in &levels {
                let t = sum(report, config, path, |d, lv| d == disturbance && lv == l);
                let _ = write!(
                    out,
                    " {}/{} · {}/{} |",
                    t.quiet_interrupted, t.quiet, t.missed, t.barge
                );
            }
            out.push('\n');
        }
        out.push('\n');
    }
    out
}
