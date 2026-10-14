//! The capacity benchmark end to end: real sockets on 127.0.0.1, the real clock.
//!
//! - against the engine server in process: a small ramp passes, every number is filled in, and
//!   the JSON and markdown carry the result;
//! - against the engine server as a child process (`brisvox-bench engine-server`, the way
//!   `capacity --sim` runs it): the child's CPU and memory are read by pid;
//! - a server that refuses calls over its limit: the step fails, the capacity is the step below;
//! - a server slower than the SLO: the step fails on p95 and says so;
//! - a dead server and a cancelled run.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::disallowed_methods // a test server on real instants
)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use brisvox_bench::BenchError;
use brisvox_bench::capacity::child::EngineChild;
use brisvox_bench::capacity::probe::Scope;
use brisvox_bench::capacity::report::{CapacityReport, Limit, SCHEMA, Verdict};
use brisvox_bench::capacity::{CapacityConfig, run_capacity};
use brisvox_bench::v2v::engine_target::{EngineServer, EngineTargetConfig};
use brisvox_bench::v2v::mock_bot::{MockBotConfig, MockBotServer};
use brisvox_core::SystemClock;
use brisvox_core::frame::SampleRate;
use brisvox_sim::fixture::{Fixture, FixtureSegment};
use bytes::Bytes;
use tokio_util::sync::CancellationToken;

const MS: Duration = Duration::from_millis(1);
const RATE: u32 = 16_000;

fn tone_pcm(total: u64, speech: &[(u64, u64)]) -> Vec<u8> {
    let mut pcm = vec![0u8; usize::try_from(total).unwrap() * 2];
    for &(start, end) in speech {
        for i in start..end {
            let sample =
                (8_000.0 * (std::f64::consts::TAU * 440.0 * i as f64 / 16_000.0).sin()) as i16;
            let at = usize::try_from(i).unwrap() * 2;
            pcm[at..at + 2].copy_from_slice(&sample.to_le_bytes());
        }
    }
    pcm
}

/// One utterance of 0.5 s in a 1.5 s clip.
fn fixture() -> Fixture {
    Fixture {
        name: "tone_one".to_owned(),
        sample_rate: SampleRate::Hz16000,
        pcm: Bytes::from(tone_pcm(24_000, &[(1_600, 9_600)])),
        segments: vec![FixtureSegment {
            start: 1_600,
            end: 9_600,
            lang: "en".to_owned(),
            text: "synthetic".to_owned(),
            source: "test".to_owned(),
        }],
    }
}

/// Writes the fixture as `<dir>/tone_one.wav` and `.json`, the way `scripts/fixtures` does.
fn write_fixture(dir: &Path, fixture: &Fixture) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join(format!("{}.wav", fixture.name)),
        brisvox_transport::wav::encode(RATE, &fixture.pcm),
    )
    .unwrap();
    let segments: Vec<_> = fixture
        .segments
        .iter()
        .map(|s| {
            serde_json::json!({
                "start": s.start, "end": s.end, "lang": s.lang, "text": s.text, "source": s.source
            })
        })
        .collect();
    let sidecar = serde_json::json!({
        "name": fixture.name,
        "sample_rate": RATE,
        "samples": fixture.pcm.len() / 2,
        "segments": segments,
    });
    std::fs::write(
        dir.join(format!("{}.json", fixture.name)),
        serde_json::to_vec(&sidecar).unwrap(),
    )
    .unwrap();
}

fn temp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "brisvox-bench-capacity-{tag}-{}",
        std::process::id()
    ))
}

/// A configuration for fast tests: the short fixture, calls started within 300 ms, no pauses.
fn config(url: &str, dir: &Path, floor: Duration, steps: Vec<usize>) -> CapacityConfig {
    let mut config = CapacityConfig::new(url, floor);
    config.fixture = "tone_one".to_owned();
    config.fixtures_dir = dir.to_path_buf();
    config.steps = steps;
    config.rounds = 1;
    config.warmup_calls = 0;
    config.spread = 300 * MS;
    config.min_gap = 3_000 * MS;
    config.pause = 50 * MS;
    config.cooldown = 200 * MS;
    config.sample_every = 20 * MS;
    config.client.response_timeout = Duration::from_secs(4);
    // Test machines are loaded: a generous allowance, and no verdict from a late generator.
    config.slo.over_floor = 700 * MS;
    config.lateness_limit = Duration::from_secs(5);
    config
}

async fn run(config: &CapacityConfig) -> Result<CapacityReport, BenchError> {
    run_capacity(SystemClock::shared(), config, &CancellationToken::new()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_small_ramp_against_the_engine_passes_and_fills_in_every_number() {
    let dir = temp_dir("engine");
    let fixture = fixture();
    write_fixture(&dir, &fixture);
    let engine = EngineTargetConfig {
        max_calls: 16,
        ..EngineTargetConfig::default()
    };
    let settings = engine.settings();
    let cancel = CancellationToken::new();
    let server = EngineServer::start(engine, SystemClock::shared(), &cancel)
        .await
        .unwrap();
    server.set_default_script(&fixture);

    let mut config = config(
        &server.url(),
        &dir,
        Duration::from_millis(settings.floor_ms),
        vec![1, 3],
    );
    config.label = "engine".to_owned();
    config.warmup_calls = 1;
    config.engine = Some(settings.clone());
    // The server is this process: its CPU and memory are readable by pid.
    config.target_pid = Some(std::process::id());
    config.target_scope = Scope::Process;
    let report = run(&config).await.unwrap();
    server.shutdown(Duration::from_secs(5)).await.unwrap();
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(report.schema, SCHEMA);
    // The platform is in the report itself: a Windows trend cannot be mistaken for a Linux run.
    let environment = report.environment.as_ref().unwrap();
    assert_eq!(environment.os, std::env::consts::OS);
    assert_eq!(environment.not_linux_run, std::env::consts::OS != "linux");
    assert_eq!(
        environment.banner.is_some(),
        environment.not_linux_run,
        "{environment:?}"
    );
    let json: serde_json::Value = serde_json::from_str(&report.to_json().unwrap()).unwrap();
    assert_eq!(json["environment"]["os"], std::env::consts::OS);
    assert_eq!(
        json["environment"]["not_linux_run"],
        std::env::consts::OS != "linux"
    );
    assert_eq!(report.steps.len(), 2);
    // The warm-up call was played alone and is not a step.
    let warmup = report.warmup.as_ref().unwrap();
    assert_eq!(
        (warmup.concurrent, warmup.calls, warmup.verdict),
        (1, 1, Verdict::Pass)
    );
    assert_eq!(report.settings.warmup_calls, 1);
    for (step, n) in report.steps.iter().zip([1usize, 3]) {
        assert_eq!(step.concurrent, n);
        assert_eq!(step.verdict, Verdict::Pass, "{:?}", step.reasons);
        assert_eq!((step.calls, step.calls_failed), (n, 0));
        assert_eq!((step.turns_expected, step.turns_answered), (n, n));
        let v2v = step.v2v.unwrap();
        assert_eq!(v2v.count, n);
        // Above the nominal floor (less a frame of grid), well below the allowance.
        assert!(v2v.p50_us >= 930_000 && v2v.p95_us <= 1_650_000, "{v2v:?}");
        assert!(step.p95_over_floor_us.unwrap() > -30_000);
        assert!(step.wall_ms > 1_000);
        let target = step.target.unwrap();
        assert!(
            target.rss_peak_bytes > 0 && target.readings >= 2,
            "{target:?}"
        );
        assert!(step.rss_before_bytes.unwrap() > 0);
        // Linux reads PSS too; a proportional set is never above the resident set.
        #[cfg(target_os = "linux")]
        {
            let pss = target.pss_peak_bytes.expect("PSS is read on Linux");
            assert!(pss > 0 && step.pss_before_bytes.unwrap() > 0, "{target:?}");
        }
        assert!(step.client.unwrap().rss_peak_bytes > 0);
    }
    assert_eq!(report.conclusion.max_concurrent, Some(3));
    assert_eq!(report.conclusion.limit, Limit::NotReached);
    assert_eq!(report.settings.slo_limit_ms, settings.floor_ms + 700);

    let markdown = report.markdown(&["command: `test`".to_owned()]);
    assert!(
        markdown.contains("max 3 concurrent calls within the SLO"),
        "{markdown}"
    );
    let back: CapacityReport = serde_json::from_str(&report.to_json().unwrap()).unwrap();
    assert_eq!(back, report);
    // The url is shown without its path.
    assert!(!report.url.contains("/call"), "{}", report.url);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_refuses_calls_over_its_limit_fails_the_step_and_bounds_the_capacity() {
    let dir = temp_dir("limit");
    let fixture = fixture();
    write_fixture(&dir, &fixture);
    let engine = EngineTargetConfig {
        max_calls: 2,
        ..EngineTargetConfig::default()
    };
    let floor = Duration::from_millis(engine.settings().floor_ms);
    let cancel = CancellationToken::new();
    let server = EngineServer::start(engine, SystemClock::shared(), &cancel)
        .await
        .unwrap();
    server.set_default_script(&fixture);

    let report = run(&config(&server.url(), &dir, floor, vec![1, 2, 6, 8]))
        .await
        .unwrap();
    server.shutdown(Duration::from_secs(5)).await.unwrap();
    std::fs::remove_dir_all(&dir).ok();

    // 1 and 2 pass; 6 asks for more calls than the server takes; the ramp stops there (8 is
    // never played).
    let verdicts: Vec<(usize, Verdict)> = report
        .steps
        .iter()
        .map(|s| (s.concurrent, s.verdict))
        .collect();
    assert_eq!(
        verdicts,
        vec![(1, Verdict::Pass), (2, Verdict::Pass), (6, Verdict::Fail)]
    );
    let failed = &report.steps[2];
    assert!(failed.calls_failed >= 1, "{failed:?}");
    assert!(!failed.failures.is_empty());
    assert!(
        failed.reasons.iter().any(|r| r.contains("calls failed")),
        "{:?}",
        failed.reasons
    );
    assert_eq!(
        (
            report.conclusion.max_concurrent,
            report.conclusion.first_failure,
            report.conclusion.limit
        ),
        (Some(2), Some(6), Limit::Found)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_slower_than_the_slo_fails_on_p95() {
    let dir = temp_dir("slow");
    write_fixture(&dir, &fixture());
    // The mock bot answers 700 ms after its 500 ms endpoint: floor 1200 ms.
    let bot_config = MockBotConfig {
        response_delay: 700 * MS,
        ..MockBotConfig::default()
    };
    let cancel = CancellationToken::new();
    let bot = MockBotServer::start(bot_config, SystemClock::shared(), &cancel)
        .await
        .unwrap();

    // Claimed floor 400 ms + 100 ms allowance: the bot's ~1.2 s is far over it.
    let mut strict = config(&bot.url(), &dir, 400 * MS, vec![1, 2]);
    strict.slo.over_floor = 100 * MS;
    let report = run(&strict).await.unwrap();
    assert_eq!(
        report.steps.len(),
        1,
        "the ramp stops at the first failing step"
    );
    let step = &report.steps[0];
    assert_eq!(step.verdict, Verdict::Fail);
    assert_eq!(step.calls_failed, 0);
    assert!(
        step.reasons
            .iter()
            .any(|r| r.contains("p95 V2V") && r.contains("limit 500 ms")),
        "{:?}",
        step.reasons
    );
    assert!(step.p95_over_floor_us.unwrap() > 600_000, "{step:?}");
    assert_eq!(report.conclusion.limit, Limit::FirstStepFailed);
    assert_eq!(report.conclusion.max_concurrent, None);

    // The same server against its true floor passes.
    let honest = config(&bot.url(), &dir, 1_200 * MS, vec![1]);
    let report = run(&honest).await.unwrap();
    assert_eq!(
        report.steps[0].verdict,
        Verdict::Pass,
        "{:?}",
        report.steps[0].reasons
    );
    bot.shutdown(Duration::from_secs(5)).await.unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_server_fails_every_call_and_a_token_in_the_url_never_leaks() {
    let dir = temp_dir("dead");
    write_fixture(&dir, &fixture());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let url = format!("ws://{addr}/call?token=hunter2");
    let mut config = config(&url, &dir, 950 * MS, vec![2]);
    config.client.connect_timeout = Duration::from_secs(2);
    let report = run(&config).await.unwrap();
    std::fs::remove_dir_all(&dir).ok();

    let step = &report.steps[0];
    assert_eq!(
        (step.verdict, step.calls_failed, step.turns_answered),
        (Verdict::Fail, 2, 0)
    );
    assert_eq!(report.conclusion.limit, Limit::FirstStepFailed);
    let everything = format!(
        "{}{:?}{}",
        report.to_json().unwrap(),
        report.steps,
        report.markdown(&[])
    );
    assert!(!everything.contains("hunter2"), "{everything}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configuration_errors_and_cancellation_are_errors() {
    let dir = temp_dir("config");
    write_fixture(&dir, &fixture());
    let mut bad = config("ws://127.0.0.1:1/", &dir, 950 * MS, vec![]);
    assert!(matches!(run(&bad).await, Err(BenchError::Config(_))));
    bad.steps = vec![1];
    bad.fixture = "missing".to_owned();
    assert!(matches!(run(&bad).await, Err(BenchError::Config(_))));

    // A cancelled run reports it instead of a partial result.
    let cancel = CancellationToken::new();
    cancel.cancel();
    let config = config("ws://127.0.0.1:1/", &dir, 950 * MS, vec![1]);
    assert!(matches!(
        run_capacity(SystemClock::shared(), &config, &cancel).await,
        Err(BenchError::Call(_))
    ));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_engine_server_runs_as_a_child_process_whose_cpu_is_read_by_pid() {
    let dir = temp_dir("child");
    write_fixture(&dir, &fixture());
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_brisvox-bench"));
    let args = vec![
        "--fixture".to_owned(),
        "tone_one".to_owned(),
        "--fixtures-dir".to_owned(),
        dir.to_string_lossy().into_owned(),
        "--min-gap-ms".to_owned(),
        "3000".to_owned(),
        "--max-calls".to_owned(),
        "8".to_owned(),
    ];
    let child = tokio::task::spawn_blocking(move || EngineChild::spawn(&exe, &args))
        .await
        .unwrap()
        .unwrap();
    assert!(
        child.url().starts_with("ws://127.0.0.1:"),
        "{}",
        child.url()
    );
    assert_ne!(child.pid(), std::process::id());

    let floor = Duration::from_millis(EngineTargetConfig::default().settings().floor_ms);
    let mut config = config(child.url(), &dir, floor, vec![2]);
    config.target_pid = Some(child.pid());
    config.target_scope = Scope::Tree;
    let report = run(&config).await.unwrap();
    tokio::task::spawn_blocking(move || child.stop())
        .await
        .unwrap();
    std::fs::remove_dir_all(&dir).ok();

    let step = &report.steps[0];
    assert_eq!(
        step.verdict,
        Verdict::Pass,
        "{:?} {:?}",
        step.reasons,
        step.failures
    );
    assert_eq!(step.turns_answered, 2);
    let target = step.target.unwrap();
    assert!(
        target.rss_peak_bytes > 0 && target.processes >= 1,
        "{target:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_generator_that_sends_late_stops_the_ramp_as_a_lower_bound_not_a_failure() {
    let dir = temp_dir("generator");
    let fixture = fixture();
    write_fixture(&dir, &fixture);
    let engine = EngineTargetConfig {
        max_calls: 8,
        ..EngineTargetConfig::default()
    };
    let floor = Duration::from_millis(engine.settings().floor_ms);
    let cancel = CancellationToken::new();
    let server = EngineServer::start(engine, SystemClock::shared(), &cancel)
        .await
        .unwrap();
    server.set_default_script(&fixture);

    // A lateness limit of one microsecond: nearly every frame the timer wakes is "late", a
    // sustained share, while the server is healthy.
    let mut late = config(&server.url(), &dir, floor, vec![1, 2]);
    late.lateness_limit = Duration::from_micros(1);
    let report = run(&late).await.unwrap();
    assert_eq!(report.steps.len(), 1, "the ramp stops at the generator");
    let step = &report.steps[0];
    assert_eq!(
        step.verdict,
        Verdict::GeneratorLimited,
        "{:?}",
        step.reasons
    );
    assert_eq!(step.calls_failed, 0);
    assert!(step.late_frames * 100 > step.timed_frames, "{step:?}");
    assert!(step.timed_frames > 0);
    assert_eq!(report.conclusion.limit, Limit::GeneratorLimited);
    assert_eq!(report.conclusion.first_failure, None);

    // With the same lateness limit but a share limit of 100 % no step is generator limited.
    let mut tolerant = config(&server.url(), &dir, floor, vec![1]);
    tolerant.lateness_limit = Duration::from_micros(1);
    tolerant.late_share_limit = 1.0;
    let report = run(&tolerant).await.unwrap();
    assert_eq!(
        report.steps[0].verdict,
        Verdict::Pass,
        "{:?}",
        report.steps[0].reasons
    );
    server.shutdown(Duration::from_secs(5)).await.unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_capacity_command_runs_sim_end_to_end_and_writes_the_environment() {
    // The CLI as a user runs it: `capacity --sim` starts the engine server child itself. The
    // child is limited to one call, so a two-call step must fail on refused calls: proof that
    // `--max-calls` reached the child; the engine block of the JSON states the flags given.
    let dir = temp_dir("cli");
    write_fixture(&dir, &fixture());
    let json = dir.join("out.json");
    let md = dir.join("out.md");
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_brisvox-bench"));
    let output = std::process::Command::new(&exe)
        .args([
            "capacity",
            "--sim",
            "--fixture",
            "tone_one",
            "--fixtures-dir",
        ])
        .arg(&dir)
        .args([
            "--steps",
            "1,2",
            "--rounds",
            "1",
            "--warmup-calls",
            "0",
            "--spread-ms",
            "0",
            "--min-gap-ms",
            "3000",
            "--cooldown-ms",
            "100",
            "--slo-over-floor-ms",
            "700",
            "--client-lateness-ms",
            "5000",
            "--max-calls",
            "1",
            "--pool-threads",
            "3",
            "--pool-queue",
            "99",
            "--server-workers",
            "2",
            "--out-json",
        ])
        .arg(&json)
        .arg("--out-md")
        .arg(&md)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
    let markdown = std::fs::read_to_string(&md).unwrap();
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(report["schema"], SCHEMA);
    assert_eq!(report["environment"]["os"], std::env::consts::OS);
    assert_eq!(report["engine"]["pool_threads"], 3);
    assert_eq!(report["engine"]["pool_queue"], 99);
    assert_eq!(report["settings"]["floor_ms"], 950);
    assert_eq!(report["steps"][0]["verdict"], "pass", "{stderr}");
    assert_eq!(report["steps"][1]["verdict"], "fail", "{stderr}");
    assert!(
        report["steps"][1]["calls_failed"].as_u64().unwrap() >= 1,
        "{stderr}"
    );
    assert_eq!(report["conclusion"]["max_concurrent"], 1);
    assert!(
        markdown.contains("3 pool threads, pool queue 99"),
        "{markdown}"
    );
}

#[test]
fn every_committed_capacity_result_states_its_platform() {
    // `bench/results/capacity-*.json` are read by later aggregations: each must say on its own
    // whether it is a Linux run, and still parse with the current report type.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/results");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !(name.starts_with("capacity-") && name.ends_with(".json")) {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        if value["schema"] == "brisvox-bench/capacity-summary/1" {
            // A median over runs (bench/capacity_summary.py): it names its platform and every
            // run file it summarises exists.
            assert_eq!(
                value["environment"]["not_linux_run"].as_bool(),
                Some(value["environment"]["os"] != "linux"),
                "{name}"
            );
            let runs = value["runs"].as_array().unwrap();
            assert!(!runs.is_empty(), "{name}: no runs");
            for run in runs {
                let file = run["file"].as_str().unwrap();
                assert!(dir.join(file).is_file(), "{name}: missing run file {file}");
            }
            seen += 1;
            continue;
        }
        let report: CapacityReport = serde_json::from_str(&text).unwrap();
        let environment = report
            .environment
            .unwrap_or_else(|| panic!("{name}: no environment block"));
        assert_eq!(report.schema, SCHEMA, "{name}");
        assert_eq!(
            environment.not_linux_run,
            environment.os != "linux",
            "{name}"
        );
        assert_eq!(
            environment.banner.is_some(),
            environment.not_linux_run,
            "{name}"
        );
        if name.contains("-windows-") {
            assert_eq!(environment.os, "windows", "{name}");
        }
        seen += 1;
    }
    assert!(seen >= 4, "only {seen} capacity results found");
}
