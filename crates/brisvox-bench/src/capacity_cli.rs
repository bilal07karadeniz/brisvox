//! The command lines of `brisvox-bench capacity` and `brisvox-bench engine-server`.
//!
//! ```text
//! brisvox-bench capacity (--sim | --url ws://HOST:PORT/PATH [--pid PID] --floor-ms MS)
//!                        [--label NAME] [--fixture en_two_utterances] [--steps 1,5,10,20]
//!                        [--rounds 2] [--spread-ms 4000] [--slo-over-floor-ms 250]
//!                        [--max-bad-rate 0] [--resolution 0] [--keep-going]
//!                        [--out-md FILE] [--out-json FILE] ...
//! brisvox-bench engine-server [--bind 127.0.0.1:0] [--fixture NAME] [--workers N]
//!                             [--max-calls 1024] [--exit-on-stdin-eof] [--stats-every-ms N] ...
//! ```
//!
//! `capacity --sim` starts `engine-server` (this very executable) as a child process, so the
//! engine is measured exactly like the Pipecat bot: a separate process behind a URL, its CPU and
//! memory read by pid. Both commands take the engine target's flags (`--engine-turn`,
//! `--vad-*`, `--stt-final-ms`, `--llm-*`, `--tts-first-byte-ms`, `--pool-threads`,
//! `--turn-threads`: the Smart Turn executor of the real models; 0 shares the pool).

use std::net::SocketAddr;
use std::time::Duration;

use brisvox_bench::BenchError;
use brisvox_bench::capacity::child::{EngineChild, ServeOptions, serve};
use brisvox_bench::capacity::probe::Scope;
use brisvox_bench::capacity::report::{CapacityReport, StepReport};
use brisvox_bench::capacity::{CapacityConfig, Slo, run_capacity};
use brisvox_bench::v2v::engine_target::{EngineTargetConfig, EngineTargetSettings, SttInterims};
use brisvox_core::SystemClock;
use tokio_util::sync::CancellationToken;

use crate::{
    USAGE, emit, environment, parse, parse_engine_turn, parse_list, redacted_command, unknown,
};

/// Calls the engine server accepts at once unless `--max-calls` says otherwise.
const DEFAULT_MAX_CALLS: usize = 1_024;

/// Handles `flag` if it is one of the engine target's flags; `forwarded` collects the flag and its
/// value for a child process.
fn engine_flag(
    flag: &str,
    args: &mut impl Iterator<Item = String>,
    engine: &mut EngineTargetConfig,
    forwarded: &mut Vec<String>,
) -> Result<bool, BenchError> {
    let ms = Duration::from_millis;
    let known = matches!(
        flag,
        "--engine-turn"
            | "--vad-threshold"
            | "--vad-start-ms"
            | "--vad-stop-ms"
            | "--stt-final-ms"
            | "--llm-first-token-ms"
            | "--llm-inter-token-ms"
            | "--tts-first-byte-ms"
            | "--pool-threads"
            | "--pool-queue"
            | "--turn-threads"
            | "--stt-interims"
    );
    if !known {
        return Ok(false);
    }
    let value = args
        .next()
        .ok_or_else(|| BenchError::Config(format!("{flag} needs a value\n{USAGE}")))?;
    forwarded.push(flag.to_owned());
    forwarded.push(value.clone());
    let v = || Some(value.clone());
    match flag {
        "--engine-turn" => engine.end_of_turn = parse_engine_turn(v())?,
        "--vad-threshold" => engine.vad_threshold = parse(flag, v())?,
        "--vad-start-ms" => engine.vad_start = ms(parse(flag, v())?),
        "--vad-stop-ms" => engine.vad_stop = ms(parse(flag, v())?),
        "--stt-final-ms" => engine.latencies.stt_final = ms(parse(flag, v())?),
        "--llm-first-token-ms" => engine.latencies.llm_first_token = ms(parse(flag, v())?),
        "--llm-inter-token-ms" => engine.latencies.llm_inter_token = ms(parse(flag, v())?),
        "--tts-first-byte-ms" => engine.latencies.tts_first_byte = ms(parse(flag, v())?),
        "--pool-threads" => engine.pool_threads = parse(flag, v())?,
        "--pool-queue" => engine.pool_queue = parse(flag, v())?,
        "--turn-threads" => engine.turn_threads = Some(parse(flag, v())?),
        _ => {
            engine.stt_interims = match value.as_str() {
                "lagging" => SttInterims::Lagging,
                "converged" => SttInterims::Converged,
                other => {
                    return Err(BenchError::Config(format!(
                        "--stt-interims: lagging or converged, not {other}\n{USAGE}"
                    )));
                }
            };
        }
    }
    Ok(true)
}

/// The options of `brisvox-bench engine-server`, parsed from `args`.
fn parse_engine_server(mut args: impl Iterator<Item = String>) -> Result<ServeOptions, BenchError> {
    let mut engine = EngineTargetConfig {
        max_calls: DEFAULT_MAX_CALLS,
        ..EngineTargetConfig::default()
    };
    let mut options = ServeOptions {
        engine,
        fixture: "en_two_utterances".to_owned(),
        fixtures_dir: brisvox_sim::fixture::fixtures_dir(),
        min_gap: Duration::from_millis(4_000),
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        workers: None,
        exit_on_stdin_eof: false,
        stats_every: None,
        models: None,
    };
    let mut unused = Vec::new();
    while let Some(flag) = args.next() {
        if engine_flag(&flag, &mut args, &mut engine, &mut unused)? {
            continue;
        }
        match flag.as_str() {
            "--bind" => options.bind = parse("--bind", args.next())?,
            "--fixture" => options.fixture = parse("--fixture", args.next())?,
            "--fixtures-dir" => {
                options.fixtures_dir = parse::<String>("--fixtures-dir", args.next())?.into();
            }
            "--min-gap-ms" => {
                options.min_gap = Duration::from_millis(parse("--min-gap-ms", args.next())?);
            }
            "--max-calls" => engine.max_calls = parse("--max-calls", args.next())?,
            "--workers" => options.workers = Some(parse("--workers", args.next())?),
            "--exit-on-stdin-eof" => options.exit_on_stdin_eof = true,
            "--stats-every-ms" => {
                let period: u64 = parse("--stats-every-ms", args.next())?;
                options.stats_every = (period > 0).then(|| Duration::from_millis(period));
            }
            "--models" => options.models = Some(parse::<String>("--models", args.next())?.into()),
            other => return Err(unknown(other)),
        }
    }
    options.engine = engine;
    Ok(options)
}

/// `brisvox-bench engine-server`: the engine over mock providers as a server process.
pub fn engine_server_cmd(args: impl Iterator<Item = String>) -> Result<bool, BenchError> {
    serve(&parse_engine_server(args)?)?;
    Ok(true)
}

fn step_progress(step: &StepReport, warmup: bool) {
    let cores = |u: &Option<brisvox_bench::capacity::report::Usage>| {
        u.as_ref()
            .and_then(|u| u.cpu_cores)
            .map_or_else(|| "-".to_owned(), |c| format!("{c:.2}"))
    };
    let p95 = step
        .v2v
        .map_or_else(|| "-".to_owned(), |d| format!("{} ms", d.p95_us / 1_000));
    eprintln!(
        "  {:>4} concurrent{}: {}/{} turns answered, {} calls failed, p95 {p95}, target CPU {} \
         cores, harness {} cores, {}",
        step.concurrent,
        if warmup {
            " (warm-up, not counted)"
        } else {
            ""
        },
        step.turns_answered,
        step.turns_expected,
        step.calls_failed,
        cores(&step.target),
        cores(&step.client),
        step.verdict.as_str(),
    );
    for reason in &step.reasons {
        eprintln!("         - {reason}");
    }
    for failure in &step.failures {
        eprintln!("         - failed call: {failure}");
    }
}

/// Problems that make the whole run unfit to commit (a wrong URL, a server that never answered).
fn problems(report: &CapacityReport) -> Vec<String> {
    let answered: usize = report.steps.iter().map(|s| s.turns_answered).sum();
    if answered == 0 {
        vec!["no turn was answered in any step: the target is not serving calls".to_owned()]
    } else {
        Vec::new()
    }
}

/// What `brisvox-bench capacity` was asked, parsed and checked before anything is started.
struct CapacityArgs {
    /// Start the engine server as a child process.
    sim: bool,
    /// The server URL (`--url`).
    url: Option<String>,
    /// The pid whose CPU and memory are read for `--url`.
    pid: Option<u32>,
    /// The floor the SLO is counted from: `--floor-ms`, or for `--sim` the mock providers' own.
    floor: Duration,
    out_md: Option<String>,
    out_json: Option<String>,
    client_workers: usize,
    server_workers: Option<usize>,
    /// The directory of the real models (`--models`), forwarded to the child process.
    models: Option<String>,
    /// The engine target flags, as given, for the child process.
    forwarded: Vec<String>,
    /// The engine target they describe.
    engine: EngineTargetConfig,
    config: CapacityConfig,
}

/// The command line of the `engine-server` child (after the `engine-server` word): every engine
/// flag the user gave, and the settings the load generator and the child must agree on (fixture,
/// gap, call limit, worker count).
fn child_args(
    forwarded: &[String],
    config: &CapacityConfig,
    engine: &EngineTargetConfig,
    server_workers: Option<usize>,
) -> Vec<String> {
    let mut args = forwarded.to_vec();
    for (flag, value) in [
        ("--fixture", config.fixture.clone()),
        (
            "--fixtures-dir",
            config.fixtures_dir.to_string_lossy().into_owned(),
        ),
        ("--min-gap-ms", config.min_gap.as_millis().to_string()),
        ("--max-calls", engine.max_calls.to_string()),
    ] {
        args.push(flag.to_owned());
        args.push(value);
    }
    if let Some(workers) = server_workers {
        args.push("--workers".to_owned());
        args.push(workers.to_string());
    }
    args
}

/// Parses the flags of `capacity`.
#[allow(clippy::too_many_lines)] // one flat flag table
fn parse_capacity(mut args: impl Iterator<Item = String>) -> Result<CapacityArgs, BenchError> {
    let ms = Duration::from_millis;
    let mut engine = EngineTargetConfig {
        max_calls: DEFAULT_MAX_CALLS,
        ..EngineTargetConfig::default()
    };
    let (mut sim, mut url, mut pid, mut floor_ms) =
        (false, None::<String>, None::<u32>, None::<u64>);
    let (mut out_md, mut out_json) = (None::<String>, None::<String>);
    let (mut client_workers, mut server_workers) = (2usize, None::<usize>);
    let mut forwarded: Vec<String> = Vec::new();
    let mut models = None::<String>;
    let mut config = CapacityConfig::new("", Duration::ZERO);
    config.label = String::new();
    while let Some(flag) = args.next() {
        if engine_flag(&flag, &mut args, &mut engine, &mut forwarded)? {
            continue;
        }
        match flag.as_str() {
            "--sim" => sim = true,
            "--url" => url = Some(parse("--url", args.next())?),
            "--pid" => pid = Some(parse("--pid", args.next())?),
            "--pid-only" => config.target_scope = Scope::Process,
            "--label" => config.label = parse("--label", args.next())?,
            "--note" => config.note = Some(parse("--note", args.next())?),
            "--floor-ms" => floor_ms = Some(parse("--floor-ms", args.next())?),
            "--slo-over-floor-ms" => {
                config.slo.over_floor = ms(parse("--slo-over-floor-ms", args.next())?);
            }
            "--fixture" => config.fixture = parse("--fixture", args.next())?,
            "--fixtures-dir" => {
                config.fixtures_dir = parse::<String>("--fixtures-dir", args.next())?.into();
            }
            "--steps" => config.steps = parse_list("--steps", args.next())?,
            "--rounds" => config.rounds = parse("--rounds", args.next())?,
            "--warmup-calls" => config.warmup_calls = parse("--warmup-calls", args.next())?,
            "--spread-ms" => config.spread = ms(parse("--spread-ms", args.next())?),
            "--min-gap-ms" => config.min_gap = ms(parse("--min-gap-ms", args.next())?),
            "--pause-ms" => config.pause = ms(parse("--pause-ms", args.next())?),
            "--cooldown-ms" => config.cooldown = ms(parse("--cooldown-ms", args.next())?),
            "--max-bad-rate" => config.max_bad_rate = parse("--max-bad-rate", args.next())?,
            "--keep-going" => config.keep_going = true,
            "--resolution" => config.resolution = parse("--resolution", args.next())?,
            "--client-lateness-ms" => {
                config.lateness_limit = ms(parse("--client-lateness-ms", args.next())?);
            }
            "--client-late-share" => {
                config.late_share_limit = parse("--client-late-share", args.next())?;
            }
            "--response-timeout-ms" => {
                config.client.response_timeout = ms(parse("--response-timeout-ms", args.next())?);
            }
            "--sample-ms" => config.sample_every = ms(parse("--sample-ms", args.next())?),
            "--client-workers" => client_workers = parse("--client-workers", args.next())?,
            "--max-calls" => engine.max_calls = parse("--max-calls", args.next())?,
            "--stats-every-ms" => {
                // Only the server child prints the lines (on the shared stderr).
                let period: u64 = parse("--stats-every-ms", args.next())?;
                forwarded.push(flag.clone());
                forwarded.push(period.to_string());
            }
            "--server-workers" => server_workers = Some(parse("--server-workers", args.next())?),
            "--models" => {
                let dir: String = parse("--models", args.next())?;
                forwarded.push(flag.clone());
                forwarded.push(dir.clone());
                models = Some(dir);
            }
            "--out-md" | "--out" => out_md = Some(parse("--out-md", args.next())?),
            "--out-json" => out_json = Some(parse("--out-json", args.next())?),
            other => return Err(unknown(other)),
        }
    }
    if models.is_some() && !sim {
        return Err(BenchError::Config(format!(
            "--models belongs to --sim (the engine started as a child process); start \
             `brisvox-bench engine-server --models DIR` yourself and use --url\n{USAGE}"
        )));
    }
    // The target: the engine as a child process, or a URL; the floor of the SLO follows.
    let floor = match (sim, &url, floor_ms) {
        (true, Some(_), _) => {
            return Err(BenchError::Config(format!(
                "--sim and --url are exclusive\n{USAGE}"
            )));
        }
        (false, None, _) => {
            return Err(BenchError::Config(format!(
                "capacity needs --sim or --url\n{USAGE}"
            )));
        }
        (_, _, Some(floor_ms)) => ms(floor_ms),
        (true, None, None) => ms(engine.settings().floor_ms),
        (false, Some(_), None) => {
            return Err(BenchError::Config(format!(
                "capacity --url needs --floor-ms (the V2V of one unloaded call against that \
                 server, e.g. from `brisvox-bench v2v --url`)\n{USAGE}"
            )));
        }
    };
    Ok(CapacityArgs {
        sim,
        url,
        pid,
        floor,
        out_md,
        out_json,
        client_workers,
        server_workers,
        models,
        forwarded,
        engine,
        config,
    })
}

/// `brisvox-bench capacity`.
pub fn capacity_cmd(args: impl Iterator<Item = String>, command: &str) -> Result<bool, BenchError> {
    let CapacityArgs {
        sim,
        url,
        pid,
        floor,
        out_md,
        out_json,
        client_workers,
        server_workers,
        models,
        forwarded,
        engine,
        mut config,
    } = parse_capacity(args)?;
    let child = if sim {
        let exe = std::env::current_exe()?;
        let args = child_args(&forwarded, &config, &engine, server_workers);
        Some(EngineChild::spawn(&exe, &args)?)
    } else {
        None
    };
    let result = capacity_run(
        &mut config,
        child.as_ref(),
        url,
        pid,
        floor,
        EngineTargetSettings {
            real_models: models.is_some(),
            smart_turn_threads: engine.resolved_turn_threads(models.is_some()),
            ..engine.settings()
        },
        client_workers,
    );
    if let Some(child) = child {
        child.stop();
    }
    let report = result?;

    let cores = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    let setup = if sim {
        format!(
            "sim: the production engine over `brisvox_sim` mock providers behind the real \
             websocket transport, as a child process (`brisvox-bench engine-server`){}: {} tokio \
             workers, {} inference pool threads (queue {}){}, up to {} calls; the load generator is \
             this process ({client_workers} tokio workers, the worker count in the line above)",
            if models.is_some() {
                ", with the real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime) \
                 instead of the energy VAD and the Smart Turn stub"
            } else {
                ""
            },
            server_workers.unwrap_or(cores),
            engine.pool_threads,
            engine.pool_queue_capacity(),
            match engine.resolved_turn_threads(models.is_some()) {
                0 => String::new(),
                n => format!(" plus {n} Smart Turn executor thread(s)"),
            },
            engine.max_calls,
        )
    } else {
        format!(
            "live: a server at the URL; the load generator is this process ({client_workers} \
             tokio workers, the worker count in the line above)"
        )
    };
    let command = redacted_command(command);
    let markdown = report.markdown(&environment(client_workers, &command, &setup));
    let problems = problems(&report);
    if problems.is_empty()
        && let Some(path) = &out_json
    {
        std::fs::write(path, report.to_json()?)?;
        eprintln!("wrote {path}");
    }
    emit(&markdown, out_md, &problems)
}

fn capacity_run(
    config: &mut CapacityConfig,
    child: Option<&EngineChild>,
    url: Option<String>,
    pid: Option<u32>,
    floor: Duration,
    settings: EngineTargetSettings,
    client_workers: usize,
) -> Result<CapacityReport, BenchError> {
    let (url, pid) = match (child, url) {
        (Some(child), _) => (child.url().to_owned(), Some(child.pid())),
        (None, Some(url)) => (url, pid),
        (None, None) => (String::new(), None),
    };
    config.slo = Slo {
        floor,
        over_floor: config.slo.over_floor,
    };
    config.url = brisvox_core::Secret::new(url);
    config.target_pid = pid;
    if config.label.is_empty() {
        config.label = if child.is_some() {
            "brisvox (engine over mock providers)"
        } else {
            "server"
        }
        .to_owned();
    }
    if child.is_some() {
        config.engine = Some(settings);
    }
    config.on_step = Some(step_progress);
    eprintln!(
        "capacity ({}): steps {:?}, {} call(s) per slot of {}, SLO p95 <= {} ms (floor {} ms + {} ms) ...",
        config.label,
        config.steps,
        config.rounds,
        config.fixture,
        config.slo.limit().as_millis(),
        config.slo.floor.as_millis(),
        config.slo.over_floor.as_millis(),
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(client_workers.max(1))
        .enable_all()
        .build()?;
    let cancel = CancellationToken::new();
    runtime.block_on(run_capacity(SystemClock::shared(), config, &cancel))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Result<CapacityArgs, BenchError> {
        parse_capacity(args.iter().map(|a| (*a).to_owned()))
    }

    fn child_of(args: &CapacityArgs) -> Vec<String> {
        child_args(
            &args.forwarded,
            &args.config,
            &args.engine,
            args.server_workers,
        )
    }

    #[test]
    fn the_engine_flags_reach_the_child_and_the_child_parses_them_to_the_same_engine() {
        let args = parsed(&[
            "--sim",
            "--pool-threads",
            "4",
            "--pool-queue",
            "77",
            "--stt-final-ms",
            "200",
            "--llm-first-token-ms",
            "400",
            "--vad-stop-ms",
            "280",
            "--engine-turn",
            "fixed:500",
            "--stt-interims",
            "converged",
            "--max-calls",
            "33",
            "--fixture",
            "tr_two_utterances",
            "--fixtures-dir",
            "some/dir",
            "--min-gap-ms",
            "2500",
            "--server-workers",
            "3",
        ])
        .unwrap();
        // What the report will state ...
        assert_eq!(args.engine.pool_threads, 4);
        assert_eq!(args.engine.pool_queue, 77);
        // ... is what the child process is started with.
        let child = parse_engine_server(child_of(&args).into_iter()).unwrap();
        assert_eq!(child.engine, args.engine);
        assert_eq!(child.engine.pool_threads, 4);
        assert_eq!(child.engine.pool_queue, 77);
        assert_eq!(child.engine.max_calls, 33);
        assert_eq!(child.fixture, "tr_two_utterances");
        assert_eq!(child.fixtures_dir, std::path::PathBuf::from("some/dir"));
        assert_eq!(child.min_gap, Duration::from_millis(2_500));
        assert_eq!(child.workers, Some(3));
        // The floor of the SLO follows the engine settings: fixed 500 ms silence (above the VAD
        // stop 280 + STT final 200) + LLM first token 400 + TTS first byte 150.
        assert_eq!(args.floor, Duration::from_millis(500 + 400 + 150));
        assert_eq!(
            args.floor.as_millis(),
            u128::from(args.engine.settings().floor_ms)
        );
    }

    #[test]
    fn without_engine_flags_the_child_gets_the_defaults() {
        let args = parsed(&["--sim"]).unwrap();
        let child = parse_engine_server(child_of(&args).into_iter()).unwrap();
        assert_eq!(child.engine.pool_threads, 2);
        assert_eq!(child.engine.max_calls, DEFAULT_MAX_CALLS);
        assert_eq!(child.fixture, args.config.fixture);
        assert_eq!(child.min_gap, args.config.min_gap);
        assert_eq!(child.workers, None);
    }

    #[test]
    fn sim_and_url_are_exclusive_and_one_of_them_is_needed() {
        for bad in [
            vec!["--sim", "--url", "ws://127.0.0.1:1/", "--floor-ms", "950"],
            vec!["--steps", "1,2"],
        ] {
            assert!(
                matches!(parsed(&bad), Err(BenchError::Config(_))),
                "{bad:?}"
            );
        }
        assert!(parsed(&["--sim"]).is_ok());
    }

    #[test]
    fn a_url_run_needs_its_floor_and_a_sim_run_does_not() {
        assert!(matches!(
            parsed(&["--url", "ws://127.0.0.1:1/"]),
            Err(BenchError::Config(m)) if m.contains("--floor-ms")
        ));
        let args = parsed(&["--url", "ws://127.0.0.1:1/", "--floor-ms", "1234"]).unwrap();
        assert_eq!(args.floor, Duration::from_millis(1_234));
        assert!(args.url.is_some() && !args.sim);
        // A stated floor also overrides the engine's own for --sim.
        let sim = parsed(&["--sim", "--floor-ms", "1000"]).unwrap();
        assert_eq!(sim.floor, Duration::from_millis(1_000));
        // The default engine's floor: VAD stop 300 + STT final 150 + LLM 350 + TTS 150.
        assert_eq!(
            parsed(&["--sim"]).unwrap().floor,
            Duration::from_millis(950)
        );
    }

    #[test]
    fn unknown_flags_and_missing_values_are_errors() {
        assert!(matches!(
            parsed(&["--sim", "--no-such-flag"]),
            Err(BenchError::Config(_))
        ));
        assert!(matches!(
            parsed(&["--sim", "--pool-queue"]),
            Err(BenchError::Config(_))
        ));
        assert!(matches!(
            parsed(&["--sim", "--stt-interims", "sideways"]),
            Err(BenchError::Config(_))
        ));
    }

    #[test]
    fn the_models_flag_reaches_the_child_and_belongs_to_sim_runs() {
        let args = parsed(&["--sim", "--models", "some/models"]).unwrap();
        assert_eq!(args.models.as_deref(), Some("some/models"));
        let child = parse_engine_server(child_of(&args).into_iter()).unwrap();
        assert_eq!(child.models, Some(std::path::PathBuf::from("some/models")));
        // Without the flag the child runs the stub.
        let plain = parsed(&["--sim"]).unwrap();
        assert!(plain.models.is_none());
        let child = parse_engine_server(child_of(&plain).into_iter()).unwrap();
        assert!(child.models.is_none());
        // A URL run cannot start a child with models.
        assert!(matches!(
            parsed(&["--url", "ws://127.0.0.1:1/", "--floor-ms", "950", "--models", "m"]),
            Err(BenchError::Config(m)) if m.contains("--sim")
        ));
    }

    #[test]
    fn the_generator_limits_reach_the_config() {
        let args = parsed(&[
            "--sim",
            "--client-lateness-ms",
            "50",
            "--client-late-share",
            "0.05",
        ])
        .unwrap();
        assert_eq!(args.config.lateness_limit, Duration::from_millis(50));
        assert!((args.config.late_share_limit - 0.05).abs() < 1e-12);
    }
}
