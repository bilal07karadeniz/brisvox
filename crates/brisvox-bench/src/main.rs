//! `brisvox-bench`: benchmark harness.
//!
//! ```text
//! brisvox-bench sim-load [--sessions 10,100,300,500] [--turns 3] [--seed 1] [--workers N]
//!                        [--out bench/results/sim-baseline.md]
//! brisvox-bench infer-load [--sessions 500] [--costs-us 100,250,500] [--threads 4]
//!                          [--seconds 10] [--out bench/results/inference-pool.md]
//! brisvox-bench fixtures [--sessions 10] [--fixtures a,b] [--seed 1] [--workers N]
//!                        [--out bench/results/fixtures-sim.md]
//! brisvox-bench v2v (--sim [--target engine|mock-bot] | --url ws://HOST:PORT/PATH)
//!                   [--fixtures a,b] [--runs 10] [--min-gap-ms 4000] [--out-md FILE]
//!                   [--out-json FILE] [--strict] [--barge-in-runs N] ...
//! brisvox-bench capacity (--sim | --url URL [--pid PID] --floor-ms MS) [--steps 1,5,10,20] ...
//! brisvox-bench model-cpu --models DIR [--configs 2:1,1:1,2:0,1:0] [--rounds 30] [--batch 20]
//!                         [--gap-ms 40] [--silero-batch 2000] [--out FILE]   (feature `models`)
//! brisvox-bench denoise (cpu | quality --models DIR | calls --set S --models DIR | safety |
//!                        transcribe --stt-url URL) [--backends none,rnnoise]
//!                        [--paths wideband,telephone] [--snrs 20,10,5,0] [--out FILE]   (feature `denoise`)
//! brisvox-bench engine-server [--bind 127.0.0.1:0] [--workers N] [--max-calls N] ...
//! ```
//!
//! `capacity` ramps N concurrent calls against a server and finds the largest N within a
//! latency SLO (`capacity_cli.rs`); `engine-server` is the engine over mock providers as
//! a server process (what `capacity --sim` measures).
//!
//! `v2v` is the voice-to-voice latency benchmark over the websocket protocol (see
//! `bench/results/README.md`): `--sim` measures the engine over mock providers in process
//! (`--target mock-bot`: the mock bot, a self-check of the harness and the transport), `--url`
//! (or the `BRISVOX_BENCH_URL` environment variable) any server.

mod capacity_cli;
#[cfg(feature = "denoise")]
mod denoise_cli;

use std::process::ExitCode;
use std::time::Duration;

use brisvox_bench::fixture_load::{self, FixtureLoadConfig, FixtureReport};
use brisvox_bench::infer_load::{self, Arrivals, InferLoadConfig};
use brisvox_bench::sim_load::{self, LoadConfig, LoadReport};
use brisvox_bench::v2v::engine_target::{EndOfTurn, EngineTargetConfig, SttInterims};
use brisvox_bench::v2v::mock_bot::MockBotConfig;
use brisvox_bench::v2v::report::{CallKind, RunReport};
use brisvox_bench::v2v::{self, BenchConfig, Target};
use brisvox_bench::{BenchError, resample_bench, spec_eval};

const USAGE: &str = "usage:\n  brisvox-bench sim-load [--sessions 10,100,300,500] [--turns 3] \
                     [--seed 1] [--workers N] [--out FILE]\n  brisvox-bench infer-load \
                     [--sessions 500] [--costs-us 100,250,500] [--threads 4] [--seconds 10] \
                     [--arrivals timers,paced] [--out FILE]\n  brisvox-bench resample \
                     [--seconds 5] [--out FILE]\n  brisvox-bench fixtures [--sessions 10] \
                     [--fixtures en_two_utterances,tr_two_utterances,en_tr_mixed] [--seed 1] \
                     [--workers N] [--out FILE]\n  brisvox-bench v2v (--sim [--target \
                     engine|mock-bot] | --url URL) [--fixtures a,b] [--fixtures-dir DIR] \
                     [--runs 10] [--concurrency 1 (live only)] [--min-gap-ms 4000] [--pause-ms 300] \
                     [--response-timeout-ms 10000] [--silence-peak 200] \
                     [--overlap-guard-ms 100] [--strict] [--keep-invalid (live)] [--out-md FILE] [--out-json FILE]\n    \
                     engine target: [--engine-turn smart|fixed:MS] [--vad-threshold 500] \
                     [--vad-start-ms 40] [--vad-stop-ms 300] [--stt-final-ms 150] \
                     [--llm-first-token-ms 350] [--llm-inter-token-ms 30] \
                     [--tts-first-byte-ms 150] [--pool-threads 2] [--pool-queue max(1024,2*max-calls)] [--barge-in-runs RUNS] \
                     [--barge-in-gap-ms 1600] [--memory-calls 8] [--speculation | \
                     --speculation-final-only] [--speculation-gate-ms 0] \
                     [--stt-interims lagging|converged]\n    \
                     mock-bot target: [--bot-endpoint-ms 500] [--bot-delay-ms 300] \
                     [--bot-jitter-ms 0] [--bot-reply-ms 600] [--bot-seed 1]\n    \
                     (BRISVOX_BENCH_URL: default --url)\n  brisvox-bench speculation \
                     [--seeds 20] [--fixtures a,b] [--turn adaptive,fixed:450,fixed:700] \
                     [--gates-ms 100,200|none] [--resume-pauses-ms 350,400,600|none] \
                     [--out FILE]\n  brisvox-bench capacity (--sim | --url URL [--pid PID] \
                     --floor-ms MS) [--label NAME] [--note TEXT] [--fixture en_two_utterances] \
                     [--steps 1,5,10,20] [--rounds 2] [--warmup-calls 1] [--spread-ms 4000] [--min-gap-ms 4000] \
                     [--pause-ms 200] [--cooldown-ms 3000] [--slo-over-floor-ms 250] \
                     [--max-bad-rate 0] [--resolution 0] [--keep-going] [--pid-only] \
                     [--client-lateness-ms 100] [--client-late-share 0.01] [--response-timeout-ms 10000] [--sample-ms 500] \
                     [--client-workers 2] [--out-md FILE] [--out-json FILE]\n    --sim: \
                     [--max-calls 1024] [--server-workers N] [--models DIR] [--turn-threads N] and the engine target flags of v2v\n  \
                     brisvox-bench engine-server [--bind 127.0.0.1:0] [--fixture NAME] \
                     [--min-gap-ms 4000] [--max-calls 1024] [--workers N] \
                     [--exit-on-stdin-eof] [--models DIR] [--turn-threads N] and the engine target flags of v2v\n  \
                     brisvox-bench model-cpu --models DIR [--configs 2:1,1:1,2:0,1:0] \
                     [--rounds 30] [--batch 20] [--gap-ms 40] [--silero-batch 2000] [--out FILE]\n  \
                     brisvox-bench denoise (cpu [--seconds 60] | quality --models DIR | calls --set \
                     guards|gates|vad|denoisers|all --models DIR [--threads 8] [--levels] \
                     [--voices-dir DIR] | safety [--force] | transcribe \
                     --stt-url ws://HOST:PORT [--noise-limit N] [--no-clean] [--dump-dir DIR] \
                     [--dump-only] [--language en] [--out-jsonl FILE]) [--backends none,rnnoise] \
                     [--paths wideband,telephone] [--snrs 20,10,5,0] [--fixtures a,b] \
                     [--noise-dir DIR] [--out FILE]  (needs --features denoise)";

#[cfg(feature = "denoise")]
fn denoise_cmd(args: impl Iterator<Item = String>, command: &str) -> Result<bool, BenchError> {
    denoise_cli::denoise_cmd(args, command)
}

#[cfg(not(feature = "denoise"))]
fn denoise_cmd(_: impl Iterator<Item = String>, _: &str) -> Result<bool, BenchError> {
    Err(BenchError::Config(
        "denoise needs a build with --features denoise".into(),
    ))
}

fn resample_cmd(mut args: impl Iterator<Item = String>, command: &str) -> Result<bool, BenchError> {
    let (mut seconds, mut out) = (5usize, None);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--seconds" => seconds = parse("--seconds", args.next())?,
            "--out" => out = Some(parse("--out", args.next())?),
            other => return Err(unknown(other)),
        }
    }
    eprintln!("resample: {seconds} s of audio per pair ...");
    let reports = resample_bench::run(seconds)?;
    let alias = resample_bench::alias_rejection_db()?;
    let setup = "single thread, steady state (fresh instance after a warm-up run)";
    let markdown = resample_bench::markdown(&reports, alias, &environment(0, command, setup));
    emit(&markdown, out, &[])
}

/// `model-cpu --models DIR [--configs 2:1,1:1,2:0,1:0] [--rounds 30] [--batch 20] [--gap-ms 40]
/// [--silero-batch 2000|0] [--out FILE]` (needs the `models` feature).
#[cfg(feature = "models")]
fn model_cpu_cmd(
    mut args: impl Iterator<Item = String>,
    command: &str,
) -> Result<bool, BenchError> {
    use brisvox_bench::model_cpu;
    let mut models = None;
    let mut out = None;
    let mut config = model_cpu::ModelCpuConfig::new(std::path::PathBuf::new());
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--models" => models = Some(parse::<String>("--models", args.next())?),
            "--configs" => {
                config.smart_turn =
                    model_cpu::parse_configs(&parse::<String>("--configs", args.next())?)?;
            }
            "--rounds" => config.rounds = parse("--rounds", args.next())?,
            "--batch" => config.batch = parse("--batch", args.next())?,
            "--gap-ms" => {
                config.gap = Duration::from_millis(parse("--gap-ms", args.next())?);
            }
            "--silero-batch" => config.silero_batch = parse("--silero-batch", args.next())?,
            "--out" => out = Some(parse("--out", args.next())?),
            other => return Err(unknown(other)),
        }
    }
    config.models_dir = models
        .ok_or_else(|| BenchError::Config(format!("model-cpu needs --models DIR\n{USAGE}")))?
        .into();
    let report = model_cpu::run(&config)?;
    let setup = "single measuring thread; CPU time is the whole process's (ONNX Runtime workers \
                 included), configurations interleaved round by round";
    let markdown = model_cpu::markdown(&report, &environment(0, command, setup));
    emit(&markdown, out, &[])
}

#[cfg(not(feature = "models"))]
fn model_cpu_cmd(_: impl Iterator<Item = String>, _: &str) -> Result<bool, BenchError> {
    Err(BenchError::Config(
        "model-cpu needs a build with --features models".into(),
    ))
}

fn parse<T: std::str::FromStr>(flag: &str, value: Option<String>) -> Result<T, BenchError> {
    value
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| BenchError::Config(format!("{flag} needs a value\n{USAGE}")))
}

fn parse_list<T: std::str::FromStr>(
    flag: &str,
    value: Option<String>,
) -> Result<Vec<T>, BenchError> {
    let list: String = parse(flag, value)?;
    list.split(',')
        .map(|n| parse(flag, Some(n.trim().to_owned())))
        .collect()
}

/// A comma-separated list of milliseconds as durations; `none` is the empty list.
fn ms_list(flag: &str, value: Option<String>) -> Result<Vec<Duration>, BenchError> {
    if value.as_deref() == Some("none") {
        return Ok(Vec::new());
    }
    Ok(parse_list::<u64>(flag, value)?
        .into_iter()
        .map(Duration::from_millis)
        .collect())
}

fn unknown(flag: &str) -> BenchError {
    BenchError::Config(format!("unknown flag {flag}\n{USAGE}"))
}

fn environment(workers: usize, command: &str, setup: &str) -> Vec<String> {
    let cores = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    let mut lines = vec![
        format!("command: `{command}`"),
        format!(
            "brisvox {} ({} build), {} {}, {cores} logical cores, {workers} tokio workers \
             (threads, not a CPU limit: other threads may use any core)",
            env!("CARGO_PKG_VERSION"),
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            std::env::consts::OS,
            std::env::consts::ARCH,
        ),
        setup.to_owned(),
    ];
    if std::env::consts::OS != "linux" {
        lines.push(brisvox_bench::capacity::report::not_linux_banner(
            std::env::consts::OS,
        ));
    }
    lines
}

/// Prints `markdown`, or writes it to `out` if the run is valid.
fn emit(markdown: &str, out: Option<String>, problems: &[String]) -> Result<bool, BenchError> {
    if !problems.is_empty() {
        // An invalid run must not replace a committed result.
        for problem in problems {
            eprintln!("INVALID RUN: {problem}");
        }
        println!("{markdown}");
        return Ok(false);
    }
    match out {
        Some(path) => {
            std::fs::write(&path, markdown)?;
            eprintln!("wrote {path}");
        }
        None => println!("{markdown}"),
    }
    Ok(true)
}

fn sim_load_cmd(mut args: impl Iterator<Item = String>, command: &str) -> Result<bool, BenchError> {
    let mut sessions = vec![10, 100, 300, 500];
    let (mut turns, mut seed, mut workers, mut out) = (3, 1, None, None);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--sessions" => sessions = parse_list("--sessions", args.next())?,
            "--turns" => turns = parse("--turns", args.next())?,
            "--seed" => seed = parse("--seed", args.next())?,
            "--workers" => workers = Some(parse("--workers", args.next())?),
            "--out" => out = Some(parse("--out", args.next())?),
            other => return Err(unknown(other)),
        }
    }
    let mut reports = Vec::new();
    for &n in &sessions {
        let mut config = LoadConfig::new(n);
        config.turns = turns;
        config.seed = seed;
        config.worker_threads = workers;
        eprintln!("sim-load: {n} concurrent calls ...");
        let r = sim_load::run(&config)?;
        eprintln!(
            "  answered {}/{}, failed {}, barge-ins {}\n  overhead {}\n    turn end -> LLM \
             request {}\n    TTS first byte -> transport {}\n  wake {} (dropped {})\n  \
             response {}\n  CPU cores on plateau {:?}",
            r.turns_answered,
            r.turns_expected,
            r.failed_sessions,
            r.interrupts,
            r.overhead,
            r.to_llm_request,
            r.to_audio_out,
            r.wake,
            r.wake_dropped,
            r.response,
            r.cpu_cores_plateau,
        );
        reports.push(r);
    }
    let problems: Vec<String> = reports.iter().flat_map(LoadReport::problems).collect();
    let workers = reports.first().map_or(0, |r| r.worker_threads);
    let setup = "mock providers in-process (brisvox-sim), real time (SystemClock), one process";
    let markdown = sim_load::markdown(&reports, &environment(workers, command, setup));
    emit(&markdown, out, &problems)
}

/// `--turn` of `speculation`: `adaptive` or `fixed:MS`.
fn parse_policy(value: &str) -> Result<spec_eval::Policy, BenchError> {
    match (value, value.strip_prefix("fixed:")) {
        ("adaptive", _) => Ok(spec_eval::Policy::Adaptive),
        (_, Some(ms)) => ms
            .parse()
            .map(|ms| spec_eval::Policy::Fixed(Duration::from_millis(ms)))
            .map_err(|_| BenchError::Config(format!("--turn fixed:MS, not {value}"))),
        _ => Err(BenchError::Config(format!(
            "--turn: adaptive or fixed:MS, not {value}\n{USAGE}"
        ))),
    }
}

fn speculation_cmd(
    mut args: impl Iterator<Item = String>,
    command: &str,
) -> Result<bool, BenchError> {
    let mut config = spec_eval::SpecEvalConfig::new();
    let mut out = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--seeds" => config.seeds = parse("--seeds", args.next())?,
            "--fixtures" => {
                let list: String = parse("--fixtures", args.next())?;
                config.fixtures = list.split(',').map(|n| n.trim().to_owned()).collect();
            }
            "--turn" => {
                let list: String = parse("--turn", args.next())?;
                config.policies = list
                    .split(',')
                    .map(|p| parse_policy(p.trim()))
                    .collect::<Result<_, _>>()?;
            }
            "--gates-ms" => config.gates = ms_list("--gates-ms", args.next())?,
            "--resume-pauses-ms" => {
                config.resume_pauses = ms_list("--resume-pauses-ms", args.next())?;
            }
            "--out" => out = Some(parse("--out", args.next())?),
            other => return Err(unknown(other)),
        }
    }
    eprintln!(
        "speculation: {} fixtures x {} seeds x {} policies (+ {} resumed-speech scenarios) x {} LLM latencies x {} modes (paused clock) ...",
        config.fixtures.len(),
        config.seeds,
        config.policies.len(),
        config.resume_pauses.len(),
        config.llm_first_token_ms.len(),
        4 + 2 * config.gates.len()
    );
    let rows = spec_eval::run(&config)?;
    let problems = spec_eval::problems(&rows);
    let setup = "engine over mock providers on a paused clock (deterministic per seed), \
                 FLEURS real-speech fixtures (fixtures/NOTICE)";
    let markdown = spec_eval::markdown(&rows, &config, &environment(1, command, setup));
    emit(&markdown, out, &problems)
}

fn fixtures_cmd(mut args: impl Iterator<Item = String>, command: &str) -> Result<bool, BenchError> {
    let mut config = FixtureLoadConfig::new();
    let mut out = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--sessions" => config.sessions = parse("--sessions", args.next())?,
            "--fixtures" => {
                let list: String = parse("--fixtures", args.next())?;
                config.fixtures = list.split(',').map(|n| n.trim().to_owned()).collect();
            }
            "--seed" => config.seed = parse("--seed", args.next())?,
            "--workers" => config.worker_threads = Some(parse("--workers", args.next())?),
            "--out" => out = Some(parse("--out", args.next())?),
            other => return Err(unknown(other)),
        }
    }
    eprintln!(
        "fixtures: {} x {} concurrent calls ...",
        config.fixtures.join(", "),
        config.sessions
    );
    let reports = fixture_load::run(&config)?;
    for r in &reports {
        eprintln!(
            "  {}: answered {}/{}, failed {}, barge-ins {}\n    response {}",
            r.name, r.turns_answered, r.turns_expected, r.failed_sessions, r.interrupts, r.response,
        );
        for (lang, summary) in &r.response_by_language {
            eprintln!("      {lang}: {summary}");
        }
    }
    let problems: Vec<String> = reports.iter().flat_map(FixtureReport::problems).collect();
    let setup = "mock providers in-process (brisvox-sim), real time (SystemClock), one process, \
                 FLEURS real-speech fixtures (fixtures/NOTICE)";
    let workers = config.worker_threads.unwrap_or(0);
    let markdown = fixture_load::markdown(&reports, &environment(workers, command, setup));
    emit(&markdown, out, &problems)
}

/// `command` with the value of every `--url` reduced to scheme and host.
fn redacted_command(command: &str) -> String {
    let mut words = command.split(' ');
    let mut out = Vec::new();
    while let Some(word) = words.next() {
        out.push(word.to_owned());
        if word == "--url"
            && let Some(url) = words.next()
        {
            out.push(brisvox_bench::v2v::report::redact_url(url));
        }
    }
    out.join(" ")
}

/// Progress line of one call, on stderr.
fn v2v_progress(run: &RunReport) {
    let answered: Vec<String> = run
        .turns
        .iter()
        .map(|t| {
            t.v2v.map_or_else(
                || t.outcome.as_str().to_owned(),
                |d| format!("{} ms", brisvox_bench::stats::ms(d)),
            )
        })
        .collect();
    eprintln!(
        "  {}{} run {}: {}",
        run.fixture,
        match run.kind {
            CallKind::Latency => "",
            CallKind::BargeIn => " (barge-in)",
            CallKind::Memory => " (memory)",
        },
        run.run,
        run.failure.as_ref().map_or_else(
            || answered.join(", "),
            |failure| format!("FAILED: {failure}")
        )
    );
}

/// `--engine-turn`: `smart` or `fixed:MS`.
fn parse_engine_turn(value: Option<String>) -> Result<EndOfTurn, BenchError> {
    let value: String = parse("--engine-turn", value)?;
    match value.split_once(':') {
        None if value == "smart" => Ok(EndOfTurn::SmartTurn),
        Some(("fixed", ms)) => ms
            .parse()
            .map(|ms| EndOfTurn::FixedSilence(Duration::from_millis(ms)))
            .map_err(|_| BenchError::Config(format!("--engine-turn fixed:MS, not {value}"))),
        _ => Err(BenchError::Config(format!(
            "--engine-turn: smart or fixed:MS, not {value}\n{USAGE}"
        ))),
    }
}

fn v2v_cmd(mut args: impl Iterator<Item = String>, command: &str) -> Result<bool, BenchError> {
    let (mut sim, mut url_flag, mut target) = (false, None::<String>, None::<String>);
    let url_env = std::env::var("BRISVOX_BENCH_URL")
        .ok()
        .filter(|url| !url.is_empty());
    let (mut strict, mut out_md, mut out_json) = (false, None::<String>, None::<String>);
    let mut keep_invalid = false;
    let mut config = BenchConfig::sim();
    let mut engine = EngineTargetConfig::default();
    let mut bot = MockBotConfig::default();
    // The last flag of each target's own set, to refuse one given for the other target.
    let (mut engine_flag, mut bot_flag) = (None::<String>, None::<String>);
    let ms = Duration::from_millis;
    while let Some(flag) = args.next() {
        if flag.starts_with("--bot-") {
            bot_flag = Some(flag.clone());
        }
        match flag.as_str() {
            "--engine-turn"
            | "--vad-threshold"
            | "--vad-start-ms"
            | "--vad-stop-ms"
            | "--stt-final-ms"
            | "--llm-first-token-ms"
            | "--llm-inter-token-ms"
            | "--tts-first-byte-ms"
            | "--pool-threads"
            | "--barge-in-runs"
            | "--barge-in-gap-ms"
            | "--speculation"
            | "--speculation-final-only"
            | "--speculation-gate-ms"
            | "--stt-interims"
            | "--memory-calls" => engine_flag = Some(flag.clone()),
            _ => {}
        }
        match flag.as_str() {
            "--sim" => sim = true,
            "--target" => target = Some(parse("--target", args.next())?),
            "--url" => url_flag = Some(parse("--url", args.next())?),
            "--strict" => strict = true,
            "--keep-invalid" => keep_invalid = true,
            "--fixtures" => {
                let list: String = parse("--fixtures", args.next())?;
                config.fixtures = list.split(',').map(|n| n.trim().to_owned()).collect();
            }
            "--fixtures-dir" => {
                config.fixtures_dir = parse::<String>("--fixtures-dir", args.next())?.into()
            }
            "--runs" => config.runs = parse("--runs", args.next())?,
            "--concurrency" => config.concurrency = parse("--concurrency", args.next())?,
            "--min-gap-ms" => config.min_gap = ms(parse("--min-gap-ms", args.next())?),
            "--pause-ms" => config.pause = ms(parse("--pause-ms", args.next())?),
            "--response-timeout-ms" => {
                config.client.response_timeout = ms(parse("--response-timeout-ms", args.next())?);
            }
            "--silence-peak" => config.client.silence_peak = parse("--silence-peak", args.next())?,
            "--overlap-guard-ms" => {
                config.client.overlap_guard = ms(parse("--overlap-guard-ms", args.next())?);
            }
            "--engine-turn" => engine.end_of_turn = parse_engine_turn(args.next())?,
            "--vad-threshold" => engine.vad_threshold = parse("--vad-threshold", args.next())?,
            "--vad-start-ms" => engine.vad_start = ms(parse("--vad-start-ms", args.next())?),
            "--vad-stop-ms" => engine.vad_stop = ms(parse("--vad-stop-ms", args.next())?),
            "--stt-final-ms" => {
                engine.latencies.stt_final = ms(parse("--stt-final-ms", args.next())?);
            }
            "--llm-first-token-ms" => {
                engine.latencies.llm_first_token = ms(parse("--llm-first-token-ms", args.next())?);
            }
            "--llm-inter-token-ms" => {
                engine.latencies.llm_inter_token = ms(parse("--llm-inter-token-ms", args.next())?);
            }
            "--tts-first-byte-ms" => {
                engine.latencies.tts_first_byte = ms(parse("--tts-first-byte-ms", args.next())?);
            }
            "--pool-threads" => engine.pool_threads = parse("--pool-threads", args.next())?,
            "--pool-queue" => engine.pool_queue = parse("--pool-queue", args.next())?,
            "--barge-in-runs" => {
                config.barge_in_runs = Some(parse("--barge-in-runs", args.next())?);
            }
            "--barge-in-gap-ms" => {
                config.barge_in_gap = ms(parse("--barge-in-gap-ms", args.next())?);
            }
            "--memory-calls" => config.memory_calls = parse("--memory-calls", args.next())?,
            "--speculation" => engine.speculation = true,
            "--speculation-final-only" => {
                engine.speculation = true;
                engine.speculation_on_partial = false;
            }
            "--speculation-gate-ms" => {
                engine.speculation_gate = ms(parse("--speculation-gate-ms", args.next())?);
            }
            "--stt-interims" => {
                engine.stt_interims = match parse::<String>("--stt-interims", args.next())?.as_str()
                {
                    "lagging" => SttInterims::Lagging,
                    "converged" => SttInterims::Converged,
                    other => {
                        return Err(BenchError::Config(format!(
                            "--stt-interims: lagging or converged, not {other}\n{USAGE}"
                        )));
                    }
                };
            }
            "--bot-endpoint-ms" => {
                bot.endpoint_silence = ms(parse("--bot-endpoint-ms", args.next())?)
            }
            "--bot-delay-ms" => bot.response_delay = ms(parse("--bot-delay-ms", args.next())?),
            "--bot-jitter-ms" => bot.response_jitter = ms(parse("--bot-jitter-ms", args.next())?),
            "--bot-reply-ms" => bot.reply = ms(parse("--bot-reply-ms", args.next())?),
            "--bot-seed" => bot.seed = parse("--bot-seed", args.next())?,
            "--out-md" | "--out" => out_md = Some(parse("--out-md", args.next())?),
            "--out-json" => out_json = Some(parse("--out-json", args.next())?),
            other => return Err(unknown(other)),
        }
    }
    let sim = sim || target.is_some();
    config.target = match (sim, url_flag, url_env) {
        (true, None, _) => match target.as_deref().unwrap_or("engine") {
            "engine" => Target::Engine(engine),
            "mock-bot" => Target::MockBot(bot),
            other => {
                return Err(BenchError::Config(format!(
                    "--target: engine or mock-bot, not {other}\n{USAGE}"
                )));
            }
        },
        (true, Some(_), _) => {
            return Err(BenchError::Config(format!(
                "--sim (or --target) and --url are exclusive\n{USAGE}"
            )));
        }
        (false, Some(url), _) | (false, None, Some(url)) => {
            Target::Live(brisvox_core::Secret::new(url))
        }
        (false, None, None) => {
            return Err(BenchError::Config(format!(
                "v2v needs --sim or --url (or BRISVOX_BENCH_URL)\n{USAGE}"
            )));
        }
    };
    let misplaced = match &config.target {
        Target::Engine(_) => bot_flag.map(|f| (f, "--target mock-bot")),
        Target::MockBot(_) => engine_flag.map(|f| (f, "--target engine")),
        Target::Live(_) => bot_flag.or(engine_flag).map(|f| (f, "--sim")),
    };
    if let Some((flag, needs)) = misplaced {
        return Err(BenchError::Config(format!(
            "{flag} applies only with {needs}\n{USAGE}"
        )));
    }
    config.on_call = Some(v2v_progress);
    let (name, setup) = match &config.target {
        Target::Engine(_) => (
            "sim, engine",
            "sim: the engine (production pipeline, `brisvox_engine::run_call`) over `brisvox_sim` \
             mock providers behind the real websocket transport, in process; one client, one \
             call at a time, real time (SystemClock)",
        ),
        Target::MockBot(_) => (
            "sim, mock bot",
            "sim: in-process mock bot behind the real websocket transport (a self-check of the \
             harness and the transport, not the engine), one client, one call at a time, real \
             time (SystemClock)",
        ),
        Target::Live(_) if config.concurrency > 1 => (
            "live",
            "live: one client, groups of concurrent calls (one group at a time), real time \
             (SystemClock), 20 ms frames",
        ),
        Target::Live(_) => (
            "live",
            "live: one client, one call at a time, real time (SystemClock), 20 ms frames",
        ),
    };
    let live = matches!(config.target, Target::Live(_));
    eprintln!(
        "v2v ({name}): {} runs of {} ...",
        config.runs,
        config.fixtures.join(", ")
    );
    let report = v2v::run_blocking(&config)?;
    let mut problems = report.problems(strict || !live);
    // A missed engine-overhead budget fails the run under an explicit `--strict` (a loaded
    // machine can miss one the engine meets, so without it the miss is a warning).
    let over_budget = report.budget_problems();
    if strict {
        problems.extend(over_budget);
    } else {
        for line in over_budget {
            eprintln!("warning: {line}");
        }
    }
    // The command line may carry a URL with a token: reports only ever show it redacted.
    let command = redacted_command(command);
    let markdown = report.markdown(&environment(2, &command, setup));
    if problems.is_empty() {
        if let Some(path) = &out_json {
            std::fs::write(path, report.to_json()?)?;
            eprintln!("wrote {path}");
        }
        emit(&markdown, out_md, &[])
    } else if keep_invalid && live {
        // A live run with unanswered turns is still data (concurrent blocks): the reports are written
        // for the caller to count the failures, and the run still exits as invalid.
        if let Some(path) = &out_json {
            std::fs::write(path, report.to_json()?)?;
            eprintln!("wrote {path} (invalid run kept: --keep-invalid)");
        }
        if let Some(path) = &out_md {
            std::fs::write(path, &markdown)?;
            eprintln!("wrote {path} (invalid run kept: --keep-invalid)");
        }
        emit(&markdown, None, &problems)
    } else {
        emit(&markdown, out_md, &problems)
    }
}

fn infer_load_cmd(
    mut args: impl Iterator<Item = String>,
    command: &str,
) -> Result<bool, BenchError> {
    let mut costs_us: Vec<u64> = vec![100, 250, 500];
    let mut arrivals = vec![Arrivals::SessionTimers, Arrivals::Paced];
    let base = InferLoadConfig::new(Duration::ZERO);
    let (mut sessions, mut threads, mut seconds, mut out) =
        (base.sessions, base.pool_threads, 10u64, None);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--sessions" => sessions = parse("--sessions", args.next())?,
            "--costs-us" => costs_us = parse_list("--costs-us", args.next())?,
            "--threads" => threads = parse("--threads", args.next())?,
            "--seconds" => seconds = parse("--seconds", args.next())?,
            "--arrivals" => {
                let list: String = parse("--arrivals", args.next())?;
                arrivals = list
                    .split(',')
                    .map(|a| match a.trim() {
                        "timers" => Ok(Arrivals::SessionTimers),
                        "paced" => Ok(Arrivals::Paced),
                        other => Err(BenchError::Config(format!(
                            "--arrivals: timers or paced, not {other}\n{USAGE}"
                        ))),
                    })
                    .collect::<Result<_, _>>()?;
            }
            "--out" => out = Some(parse("--out", args.next())?),
            other => return Err(unknown(other)),
        }
    }
    let mut reports = Vec::new();
    let runs: Vec<(Arrivals, u64)> = arrivals
        .iter()
        .flat_map(|a| costs_us.iter().map(move |c| (*a, *c)))
        .collect();
    for (mode, cost) in runs {
        let mut config = InferLoadConfig::new(Duration::from_micros(cost));
        config.arrivals = mode;
        config.sessions = sessions;
        config.pool_threads = threads;
        config.duration = Duration::from_secs(seconds);
        eprintln!(
            "infer-load: {mode:?}, {sessions} sessions, {cost} us jobs, offered load {:.2} ...",
            config.offered_load()
        );
        let r = infer_load::run(&config)?;
        eprintln!(
            "  done {}, rejected {}, timed out {}, missed windows {}/{}\n  wait {}\n  \
             latency {}\n  tick late {}\n  utilisation {:.2}",
            r.pool.completed,
            r.pool.rejected,
            r.pool.timed_out,
            r.missed_windows,
            r.windows,
            r.wait,
            r.latency,
            r.tick_late,
            r.utilisation
        );
        reports.push(r);
    }
    // Saturation (rejections, timeouts, missed windows) is a result here, not an invalid run;
    // only a panicking job invalidates it.
    let problems: Vec<String> = reports
        .iter()
        .filter(|r| r.pool.panicked > 0)
        .map(|r| format!("{} panicked jobs", r.pool.panicked))
        .collect();
    let workers = base.worker_threads;
    let setup = format!(
        "inference pool with {threads} OS threads; sessions on the tokio runtime; jobs busy-spin \
         (real CPU), {seconds} s per row"
    );
    let markdown = infer_load::markdown(&reports, &environment(workers, command, &setup));
    emit(&markdown, out, &problems)
}

fn run() -> Result<bool, BenchError> {
    let command = std::iter::once("brisvox-bench".to_owned())
        .chain(std::env::args().skip(1))
        .collect::<Vec<_>>()
        .join(" ");
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("sim-load") => sim_load_cmd(args, &command),
        Some("infer-load") => infer_load_cmd(args, &command),
        Some("resample") => resample_cmd(args, &command),
        Some("model-cpu") => model_cpu_cmd(args, &command),
        Some("denoise") => denoise_cmd(args, &command),
        Some("fixtures") => fixtures_cmd(args, &command),
        Some("v2v") => v2v_cmd(args, &command),
        Some("speculation") => speculation_cmd(args, &command),
        Some("capacity") => capacity_cli::capacity_cmd(args, &command),
        Some("engine-server") => capacity_cli::engine_server_cmd(args),
        _ => Err(BenchError::Config(USAGE.into())),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("brisvox-bench: {e}");
            ExitCode::FAILURE
        }
    }
}
