//! Early Smart Turn analysis on the fixtures with the real models: the Silero VAD (tract)
//! and Smart Turn (ONNX Runtime). Two measurements, printed as tables (`-- --nocapture`) and
//! recorded in `docs/LATENCY.md`:
//!
//! 1. AGREEMENT (offline, audio time): for each fall the VAD hints, the verdict on the audio at
//!    the hint versus the verdict on the audio at the confirmed stop of the same fall, over a
//!    sweep of the VAD's `min_silence` and the hint delay.
//! 2. LATENCY (real time, the production stages): `AudioIn` with the Silero worker -> `TurnDriver`
//!    with Smart Turn on a shared inference pool, fed in real time, with early analysis off and
//!    on: mean VAD-stop-to-verdict latency and the extra analyses.
//!
//! Needs `models/smart-turn-v3.2-cpu.onnx` (`scripts/models/fetch_models.py`, or the directory in
//! `BRISVOX_MODELS_DIR`).
#![cfg(all(feature = "silero-tract", feature = "smart-turn-ort"))]
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#![allow(clippy::disallowed_methods)] // real time

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_core::frame::{AudioFrame, Frame, SampleRate};
use brisvox_core::{
    Clock, Direction, EpochPolicy, Links, Pipeline, Processor, ProcessorContext, SharedClock,
    SystemClock, TimelineRecorder, TurnEpoch,
};
use brisvox_engine::{
    AudioIn, EarlyAnalysisConfig, EngineConfig, Offload, TurnDriver, TurnDriverConfig,
    TurnDriverStatsSnapshot, VadSetup, VadWorkerConfig,
};
use brisvox_inference::silero_tract::SileroTractModel;
use brisvox_inference::smart_turn::ort_backend::SmartTurnOrt;
use brisvox_inference::smart_turn::{MODEL_FILE, SmartTurn, THRESHOLD, TurnModel};
use brisvox_inference::{InferencePool, PoolConfig, VadEvent, VadParams, VadSignal, VadStream};
use brisvox_sim::fixture::{Fixture, fixtures_dir};
use common::{MS, samples};
use tokio_util::sync::CancellationToken;

const FIXTURES: [&str; 3] = ["en_two_utterances", "tr_two_utterances", "en_tr_mixed"];
const FRAME: Duration = Duration::from_millis(20);

fn model_path() -> PathBuf {
    let dir = std::env::var_os("BRISVOX_MODELS_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models"),
        PathBuf::from,
    );
    dir.join(MODEL_FILE)
}

fn smart_turn_model() -> Arc<SmartTurnOrt> {
    let path = model_path();
    SmartTurnOrt::load(&path).unwrap_or_else(|e| {
        panic!(
            "{e}: run scripts/models/fetch_models.py ({})",
            path.display()
        )
    })
}

// ---------------------------------------------------------------------------------------------
// 1. Agreement

#[derive(Debug, Default, Clone, Copy)]
struct Row {
    falls: u32,
    stops: u32,
    /// Stops whose own fall's hint was available.
    pairs: u32,
    /// Pairs whose verdicts (complete or not) are the same.
    agree: u32,
    max_dp: f32,
    sum_headstart_ms: u64,
    /// Hints that did not end in a stop of their own (superseded or never confirmed).
    wasted: u32,
}

/// Runs one fixture through the VAD and the ring in audio time (20 ms chunks), analysing at
/// every hint and at every stop.
fn agreement(
    fixture: &Fixture,
    vad: &Arc<SileroTractModel>,
    turn: &Arc<SmartTurnOrt>,
    min_silence: Duration,
    delay: Duration,
    row: &mut Row,
) {
    let clock = SystemClock;
    let origin = clock.now();
    let params = VadParams {
        min_silence,
        ..VadParams::default()
    };
    let mut stream = VadStream::new(vad.stream().unwrap(), params).with_falling_hint(Some(delay));
    let model: Arc<dyn TurnModel> = turn.clone();
    let mut ring = SmartTurn::new(model);
    let pcm = samples(&fixture.pcm);
    // The pending hint: (its time, the probability on the audio then, the audio time of its
    // detection).
    let mut pending: Option<(tokio::time::Instant, f32, Duration)> = None;
    let mut signals = Vec::new();
    for (i, chunk) in pcm.chunks(320).enumerate() {
        let now = FRAME * u32::try_from(i + 1).unwrap();
        ring.push(chunk);
        signals.clear();
        stream
            .push_signals(
                chunk,
                origin + FRAME * u32::try_from(i).unwrap(),
                &mut signals,
            )
            .unwrap();
        for signal in &signals {
            match *signal {
                VadSignal::Falling { at } => {
                    row.falls += 1;
                    if pending.is_some() {
                        row.wasted += 1;
                    }
                    pending = Some((at, ring.analyze_blocking().unwrap(), now));
                }
                VadSignal::Event(VadEvent::Start { .. }) => {
                    if pending.take().is_some() {
                        row.wasted += 1;
                    }
                }
                VadSignal::Event(VadEvent::Stop { at }) => {
                    row.stops += 1;
                    let at_stop = ring.analyze_blocking().unwrap();
                    match pending.take() {
                        Some((hint_at, early, detected)) if hint_at == at => {
                            row.pairs += 1;
                            row.agree += u32::from((early > THRESHOLD) == (at_stop > THRESHOLD));
                            row.max_dp = row.max_dp.max((early - at_stop).abs());
                            row.sum_headstart_ms +=
                                u64::try_from((now - detected).as_millis()).unwrap();
                        }
                        Some(_) => row.wasted += 1,
                        None => {}
                    }
                }
            }
        }
    }
    if pending.is_some() {
        row.wasted += 1;
    }
}

#[test]
fn the_early_verdict_agrees_with_the_one_at_the_stop_on_the_fixtures() {
    let vad = SileroTractModel::bundled().unwrap();
    let turn = smart_turn_model();
    turn.warm(1).unwrap();
    println!(
        "min_silence delay | falls stops pairs agree  max|dp| mean_headstart  wasted (3 fixtures)"
    );
    let mut default_row = Row::default();
    // Debug builds compute the features ~100x slower: only the settings that matter.
    let sweep: &[u64] = if cfg!(debug_assertions) {
        &[100]
    } else {
        &[100, 200, 300, 500]
    };
    for &min_silence in sweep {
        for delay in [0_u64, 32, 64] {
            if delay >= min_silence {
                continue;
            }
            let mut row = Row::default();
            for name in FIXTURES {
                let fixture = Fixture::load(&fixtures_dir(), name).unwrap();
                agreement(
                    &fixture,
                    &vad,
                    &turn,
                    Duration::from_millis(min_silence),
                    Duration::from_millis(delay),
                    &mut row,
                );
            }
            let headstart = if row.pairs == 0 {
                0
            } else {
                row.sum_headstart_ms / u64::from(row.pairs)
            };
            println!(
                "{min_silence:>10} {delay:>5} | {:>5} {:>5} {:>5} {:>5}  {:>7.3}  {headstart:>7} ms  {:>6}",
                row.falls, row.stops, row.pairs, row.agree, row.max_dp, row.wasted
            );
            if min_silence == 100 && delay == 64 {
                default_row = row;
            }
            assert!(row.pairs <= row.stops && row.agree <= row.pairs, "{row:?}");
        }
    }
    // The six real stops of the fixtures (two utterances each) all have a hint of their own at
    // the default settings (min_silence 100 ms, hint after 64 ms).
    assert!(default_row.stops >= 6, "{default_row:?}");
    assert_eq!(default_row.pairs, default_row.stops, "{default_row:?}");
    // The measured agreement at the default settings (docs/LATENCY.md): every early verdict is
    // the same decision as the one at the stop. A snapshot taken at the wrong point, or a hint
    // dated wrongly, would break this.
    assert_eq!(
        default_row.agree, default_row.pairs,
        "the early verdict must agree with the one at the stop: {default_row:?}"
    );
}

/// The stability of the verdict under appended silence, on far more cut points than the
/// fixtures have stops: every utterance cut every `step` from a third of its length to its end
/// (the abrupt cut stands in for a pause inside a sentence), once with the silence a 64 ms hint
/// has seen (96 ms of zeros) and once with the silence at the stop (160 ms).
#[derive(Debug, Default)]
struct Sweep {
    pairs: u32,
    agree: u32,
    max_dp: f32,
    borderline: u32,
    borderline_disagree: u32,
    complete: u32,
    /// Complete early, incomplete at the stop: the turn would end where the stop-time verdict
    /// says the user may go on.
    early_only_complete: u32,
    /// Incomplete early, complete at the stop: the turn would wait for the fallback.
    stop_only_complete: u32,
}

fn cut_sweep(fixture: &Fixture, turn: &Arc<SmartTurnOrt>, step: usize, sweep: &mut Sweep) {
    let pcm = samples(&fixture.pcm);
    let at_hint = |cut: usize, silence: usize| {
        let mut audio = pcm[..cut].to_vec();
        audio.resize(cut + silence, 0);
        SmartTurn::analyze_snapshot_blocking(turn.as_ref(), &audio).unwrap()
    };
    for segment in &fixture.segments {
        let (start, end) = (
            usize::try_from(segment.start).unwrap(),
            usize::try_from(segment.end).unwrap(),
        );
        let mut cut = start + (end - start) / 3;
        while cut <= end {
            let early = at_hint(cut, 1_536); // 96 ms
            let stop = at_hint(cut, 2_560); // 160 ms
            sweep.pairs += 1;
            let agree = (early > THRESHOLD) == (stop > THRESHOLD);
            sweep.agree += u32::from(agree);
            sweep.early_only_complete += u32::from(early > THRESHOLD && stop <= THRESHOLD);
            sweep.stop_only_complete += u32::from(early <= THRESHOLD && stop > THRESHOLD);
            sweep.max_dp = sweep.max_dp.max((early - stop).abs());
            sweep.complete += u32::from(stop > THRESHOLD);
            if (0.3..0.7).contains(&early) {
                sweep.borderline += 1;
                sweep.borderline_disagree += u32::from(!agree);
            }
            cut += step;
        }
    }
}

/// The true end of every utterance with the REAL audio that follows it (the fixtures leave a
/// second or more of room noise): the verdict after `early` ms of it against the verdict after
/// `early + extra` ms, for early in 48..=128 ms and extra in 32..=128 ms.
fn end_grid(fixture: &Fixture, turn: &Arc<SmartTurnOrt>, grid: &mut Sweep) {
    let pcm = samples(&fixture.pcm);
    for (i, segment) in fixture.segments.iter().enumerate() {
        let end = usize::try_from(segment.end).unwrap();
        let limit = fixture
            .segments
            .get(i + 1)
            .map_or(pcm.len(), |next| usize::try_from(next.start).unwrap());
        let (early_step, extra_step) = if cfg!(debug_assertions) {
            (80, 96)
        } else {
            (16, 32)
        };
        for early_ms in (48..=128).step_by(early_step) {
            for extra_ms in (32..=128).step_by(extra_step) {
                let (a, b) = (end + early_ms * 16, end + (early_ms + extra_ms) * 16);
                if b > limit {
                    continue;
                }
                let early = SmartTurn::analyze_snapshot_blocking(turn.as_ref(), &pcm[..a]).unwrap();
                let stop = SmartTurn::analyze_snapshot_blocking(turn.as_ref(), &pcm[..b]).unwrap();
                grid.pairs += 1;
                let agree = (early > THRESHOLD) == (stop > THRESHOLD);
                grid.agree += u32::from(agree);
                grid.early_only_complete += u32::from(early > THRESHOLD && stop <= THRESHOLD);
                grid.stop_only_complete += u32::from(early <= THRESHOLD && stop > THRESHOLD);
                grid.max_dp = grid.max_dp.max((early - stop).abs());
                grid.complete += u32::from(stop > THRESHOLD);
            }
        }
    }
}

#[test]
fn the_verdict_is_stable_under_the_silence_between_the_hint_and_the_stop() {
    let turn = smart_turn_model();
    turn.warm(1).unwrap();
    let step = if cfg!(debug_assertions) {
        16_000
    } else {
        1_600
    };
    let mut sweep = Sweep::default();
    let mut grid = Sweep::default();
    for name in FIXTURES {
        let fixture = Fixture::load(&fixtures_dir(), name).unwrap();
        cut_sweep(&fixture, &turn, step, &mut sweep);
        end_grid(&fixture, &turn, &mut grid);
    }
    let report = |label: &str, sweep: &Sweep| {
        println!(
            "{label}: {} pairs, {} agree ({:.1}%), max |dp| {:.3}, {} complete at the stop, {} \
             borderline (0.3..0.7) of which {} flip; flips: {} complete only early, {} complete \
             only at the stop",
            sweep.pairs,
            sweep.agree,
            100.0 * f64::from(sweep.agree) / f64::from(sweep.pairs.max(1)),
            sweep.max_dp,
            sweep.complete,
            sweep.borderline,
            sweep.borderline_disagree,
            sweep.early_only_complete,
            sweep.stop_only_complete
        );
    };
    report(
        &format!("cut sweep, zeros appended, step {} ms", step / 16),
        &sweep,
    );
    report("real utterance ends, real following audio", &grid);
    assert!(sweep.pairs >= 6 && grid.pairs >= 6, "{sweep:?} {grid:?}");
    // Measured 2026-10-08 (release): 100% on the real utterance ends, 90% on the abrupt cuts.
    assert!(
        grid.agree * 100 >= grid.pairs * 95,
        "real utterance ends: {grid:?}"
    );
    // The abrupt cuts (the end of a thin slice of speech, then zeros) sit on the model's decision
    // edge, and WHERE the edge is depends on the ONNX Runtime kernels: the int8 model is chaotic
    // in the last bit (docs/providers/smart-turn.md, "Platform dependence"). The 80% measured
    // above belongs to the reference platform (23 pairs: 20 agree, 87%, on Linux with VNNI; 90%
    // on Windows). Without that platform's kernels the same cuts gave 17 of 23 (74%, six
    // borderline pairs of which five flip, all of them "incomplete early, complete at the
    // stop"), identical on a GitHub runner and on an emulated Haswell, Skylake-X and EPYC. There
    // the bound is looser.
    //
    // "Complete early, incomplete at the stop" (the flip that would end a turn too soon) is a rate,
    // not something a graph never does: on 4000 clips of the official Smart Turn v3.2 test set
    // (each + 96 ms vs + 160 ms of silence) the published and the fused graph both did it 63 times
    // (1.6%) with VNNI and 8 times in 1000 (0.8%) on an emulated CPU without it, and ended the same
    // number of turns too soon overall (docs/providers/smart-turn.md, "Fused GELU"). On these 23
    // abrupt cuts that rate allows one; more would be a real change. On the real utterance ends
    // (the grid) it stays at none.
    assert!(
        sweep.early_only_complete <= 1,
        "early verdicts completed turns the stop verdict did not: {sweep:?}"
    );
    assert_eq!(grid.early_only_complete, 0, "{grid:?}");
    // On the large sets both graphs agree on 96.8-97.4% of the clips; on these 23 cuts the
    // fused graph agrees on 18 (78%) on the reference platform.
    let least_agreement = if brisvox_sim::platform::smart_turn_goldens_apply_here() {
        75
    } else {
        60
    };
    assert!(
        sweep.agree * 100 >= sweep.pairs * least_agreement,
        "cuts with zeros appended: {sweep:?} (at least {least_agreement}% must agree)"
    );
}

// ---------------------------------------------------------------------------------------------
// 2. Latency, production stages in real time

/// Swallows what the driver emits.
struct Sink(Arc<Mutex<u64>>);

impl Processor for Sink {
    fn name(&self) -> &'static str {
        "sink"
    }

    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        _frame: Frame,
        _direction: Direction,
        _ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        Ok(())
    }
}

/// One fixture through `AudioIn` (Silero on the pool) and the `TurnDriver` (Smart Turn on the
/// pool), paced in real time.
async fn pipeline_run(
    fixture: &Fixture,
    vad: &Arc<SileroTractModel>,
    turn: &Arc<SmartTurnOrt>,
    early: bool,
) -> TurnDriverStatsSnapshot {
    let shared: SharedClock = SystemClock::shared();
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 3,
            queue_capacity: 64,
            name: "early-rt".to_owned(),
        },
        Arc::clone(&shared),
    )
    .unwrap();
    let mut engine = EngineConfig::DEFAULT;
    if early {
        engine.early_analysis = EarlyAnalysisConfig::ENABLED;
    }
    let vad_model = Arc::clone(vad);
    let setup = VadSetup::new(
        Offload::Pool(pool.handle()),
        VadWorkerConfig::from_engine(&engine),
        move || vad_model.stream(),
    )
    .with_early_analysis(&engine);
    let audio_in = AudioIn::new(&engine).with_vad(setup);
    let vad_stats = audio_in.vad_stats();
    let model: Arc<dyn TurnModel> = turn.clone();
    let driver = TurnDriver::new(
        TurnDriverConfig::from_engine(&engine),
        Arc::clone(&shared),
        TimelineRecorder::new(),
    )
    .with_smart_turn(model, Offload::Pool(pool.handle()));
    let stats = driver.stats();
    let running = Pipeline::new("early-rt")
        .then_with_queue(audio_in, engine.queues.input)
        .then(driver)
        .then(Sink(Arc::new(Mutex::new(0))))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    let clock = SystemClock;
    let origin = clock.now();
    let frames = fixture.pcm.len().div_ceil(640);
    for i in 0..frames {
        let range = i * 640..((i + 1) * 640).min(fixture.pcm.len());
        let at = origin + FRAME * u32::try_from(i).unwrap();
        brisvox_core::clock::wait_until(&clock, at).await;
        let frame = AudioFrame::new(
            fixture.pcm.slice(range),
            SampleRate::Hz16000,
            at,
            i as u64,
            TurnEpoch::ZERO,
        )
        .unwrap();
        running.input().send(frame.into()).await.unwrap();
    }
    // The last stop and its verdict.
    let deadline = clock.now() + Duration::from_secs(5);
    while clock.now() < deadline {
        let snapshot = stats.snapshot();
        if snapshot.vad_stops >= 2 && snapshot.verdicts_timed >= snapshot.vad_stops {
            break;
        }
        clock.sleep(10 * MS).await;
    }
    clock.sleep(100 * MS).await;
    let vad_snapshot = vad_stats.snapshot();
    assert!(!vad_snapshot.failed_open, "{vad_snapshot:?}");
    assert_eq!(vad_snapshot.falls > 0, early, "{vad_snapshot:?}");
    let snapshot = stats.snapshot();
    drop(running);
    pool.shutdown();
    snapshot
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn early_analysis_cuts_the_stop_to_verdict_latency_in_the_real_pipeline() {
    let vad = SileroTractModel::bundled().unwrap();
    let turn = smart_turn_model();
    turn.warm(8).unwrap();
    let mut runs = Vec::new();
    for early in [false, true] {
        for name in FIXTURES {
            let fixture = Fixture::load(&fixtures_dir(), name).unwrap();
            let vad = Arc::clone(&vad);
            let turn = Arc::clone(&turn);
            runs.push((
                early,
                name,
                tokio::spawn(async move { pipeline_run(&fixture, &vad, &turn, early).await }),
            ));
        }
    }
    let mut total = [(0_u64, 0_u64); 2]; // (verdict latency us, verdicts) per mode
    let mut extra = [0_u64; 2];
    let mut reused_total = 0_u64;
    println!(
        "mode  fixture            stops verdicts mean_ms  hints started reused ready wasted analyses"
    );
    for (early, name, handle) in runs {
        let s = handle.await.unwrap();
        let mean_ms = s.verdict_latency_us as f64 / s.verdicts_timed.max(1) as f64 / 1_000.0;
        println!(
            "{:<5} {name:<18} {:>5} {:>8} {mean_ms:>7.1} {:>6} {:>7} {:>6} {:>5} {:>6} {:>8}",
            if early { "early" } else { "base" },
            s.vad_stops,
            s.verdicts_timed,
            s.early_hints,
            s.early_started,
            s.early_reused,
            s.early_ready,
            s.early_wasted,
            s.analyses_started
        );
        let mode = usize::from(early);
        total[mode].0 += s.verdict_latency_us;
        total[mode].1 += s.verdicts_timed;
        extra[mode] += s.early_started + s.analyses_started;
        assert_eq!(s.vad_stops, 2, "{name}: {s:?}");
        assert_eq!(
            s.verdicts_timed, 2,
            "{name}: every stop got a verdict: {s:?}"
        );
        assert_eq!(s.analyses_failed + s.early_failed, 0, "{name}: {s:?}");
        if early {
            assert_eq!(
                s.early_started,
                s.early_reused + s.early_wasted + s.early_failed,
                "{name}: every early analysis was used, wasted or failed: {s:?}"
            );
            assert_eq!(s.analyses_started + s.early_reused, 2, "{name}: {s:?}");
            // The hint's dating must survive AudioIn -> TurnDriver: a hint dated differently
            // from its stop would make every early job wasted and run the normal analysis
            // instead, which the identities above cannot tell apart.
            assert!(
                s.early_reused >= 1,
                "{name}: no early verdict was ever reused: {s:?}"
            );
            assert_eq!(s.early_failed, 0, "{name}: {s:?}");
            reused_total += s.early_reused;
        } else {
            assert_eq!(s.early_hints + s.early_started, 0, "{name}: {s:?}");
            assert_eq!(s.analyses_started, 2, "{name}: {s:?}");
        }
    }
    // docs/LATENCY.md: all 6 stops reuse their early verdict at the default 64 ms delay; a
    // little slack for a real-time run on a loaded machine (a stop that outran its hint).
    assert!(
        reused_total >= 5,
        "at least 5 of the 6 stops must reuse their early verdict, got {reused_total}"
    );
    let mean = |mode: usize| total[mode].0 as f64 / total[mode].1.max(1) as f64 / 1_000.0;
    println!(
        "mean stop-to-verdict: base {:.1} ms, early {:.1} ms (saved {:.1} ms); analyses {} vs {}",
        mean(0),
        mean(1),
        mean(0) - mean(1),
        extra[0],
        extra[1]
    );
    // Note: the early mean is ~0 by construction when the verdict is ready at the stop (the
    // stop is timed from its arrival at the driver). That the saved time reaches the end of the
    // turn only when the final transcript is slower than the verdict is a design argument
    // (docs/LATENCY.md), not something this test measures.
    assert!(
        mean(1) < mean(0),
        "early analysis must cut the stop-to-verdict latency: {} vs {} ms",
        mean(1),
        mean(0)
    );
}
