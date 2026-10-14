//! Model warm-up on a real inference pool (carried over from the simulator suite): when a call starts, the
//! VAD model is warmed once on EVERY pool thread (the ONNX Runtime Silero keeps one session per
//! thread) and the Smart Turn model is asked for one analysis session per pool thread. Real time,
//! real pool threads, a probe model that records where `warm` ran.
#![allow(
    clippy::unwrap_used,
    clippy::unwrap_in_result,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_methods // real time: the probe models sleep their pool thread
)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

use brisvox_core::{SharedClock, SystemClock, TimelineRecorder};
use brisvox_engine::sim::open_loopback;
use brisvox_engine::{
    EnergyModel, EngineConfig, Offload, SessionBuilder, VadSetup, run_call, smart_turn_warm_jobs,
};
use brisvox_inference::smart_turn::TurnModel;
use brisvox_inference::{InferencePool, PoolConfig, SpeechModel, VadParams};
use brisvox_sim::session::SimSession;

/// An energy model whose `warm` records the thread it ran on and takes a while (a model load).
#[derive(Clone)]
struct ProbeVad {
    inner: EnergyModel,
    threads: Arc<Mutex<Vec<ThreadId>>>,
}

impl SpeechModel for ProbeVad {
    fn window(&self) -> usize {
        self.inner.window()
    }
    fn context(&self) -> usize {
        self.inner.context()
    }
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }
    fn infer(&mut self, input: &[f32]) -> brisvox_inference::Result<f32> {
        self.inner.infer(input)
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn warm(&self) -> brisvox_inference::Result<()> {
        // A real load takes tens of milliseconds: the other pool threads take the other jobs.
        std::thread::sleep(Duration::from_millis(60));
        self.threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        Ok(())
    }
}

/// A Smart Turn model recording how many sessions it was asked to prepare.
struct ProbeTurn {
    asked: Mutex<Vec<usize>>,
    calls: AtomicUsize,
}

impl TurnModel for ProbeTurn {
    fn predict(&self, _features: &[f32]) -> brisvox_inference::Result<f32> {
        Ok(0.9)
    }
    fn warm(&self, sessions: usize) -> brisvox_inference::Result<()> {
        self.asked.lock().unwrap().push(sessions);
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn pool(threads: usize) -> InferencePool {
    let clock: SharedClock = SystemClock::shared();
    InferencePool::new(
        &PoolConfig {
            threads,
            queue_capacity: 64,
            name: "warm-test".to_owned(),
        },
        clock,
    )
    .unwrap()
}

async fn eventually(what: &str, condition: impl Fn() -> bool) {
    for _ in 0..500 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warm_each_runs_the_job_once_on_every_pool_thread() {
    let pool = pool(4);
    let offload = Offload::Pool(pool.handle());
    assert_eq!(offload.threads(), 4);
    let threads = Arc::new(Mutex::new(HashSet::new()));
    let seen = Arc::clone(&threads);
    let report = offload
        .warm_each(Duration::from_secs(5), move || {
            std::thread::sleep(Duration::from_millis(60));
            seen.lock().unwrap().insert(std::thread::current().id());
            Ok(())
        })
        .await;
    assert_eq!((report.ok, report.failed), (4, 0));
    assert_eq!(threads.lock().unwrap().len(), 4, "four distinct threads");
    drop(offload);
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warm_each_counts_failures_and_a_closed_pool() {
    let pool = pool(2);
    let offload = Offload::Pool(pool.handle());
    let report = offload
        .warm_each(Duration::from_secs(5), || {
            Err(brisvox_inference::Error::JobPanicked)
        })
        .await;
    assert_eq!((report.ok, report.failed), (0, 2));
    let handle = pool.handle();
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
    let gone = Offload::Pool(handle);
    let report = gone.warm_each(Duration::from_secs(1), || Ok(())).await;
    assert_eq!(
        (report.ok, report.failed),
        (0, 1),
        "a closed pool fails once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_warms_the_vad_on_every_pool_thread_and_asks_smart_turn_for_one_session_each() {
    let pool = pool(3);
    let clock: SharedClock = SystemClock::shared();
    let sim = SimSession::new(clock.clone());
    let config = EngineConfig::DEFAULT;
    let vad_threads = Arc::new(Mutex::new(Vec::new()));
    let turn = Arc::new(ProbeTurn {
        asked: Mutex::new(Vec::new()),
        calls: AtomicUsize::new(0),
    });
    let call = open_loopback(&clock, 1_024).await.unwrap();
    let offload = Offload::Pool(pool.handle());
    let probe = Arc::clone(&vad_threads);
    let vad = VadSetup::new(
        offload.clone(),
        brisvox_engine::VadWorkerConfig::from_engine(&config),
        move || {
            Ok(ProbeVad {
                inner: EnergyModel::from_pcm_rms(500.0),
                threads: Arc::clone(&probe),
            })
        },
    );
    let _ = VadParams::default();
    let builder = SessionBuilder::new(
        config,
        clock.clone(),
        TimelineRecorder::new(),
        offload,
        Arc::new(sim.stt.clone()),
        Arc::new(sim.llm.clone()),
        Arc::new(sim.tts.clone()),
    )
    .unwrap()
    .with_vad(vad)
    .with_smart_turn(Arc::clone(&turn) as Arc<dyn TurnModel>);
    let cancel = call.cancel.clone();
    let mut running = run_call(call.session, builder, &cancel).await.unwrap();

    let handles = running.handles().clone();
    eventually("the warm-ups", || {
        handles.vad.snapshot().warms == 3 && handles.driver.snapshot().warms == 1
    })
    .await;
    let distinct: HashSet<_> = vad_threads.lock().unwrap().iter().copied().collect();
    assert_eq!(
        distinct.len(),
        3,
        "the VAD model warmed on every pool thread"
    );
    assert_eq!(handles.vad.snapshot().warm_failures, 0);
    // Fanned out: two jobs on three threads, each asking for the same target (the
    // model reserves sessions, so together they create three).
    assert_eq!(smart_turn_warm_jobs(3), 2);
    eventually("the second smart turn job", || {
        turn.asked.lock().unwrap().len() == 2
    })
    .await;
    assert_eq!(
        *turn.asked.lock().unwrap(),
        [3, 3],
        "smart turn was asked for one session per pool thread, by every warm job"
    );
    assert_eq!(handles.driver.snapshot().warm_failures, 0);

    running.stop().await.unwrap();
    drop(running);
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_model_warm_up_is_counted_and_the_call_goes_on() {
    struct Failing;
    impl TurnModel for Failing {
        fn predict(&self, _features: &[f32]) -> brisvox_inference::Result<f32> {
            Ok(0.9)
        }
        fn warm(&self, _sessions: usize) -> brisvox_inference::Result<()> {
            Err(brisvox_inference::Error::JobPanicked)
        }
    }
    let pool = pool(2);
    let clock: SharedClock = SystemClock::shared();
    let sim = SimSession::new(clock.clone());
    let call = open_loopback(&clock, 1_024).await.unwrap();
    let builder = SessionBuilder::new(
        EngineConfig::DEFAULT,
        clock.clone(),
        TimelineRecorder::new(),
        Offload::Pool(pool.handle()),
        Arc::new(sim.stt.clone()),
        Arc::new(sim.llm.clone()),
        Arc::new(sim.tts.clone()),
    )
    .unwrap()
    .with_smart_turn(Arc::new(Failing));
    let cancel = call.cancel.clone();
    let mut running = run_call(call.session, builder, &cancel).await.unwrap();
    let handles = running.handles().clone();
    eventually("the failed warm-up", || {
        handles.driver.snapshot().warm_failures == 1
    })
    .await;
    assert_eq!(handles.driver.snapshot().warms, 0);
    // The call is alive: it stops gracefully.
    running.stop().await.unwrap();
    drop(running);
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
}

fn small_pool(threads: usize, queue_capacity: usize) -> InferencePool {
    let clock: SharedClock = SystemClock::shared();
    InferencePool::new(
        &PoolConfig {
            threads,
            queue_capacity,
            name: "warm-burst".to_owned(),
        },
        clock,
    )
    .unwrap()
}

/// One running call and the client side of its line (dropping the client hangs up).
struct Running {
    call: brisvox_engine::Call,
    _client: brisvox_transport::LoopbackClient,
}

/// Starts one call whose builder uses `offload`, with a probe VAD built on `vad_offload` and the
/// shared Smart Turn probe `turn`.
async fn start_call(
    clock: &SharedClock,
    offload: Offload,
    vad_offload: Offload,
    vad_threads: &Arc<Mutex<Vec<ThreadId>>>,
    turn: &Arc<ProbeTurn>,
) -> Running {
    let sim = SimSession::new(clock.clone());
    let config = EngineConfig::DEFAULT;
    let call = open_loopback(clock, 1_024).await.unwrap();
    let probe = Arc::clone(vad_threads);
    let vad = VadSetup::new(
        vad_offload,
        brisvox_engine::VadWorkerConfig::from_engine(&config),
        move || {
            Ok(ProbeVad {
                inner: EnergyModel::from_pcm_rms(500.0),
                threads: Arc::clone(&probe),
            })
        },
    );
    let builder = SessionBuilder::new(
        config,
        clock.clone(),
        TimelineRecorder::new(),
        offload,
        Arc::new(sim.stt.clone()),
        Arc::new(sim.llm.clone()),
        Arc::new(sim.tts.clone()),
    )
    .unwrap()
    .with_vad(vad)
    .with_smart_turn(Arc::clone(turn) as Arc<dyn TurnModel>);
    let cancel = call.cancel.clone();
    Running {
        call: run_call(call.session, builder, &cancel).await.unwrap(),
        _client: call.client,
    }
}

fn probe_turn() -> Arc<ProbeTurn> {
    Arc::new(ProbeTurn {
        asked: Mutex::new(Vec::new()),
        calls: AtomicUsize::new(0),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_of_call_starts_warms_each_model_once_and_never_saturates_the_pool() {
    // Two threads, room for four queued jobs. Twelve calls start in the same instant: warming
    // per call would submit 24 VAD jobs and 12 Smart Turn jobs and the queue would reject most
    // of them, exactly when a running call's real VAD job could be the one rejected (its VAD
    // would fail open). Warmed once per pool, the burst submits two VAD jobs and one Smart Turn
    // job.
    let pool = small_pool(2, 4);
    let clock: SharedClock = SystemClock::shared();
    let offload = Offload::Pool(pool.handle());
    let vad_threads = Arc::new(Mutex::new(Vec::new()));
    let turn = probe_turn();
    let mut calls = Vec::new();
    for _ in 0..12 {
        calls.push(
            start_call(
                &clock,
                offload.clone(),
                offload.clone(),
                &vad_threads,
                &turn,
            )
            .await,
        );
    }
    let handles: Vec<_> = calls.iter().map(|c| c.call.handles().clone()).collect();
    eventually("the one set of warm-ups", || {
        let vad: u64 = handles.iter().map(|h| h.vad.snapshot().warms).sum();
        let st: u64 = handles.iter().map(|h| h.driver.snapshot().warms).sum();
        vad == 2 && st == 1
    })
    .await;
    // Give any (wrong) late warm job time to show up.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let vad: u64 = handles.iter().map(|h| h.vad.snapshot().warms).sum();
    let vad_failed: u64 = handles.iter().map(|h| h.vad.snapshot().warm_failures).sum();
    let st_failed: u64 = handles
        .iter()
        .map(|h| h.driver.snapshot().warm_failures)
        .sum();
    assert_eq!((vad, vad_failed, st_failed), (2, 0, 0));
    assert_eq!(
        vad_threads.lock().unwrap().len(),
        2,
        "two VAD warm jobs ran"
    );
    assert_eq!(*turn.asked.lock().unwrap(), [2], "smart turn warmed once");
    let metrics = pool.metrics();
    assert_eq!(metrics.rejected, 0, "{metrics:?}");

    // A later call on the warm pool submits nothing.
    let before = pool.metrics().submitted;
    let mut late = start_call(&clock, offload.clone(), offload, &vad_threads, &turn).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(late.call.handles().vad.snapshot().warms, 0);
    assert_eq!(late.call.handles().driver.snapshot().warms, 0);
    assert_eq!(
        pool.metrics().submitted,
        before,
        "no warm job for a warm pool"
    );

    late.call.stop().await.unwrap();
    for mut running in calls {
        running.call.stop().await.unwrap();
    }
    drop(late);
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_vad_runs_on_the_builders_offload_whatever_its_setup_carried() {
    // A pool builder given a setup built for inline jobs: the builder's pool wins, so the warm-up
    // runs once on each of the three pool threads (inline it would run once, on a tokio worker).
    let pool = pool(3);
    let clock: SharedClock = SystemClock::shared();
    let vad_threads = Arc::new(Mutex::new(Vec::new()));
    let turn = probe_turn();
    let mut call = start_call(
        &clock,
        Offload::Pool(pool.handle()),
        Offload::Inline(clock.clone()),
        &vad_threads,
        &turn,
    )
    .await;
    let handles = call.call.handles().clone();
    eventually("the vad warm-up", || handles.vad.snapshot().warms == 3).await;
    let distinct: HashSet<_> = vad_threads.lock().unwrap().iter().copied().collect();
    assert_eq!(distinct.len(), 3, "the VAD warmed on the pool's threads");
    call.call.stop().await.unwrap();
    drop(call);
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
}

// -- the warm-up claim, the busy-pool gate and the Smart Turn fan-out ----------------

/// Occupies every thread of `pool` and queues `queued` more jobs; all of them end when the
/// returned sender is dropped (each blocking job waits at most 5 s).
fn occupy(pool: &InferencePool, threads: usize, queued: usize) -> std::sync::mpsc::Sender<()> {
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let wait = Arc::new(Mutex::new(wait));
    let handle = pool.handle();
    for _ in 0..threads + queued {
        let (handle, wait) = (handle.clone(), Arc::clone(&wait));
        tokio::spawn(async move {
            handle
                .run(Duration::from_secs(10), move || {
                    let _ = wait.lock().unwrap().recv_timeout(Duration::from_secs(5));
                })
                .await
        });
    }
    release
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_pool_is_not_warmed_and_the_claims_cool_down() {
    // One thread, busy, with two more jobs waiting: warm jobs would queue ahead of the calls'
    // VAD windows. Nothing is submitted, both warm-ups count a skip, and the claims cool down:
    // the next call does not try again.
    let pool = small_pool(1, 16);
    let clock: SharedClock = SystemClock::shared();
    let release = occupy(&pool, 1, 2);
    eventually("the backlog", || pool.metrics().queue_depth == 2).await;
    let submitted = pool.metrics().submitted;
    let offload = Offload::Pool(pool.handle());
    let vad_threads = Arc::new(Mutex::new(Vec::new()));
    let turn = probe_turn();
    let mut first = start_call(
        &clock,
        offload.clone(),
        offload.clone(),
        &vad_threads,
        &turn,
    )
    .await;
    let handles = first.call.handles().clone();
    eventually("both skips", || {
        handles.vad.snapshot().warm_skips == 1 && handles.driver.snapshot().warm_skips == 1
    })
    .await;
    let mut second = start_call(&clock, offload.clone(), offload, &vad_threads, &turn).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let later = second.call.handles().clone();
    assert_eq!(
        (
            later.vad.snapshot().warm_skips,
            later.driver.snapshot().warm_skips
        ),
        (0, 0),
        "the claims are cooling down: the second call does not even look"
    );
    assert_eq!(
        pool.metrics().submitted,
        submitted,
        "no warm job was queued"
    );
    assert!(turn.asked.lock().unwrap().is_empty());
    assert!(vad_threads.lock().unwrap().is_empty());
    assert_eq!(
        (
            handles.vad.snapshot().warms,
            handles.driver.snapshot().warm_failures
        ),
        (0, 0)
    );
    drop(release);
    first.call.stop().await.unwrap();
    second.call.stop().await.unwrap();
    drop((first, second));
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_warm_job_that_outlives_its_timeout_keeps_the_claim_until_it_ends() {
    // The caller gives up after 50 ms; the job (a 300 ms model load) runs on. Giving the claim
    // back at the timeout would let the next call start a second warm-up of the same model
    // while the first still runs.
    let pool = pool(2);
    let offload = Offload::Pool(pool.handle());
    let loads = Arc::new(AtomicUsize::new(0));
    let claim = offload.claim_warm("slow-model").unwrap();
    let counted = Arc::clone(&loads);
    let report = offload
        .warm(claim, Duration::from_millis(50), 1, move || {
            std::thread::sleep(Duration::from_millis(300));
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;
    assert_eq!((report.ok, report.failed, report.skipped), (0, 1, 0));
    assert!(
        offload.claim_warm("slow-model").is_none(),
        "the warm-up is still running: no second one"
    );
    assert_eq!(
        pool.handle().warm_cooling_until("slow-model"),
        None,
        "still claimed by the running job"
    );
    eventually("the load to finish", || loads.load(Ordering::SeqCst) == 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // It succeeded after all: the model is warm, the claim is kept (not merely cooling down,
    // which a claim given back at the caller's timeout would be).
    assert!(offload.claim_warm("slow-model").is_none());
    assert_eq!(pool.handle().warm_cooling_until("slow-model"), None);

    // The same with a load that fails after its timeout: the claim comes back only once the
    // job has ended, and then after the busy-pool cool-down (the caller saw a timeout).
    let claim = offload.claim_warm("broken-model").unwrap();
    let report = offload
        .warm(claim, Duration::from_millis(50), 1, || {
            std::thread::sleep(Duration::from_millis(200));
            Err(brisvox_inference::Error::Model("bad file".to_owned()))
        })
        .await;
    assert_eq!(report.failed, 1);
    assert!(
        offload.claim_warm("broken-model").is_none(),
        "still running"
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        offload.claim_warm("broken-model").is_none(),
        "cooling down after a timeout"
    );
    assert!(pool.handle().warm_cooling_until("broken-model").is_some());

    // A load that fails at once (not a busy pool) gives the claim back at once.
    let claim = offload.claim_warm("bad-model").unwrap();
    let report = offload
        .warm(claim, Duration::from_secs(1), 2, || {
            Err(brisvox_inference::Error::Model("bad file".to_owned()))
        })
        .await;
    assert_eq!((report.ok, report.failed), (0, 2));
    assert!(
        offload.claim_warm("bad-model").is_some(),
        "released at once"
    );
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
}

/// A Smart Turn model whose `warm` takes a while and records the thread it ran on.
struct FanTurn {
    asked: Mutex<Vec<(usize, ThreadId)>>,
}

impl TurnModel for FanTurn {
    fn predict(&self, _features: &[f32]) -> brisvox_inference::Result<f32> {
        Ok(0.9)
    }
    fn warm(&self, sessions: usize) -> brisvox_inference::Result<()> {
        std::thread::sleep(Duration::from_millis(80));
        self.asked
            .lock()
            .unwrap()
            .push((sessions, std::thread::current().id()));
        Ok(())
    }
    fn warm_key(&self) -> Option<String> {
        Some("fan-turn".to_owned())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_smart_turn_warm_up_fans_out_over_half_the_pool() {
    // One job preparing a session per thread in a row holds one thread for
    // threads x 100 ms. Fanned out, four jobs run at once on an eight-thread pool.
    let pool = pool(8);
    let clock: SharedClock = SystemClock::shared();
    let sim = SimSession::new(clock.clone());
    let call = open_loopback(&clock, 1_024).await.unwrap();
    let turn = Arc::new(FanTurn {
        asked: Mutex::new(Vec::new()),
    });
    let builder = SessionBuilder::new(
        EngineConfig::DEFAULT,
        clock.clone(),
        TimelineRecorder::new(),
        Offload::Pool(pool.handle()),
        Arc::new(sim.stt.clone()),
        Arc::new(sim.llm.clone()),
        Arc::new(sim.tts.clone()),
    )
    .unwrap()
    .with_smart_turn(Arc::clone(&turn) as Arc<dyn TurnModel>);
    let cancel = call.cancel.clone();
    let mut running = run_call(call.session, builder, &cancel).await.unwrap();
    let handles = running.handles().clone();
    eventually("the warm-up", || handles.driver.snapshot().warms == 1).await;
    let asked = turn.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), smart_turn_warm_jobs(8));
    assert_eq!(asked.len(), 4);
    assert!(
        asked.iter().all(|(sessions, _)| *sessions == 8),
        "{asked:?}"
    );
    let threads: HashSet<_> = asked.iter().map(|(_, thread)| *thread).collect();
    assert_eq!(threads.len(), 4, "the jobs ran side by side");
    running.stop().await.unwrap();
    drop(running);
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vad_warm_ups_are_claimed_per_model_id() {
    // Two model files of the same type each need their own warm-up; the same file only one.
    let pool = pool(2);
    let clock: SharedClock = SystemClock::shared();
    let offload = Offload::Pool(pool.handle());
    let vad_threads = Arc::new(Mutex::new(Vec::new()));
    let mut calls = Vec::new();
    for (id, expected) in [("a.onnx", 2u64), ("b.onnx", 2), ("a.onnx", 0)] {
        let sim = SimSession::new(clock.clone());
        let call = open_loopback(&clock, 1_024).await.unwrap();
        let probe = Arc::clone(&vad_threads);
        let vad = VadSetup::new(
            offload.clone(),
            brisvox_engine::VadWorkerConfig::from_engine(&EngineConfig::DEFAULT),
            move || {
                Ok(ProbeVad {
                    inner: EnergyModel::from_pcm_rms(500.0),
                    threads: Arc::clone(&probe),
                })
            },
        )
        .with_model_id(id);
        let builder = SessionBuilder::new(
            EngineConfig::DEFAULT,
            clock.clone(),
            TimelineRecorder::new(),
            offload.clone(),
            Arc::new(sim.stt.clone()),
            Arc::new(sim.llm.clone()),
            Arc::new(sim.tts.clone()),
        )
        .unwrap()
        .with_vad(vad);
        let cancel = call.cancel.clone();
        let running = run_call(call.session, builder, &cancel).await.unwrap();
        let handles = running.handles().clone();
        if expected > 0 {
            eventually("the warm-up", || handles.vad.snapshot().warms == expected).await;
        } else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(handles.vad.snapshot().warms, 0, "{id} is warm already");
        }
        calls.push((running, call.client));
    }
    assert_eq!(vad_threads.lock().unwrap().len(), 4);
    for (mut running, _client) in calls {
        running.stop().await.unwrap();
    }
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
}

/// A [`ProbeVad`] over a named model file: the key its warm state belongs to.
#[derive(Clone)]
struct FileVad {
    probe: ProbeVad,
    file: &'static str,
}

impl SpeechModel for FileVad {
    fn window(&self) -> usize {
        self.probe.window()
    }
    fn context(&self) -> usize {
        self.probe.context()
    }
    fn sample_rate(&self) -> u32 {
        self.probe.sample_rate()
    }
    fn infer(&mut self, input: &[f32]) -> brisvox_inference::Result<f32> {
        self.probe.infer(input)
    }
    fn reset(&mut self) {
        self.probe.reset();
    }
    fn warm(&self) -> brisvox_inference::Result<()> {
        self.probe.warm()
    }
    fn warm_key(&self) -> Option<String> {
        Some(format!("file:{}", self.file))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_setup_from_a_model_claims_its_warm_up_by_the_models_own_key() {
    // Carry-over (e): without any opt-in, two setups of one model type over different files
    // each warm their own file; the same file again warms nothing.
    let pool = pool(2);
    let clock: SharedClock = SystemClock::shared();
    let offload = Offload::Pool(pool.handle());
    let vad_threads = Arc::new(Mutex::new(Vec::new()));
    let mut calls = Vec::new();
    for (file, expected) in [("a.onnx", 2u64), ("b.onnx", 2), ("a.onnx", 0)] {
        let sim = SimSession::new(clock.clone());
        let call = open_loopback(&clock, 1_024).await.unwrap();
        let model = FileVad {
            probe: ProbeVad {
                inner: EnergyModel::from_pcm_rms(500.0),
                threads: Arc::clone(&vad_threads),
            },
            file,
        };
        let vad = VadSetup::from_model(
            offload.clone(),
            brisvox_engine::VadWorkerConfig::from_engine(&EngineConfig::DEFAULT),
            model,
        );
        let builder = SessionBuilder::new(
            EngineConfig::DEFAULT,
            clock.clone(),
            TimelineRecorder::new(),
            offload.clone(),
            Arc::new(sim.stt.clone()),
            Arc::new(sim.llm.clone()),
            Arc::new(sim.tts.clone()),
        )
        .unwrap()
        .with_vad(vad);
        let cancel = call.cancel.clone();
        let running = run_call(call.session, builder, &cancel).await.unwrap();
        let handles = running.handles().clone();
        if expected > 0 {
            eventually("the warm-up", || handles.vad.snapshot().warms == expected).await;
        } else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(handles.vad.snapshot().warms, 0, "{file} is warm already");
        }
        calls.push((running, call.client));
    }
    assert_eq!(vad_threads.lock().unwrap().len(), 4);
    for (mut running, _client) in calls {
        running.stop().await.unwrap();
    }
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_saturated_pool_degrades_the_call_but_leaves_it_alive() {
    // Pool saturation and job timeouts injected into a running call. The pool's only
    // thread is busy for 1.2 s with a full queue behind it: the call's VAD batches are refused
    // (or time out) and its Smart Turn analyses fail, the call keeps flowing (audio still goes
    // to STT), and once the pool is free again the VAD recovers. The call then stops cleanly.
    let pool = small_pool(1, 2);
    let clock: SharedClock = SystemClock::shared();
    let sim = SimSession::new(clock.clone());
    let config = EngineConfig::DEFAULT;
    let call = open_loopback(&clock, 65_536).await.unwrap();
    let offload = Offload::Pool(pool.handle());
    let builder = SessionBuilder::new(
        config,
        clock.clone(),
        TimelineRecorder::new(),
        offload.clone(),
        Arc::new(sim.stt.clone()),
        Arc::new(sim.llm.clone()),
        Arc::new(sim.tts.clone()),
    )
    .unwrap()
    .with_vad(brisvox_engine::sim::energy_vad_setup(
        offload, &sim.vad, &config,
    ))
    .with_smart_turn(Arc::new(brisvox_engine::sim::FixedTurnModel(0.9)) as Arc<dyn TurnModel>);
    let cancel = call.cancel.clone();
    let mut running = run_call(call.session, builder, &cancel).await.unwrap();
    let handles = running.handles().clone();
    let (mut caller, mut listener) = call.client.split();
    let reader = tokio::spawn(async move { while listener.recv().await.is_some() {} });
    // Let the warm-ups finish, then fill the pool: one long job running, two waiting.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut release = Some(occupy(&pool, 1, 2));
    // 2.4 s of caller audio: 0.6 s silence, 1 s speech, 0.8 s silence, in real time (then 6 s
    // of silence below).
    let loud: bytes::Bytes = (0..320)
        .flat_map(|i: i32| {
            let sample: i16 = if i % 16 < 8 { 3_000 } else { -3_000 };
            sample.to_le_bytes()
        })
        .collect::<Vec<u8>>()
        .into();
    let quiet = bytes::Bytes::from(vec![0u8; 640]);
    let started = tokio::time::Instant::now();
    for i in 0..120u32 {
        let pcm = if (30..80).contains(&i) {
            loud.clone()
        } else {
            quiet.clone()
        };
        caller
            .send_audio(pcm, brisvox_core::frame::SampleRate::Hz16000)
            .await
            .unwrap();
        if i == 60 {
            // The pool frees up in the middle of the speech.
            release = None;
        }
        tokio::time::sleep_until(started + Duration::from_millis(20 * u64::from(i + 1))).await;
    }
    assert!(release.is_none());
    let vad = handles.vad.snapshot();
    assert!(
        vad.saturated + vad.timeouts > 0,
        "the injected saturation reached the VAD: {vad:?}"
    );
    // Every frame is accounted for: taken by the worker, or dropped by the tee (oldest first)
    // when the worker fell behind on a loaded machine. The worker may still be taking the last
    // ones.
    eventually("the VAD to account for every frame", || {
        let vad = handles.vad.snapshot();
        vad.frames + vad.tee_dropped == handles.audio_in.snapshot().frames_in
    })
    .await;
    // Every frame still reached STT.
    eventually("the audio to reach STT", || {
        handles.stt.snapshot().audio_in >= 120
    })
    .await;
    // The pool is free again. A VAD that failed open (enough refused batches over a second of
    // audio) probes again after `recovery_interval` (5 s) of audio: send 6 s of silence.
    let before = handles.vad.snapshot().jobs_ok;
    for i in 0..300u32 {
        caller
            .send_audio(quiet.clone(), brisvox_core::frame::SampleRate::Hz16000)
            .await
            .unwrap();
        tokio::time::sleep_until(started + Duration::from_millis(20 * u64::from(121 + i))).await;
    }
    eventually("the VAD to run again", || {
        let vad = handles.vad.snapshot();
        vad.jobs_ok > before && !vad.failed_open
    })
    .await;
    let vad = handles.vad.snapshot();
    assert!(
        !vad.failed_open && (vad.recoveries > 0 || vad.saturated + vad.timeouts < 5),
        "{vad:?}"
    );
    running.stop().await.unwrap();
    drop(running);
    reader.abort();
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
}
