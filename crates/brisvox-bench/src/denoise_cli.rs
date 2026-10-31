//! `brisvox-bench denoise`: the noise suppression measurements (feature `denoise`).
//!
//! ```text
//! brisvox-bench denoise cpu [--seconds 60] [--backends none,rnnoise] [--fixtures a,b] [--out FILE]
//! brisvox-bench denoise quality --models DIR [--backends none,rnnoise] [--paths wideband,telephone]
//!                            [--snrs 20,10,5,0] [--fixtures a,b] [--noise-dir DIR]
//!                            [--noise-alone-s 8] [--out FILE]
//! brisvox-bench denoise calls --set guards|gates|vad|denoisers|all --models DIR [--threads 8]
//!                            [--voices-dir DIR] [--noise-dir DIR] [--paths wideband,telephone]
//!                            [--levels] [--out FILE]
//! brisvox-bench denoise safety [--backends none,rnnoise,hush] [--paths wideband,telephone] [--force] [--out FILE]
//! brisvox-bench denoise transcribe --stt-url ws://HOST:PORT [--backends none,rnnoise]
//!                            [--paths wideband,telephone] [--snrs 20,10,5,0] [--noise-limit N]
//!                            [--no-clean] [--dump-dir DIR] [--dump-only] [--language en]
//!                            [--out-jsonl FILE] [--out FILE]
//! ```
//! `--backends` also takes `fastenhancer` (an evaluation backend, feature `denoise-fastenhancer`,
//! the model from `--fastenhancer-model`, default `<models>/fastenhancer_t.onnx`).
//! Method and results: `docs/LATENCY.md`, `bench/results/denoise-*.md`.

use std::path::PathBuf;
use std::time::Duration;

use brisvox_bench::BenchError;
use brisvox_bench::denoise_calls::{
    Disturbance, Suite, calls_markdown, levels_markdown, load_voices, run_suite, standard_scenarios,
};
use brisvox_bench::denoise_catalog::catalog;
use brisvox_bench::denoise_eval::{
    EvalConfig, Path16, Variant, cpu, cpu_markdown, eval_markdown, evaluate, load_noise,
    load_speech,
};
use brisvox_bench::denoise_safety::{safety, safety_markdown};
use brisvox_bench::denoise_stt::{TranscribeConfig, jsonl, markdown, run};
use brisvox_inference::silero_tract::SileroTractModel;
use brisvox_inference::smart_turn::MODEL_FILE;
use brisvox_inference::smart_turn::ort_backend::SmartTurnOrt;

use crate::{USAGE, emit, environment, parse, parse_list, unknown};

const DEFAULT_FIXTURES: &str = "en_two_utterances,tr_two_utterances,en_tr_mixed";

/// Flags shared by the subcommands.
struct Common {
    fixtures: Vec<String>,
    fixtures_dir: Option<PathBuf>,
    noise_dir: Option<PathBuf>,
    backends: Vec<String>,
    fastenhancer: Option<PathBuf>,
    paths: Vec<String>,
    snrs: Vec<f32>,
    models: Option<PathBuf>,
    out: Option<String>,
}

impl Common {
    fn new() -> Self {
        Self {
            fixtures: DEFAULT_FIXTURES.split(',').map(str::to_owned).collect(),
            fixtures_dir: None,
            noise_dir: None,
            backends: vec!["none".into(), "rnnoise".into()],
            fastenhancer: None,
            paths: vec!["wideband".into(), "telephone".into()],
            snrs: vec![20.0, 10.0, 5.0, 0.0],
            models: None,
            out: None,
        }
    }

    /// Handles `flag` if it is a shared one.
    fn flag(
        &mut self,
        flag: &str,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<bool, BenchError> {
        match flag {
            "--fixtures" => self.fixtures = parse_list("--fixtures", args.next())?,
            "--fixtures-dir" => {
                self.fixtures_dir = Some(parse::<String>(flag, args.next())?.into())
            }
            "--noise-dir" => self.noise_dir = Some(parse::<String>(flag, args.next())?.into()),
            "--backends" => self.backends = parse_list("--backends", args.next())?,
            "--fastenhancer-model" => {
                self.fastenhancer = Some(parse::<String>(flag, args.next())?.into());
            }
            "--paths" => self.paths = parse_list("--paths", args.next())?,
            "--snrs" => self.snrs = parse_list("--snrs", args.next())?,
            "--models" => self.models = Some(parse::<String>(flag, args.next())?.into()),
            "--out" => self.out = Some(parse("--out", args.next())?),
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn variants(&self) -> Result<Vec<Variant>, BenchError> {
        self.backends
            .iter()
            .map(|name| match name.as_str() {
                "none" => Ok(Variant::None),
                "rnnoise" => Ok(Variant::Rnnoise),
                "hush" => Ok(Variant::Hush),
                "fastenhancer" => {
                    let path = self.fastenhancer.clone().unwrap_or_else(|| {
                        self.models
                            .clone()
                            .unwrap_or_else(|| PathBuf::from("models"))
                            .join("fastenhancer_t.onnx")
                    });
                    Ok(Variant::FastEnhancer(path))
                }
                other => Err(BenchError::Config(format!(
                    "--backends: none, rnnoise, hush or fastenhancer, not {other}\n{USAGE}"
                ))),
            })
            .collect()
    }

    fn paths(&self) -> Result<Vec<Path16>, BenchError> {
        self.paths
            .iter()
            .map(|name| match name.as_str() {
                "wideband" => Ok(Path16::Wideband),
                "telephone" => Ok(Path16::Telephone),
                other => Err(BenchError::Config(format!(
                    "--paths: wideband or telephone, not {other}\n{USAGE}"
                ))),
            })
            .collect()
    }

    fn noise_dir(&self) -> PathBuf {
        self.noise_dir
            .clone()
            .unwrap_or_else(|| brisvox_sim::fixture::fixtures_dir().join("noise"))
    }
}

/// `denoise cpu|quality|transcribe ...`.
pub(crate) fn denoise_cmd(
    mut args: impl Iterator<Item = String>,
    command: &str,
) -> Result<bool, BenchError> {
    match args.next().as_deref() {
        Some("cpu") => cpu_cmd(args, command),
        Some("quality") => quality_cmd(args, command),
        Some("calls") => calls_cmd(args, command),
        Some("safety") => safety_cmd(args, command),
        Some("transcribe") => transcribe_cmd(args, command),
        _ => Err(BenchError::Config(format!(
            "denoise: cpu, quality or transcribe\n{USAGE}"
        ))),
    }
}

fn cpu_cmd(mut args: impl Iterator<Item = String>, command: &str) -> Result<bool, BenchError> {
    let mut common = Common::new();
    let mut seconds = 60usize;
    while let Some(flag) = args.next() {
        if flag == "--seconds" {
            seconds = parse("--seconds", args.next())?;
        } else if !common.flag(&flag, &mut args)? {
            return Err(unknown(&flag));
        }
    }
    let speech = load_speech(common.fixtures_dir.as_deref(), &common.fixtures)?;
    let variants: Vec<Variant> = common
        .variants()?
        .into_iter()
        .filter(|v| *v != Variant::None)
        .collect();
    eprintln!("denoise cpu: {seconds} s of audio per backend and rate ...");
    let rows = cpu(&variants, &speech, seconds)?;
    let setup = format!(
        "{seconds} s of speech fixtures per backend and rate, one thread; the 8 kHz rows carry a \
         telephone call's audio at its own rate (the backend converts to its model's)"
    );
    let markdown = cpu_markdown(&rows, &environment(0, command, &setup));
    emit(&markdown, common.out, &[])
}

fn safety_cmd(mut args: impl Iterator<Item = String>, command: &str) -> Result<bool, BenchError> {
    let mut common = Common::new();
    let mut force = false;
    common.backends = vec!["none".into(), "rnnoise".into(), "hush".into()];
    while let Some(flag) = args.next() {
        if flag == "--force" {
            force = true;
        } else if !common.flag(&flag, &mut args)? {
            return Err(unknown(&flag));
        }
    }
    let silero = SileroTractModel::bundled()
        .map_err(|e| BenchError::Config(format!("silero model: {e}")))?;
    let speech = load_speech(common.fixtures_dir.as_deref(), &common.fixtures)?;
    let rows = safety(
        &common.variants()?,
        &common.paths()?,
        &speech,
        &silero,
        force,
    )?;
    let setup = if force {
        "backends measured on every path, the engine's refusal below their minimum rate ignored"
    } else {
        "in process; a backend below its minimum sample rate on a path is refused as the engine does"
    };
    let (markdown, problems) = safety_markdown(&rows, &environment(0, command, setup));
    // A backend that fails the check is the result of this run, not a broken run: the report is
    // written to `--out` as well, and the exit status still says the check failed.
    if !problems.is_empty()
        && let Some(path) = &common.out
    {
        std::fs::write(path, &markdown)?;
        eprintln!("wrote {path} (the safety check failed: exit status 1)");
    }
    emit(&markdown, common.out, &problems)
}

fn calls_cmd(mut args: impl Iterator<Item = String>, command: &str) -> Result<bool, BenchError> {
    let mut common = Common::new();
    let (mut set, mut threads, mut levels) = ("guards".to_owned(), 8usize, false);
    let mut voices_dir = None::<PathBuf>;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--set" => set = parse("--set", args.next())?,
            "--threads" => threads = parse("--threads", args.next())?,
            "--levels" => levels = true,
            "--voices-dir" => voices_dir = Some(parse::<String>(&flag, args.next())?.into()),
            other => {
                if !common.flag(other, &mut args)? {
                    return Err(unknown(other));
                }
            }
        }
    }
    let models = common
        .models
        .clone()
        .ok_or_else(|| BenchError::Config(format!("denoise calls needs --models DIR\n{USAGE}")))?;
    let turn = SmartTurnOrt::load(&models.join(MODEL_FILE))
        .map_err(|e| BenchError::Config(format!("smart turn model: {e}")))?;
    let silero = SileroTractModel::bundled()
        .map_err(|e| BenchError::Config(format!("silero model: {e}")))?;
    let suite = Suite {
        speech: load_speech(common.fixtures_dir.as_deref(), &common.fixtures)?,
        voices: load_voices(
            &voices_dir.unwrap_or_else(|| brisvox_sim::fixture::fixtures_dir().join("interferers")),
        )?,
        noise: load_noise(&common.noise_dir())?,
    };
    let paths = common.paths()?;
    let scenarios = standard_scenarios(&suite, &paths);
    let configs = catalog(&set)?;
    eprintln!(
        "denoise calls: {} configurations x {} calls on {threads} threads ...",
        configs.len(),
        scenarios.len()
    );
    let report = run_suite(&suite, &configs, &scenarios, &silero, Some(turn), threads)?;
    let setup =
        format!("set `{set}`, in process, the real TurnMachine and InterruptionPolicy, oracle STT");
    let notes = "Cells are calls where the bot is speaking. *Quiet calls interrupted*: calls in which the \
        caller says nothing and the bot was interrupted anyway (false barge-ins, after the guards); the \
        columns split them by what is on the line. *VAD starts*: the same quiet calls counted at the VAD, \
        before the guards. *Missed*: calls in which the caller spoke over the bot and never got through. \
        *Too early*: the bot was interrupted before the caller spoke. *Time to interrupt*: from the \
        caller's first word to the interruption. *Fragments*: extra VAD starts inside the caller's \
        utterance. *Flips*: Smart Turn verdicts at the end of the caller's utterance that differ from \
        the verdict on the caller's audio alone.";
    let mut markdown = calls_markdown(
        &report,
        &environment(0, command, &setup),
        "Interruptions of the bot with the engine's guards",
        notes,
    );
    if levels {
        let _ = std::fmt::Write::write_str(
            &mut markdown,
            "\n## By level\n\nEach cell: quiet calls interrupted/quiet calls · real barge-ins missed/barge-in calls.\n\n",
        );
        markdown.push_str(&levels_markdown(
            &report,
            Disturbance::Speech,
            "dB below the caller",
        ));
        markdown.push_str(&levels_markdown(
            &report,
            Disturbance::Noise,
            "dB signal-to-noise",
        ));
        markdown.push_str(&levels_markdown(
            &report,
            Disturbance::Echo,
            "dB below the caller",
        ));
    }
    emit(&markdown, common.out, &[])
}

fn quality_cmd(mut args: impl Iterator<Item = String>, command: &str) -> Result<bool, BenchError> {
    let mut common = Common::new();
    let mut noise_alone_s = 8usize;
    while let Some(flag) = args.next() {
        if flag == "--noise-alone-s" {
            noise_alone_s = parse("--noise-alone-s", args.next())?;
        } else if !common.flag(&flag, &mut args)? {
            return Err(unknown(&flag));
        }
    }
    let models = common.models.clone().ok_or_else(|| {
        BenchError::Config(format!("denoise quality needs --models DIR\n{USAGE}"))
    })?;
    let turn = SmartTurnOrt::load(&models.join(MODEL_FILE))
        .map_err(|e| BenchError::Config(format!("smart turn model: {e}")))?;
    let silero = SileroTractModel::bundled()
        .map_err(|e| BenchError::Config(format!("silero model: {e}")))?;
    let config = EvalConfig {
        variants: common.variants()?,
        paths: common.paths()?,
        snrs: common.snrs.clone(),
        speech: load_speech(common.fixtures_dir.as_deref(), &common.fixtures)?,
        noise: load_noise(&common.noise_dir())?,
        turn,
        noise_alone_s,
    };
    eprintln!(
        "denoise quality: {} backends x {} paths x {} SNRs x {} fixtures x {} noises ...",
        config.variants.len(),
        config.paths.len(),
        config.snrs.len(),
        config.speech.len(),
        config.noise.len()
    );
    let mut report = evaluate(&config, &silero)?;
    if let Some(dir) = &common.noise_dir {
        report.noise_source = dir.display().to_string();
    }
    let setup = "in process, no pool: the VAD stream and Smart Turn run on this thread";
    let markdown = eval_markdown(&report, &environment(0, command, setup));
    emit(&markdown, common.out, &[])
}

fn transcribe_cmd(
    mut args: impl Iterator<Item = String>,
    command: &str,
) -> Result<bool, BenchError> {
    let mut common = Common::new();
    let (mut url, mut limit, mut clean) = (None::<String>, usize::MAX, true);
    let (mut dump_dir, mut dump_only, mut language, mut out_jsonl) =
        (None::<PathBuf>, false, None::<String>, None::<String>);
    let mut timeout = Duration::from_secs(30);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--stt-url" => url = Some(parse("--stt-url", args.next())?),
            "--noise-limit" => limit = parse("--noise-limit", args.next())?,
            "--no-clean" => clean = false,
            "--dump-dir" => dump_dir = Some(parse::<String>("--dump-dir", args.next())?.into()),
            "--dump-only" => dump_only = true,
            "--language" => language = Some(parse("--language", args.next())?),
            "--out-jsonl" => out_jsonl = Some(parse("--out-jsonl", args.next())?),
            "--timeout-s" => timeout = Duration::from_secs(parse("--timeout-s", args.next())?),
            other => {
                if !common.flag(other, &mut args)? {
                    return Err(unknown(other));
                }
            }
        }
    }
    if url.is_none() && !dump_only {
        return Err(BenchError::Config(format!(
            "denoise transcribe needs --stt-url ws://HOST:PORT (or --dump-only)\n{USAGE}"
        )));
    }
    let mut noise = load_noise(&common.noise_dir())?;
    noise.truncate(limit);
    let config = TranscribeConfig {
        url: url.unwrap_or_default(),
        variants: common.variants()?,
        paths: common.paths()?,
        snrs: common.snrs.clone(),
        speech: load_speech(common.fixtures_dir.as_deref(), &common.fixtures)?,
        noise,
        clean,
        dump_dir,
        dump_only,
        language,
        timeout,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let rows = runtime.block_on(run(&config))?;
    if let Some(path) = out_jsonl {
        std::fs::write(&path, jsonl(&rows)?)?;
        eprintln!("wrote {path}");
    }
    if config.dump_only {
        eprintln!("dumped {} clips (no server contacted)", rows.len());
        return Ok(true);
    }
    let setup = format!("STT server {}", config.url);
    let markdown = markdown(&rows, &environment(2, command, &setup));
    emit(&markdown, common.out, &[])
}
