//! The V2V benchmark end to end: real sockets on 127.0.0.1, the real clock.
//!
//! - against a scripted server with known injected delays: the measurement lands on them, the
//!   client paces frames in real time, answers marks like a phone and forgets marks on a clear;
//! - against the in-process mock bot: N runs aggregate into percentiles, JSON and markdown, and
//!   the result sits above the bot's configured latency floor;
//! - against the engine (sim mode's target): every turn is answered above the configured floor
//!   of the mock providers and the end of turn, and a call the server was not told about is an
//!   error, not a silent pass;
//! - against a dead server: the failure is recorded, the benchmark stops early, and a token in
//!   the URL appears nowhere.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::disallowed_methods // a test server on real instants
)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_bench::BenchError;
use brisvox_bench::v2v::client::{ClientConfig, run_call};
use brisvox_bench::v2v::engine_target::{EndOfTurn, EngineServer, EngineTargetConfig};
use brisvox_bench::v2v::measure::Outcome;
use brisvox_bench::v2v::mock_bot::{MockBotConfig, MockBotServer};
use brisvox_bench::v2v::report::{BenchReport, CallKind, Mode};
use brisvox_bench::v2v::{BenchConfig, MAX_CONSECUTIVE_FAILURES, Target, run_bench};
use brisvox_core::frame::SampleRate;
use brisvox_core::{Secret, SystemClock};
use brisvox_sim::fixture::{Fixture, FixtureSegment};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
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

/// A synthetic fixture: `segments` are `(start sample, end sample, language)`.
fn fixture(name: &str, total: u64, segments: &[(u64, u64, &str)]) -> Fixture {
    let speech: Vec<(u64, u64)> = segments.iter().map(|&(s, e, _)| (s, e)).collect();
    Fixture {
        name: name.to_owned(),
        sample_rate: SampleRate::Hz16000,
        pcm: Bytes::from(tone_pcm(total, &speech)),
        segments: segments
            .iter()
            .map(|&(start, end, lang)| FixtureSegment {
                start,
                end,
                lang: lang.to_owned(),
                text: "synthetic".to_owned(),
                source: "test".to_owned(),
            })
            .collect(),
    }
}

/// Writes `fixture` as `<dir>/<name>.wav` and `.json`, the way `scripts/fixtures` does.
fn write_fixture(dir: &std::path::Path, fixture: &Fixture) {
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
    std::env::temp_dir().join(format!("brisvox-bench-v2v-{tag}-{}", std::process::id()))
}

// ---------------------------------------------------------------------------------------------
// A scripted server: speaks the protocol by hand and answers on cue.

#[derive(Clone)]
enum Action {
    /// This many 20 ms frames of audible audio, sent back to back.
    Audio(usize),
    Mark(&'static str),
    Clear,
}

/// After the `after_frames`-th binary frame has arrived, wait `delay`, then do `actions`.
struct Step {
    after_frames: usize,
    delay: Duration,
    actions: Vec<Action>,
}

#[derive(Default)]
struct Seen {
    started_at: Option<Instant>,
    frame_arrivals: Vec<Instant>,
    marks_echoed: Vec<(String, Instant)>,
    marks_sent: Vec<(String, Instant)>,
    /// When each audio frame went out (stamped just before the write).
    audio_sent: Vec<Instant>,
}

struct Scripted {
    url: String,
    seen: Arc<Mutex<Seen>>,
}

impl Scripted {
    async fn start(steps: Vec<Step>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/call", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Seen::default()));
        tokio::spawn(serve_scripted(listener, steps, Arc::clone(&seen)));
        Self { url, seen }
    }
}

async fn serve_scripted(listener: TcpListener, steps: Vec<Step>, seen: Arc<Mutex<Seen>>) {
    let (stream, _) = listener.accept().await.unwrap();
    stream.set_nodelay(true).unwrap();
    let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
    let (mut sink, mut source) = ws.split();
    let audible = Bytes::from(
        std::iter::repeat_n(8_000i16.to_le_bytes(), 320)
            .flatten()
            .collect::<Vec<u8>>(),
    );
    let mut frames = 0usize;
    // (due, message, mark id if it is a mark), in the order they were scheduled.
    let mut pending: Vec<(Instant, Message, Option<String>)> = Vec::new();
    loop {
        let next_due = pending.iter().map(|(due, ..)| *due).min();
        tokio::select! {
            message = source.next() => {
                let now = Instant::now();
                match message {
                    None | Some(Err(_) | Ok(Message::Close(_))) => break,
                    Some(Ok(Message::Text(text))) => {
                        let json: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
                        match json["type"].as_str().unwrap() {
                            "start" => {
                                seen.lock().unwrap().started_at = Some(now);
                                let started = r#"{"type":"started","version":1,"call_id":"scripted","sample_rate":16000,"encoding":"pcm16"}"#;
                                sink.send(Message::text(started)).await.unwrap();
                            }
                            "mark" => seen.lock().unwrap().marks_echoed.push((
                                json["id"].as_str().unwrap().to_owned(),
                                now,
                            )),
                            "stop" => {
                                sink.send(Message::text(r#"{"type":"stop","reason":"ended"}"#)).await.ok();
                                sink.close().await.ok();
                                break;
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Message::Binary(_))) => {
                        frames += 1;
                        seen.lock().unwrap().frame_arrivals.push(now);
                        for step in steps.iter().filter(|s| s.after_frames == frames) {
                            let due = now + step.delay;
                            for action in &step.actions {
                                match action {
                                    Action::Audio(n) => {
                                        for _ in 0..*n {
                                            pending.push((due, Message::Binary(audible.clone()), None));
                                        }
                                    }
                                    Action::Mark(id) => pending.push((
                                        due,
                                        Message::text(format!(r#"{{"type":"mark","id":"{id}"}}"#)),
                                        Some((*id).to_owned()),
                                    )),
                                    Action::Clear => pending.push((
                                        due,
                                        Message::text(r#"{"type":"clear","epoch":1}"#),
                                        None,
                                    )),
                                }
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                }
            }
            () = async {
                match next_due {
                    Some(due) => tokio::time::sleep_until(due).await,
                    None => std::future::pending().await,
                }
            } => {
                let now = Instant::now();
                let (due, later): (Vec<_>, Vec<_>) =
                    pending.drain(..).partition(|(due, ..)| *due <= now);
                pending = later;
                for (_, message, mark) in due {
                    if let Some(id) = mark {
                        seen.lock().unwrap().marks_sent.push((id, Instant::now()));
                    }
                    if matches!(message, Message::Binary(_)) {
                        seen.lock().unwrap().audio_sent.push(Instant::now());
                    }
                    if sink.send(message).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

fn client(url: &str) -> ClientConfig {
    let mut config = ClientConfig::new(url);
    config.response_timeout = Duration::from_secs(3);
    config
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_measured_v2v_lands_on_the_injected_delay() {
    // Two utterances ending at 1.0 s (frame 50) and 2.4 s (frame 120); the server answers the
    // first 150 ms and the second 300 ms after the frame that ends the speech arrived.
    let fixture = fixture(
        "known_delays",
        48_000,
        &[(3_200, 16_000, "en"), (32_000, 38_400, "tr")],
    );
    let server = Scripted::start(vec![
        Step {
            after_frames: 50,
            delay: 150 * MS,
            actions: vec![Action::Audio(5)],
        },
        Step {
            after_frames: 120,
            delay: 300 * MS,
            actions: vec![Action::Audio(5)],
        },
    ])
    .await;
    let outcome = run_call(
        SystemClock::shared(),
        &client(&server.url),
        &fixture,
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.failure, None);
    assert_eq!(outcome.call_id, "scripted");
    assert_eq!(outcome.frames_sent, 150, "3 s of 20 ms frames");
    assert_eq!(outcome.turns.len(), 2);
    for (turn, delay) in outcome.turns.iter().zip([150 * MS, 300 * MS]) {
        assert_eq!(turn.outcome, Outcome::Answered, "{turn:?}");
        let v2v = turn.v2v.unwrap();
        // The server waits `delay` after the last speech frame arrived (at the end of speech,
        // plus a few ms of loopback and scheduling); timers never fire early, and a timer tick
        // is up to ~16 ms on Windows.
        assert!(
            v2v >= delay,
            "turn {} measured {v2v:?}, injected {delay:?}",
            turn.index
        );
        assert!(
            v2v <= delay + 80 * MS,
            "turn {} measured {v2v:?}, injected {delay:?}",
            turn.index
        );
    }
    assert_eq!(outcome.turns[0].eos, 1_000 * MS);
    assert_eq!(outcome.turns[1].eos, 2_400 * MS);
    assert_eq!(outcome.turns[1].lang, "tr");
    // The two delays are 150 ms apart, and the measurement resolves that difference.
    let spread = outcome.turns[1]
        .v2v
        .unwrap()
        .abs_diff(outcome.turns[0].v2v.unwrap());
    assert!(
        spread.abs_diff(150 * MS) <= 60 * MS,
        "delays differ by 150 ms, measured {spread:?}"
    );

    // Server-side ground truth on the same monotonic clock: the reply left the server
    // `server_v2v` after the frame that ends the speech arrived there. That frame cannot leave
    // the client before the end of speech and the reply cannot arrive before it was sent, so the
    // measurement is never below `server_v2v`; it exceeds it only by the send lateness of the
    // final speech frame plus two loopback hops. (A client that sent frames early, or stamped
    // arrivals late, breaks one of the two bounds independently of timer resolution.)
    let seen = server.seen.lock().unwrap();
    assert_eq!(seen.audio_sent.len(), 10);
    for (turn, (eos_frame, first_audio)) in outcome.turns.iter().zip([(49, 0), (119, 5)]) {
        let server_v2v = seen.audio_sent[first_audio] - seen.frame_arrivals[eos_frame];
        // Reports are in whole microseconds.
        let server_v2v = Duration::from_micros(server_v2v.as_micros() as u64);
        let v2v = turn.v2v.unwrap();
        assert!(
            v2v >= server_v2v,
            "turn {}: measured {v2v:?} < server-side {server_v2v:?}",
            turn.index
        );
        let slack = outcome.max_send_lateness + 15 * MS;
        assert!(
            v2v <= server_v2v + slack,
            "turn {}: measured {v2v:?} > server-side {server_v2v:?} + {slack:?}",
            turn.index
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frames_are_paced_in_real_time_and_marks_and_clears_behave_like_a_phone() {
    let fixture = fixture("marks", 24_000, &[(3_200, 16_000, "en")]);
    let mut config = client("");
    config.response_timeout = 300 * MS;
    let server = Scripted::start(vec![
        // 60 ms of audio then a mark: the client echoes it once the 60 ms have played.
        Step {
            after_frames: 10,
            delay: Duration::ZERO,
            actions: vec![Action::Audio(3), Action::Mark("m1")],
        },
        // A second of audio and a mark behind it, then a clear 100 ms after that: the client
        // forgets the mark and must never echo it.
        Step {
            after_frames: 20,
            delay: Duration::ZERO,
            actions: vec![Action::Audio(50), Action::Mark("m2")],
        },
        Step {
            after_frames: 25,
            delay: Duration::ZERO,
            actions: vec![Action::Clear],
        },
    ])
    .await;
    config.url = Secret::new(server.url.clone());
    let outcome = run_call(
        SystemClock::shared(),
        &config,
        &fixture,
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.failure, None);
    assert_eq!(outcome.clears, 1);
    assert_eq!(outcome.audio_frames_received, 53);
    assert_eq!(outcome.audible_frames, 53);
    // No audio after the end of speech: not answered, and not a failure of the call.
    assert_eq!(outcome.turns[0].outcome, Outcome::NoResponse);

    let seen = server.seen.lock().unwrap();
    let echoed: Vec<&str> = seen
        .marks_echoed
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(echoed, ["m1"], "m2 was cleared and must not be echoed");
    let sent = seen.marks_sent.iter().find(|(id, _)| id == "m1").unwrap().1;
    let echo = seen.marks_echoed[0].1;
    let lag = echo - sent;
    assert!(
        lag >= 55 * MS && lag <= 300 * MS,
        "m1 follows 60 ms of audio, echoed after {lag:?}"
    );

    // Pacing: frame k (1-based) is sent when its last sample has been spoken, k * 20 ms after
    // the call started, never earlier, and does not drift. After the 75 frames of the fixture the
    // line stays open on the same grid with silence until the 300 ms response window closes.
    let started = seen.started_at.unwrap();
    let frames = seen.frame_arrivals.len();
    assert!((85..=90).contains(&frames), "{frames} frames");
    assert_eq!(u64::try_from(frames).unwrap(), outcome.frames_sent);
    for k in [1usize, 10, 40, 75, 85] {
        let after = seen.frame_arrivals[k - 1] - started;
        let ideal = Duration::from_millis(20 * k as u64);
        assert!(
            after >= ideal,
            "frame {k} arrived after {after:?}, before {ideal:?}"
        );
        assert!(
            after <= ideal + 120 * MS,
            "frame {k} arrived after {after:?} for {ideal:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n_runs_against_the_mock_bot_aggregate_into_percentiles() {
    // Two utterances of 0.5 s (en, tr), 1.0 s apart on the recording, padded to 1.5 s.
    let dir = temp_dir("sim");
    write_fixture(
        &dir,
        &fixture(
            "tone_two",
            40_000,
            &[(1_600, 9_600, "en"), (25_600, 33_600, "tr")],
        ),
    );
    let bot = MockBotConfig {
        endpoint_silence: 200 * MS,
        response_delay: 150 * MS,
        response_jitter: 50 * MS,
        reply: 300 * MS,
        ..MockBotConfig::default()
    };
    let floor = Duration::from_millis(bot.settings().floor_ms());
    let mut config = BenchConfig::sim();
    config.target = Target::MockBot(bot);
    config.fixtures = vec!["tone_two".to_owned()];
    config.fixtures_dir = dir.clone();
    config.runs = 3;
    config.min_gap = 1_500 * MS;
    config.pause = 50 * MS;
    config.client.response_timeout = Duration::from_secs(3);

    let report = run_bench(SystemClock::shared(), &config, &CancellationToken::new())
        .await
        .unwrap();
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(report.mode, Mode::Sim);
    assert_eq!(report.runs.len(), 3);
    assert_eq!(report.failed_runs, 0);
    assert!(
        report.problems(true).is_empty(),
        "{:?}",
        report.problems(true)
    );
    assert!(
        report.target.starts_with("ws://127.0.0.1:"),
        "{}",
        report.target
    );
    // 3 runs x 2 turns, all clean samples.
    assert_eq!(report.overall.turns, 6);
    assert_eq!(report.overall.answered, 6);
    assert_eq!(report.overall.v2v.count, 6);
    let v2v = &report.overall.v2v;
    assert!(v2v.p50 <= v2v.p90 && v2v.p90 <= v2v.p99 && v2v.p99 <= v2v.max);
    // The bot cannot answer before its floor; the remainder (jitter 0..50 ms, frame granularity,
    // pacer phase and transport) stays small.
    assert!(
        v2v.p50 >= floor - 2 * MS && v2v.max <= floor + 50 * MS + 150 * MS,
        "floor {floor:?}, measured {v2v:?}"
    );
    assert_eq!(report.mock_bot.unwrap().floor_ms(), 350);
    // Per fixture, language and turn position.
    assert_eq!(report.per_fixture.len(), 1);
    let languages: Vec<&str> = report
        .per_language
        .iter()
        .map(|l| l.name.as_str())
        .collect();
    assert_eq!(languages, ["en", "tr"]);
    assert_eq!(report.per_turn.len(), 2);
    assert!(report.per_turn.iter().all(|t| t.aggregate.v2v.count == 3));
    // The padding moved the second utterance: it starts 1.5 s after the first ended (0.6 s).
    assert_eq!(report.per_turn[1].eos, 2_600 * MS);
    assert!(
        report.max_send_lateness < 200 * MS,
        "{:?}",
        report.max_send_lateness
    );
    // Both output formats carry the result.
    let json = report.to_json().unwrap();
    let back: BenchReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back, report);
    let markdown = report.markdown(&["command: `test`".to_owned()]);
    assert!(
        markdown.contains("| all | 6 | 6 | 0 | 0 | 0 |"),
        "{markdown}"
    );
    assert!(markdown.contains("Latency floor"), "{markdown}");
}

/// Memory calls the engine benches play at once.
fn config_memory_calls() -> usize {
    4
}

/// Runs `runs` calls of a two-utterance fixture (en, tr; 0.5 s each, padded 3 s apart) against
/// the engine configured by `engine`, checks the run is clean and every turn answered, and
/// returns the report with every answered turn's V2V.
async fn engine_bench(
    label: &str,
    engine: EngineTargetConfig,
    runs: usize,
) -> (BenchReport, Vec<Duration>) {
    let dir = temp_dir(label);
    write_fixture(
        &dir,
        &fixture(
            "tone_two",
            40_000,
            &[(1_600, 9_600, "en"), (25_600, 33_600, "tr")],
        ),
    );
    let settings = engine.settings();
    let mut config = BenchConfig::sim();
    config.target = Target::Engine(engine);
    config.fixtures = vec!["tone_two".to_owned()];
    config.fixtures_dir = dir.clone();
    config.runs = runs;
    config.min_gap = 3_000 * MS;
    config.pause = 50 * MS;
    config.memory_calls = config_memory_calls();
    config.client.response_timeout = Duration::from_secs(4);

    let report = run_bench(SystemClock::shared(), &config, &CancellationToken::new())
        .await
        .unwrap();
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(report.mode, Mode::Sim);
    assert!(report.mock_bot.is_none());
    assert_eq!(report.engine.as_ref(), Some(&settings));
    assert!(
        report.problems(true).is_empty(),
        "{:?}",
        report.problems(true)
    );
    assert_eq!(report.overall.turns, 2 * runs);
    assert_eq!(report.overall.answered, 2 * runs);
    // As many barge-in calls followed (one barge-in each); they stay out of the latency.
    assert_eq!(
        report
            .runs
            .iter()
            .filter(|r| r.kind == CallKind::BargeIn)
            .count(),
        runs
    );
    // The engine's own stage times: every latency turn gives each turn stage, every barge-in
    // call the clear; the memory was read.
    let stages = report.engine_stages.as_ref().expect("the engine's stages");
    let count = |name: &str| {
        stages
            .stages
            .iter()
            .find(|s| s.name == name)
            .unwrap()
            .dist
            .count
    };
    for stage in [
        "vad_stop_to_finalize",
        "turn_end_to_llm_request",
        "llm_request_to_first_token",
        "first_token_to_tts_text",
        "first_chunk_to_tts_text",
        "tts_request_to_first_byte",
        "tts_audio_to_transport",
        "turn_end_to_first_audio",
    ] {
        assert_eq!(count(stage), 2 * runs, "{stage}: {stages:?}");
    }
    assert!(stages.interrupts >= runs, "{stages:?}");
    assert_eq!(count("interrupt_to_clear"), stages.interrupts, "{stages:?}");
    let rss = stages.rss.expect("resident memory is reported");
    assert!(
        rss.cold_bytes > 0 && rss.peak_bytes >= rss.warm_bytes,
        "{rss:?}"
    );
    // The memory calls ran at once, every one of them clean, and stay out of the latency.
    assert_eq!(rss.calls_at_once, config_memory_calls());
    let memory: Vec<_> = report
        .runs
        .iter()
        .filter(|r| r.kind == CallKind::Memory)
        .collect();
    assert_eq!(memory.len(), config_memory_calls());
    assert!(memory.iter().all(|r| r.failure.is_none()), "{memory:?}");
    let samples: Vec<Duration> = report
        .runs
        .iter()
        .filter(|run| run.kind == CallKind::Latency)
        .flat_map(|run| &run.turns)
        .filter(|turn| turn.outcome == Outcome::Answered)
        .map(|turn| turn.v2v.unwrap())
        .collect();
    assert_eq!(samples.len(), 2 * runs);
    (report, samples)
}

/// Every answered turn lands at or above `floor` (less one 20 ms frame of grid) and at most
/// `slack` above it.
fn assert_every_turn_above_the_floor(samples: &[Duration], floor: Duration, slack: Duration) {
    for v2v in samples {
        assert!(
            *v2v >= floor - 20 * MS && *v2v <= floor + slack,
            "floor {floor:?}, slack {slack:?}, every turn: {samples:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sim_mode_measures_the_engine_above_its_configured_floor() {
    // The engine (run_call over the mock providers, Smart Turn end of turn) answers each
    // utterance; the LLM's first token is set apart from the default so the floor proves the
    // configured providers are on the measured path.
    let mut engine = EngineTargetConfig::default();
    engine.latencies.llm_first_token = 500 * MS;
    let settings = engine.settings();
    // 300 ms VAD stop + 150 ms STT final + 500 ms + 150 ms.
    assert_eq!(settings.end_of_turn_floor_ms, 450);
    assert_eq!(settings.floor_ms, 1_100);
    let floor = Duration::from_millis(settings.floor_ms);

    let (report, samples) = engine_bench("engine", engine, 2).await;

    // The tone's energy stop is backdated to the ground-truth EOS, so no turn can come before
    // the floor (less one 20 ms frame of grid): a floor the engine beats on any turn would be a
    // floor the report overstates. The engine's own time and the transport stay well under half
    // a second even in a debug build on a busy machine.
    assert_every_turn_above_the_floor(&samples, floor, 500 * MS);
    let markdown = report.markdown(&[]);
    assert!(markdown.contains("latency floor 1100 ms"), "{markdown}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sim_mode_measures_the_engine_with_a_fixed_silence_end_of_turn() {
    // `--engine-turn fixed:600`: the end of turn is 600 ms after the backdated VAD stop (longer
    // than the 450 ms VAD stop + STT final gate), so the floor is 600 + 350 + 150.
    let engine = EngineTargetConfig {
        end_of_turn: EndOfTurn::FixedSilence(600 * MS),
        ..EngineTargetConfig::default()
    };
    let settings = engine.settings();
    assert_eq!(settings.end_of_turn, "fixed_silence");
    assert_eq!(settings.turn_silence_ms, Some(600));
    assert_eq!(settings.end_of_turn_floor_ms, 600);
    assert_eq!(settings.floor_ms, 1_100);
    let floor = Duration::from_millis(settings.floor_ms);

    let (_, samples) = engine_bench("engine-fixed", engine, 1).await;

    // The silence counts from the backdated stop: counted from the reported stop (300 ms of VAD
    // hangover later) every turn would land at least 300 ms above the floor, past this slack.
    assert_every_turn_above_the_floor(&samples, floor, 250 * MS);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_engine_call_without_a_script_is_recorded_as_an_error() {
    // The engine's mock STT knows what to transcribe only from `expect`: a call that arrives
    // without it hears nothing and must not pass silently.
    let fixture = fixture("unscripted", 24_000, &[(1_600, 9_600, "en")]);
    let cancel = CancellationToken::new();
    let server = EngineServer::start(
        EngineTargetConfig::default(),
        SystemClock::shared(),
        &cancel,
    )
    .await
    .unwrap();
    let mut config = client(&server.url());
    config.response_timeout = Duration::from_millis(1_500);
    let outcome = run_call(SystemClock::shared(), &config, &fixture, &cancel)
        .await
        .unwrap();
    assert!(server.wait_idle(Duration::from_secs(10)).await);
    let errors = server.take_errors();
    server.shutdown(Duration::from_secs(5)).await.unwrap();

    assert_eq!(outcome.turns[0].outcome, Outcome::NoResponse);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("without a script"), "{errors:?}");
    assert_eq!(server.active_calls(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_server_is_recorded_and_stops_the_benchmark_and_the_url_token_never_leaks() {
    let dir = temp_dir("dead");
    write_fixture(&dir, &fixture("tone_one", 24_000, &[(1_600, 9_600, "en")]));
    // A port nobody listens on.
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let mut config = BenchConfig::live(format!(
        "ws://user:pw@127.0.0.1:{port}/call/SECRETPATH?token=SECRETTOKEN"
    ));
    config.fixtures = vec!["tone_one".to_owned()];
    config.fixtures_dir = dir.clone();
    config.runs = 10;
    config.pause = Duration::ZERO;
    config.client.connect_timeout = Duration::from_secs(3);

    let debug = format!("{config:?}");
    assert!(
        !debug.contains("SECRET") && !debug.contains("pw@"),
        "{debug}"
    );

    let report = run_bench(SystemClock::shared(), &config, &CancellationToken::new())
        .await
        .unwrap();
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(report.mode, Mode::Live);
    assert_eq!(
        report.runs.len(),
        MAX_CONSECUTIVE_FAILURES,
        "gave up after three failures in a row"
    );
    assert_eq!(report.failed_runs, MAX_CONSECUTIVE_FAILURES);
    assert_eq!(report.overall.turns, 0);
    let problems = report.problems(false);
    assert!(
        problems.iter().any(|p| p.contains("connect")),
        "{problems:?}"
    );
    assert!(
        problems.iter().any(|p| p.contains("no turn was answered")),
        "{problems:?}"
    );
    for text in [
        report.to_json().unwrap(),
        report.markdown(&[]),
        format!("{report:?}"),
        problems.join("\n"),
    ] {
        for secret in ["SECRETTOKEN", "SECRETPATH", "pw@", "user:"] {
            assert!(!text.contains(secret), "{secret} leaked into: {text}");
        }
    }
    assert_eq!(report.target, format!("ws://127.0.0.1:{port}/..."));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_line_stays_open_until_a_bot_that_needs_more_silence_than_the_fixture_has_answers() {
    // 0.4 s of trailing silence after the only utterance, but the bot ends a turn only after
    // 1.0 s of silence counted from the frames it receives (as VAD and smart-turn analyzers do).
    // A client that went quiet at the end of the fixture would never get the answer.
    let fixture = fixture("short_tail", 22_400, &[(3_200, 16_000, "en")]);
    let cancel = CancellationToken::new();
    let bot_config = MockBotConfig {
        endpoint_silence: 1_000 * MS,
        response_delay: 100 * MS,
        ..MockBotConfig::default()
    };
    let bot = MockBotServer::start(bot_config, SystemClock::shared(), &cancel)
        .await
        .unwrap();
    let outcome = run_call(
        SystemClock::shared(),
        &client(&bot.url()),
        &fixture,
        &cancel,
    )
    .await
    .unwrap();
    bot.shutdown(Duration::from_secs(3)).await.unwrap();

    assert_eq!(outcome.failure, None);
    let turn = &outcome.turns[0];
    assert_eq!(turn.outcome, Outcome::Answered, "{turn:?}");
    let v2v = turn.v2v.unwrap();
    assert!(v2v >= 1_100 * MS, "below the bot's floor: {v2v:?}");
    assert!(v2v <= 1_400 * MS, "{v2v:?}");
    // 70 frames of fixture, then silence until the reply (about 1.1 s after EOS, 0.7 s after the
    // fixture ended), not until the response window closed.
    assert!(
        (100..=120).contains(&outcome.frames_sent),
        "{}",
        outcome.frames_sent
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_pings_but_never_starts_the_call_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/call", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let (mut sink, mut source) = ws.split();
        // Drain whatever the client sends (start, pongs) without ever answering it.
        tokio::spawn(async move { while source.next().await.is_some() {} });
        loop {
            tokio::time::sleep(100 * MS).await;
            let ping = sink.send(Message::Ping(Bytes::from_static(b"hi"))).await;
            let keepalive = sink.send(Message::text(r#"{"type":"keepalive"}"#)).await;
            if ping.is_err() || keepalive.is_err() {
                return;
            }
        }
    });
    let mut config = client(&url);
    config.io_timeout = 600 * MS;
    let fixture = fixture("never_started", 16_000, &[(1_600, 9_600, "en")]);
    let begin = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_call(
            SystemClock::shared(),
            &config,
            &fixture,
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("run_call must give up on its own");
    match result {
        Err(BenchError::Call(message)) => {
            assert!(message.contains("no answer to start"), "{message}")
        }
        other => panic!("expected a call error, got {other:?}"),
    }
    assert!(
        begin.elapsed() < Duration::from_secs(3),
        "{:?}",
        begin.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_live_calls_are_played_in_groups_and_reported_in_plan_order() {
    let dir = temp_dir("concurrent");
    write_fixture(
        &dir,
        &fixture(
            "tone_two",
            40_000,
            &[(1_600, 9_600, "en"), (25_600, 33_600, "tr")],
        ),
    );
    let cancel = CancellationToken::new();
    let bot = MockBotServer::start(
        MockBotConfig {
            endpoint_silence: 200 * MS,
            response_delay: 150 * MS,
            reply: 300 * MS,
            ..MockBotConfig::default()
        },
        SystemClock::shared(),
        &cancel,
    )
    .await
    .unwrap();
    let mut config = BenchConfig::live(bot.url());
    config.fixtures = vec!["tone_two".to_owned()];
    config.fixtures_dir = dir.clone();
    config.runs = 5;
    config.concurrency = 3;
    config.min_gap = 1_500 * MS;
    config.pause = 50 * MS;
    config.client.response_timeout = Duration::from_secs(3);

    let report = run_bench(SystemClock::shared(), &config, &cancel)
        .await
        .unwrap();
    bot.shutdown(Duration::from_secs(5)).await.unwrap();
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(report.mode, Mode::Live);
    assert_eq!(report.settings.concurrency, 3);
    assert!(report.markdown(&[]).contains("3 calls played at once"));
    assert_eq!(report.failed_runs, 0, "{:?}", report.problems(false));
    let order: Vec<usize> = report.runs.iter().map(|r| r.run).collect();
    assert_eq!(order, [0, 1, 2, 3, 4], "plan order, not completion order");
    assert!(report.runs.iter().all(|r| r.turns.len() == 2));
    assert_eq!(report.overall.turns, 10);
}

#[tokio::test]
async fn concurrency_is_checked_before_any_call() {
    let cancel = CancellationToken::new();
    let mut config = BenchConfig::sim();
    config.concurrency = 2;
    let error = run_bench(SystemClock::shared(), &config, &cancel)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("live mode"), "{error}");
    let mut config = BenchConfig::live("ws://127.0.0.1:1");
    config.concurrency = 0;
    let error = run_bench(SystemClock::shared(), &config, &cancel)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("concurrency"), "{error}");
}

#[tokio::test]
async fn configuration_errors_are_reported_before_any_call() {
    let mut config = BenchConfig::sim();
    config.runs = 0;
    let cancel = CancellationToken::new();
    let error = run_bench(SystemClock::shared(), &config, &cancel)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("runs"), "{error}");
    let mut config = BenchConfig::sim();
    config.fixtures = vec!["no_such_fixture".to_owned()];
    let error = run_bench(SystemClock::shared(), &config, &cancel)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no_such_fixture"), "{error}");
    let config = BenchConfig::live("");
    let error = run_bench(SystemClock::shared(), &config, &cancel)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("URL"), "{error}");
}
