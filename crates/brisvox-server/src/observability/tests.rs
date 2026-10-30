//! Observability tests: stage spans, interrupted turns, the exposition, the health route,
//! settings and redaction of the real log writer.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_core::{SharedClock, SimClock, TimelineRecorder, TurnEpoch, TurnEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
use tokio::time::Instant;

use super::exposition::{Family, parse};
use super::trace::{AttrValue, IdGen, MemoryExporter, TurnTrace};
use super::*;
use crate::session::ManagerConfig;
use crate::telemetry::{LogFormat, build_filter, build_subscriber};

const MS: Duration = Duration::from_millis(1);

fn epoch(n: u32) -> TurnEpoch {
    TurnEpoch::new(n)
}

/// A rig: observability with an in-memory exporter, one call's observer and its recorder.
struct Rig {
    obs: Arc<Observability>,
    exporter: Arc<MemoryExporter>,
    clock: SharedClock,
    rec: TimelineRecorder,
    call: Arc<CallObserver>,
}

impl Rig {
    fn new() -> Self {
        let exporter = MemoryExporter::new(64);
        let obs = Arc::new(
            Observability::new()
                .with_exporter(exporter.clone())
                .with_ids(IdGen::with_seed(42)),
        );
        let clock = SimClock::shared();
        let call = obs.call(7, "test-agent", clock.clone());
        let rec = call.recorder();
        Self {
            obs,
            exporter,
            clock,
            rec,
            call,
        }
    }

    /// A complete turn with the stage gaps of the timeline tests (ms after `t0`).
    fn full_turn(&self, e: u32, t0: Instant) {
        use TurnEvent as E;
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let rec = &self.rec;
        rec.mark(epoch(e), E::UserSpeechStart, at(0));
        rec.mark(epoch(e), E::UserSpeechEnd, at(1_000));
        rec.mark(epoch(e), E::SttFinal, at(1_150));
        rec.mark(epoch(e), E::TurnEnd, at(1_400));
        let round = rec.llm_request(epoch(e), at(1_405)).unwrap();
        rec.llm_first_token(epoch(e), round, at(1_705));
        rec.mark(epoch(e), E::LlmFirstSentence, at(1_805));
        rec.llm_done(epoch(e), round, at(1_900));
        rec.mark(epoch(e), E::TtsRequest, at(1_806));
        rec.mark(epoch(e), E::TtsFirstByte, at(1_956));
        rec.mark(epoch(e), E::FirstAudioOut, at(1_960));
    }
}

fn span_names(trace: &TurnTrace) -> Vec<&'static str> {
    trace.spans.iter().map(|s| s.name).collect()
}

fn int(trace: &TurnTrace, span: &str, key: &str) -> Option<i64> {
    match trace.span(span)?.attribute(key)? {
        AttrValue::Int(i) => Some(*i),
        _ => None,
    }
}

fn manager() -> SessionManager {
    SessionManager::new(ManagerConfig::new(4).unwrap(), SimClock::shared()).unwrap()
}

#[tokio::test(start_paused = true)]
async fn a_turn_becomes_one_trace_with_the_stage_spans() {
    let rig = Rig::new();
    let t0 = rig.clock.now();
    rig.full_turn(0, t0);
    assert!(rig.exporter.traces().is_empty(), "the turn is still open");
    rig.rec.finish();
    let traces = rig.exporter.take();
    assert_eq!(traces.len(), 1, "one trace per turn");
    let trace = &traces[0];
    assert_eq!(
        span_names(trace),
        [
            "turn",
            "user_speech",
            "end_of_turn",
            "stt_final",
            "llm",
            "tts",
            "audio_out"
        ],
        "root first, then the stages in start order (ties keep the table order)"
    );
    // Parentage, ids.
    let root = trace.root().unwrap();
    assert!(
        trace.spans[1..]
            .iter()
            .all(|s| s.parent == Some(root.span_id))
    );
    assert!(trace.spans.iter().all(|s| s.trace_id == trace.trace_id));
    let mut ids: Vec<_> = trace.spans.iter().map(|s| s.span_id.0).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), trace.spans.len(), "span ids are unique");
    // Timing is the clock's: the anchor is the wall time at `t0`.
    let wall0 = u64::try_from(
        rig.clock
            .wall()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
    .unwrap();
    assert_eq!(root.start_unix_nanos, wall0);
    assert_eq!(root.duration(), 1_960 * MS);
    let stage = |name: &str| trace.span(name).unwrap();
    assert_eq!(stage("user_speech").duration(), 1_000 * MS);
    assert_eq!(stage("end_of_turn").duration(), 400 * MS);
    assert_eq!(stage("stt_final").duration(), 150 * MS);
    assert_eq!(stage("llm").duration(), 495 * MS);
    assert_eq!(stage("llm").start_unix_nanos, wall0 + 1_405_000_000);
    assert_eq!(stage("tts").duration(), 150 * MS);
    assert_eq!(stage("audio_out").duration(), 4 * MS);
    // What identifies the turn.
    assert_eq!(int(trace, "turn", "brisvox.session.id"), Some(7));
    assert_eq!(int(trace, "turn", "brisvox.turn.epoch"), Some(0));
    assert_eq!(int(trace, "llm", "brisvox.llm.round"), Some(0));
    assert_eq!(
        int(trace, "llm", "brisvox.llm.time_to_first_token_us"),
        Some(300_000)
    );
    assert_eq!(
        trace.root().unwrap().attribute("brisvox.agent"),
        Some(&AttrValue::Str("test-agent".into()))
    );
    assert_eq!(rig.obs.metrics().traces.exported.get(), 1);
}

#[tokio::test(start_paused = true)]
async fn every_turn_of_a_call_gets_its_own_trace() {
    let rig = Rig::new();
    let t0 = rig.clock.now();
    rig.full_turn(0, t0);
    rig.full_turn(1, t0 + 5_000 * MS);
    rig.full_turn(2, t0 + 10_000 * MS);
    rig.rec.finish();
    let traces = rig.exporter.take();
    assert_eq!(traces.len(), 3);
    let epochs: Vec<_> = traces
        .iter()
        .map(|t| int(t, "turn", "brisvox.turn.epoch").unwrap())
        .collect();
    assert_eq!(epochs, [0, 1, 2]);
    let mut trace_ids: Vec<_> = traces.iter().map(|t| t.trace_id.0).collect();
    trace_ids.sort_unstable();
    trace_ids.dedup();
    assert_eq!(trace_ids.len(), 3);
}

#[tokio::test(start_paused = true)]
async fn an_interrupted_turn_waits_for_its_silence_and_is_exported_once() {
    let rig = Rig::new();
    let t0 = rig.clock.now();
    rig.full_turn(0, t0);
    rig.rec
        .interrupt(epoch(0), epoch(1), t0 + 2_000 * MS, Some(t0 + 1_990 * MS));
    assert!(
        rig.exporter.traces().is_empty(),
        "turn 0 is held until the output reports silence"
    );
    rig.rec
        .mark(epoch(0), TurnEvent::AudioOutStopped, t0 + 2_040 * MS);
    let traces = rig.exporter.take();
    assert_eq!(traces.len(), 1);
    let silence = traces[0].span("interruption").unwrap();
    assert_eq!(silence.duration(), 40 * MS);
    assert_eq!(
        traces[0]
            .root()
            .unwrap()
            .attribute("brisvox.turn.interrupted"),
        Some(&AttrValue::Bool(true))
    );
    // Turn 1 follows on its own, and nothing is exported twice.
    rig.rec.finish();
    rig.call.call_ended(None, None);
    let rest = rig.exporter.take();
    assert_eq!(rest.len(), 1);
    assert_eq!(int(&rest[0], "turn", "brisvox.turn.epoch"), Some(1));
    // The metrics saw one barge-in, and how long the bot took to go silent.
    let m = rig.obs.metrics();
    assert_eq!(m.turns(), 2);
    assert_eq!(m.turns_interrupted(), 1);
    let silence = m.stage(Stage::TimeToSilence).unwrap().snapshot();
    assert_eq!((silence.count, silence.sum_us), (1, 40_000));
}

#[tokio::test(start_paused = true)]
async fn a_barge_in_without_silence_report_is_exported_when_the_next_turn_finishes() {
    let rig = Rig::new();
    let t0 = rig.clock.now();
    rig.full_turn(0, t0);
    rig.rec
        .interrupt(epoch(0), epoch(1), t0 + 2_000 * MS, Some(t0 + 1_990 * MS));
    assert!(rig.exporter.traces().is_empty());
    rig.rec.finish();
    let traces = rig.exporter.take();
    let epochs: Vec<_> = traces
        .iter()
        .map(|t| int(t, "turn", "brisvox.turn.epoch").unwrap())
        .collect();
    assert_eq!(epochs, [0, 1], "the held turn goes out first, in order");
    assert_eq!(
        traces[0].span("interruption").unwrap().duration(),
        Duration::ZERO
    );
}

#[tokio::test(start_paused = true)]
async fn a_held_turn_is_flushed_when_the_call_ends() {
    let rig = Rig::new();
    let t0 = rig.clock.now();
    rig.full_turn(0, t0);
    rig.rec.interrupt(epoch(0), epoch(1), t0 + 2_000 * MS, None);
    // The call ends without ever finishing turn 1 or hearing of the silence.
    rig.call.call_ended(None, None);
    assert_eq!(rig.exporter.take().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn without_an_exporter_no_trace_is_built() {
    let obs = Arc::new(Observability::new());
    let clock = SimClock::shared();
    let call = obs.call(1, "a", clock.clone());
    let rec = call.recorder();
    let t0 = clock.now();
    Rig {
        obs: obs.clone(),
        exporter: MemoryExporter::new(1),
        clock,
        rec: rec.clone(),
        call,
    }
    .full_turn(0, t0);
    rec.finish();
    assert_eq!(obs.metrics().turns(), 1);
    assert_eq!(obs.metrics().traces.exported.get(), 0);
}

fn family<'a>(families: &'a [Family], name: &str) -> &'a Family {
    families.iter().find(|f| f.name == name).unwrap_or_else(|| {
        panic!(
            "no family {name}; have {:?}",
            families.iter().map(|f| &f.name).collect::<Vec<_>>()
        )
    })
}

#[tokio::test(start_paused = true)]
async fn the_exposition_parses_and_carries_the_stage_histograms() {
    let rig = Rig::new();
    let t0 = rig.clock.now();
    rig.full_turn(0, t0);
    rig.rec.finish();
    let text = rig.obs.render(&manager());
    let families = parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    for name in [
        "brisvox_stage_latency_seconds",
        "brisvox_turns_total",
        "brisvox_sessions_active",
        "brisvox_sessions_max",
        "brisvox_sessions_started_total",
        "brisvox_sessions_ended_total",
        "brisvox_session_admissions_rejected_total",
        "brisvox_provider_errors_total",
        "brisvox_engine_events_total",
        "brisvox_calls_ended_total",
        "brisvox_language_switches_total",
        "brisvox_llm_tokens_total",
        "brisvox_stt_audio_seconds_total",
        "brisvox_tts_characters_total",
        "brisvox_call_duration_seconds",
        "brisvox_traces_total",
        "brisvox_trace_batches_total",
    ] {
        family(&families, name);
    }
    // No resilience source was set: those families are absent, not empty.
    assert!(families.iter().all(|f| f.name != "brisvox_breaker_state"));
    let stages = family(&families, "brisvox_stage_latency_seconds");
    assert_eq!(stages.kind, "histogram");
    for (stage, seconds) in [
        ("end_of_turn", 0.4),
        ("stt_final", 0.15),
        ("llm_first_token", 0.3),
        ("llm_first_sentence", 0.4),
        ("tts_first_byte", 0.15),
        ("into_llm", 0.005),
        ("out_of_tts", 0.004),
        ("post_endpoint", 0.56),
        ("response", 0.96),
        ("llm_round", 0.495),
    ] {
        let count = stages
            .find("brisvox_stage_latency_seconds_count", &[("stage", stage)])
            .unwrap_or_else(|| panic!("no count for {stage}"));
        assert_eq!(count.value, 1.0, "{stage}");
        let sum = stages
            .find("brisvox_stage_latency_seconds_sum", &[("stage", stage)])
            .unwrap();
        assert!((sum.value - seconds).abs() < 1e-9, "{stage}: {}", sum.value);
    }
    // Stages that did not happen are present with a zero count.
    let silence = stages
        .find(
            "brisvox_stage_latency_seconds_count",
            &[("stage", "time_to_silence")],
        )
        .unwrap();
    assert_eq!(silence.value, 0.0);
    let turns = family(&families, "brisvox_turns_total");
    assert_eq!(
        turns
            .find("brisvox_turns_total", &[("outcome", "completed")])
            .unwrap()
            .value,
        1.0
    );
    assert_eq!(
        turns
            .find("brisvox_turns_total", &[("outcome", "interrupted")])
            .unwrap()
            .value,
        0.0
    );
}

#[tokio::test(start_paused = true)]
async fn a_late_barge_in_never_lowers_the_completed_counter() {
    let rig = Rig::new();
    let t0 = rig.clock.now();
    let m = rig.obs.metrics();
    let completed = || {
        let text = rig.obs.render(&manager());
        let families = parse(&text).unwrap();
        let turns = family(&families, "brisvox_turns_total");
        let get = |outcome| {
            turns
                .find("brisvox_turns_total", &[("outcome", outcome)])
                .unwrap()
                .value
        };
        (get("completed"), get("interrupted"))
    };
    rig.full_turn(0, t0);
    // An early mark of turn 1 finishes turn 0 as completed.
    rig.rec
        .mark(epoch(1), TurnEvent::UserSpeechStart, t0 + 2_000 * MS);
    assert_eq!(m.turns(), 1);
    assert_eq!(completed(), (1.0, 0.0));
    // The barge-in for turn 0 arrives after its summary and is patched in.
    rig.rec
        .interrupt(epoch(0), epoch(1), t0 + 2_010 * MS, Some(t0 + 2_000 * MS));
    assert_eq!(m.turns_interrupted(), 1, "the late barge-in is counted");
    let (done, interrupted) = completed();
    assert_eq!(done, 1.0, "a counter must never go down");
    assert_eq!(interrupted, 1.0);
    // A second patch of the same turn is not counted twice.
    rig.rec
        .mark(epoch(0), TurnEvent::AudioOutStopped, t0 + 2_050 * MS);
    assert_eq!(m.turns_interrupted(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_guard_ends_the_call_when_dropped_mid_call() {
    let rig = Rig::new();
    let t0 = rig.clock.now();
    rig.full_turn(0, t0);
    // Turn 0 was interrupted and waits for its silence report; turn 1 is still open.
    rig.rec
        .interrupt(epoch(0), epoch(1), t0 + 2_000 * MS, Some(t0 + 1_990 * MS));
    let summarizer =
        crate::summary::CallSummarizer::new(rig.clock.clone(), 7, "test-agent", no_models(), None);
    let guard = CallGuard::new(
        Some(rig.call.clone()),
        summarizer,
        rig.rec.clone(),
        |_: &()| None::<brisvox_engine::session::CallStats>,
    );
    assert_eq!(rig.obs.metrics().calls_ended(), 0);
    // The call future is cancelled: its locals, the guard among them, drop.
    drop(guard);
    assert_eq!(rig.obs.metrics().calls_ended(), 1);
    assert_eq!(rig.obs.metrics().turns(), 2, "the open turn was finished");
    assert_eq!(rig.exporter.take().len(), 2, "the held trace was exported");
    // The normal end after it counts nothing more.
    rig.call.call_ended(None, None);
    assert_eq!(rig.obs.metrics().calls_ended(), 1);
}

/// A fixed resilience layer: a primary STT whose breaker is open, a healthy secondary.
pub(super) struct FakeResilience;

impl ResilienceSource for FakeResilience {
    fn resilience_snapshot(&self) -> ResilienceSnapshot {
        use brisvox_providers::resilience::{BreakerState, BreakerStats};
        ResilienceSnapshot {
            breakers: vec![
                BreakerSnapshot {
                    stage: ProviderStage::Stt,
                    role: "primary",
                    provider: "deepgram",
                    state: BreakerState::Open,
                    stats: BreakerStats {
                        admitted: 9,
                        refused: 4,
                        failures: 5,
                        opened: 1,
                        probes: 0,
                        closed: 0,
                    },
                },
                BreakerSnapshot {
                    stage: ProviderStage::Stt,
                    role: "secondary",
                    provider: "assemblyai",
                    state: BreakerState::Closed,
                    stats: BreakerStats::default(),
                },
            ],
            budget: BudgetSnapshot {
                available: 41,
                granted: 9,
                denied: 2,
            },
        }
    }
}

#[tokio::test(start_paused = true)]
async fn breaker_and_budget_telemetry_is_exported() {
    let obs = Observability::new();
    assert!(obs.set_resilience_source(Arc::new(FakeResilience)));
    assert!(
        !obs.set_resilience_source(Arc::new(FakeResilience)),
        "set once"
    );
    let text = obs.render(&manager());
    let families = parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    let state = family(&families, "brisvox_breaker_state");
    let open = state
        .find(
            "brisvox_breaker_state",
            &[
                ("stage", "stt"),
                ("role", "primary"),
                ("provider", "deepgram"),
            ],
        )
        .unwrap();
    assert_eq!(open.value, 2.0, "2 is open");
    let closed = state
        .find("brisvox_breaker_state", &[("role", "secondary")])
        .unwrap();
    assert_eq!(closed.value, 0.0);
    let events = family(&families, "brisvox_breaker_events_total");
    assert_eq!(
        events
            .find(
                "brisvox_breaker_events_total",
                &[("provider", "deepgram"), ("event", "refused")]
            )
            .unwrap()
            .value,
        4.0
    );
    assert_eq!(
        family(&families, "brisvox_retry_budget_available").samples[0].value,
        41.0
    );
    let budget = family(&families, "brisvox_retry_budget_total");
    assert_eq!(
        budget
            .find("brisvox_retry_budget_total", &[("outcome", "refused")])
            .unwrap()
            .value,
        2.0
    );
    // The per-call counters exist for every stage, even before any failover.
    let resilience = family(&families, "brisvox_resilience_events_total");
    assert!(
        resilience
            .find(
                "brisvox_resilience_events_total",
                &[("stage", "tts"), ("event", "failovers")]
            )
            .is_some()
    );
}

#[tokio::test(start_paused = true)]
async fn the_health_listener_serves_metrics_only_when_asked() {
    async fn get(manager: &SessionManager, request: &str) -> String {
        let (mut client, server) = duplex(64 * 1024);
        let serve = crate::health::serve_connection(server, manager);
        let talk = async {
            client.write_all(request.as_bytes()).await.unwrap();
            client.shutdown().await.unwrap();
            let mut out = String::new();
            client.read_to_string(&mut out).await.unwrap();
            out
        };
        let ((), out) = tokio::join!(serve, talk);
        out
    }
    let request = "GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n";
    let mgr = manager();
    // Nothing attached: not found.
    assert!(get(&mgr, request).await.starts_with("HTTP/1.1 404"));
    // Attached, endpoint on.
    let obs = Arc::new(Observability::new());
    assert!(mgr.attach_observability(obs.clone()));
    let out = get(&mgr, request).await;
    assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{out}");
    assert!(
        out.contains("Content-Type: text/plain; version=0.0.4; charset=utf-8\r\n"),
        "{out}"
    );
    let body = out.split_once("\r\n\r\n").unwrap().1;
    parse(body).unwrap_or_else(|e| panic!("{e}\n{body}"));
    assert!(body.contains("brisvox_sessions_max 4\n"), "{body}");
    // HEAD answers without the body, other methods are refused.
    let head = get(&mgr, "HEAD /metrics HTTP/1.1\r\n\r\n").await;
    assert!(
        head.starts_with("HTTP/1.1 200") && head.ends_with("\r\n\r\n"),
        "{head}"
    );
    assert!(
        get(&mgr, "POST /metrics HTTP/1.1\r\n\r\n")
            .await
            .starts_with("HTTP/1.1 405")
    );
    // The endpoint switched off hides it, and /healthz is unaffected.
    let hidden = manager();
    hidden.attach_observability(Arc::new(Observability::new().with_metrics_endpoint(false)));
    assert!(get(&hidden, request).await.starts_with("HTTP/1.1 404"));
    assert!(
        get(&hidden, "GET /healthz HTTP/1.1\r\n\r\n")
            .await
            .starts_with("HTTP/1.1 200")
    );
}

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[test]
fn settings_default_to_metrics_on_and_no_traces() {
    let s = ObservabilitySettings::from_env(&env(&[])).unwrap();
    assert!(s.metrics);
    assert!(matches!(s.traces, TraceSink::Off));
    let s = ObservabilitySettings::from_env(&env(&[(ENV_METRICS, "off"), (ENV_TRACES, "OFF")]))
        .unwrap();
    assert!(!s.metrics);
    assert!(matches!(s.traces, TraceSink::Off));
}

#[test]
fn bad_settings_name_the_variable() {
    for (name, value) in [(ENV_METRICS, "maybe"), (ENV_TRACES, "jaeger")] {
        let err = ObservabilitySettings::from_env(&env(&[(name, value)])).unwrap_err();
        assert!(err.to_string().contains(name), "{err}");
    }
}

#[cfg(not(feature = "otlp"))]
#[test]
fn otlp_traces_need_the_feature() {
    let err = ObservabilitySettings::from_env(&env(&[(ENV_TRACES, "otlp")])).unwrap_err();
    assert!(err.to_string().contains("`otlp` feature"), "{err}");
}

#[cfg(feature = "otlp")]
#[test]
fn otlp_settings_read_the_endpoint_and_limits() {
    let s = ObservabilitySettings::from_env(&env(&[
        (ENV_TRACES, "otlp"),
        ("BRISVOX_OTLP_ENDPOINT", "http://alloy:4318"),
        ("BRISVOX_OTLP_BATCH", "8"),
        ("BRISVOX_OTLP_FLUSH", "500ms"),
    ]))
    .unwrap();
    let TraceSink::Otlp(c) = s.traces else {
        panic!("not otlp")
    };
    assert_eq!((c.host.as_str(), c.port, c.batch), ("alloy", 4318, 8));
    assert_eq!(c.flush, Duration::from_millis(500));
    for (name, value) in [
        ("BRISVOX_OTLP_ENDPOINT", "https://alloy:4318"),
        ("BRISVOX_OTLP_BATCH", "0"),
        ("BRISVOX_OTLP_TIMEOUT", "0s"),
    ] {
        let err = ObservabilitySettings::from_env(&env(&[(ENV_TRACES, "otlp"), (name, value)]))
            .unwrap_err();
        assert!(err.to_string().contains(name), "{err}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_turn_costs_microseconds() {
    // What observing adds to a turn: the histograms, one trace built and one export, on the task
    // that finishes the turn. Measured against the same marks on a recorder nobody observes. The
    // bound is loose on purpose (a debug build on a shared machine); it still catches an O(n)
    // or a lock held across work. In release builds the difference is a few microseconds.
    const TURNS: u32 = 2_000;
    let rig = Rig::new();
    let t0 = rig.clock.now();
    let plain = TimelineRecorder::new();
    let bare = Rig {
        rec: plain.clone(),
        ..Rig::new()
    };
    // Real time on purpose: this measures CPU cost, not simulated time.
    #[allow(clippy::disallowed_methods)]
    let started = std::time::Instant::now();
    for e in 0..TURNS {
        bare.full_turn(e, t0 + Duration::from_secs(u64::from(e)));
    }
    plain.finish();
    #[allow(clippy::disallowed_methods)]
    let baseline = started.elapsed();
    #[allow(clippy::disallowed_methods)]
    let started = std::time::Instant::now();
    for e in 0..TURNS {
        rig.full_turn(e, t0 + Duration::from_secs(u64::from(e)));
    }
    rig.rec.finish();
    #[allow(clippy::disallowed_methods)]
    let observed = started.elapsed();
    let added = observed.saturating_sub(baseline) / TURNS;
    // A scrape renders everything once; it runs on the health listener's task.
    let manager = manager();
    #[allow(clippy::disallowed_methods)]
    let started = std::time::Instant::now();
    for _ in 0..100 {
        assert!(!rig.obs.render(&manager).is_empty());
    }
    #[allow(clippy::disallowed_methods)]
    let render = started.elapsed() / 100;
    eprintln!(
        "observing adds {added:?} per turn ({:?} -> {:?} per turn with 11 marks); a scrape renders in {render:?}",
        baseline / TURNS,
        observed / TURNS
    );
    assert!(
        added < Duration::from_millis(1),
        "observing a turn added {added:?}"
    );
    assert!(
        render < Duration::from_millis(5),
        "a scrape took {render:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Redaction in the real log writer

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

const LIVE_KEY: &str = "sk-live-9f8e7d6c5b4a3210";
const SECOND_KEY: &str = "dg_0123456789abcdef0123";

/// Everything a careless log line can do with a credential.
fn leak_everywhere() {
    use brisvox_core::Secret;
    let wrapped = Secret::new(LIVE_KEY.to_owned());
    tracing::info!(config = ?wrapped, "typed secret is safe by itself");
    tracing::warn!("provider said: 401 for key {LIVE_KEY}");
    tracing::warn!(
        url = "https://api.example.com/v1/listen?model=nova&key=AIzaSyLeakedQueryKey1",
        "request failed"
    );
    tracing::error!(
        headers = "Authorization: Bearer abcdefghijklmnop0123",
        "dump"
    );
    tracing::error!(
        body = r#"{"api_key":"abcdefgh12345678","user":"bob"}"#,
        "echo"
    );
    tracing::info!("dsn postgres://app:hunter2hunter2@db.internal:5432/calls");
    tracing::info!(token = SECOND_KEY, "field named token");
}

fn assert_clean(output: &str) {
    for leaked in [
        LIVE_KEY,
        SECOND_KEY,
        "AIzaSyLeakedQueryKey1",
        "abcdefghijklmnop0123",
        "abcdefgh12345678",
        "hunter2hunter2",
    ] {
        assert!(
            !output.contains(leaked),
            "{leaked} leaked into the log:\n{output}"
        );
    }
    assert!(output.contains("[REDACTED]"), "{output}");
    // The rest of each line survives, so the log stays useful.
    assert!(output.contains("request failed"), "{output}");
    assert!(output.contains("db.internal"), "{output}");
    assert!(output.contains("nova"), "{output}");
}

fn logged(format: LogFormat) -> String {
    let capture = Capture::default();
    let sink = capture.clone();
    let redactor = Arc::new(Redactor::from_vars([("OPENAI_API_KEY", LIVE_KEY)]));
    let subscriber = build_subscriber(
        format,
        build_filter(Some("trace")).unwrap(),
        redactor,
        move || sink.clone(),
    );
    tracing::subscriber::with_default(subscriber, leak_everywhere);
    capture.text()
}

#[test]
fn secrets_never_appear_in_text_logs() {
    assert_clean(&logged(LogFormat::Text));
}

#[test]
fn secrets_never_appear_in_json_logs() {
    let out = logged(LogFormat::Json);
    assert_clean(&out);
    // Still one JSON object per line.
    for line in out.lines() {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|e| panic!("redaction broke the JSON ({e}): {line}"));
    }
}

// ---------------------------------------------------------------------------------------------
// The OTLP exporter against a scripted collector (feature `otlp`)

#[cfg(feature = "otlp")]
mod otlp_collector {
    use std::net::SocketAddr;

    use brisvox_core::SystemClock;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::observability::otlp::{OtlpConfig, OtlpExporter, encode_batch};
    use crate::observability::registry::TraceCounters;

    /// A collector that answers every request with `status` after reading it, or never answers
    /// (`status` 0), and hands the requests to the test.
    async fn collector(status: u16) -> (SocketAddr, mpsc::UnboundedReceiver<(String, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let (head, body) = loop {
                        let n = stream.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        data.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&data).into_owned();
                        let Some((head, body)) = text.split_once("\r\n\r\n") else {
                            continue;
                        };
                        let length = head
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(0);
                        if body.len() >= length {
                            break (head.to_owned(), body.to_owned());
                        }
                    };
                    let _ = tx.send((head, body));
                    if status == 0 {
                        // Never answers: hold the socket open.
                        std::future::pending::<()>().await;
                    }
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(reply.as_bytes()).await;
                });
            }
        });
        (addr, rx)
    }

    fn config(addr: SocketAddr) -> OtlpConfig {
        OtlpConfig {
            host: addr.ip().to_string(),
            port: addr.port(),
            batch: 2,
            flush: Duration::from_millis(60),
            timeout: Duration::from_secs(2),
            ..OtlpConfig::default()
        }
    }

    /// Two traces made by the real builder.
    fn two_traces() -> Vec<TurnTrace> {
        let rig = Rig::new();
        let t0 = rig.clock.now();
        rig.full_turn(0, t0);
        rig.full_turn(1, t0 + 5_000 * MS);
        rig.rec.finish();
        rig.exporter.take()
    }

    async fn settle(what: &str, done: impl Fn() -> bool) {
        for _ in 0..200 {
            if done() {
                return;
            }
            SystemClock::shared().sleep(Duration::from_millis(20)).await;
        }
        panic!("{what} did not happen");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn traces_reach_the_collector_as_otlp_json() {
        let (addr, mut requests) = collector(200).await;
        let counters = Arc::new(TraceCounters::default());
        let cancel = CancellationToken::new();
        let exporter = OtlpExporter::spawn(
            &config(addr),
            SystemClock::shared(),
            cancel.clone(),
            counters.clone(),
        )
        .unwrap();
        let traces = two_traces();
        for trace in traces.clone() {
            exporter.export(trace);
        }
        let (head, body) = tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await
            .expect("the collector got nothing")
            .unwrap();
        assert!(head.starts_with("POST /v1/traces HTTP/1.1"), "{head}");
        assert!(head.contains("Content-Type: application/json"), "{head}");
        let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
        let resource = &doc["resourceSpans"][0];
        assert_eq!(
            resource["resource"]["attributes"][0],
            serde_json::json!({"key": "service.name", "value": {"stringValue": "brisvox-server"}})
        );
        let spans = resource["scopeSpans"][0]["spans"].as_array().unwrap();
        let expected: usize = traces.iter().map(|t| t.spans.len()).sum();
        assert_eq!(
            spans.len(),
            expected,
            "both traces, every span, in one batch"
        );
        let root = spans.iter().find(|s| s["name"] == "turn").unwrap();
        assert_eq!(root["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(root["spanId"].as_str().unwrap().len(), 16);
        assert!(root.get("parentSpanId").is_none());
        let llm = spans.iter().find(|s| s["name"] == "llm").unwrap();
        assert_eq!(llm["parentSpanId"], root["spanId"]);
        // OTLP/JSON writes 64-bit values as strings.
        assert!(llm["startTimeUnixNano"].is_string());
        assert!(
            llm["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["key"] == "brisvox.llm.round" && a["value"]["intValue"] == "0")
        );
        settle("the batch counted as sent", || {
            counters.batches_sent.get() == 1
        })
        .await;
        assert_eq!(counters.dropped.get(), 0);
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_partial_batch_is_sent_after_the_flush_interval() {
        let (addr, mut requests) = collector(200).await;
        let counters = Arc::new(TraceCounters::default());
        let cancel = CancellationToken::new();
        let exporter = OtlpExporter::spawn(
            &config(addr),
            SystemClock::shared(),
            cancel.clone(),
            counters,
        )
        .unwrap();
        exporter.export(two_traces().remove(0));
        let (_, body) = tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await
            .expect("no flush")
            .unwrap();
        assert!(body.contains("\"turn\""), "{body}");
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failing_collector_drops_the_batch_and_counts_it() {
        let (addr, mut requests) = collector(503).await;
        let counters = Arc::new(TraceCounters::default());
        let cancel = CancellationToken::new();
        let exporter = OtlpExporter::spawn(
            &config(addr),
            SystemClock::shared(),
            cancel.clone(),
            counters.clone(),
        )
        .unwrap();
        for trace in two_traces() {
            exporter.export(trace);
        }
        requests.recv().await.unwrap();
        settle("the failed batch counted", || {
            counters.batches_failed.get() == 1
        })
        .await;
        assert_eq!(
            counters.dropped.get(),
            2,
            "the two traces of the failed batch"
        );
        assert_eq!(counters.batches_sent.get(), 0);
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_full_queue_drops_the_newest_trace_without_blocking() {
        // The collector never answers, so the task is stuck in its first request.
        let (addr, mut requests) = collector(0).await;
        let counters = Arc::new(TraceCounters::default());
        let cancel = CancellationToken::new();
        let mut cfg = config(addr);
        cfg.queue = 1;
        cfg.batch = 1;
        cfg.timeout = Duration::from_secs(30);
        let exporter = OtlpExporter::spawn(
            &cfg,
            SystemClock::shared(),
            cancel.clone(),
            counters.clone(),
        )
        .unwrap();
        let one = two_traces().remove(0);
        exporter.export(one.clone());
        requests.recv().await.unwrap(); // the task now waits for the answer
        exporter.export(one.clone()); // fills the queue of one
        exporter.export(one.clone());
        exporter.export(one);
        assert_eq!(counters.dropped.get(), 2, "newest dropped, nobody blocked");
        cancel.cancel();
    }

    #[test]
    fn the_encoder_handles_an_empty_batch() {
        let doc: serde_json::Value = serde_json::from_str(&encode_batch("svc", &[])).unwrap();
        assert_eq!(
            doc["resourceSpans"][0]["scopeSpans"][0]["spans"],
            serde_json::json!([])
        );
    }
}

#[tokio::test(start_paused = true)]
async fn the_smart_turn_executor_is_exported_from_its_pool_counters() {
    // The registry renders what the executor's pool counted.
    let pool = brisvox_inference::PoolMetrics {
        queue_depth: 3,
        rejected: 5,
        timed_out: 2,
        ..brisvox_inference::PoolMetrics::default()
    };
    let text = Metrics::new().render(&manager(), None, Some(&pool));
    let families = parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    for (name, value) in [
        ("brisvox_smart_turn_queue_depth", 3.0),
        ("brisvox_smart_turn_rejected_total", 5.0),
        ("brisvox_smart_turn_timed_out_total", 2.0),
    ] {
        assert_eq!(family(&families, name).samples[0].value, value, "{name}");
    }
}

#[tokio::test(start_paused = true)]
async fn the_smart_turn_series_exist_only_with_an_executor() {
    let obs = Observability::new();
    let none = parse(&obs.render(&manager())).unwrap();
    assert!(
        none.iter()
            .all(|f| !f.name.starts_with("brisvox_smart_turn")),
        "no executor (BRISVOX_SMART_TURN_THREADS=0): no series"
    );
    let pool = brisvox_inference::InferencePool::new(
        &brisvox_inference::PoolConfig {
            threads: 1,
            queue_capacity: 4,
            name: "brisvox-turn".to_owned(),
        },
        SimClock::shared(),
    )
    .unwrap();
    assert!(obs.set_smart_turn_pool(pool.handle()));
    assert!(!obs.set_smart_turn_pool(pool.handle()), "set once");
    let text = obs.render(&manager());
    let families = parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    for name in [
        "brisvox_smart_turn_queue_depth",
        "brisvox_smart_turn_rejected_total",
        "brisvox_smart_turn_timed_out_total",
    ] {
        assert_eq!(family(&families, name).samples[0].value, 0.0, "{name}");
    }
    pool.shutdown();
}

fn no_models() -> crate::config::ProviderModels {
    crate::config::ProviderModels {
        stt: None,
        llm: None,
        tts: None,
    }
}

pub(super) fn models(model: &str) -> crate::config::ProviderModels {
    use crate::config::ProviderModel;
    let named = |provider: &'static str, model: &str| {
        Some(ProviderModel {
            provider,
            model: model.to_owned(),
        })
    };
    crate::config::ProviderModels {
        stt: named("deepgram", "nova-3"),
        llm: named("openai", model),
        tts: named("cartesia", "sonic"),
    }
}

/// The end-of-call statistics of a call that used `tokens` (input, cached, output), `audio_ms`
/// of STT audio and `chars` TTS characters.
fn stats_with(tokens: [u64; 3], audio_ms: u64, chars: u64) -> brisvox_engine::session::CallStats {
    use brisvox_engine::session::{CallStats, StageCounters};
    use brisvox_engine::{LlmStatsSnapshot, SttStatsSnapshot, TtsStatsSnapshot};
    CallStats {
        pipeline: Default::default(),
        pump: Default::default(),
        stages: StageCounters {
            audio_in: Default::default(),
            vad: Default::default(),
            stt: SttStatsSnapshot {
                audio_ms_sent: audio_ms,
                ..Default::default()
            },
            driver: Default::default(),
            llm: LlmStatsSnapshot {
                input_tokens: tokens[0],
                cached_input_tokens: tokens[1],
                output_tokens: tokens[2],
                ..Default::default()
            },
            speculation: Default::default(),
            requests: Default::default(),
            segmenter: Default::default(),
            tts: TtsStatsSnapshot {
                chars_sent: chars,
                ..Default::default()
            },
            out: Default::default(),
            book: Default::default(),
            truncations: Default::default(),
            unnoted_clear_cuts: 0,
            language: Default::default(),
        },
    }
}

fn observe(
    obs: &Observability,
    names: &crate::config::ProviderModels,
    stats: &brisvox_engine::session::CallStats,
    duration: Duration,
) {
    let summary = crate::summary::build_summary(
        duration,
        &crate::summary::TurnTotals::default(),
        names,
        &stats.stages.llm,
        &stats.stages.stt,
        &stats.stages.tts,
    );
    obs.metrics().observe_call(Some(stats), Some(&summary));
}

/// What a finished call adds to `/metrics`: tokens, audio and characters under the agent's
/// provider and model labels, and the call's duration in the histogram.
#[tokio::test(start_paused = true)]
async fn call_usage_and_duration_reach_the_exposition() {
    let obs = Arc::new(Observability::new());
    let names = models("gpt-test");
    obs.metrics().register_usage(&names);
    // Registered at zero from startup.
    let text = obs.render(&manager());
    let families = parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    let tts = family(&families, "brisvox_tts_characters_total");
    assert_eq!(
        tts.find(
            "brisvox_tts_characters_total",
            &[("provider", "cartesia"), ("model", "sonic")]
        )
        .unwrap()
        .value,
        0.0
    );

    observe(
        &obs,
        &names,
        &stats_with([1_000, 600, 50], 12_345, 300),
        Duration::from_millis(4_000),
    );
    observe(
        &obs,
        &names,
        &stats_with([200, 0, 25], 2_000, 40),
        Duration::from_millis(95_000),
    );
    // A call without statistics still counts as ended but adds no usage.
    obs.metrics().observe_call(None, None);

    let m = obs.metrics();
    assert_eq!(m.llm_tokens("openai", "gpt-test"), [1_200, 600, 75]);
    assert_eq!(m.stt_audio_ms("deepgram", "nova-3"), 14_345);
    assert_eq!(m.tts_characters("cartesia", "sonic"), 340);
    assert_eq!(m.call_durations(), 2);
    assert_eq!(m.calls_ended(), 3);

    let text = obs.render(&manager());
    let families = parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    let tokens = family(&families, "brisvox_llm_tokens_total");
    assert_eq!(tokens.kind, "counter");
    for (kind, want) in [
        ("input", 1_200.0),
        ("cached_input", 600.0),
        ("output", 75.0),
    ] {
        let sample = tokens
            .find(
                "brisvox_llm_tokens_total",
                &[
                    ("provider", "openai"),
                    ("model", "gpt-test"),
                    ("kind", kind),
                ],
            )
            .unwrap_or_else(|| panic!("no {kind}:\n{text}"));
        assert_eq!(sample.value, want, "{kind}");
    }
    let audio = family(&families, "brisvox_stt_audio_seconds_total");
    let sample = audio
        .find(
            "brisvox_stt_audio_seconds_total",
            &[("provider", "deepgram"), ("model", "nova-3")],
        )
        .unwrap();
    assert!((sample.value - 14.345).abs() < 1e-9, "{}", sample.value);
    let chars = family(&families, "brisvox_tts_characters_total");
    assert_eq!(
        chars
            .find(
                "brisvox_tts_characters_total",
                &[("provider", "cartesia"), ("model", "sonic")]
            )
            .unwrap()
            .value,
        340.0
    );
    let duration = family(&families, "brisvox_call_duration_seconds");
    assert_eq!(duration.kind, "histogram");
    let at = |sample: &str, le: Option<&str>| {
        let labels: Vec<(&str, &str)> = le.map(|le| ("le", le)).into_iter().collect();
        duration.find(sample, &labels).unwrap().value
    };
    assert_eq!(at("brisvox_call_duration_seconds_count", None), 2.0);
    assert!((at("brisvox_call_duration_seconds_sum", None) - 99.0).abs() < 1e-9);
    assert_eq!(at("brisvox_call_duration_seconds_bucket", Some("5")), 1.0);
    assert_eq!(at("brisvox_call_duration_seconds_bucket", Some("60")), 1.0);
    assert_eq!(at("brisvox_call_duration_seconds_bucket", Some("120")), 2.0);
    assert_eq!(
        at("brisvox_call_duration_seconds_bucket", Some("+Inf")),
        2.0
    );
}

/// The label values come from the agents; even so the table has a ceiling, past which a new
/// series is counted under `other`.
#[tokio::test(start_paused = true)]
async fn the_usage_series_are_bounded() {
    let obs = Arc::new(Observability::new());
    for n in 0..40 {
        obs.metrics().register_usage(&models(&format!("model-{n}")));
    }
    let text = obs.render(&manager());
    let llm_series = text
        .lines()
        .filter(|l| l.starts_with("brisvox_llm_tokens_total{") && l.contains("kind=\"input\""))
        .count();
    assert_eq!(llm_series, 17, "16 models and `other`:\n{text}");
    assert!(text.contains("brisvox_llm_tokens_total{provider=\"other\",model=\"other\""));
    // A known series keeps counting.
    observe(
        &obs,
        &models("model-3"),
        &stats_with([5, 0, 1], 0, 0),
        Duration::from_secs(1),
    );
    assert_eq!(obs.metrics().llm_tokens("openai", "model-3"), [5, 0, 1]);
}

/// One `call summary` line at info on `brisvox::server`, with the numbers as fields.
#[test]
fn the_call_summary_is_one_structured_info_line() {
    use brisvox_agent_link::protocol::StageLatency;

    let mut summary = crate::summary::build_summary(
        Duration::from_millis(93_412),
        &crate::summary::TurnTotals::default(),
        &models("gpt-test"),
        &stats_with([1_200, 800, 90], 0, 0).stages.llm,
        &stats_with([0; 3], 93_125, 0).stages.stt,
        &stats_with([0; 3], 0, 812).stages.tts,
    );
    summary.turns = 7;
    summary.interrupted_turns = 1;
    summary.latency.response = Some(StageLatency {
        avg_ms: 512.5,
        max_ms: 800.0,
    });
    let capture = Capture::default();
    let sink = capture.clone();
    let subscriber = build_subscriber(
        LogFormat::Json,
        build_filter(Some("info")).unwrap(),
        Arc::new(Redactor::from_vars([("OPENAI_API_KEY", LIVE_KEY)])),
        move || sink.clone(),
    );
    tracing::subscriber::with_default(subscriber, || {
        crate::summary::log_summary(17, "support", &summary);
    });
    let out = capture.text();
    let lines: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1, "{out}");
    let line = &lines[0];
    assert_eq!(line["level"], "INFO");
    assert_eq!(line["target"], "brisvox::server");
    let fields = &line["fields"];
    assert_eq!(fields["message"], "call summary");
    assert_eq!(fields["session"], 17);
    assert_eq!(fields["agent"], "support");
    assert_eq!(fields["duration_ms"], 93_412);
    assert_eq!(fields["turns"], 7);
    assert_eq!(fields["interrupted_turns"], 1);
    assert_eq!(fields["response_avg_ms"], 512.5);
    assert_eq!(fields["llm_provider"], "openai");
    assert_eq!(fields["llm_model"], "gpt-test");
    assert_eq!(fields["llm_input_tokens"], 1_200);
    assert_eq!(fields["llm_cached_input_tokens"], 800);
    assert_eq!(fields["llm_output_tokens"], 90);
    assert_eq!(fields["stt_audio_seconds"], 93.125);
    assert_eq!(fields["tts_characters"], 812);
    // A stage without samples has no field value, not a made-up zero.
    assert!(fields.get("llm_ttft_avg_ms").is_none(), "{line}");
}
