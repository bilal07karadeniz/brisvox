//! `brisvox-bench v2v --keep-invalid` end to end: the real binary against a live target
//! whose calls fail. The run is invalid (non-zero exit, `INVALID RUN` lines), but with
//! `--keep-invalid` the reports are still written for the caller to count the failures; without
//! it nothing is written, so an invalid run never replaces a committed result.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
// test helpers

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const RATE: u32 = 16_000;

fn temp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("brisvox-bench-cli-{tag}-{}", std::process::id()))
}

/// A 1.5 s fixture with one tone burst, written as `<dir>/tone_one.wav` and `.json`.
fn write_fixture(dir: &Path) {
    let total = 24_000u64;
    let (start, end) = (1_600u64, 9_600u64);
    let mut pcm = vec![0u8; usize::try_from(total).unwrap() * 2];
    for i in start..end {
        let sample =
            (8_000.0 * (std::f64::consts::TAU * 440.0 * i as f64 / f64::from(RATE)).sin()) as i16;
        let at = usize::try_from(i).unwrap() * 2;
        pcm[at..at + 2].copy_from_slice(&sample.to_le_bytes());
    }
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("tone_one.wav"),
        brisvox_transport::wav::encode(RATE, &pcm),
    )
    .unwrap();
    let sidecar = serde_json::json!({
        "name": "tone_one",
        "sample_rate": RATE,
        "samples": total,
        "segments": [{
            "start": start, "end": end, "lang": "en", "text": "synthetic", "source": "test"
        }],
    });
    std::fs::write(
        dir.join("tone_one.json"),
        serde_json::to_vec(&sidecar).unwrap(),
    )
    .unwrap();
}

/// A TCP port nobody listens on, so every call fails to connect at once.
fn dead_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn bench(dir: &Path, extra: &[&str], md: &Path, json: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_brisvox-bench"))
        .args(["v2v", "--url"])
        .arg(format!("ws://127.0.0.1:{}/call/SECRETPATH", dead_port()))
        .arg("--fixtures-dir")
        .arg(dir)
        .args(["--fixtures", "tone_one", "--runs", "3", "--pause-ms", "0"])
        .args(extra)
        .arg("--out-md")
        .arg(md)
        .arg("--out-json")
        .arg(json)
        .env_remove("BRISVOX_BENCH_URL")
        .output()
        .unwrap()
}

#[test]
fn keep_invalid_writes_the_reports_of_a_live_run_with_failed_calls_and_still_exits_invalid() {
    let dir = temp_dir("keep");
    write_fixture(&dir);
    let (md, json) = (dir.join("report.md"), dir.join("report.json"));
    let output = bench(&dir, &["--keep-invalid"], &md, &json);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "an invalid run must exit non-zero: {stderr}"
    );
    assert!(stderr.contains("INVALID RUN"), "{stderr}");
    assert!(
        stderr.contains("invalid run kept: --keep-invalid"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("SECRETPATH"),
        "the URL path leaked: {stderr}"
    );

    let markdown = std::fs::read_to_string(&md).expect("the markdown report is written");
    assert!(markdown.contains("tone_one"), "{markdown}");
    assert!(!markdown.contains("SECRETPATH"), "{markdown}");
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&json).expect("the json report is written")).unwrap();
    assert!(
        report["failed_runs"].as_u64().unwrap_or(0) >= 1,
        "the failed calls are countable in the json: {report}"
    );
    assert_eq!(report["overall"]["turns"].as_u64(), Some(0), "{report}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn without_keep_invalid_an_invalid_live_run_writes_nothing() {
    let dir = temp_dir("drop");
    write_fixture(&dir);
    let (md, json) = (dir.join("report.md"), dir.join("report.json"));
    let output = bench(&dir, &[], &md, &json);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("INVALID RUN"));
    assert!(
        !md.exists() && !json.exists(),
        "an invalid run must not write reports"
    );
    std::fs::remove_dir_all(&dir).ok();
}
