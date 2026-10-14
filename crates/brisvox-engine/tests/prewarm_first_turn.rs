//! Pre-warming on the production assembly: a call started by [`run_call`] connects
//! its STT and TTS sessions and warms every provider (and the Smart Turn model) when it starts,
//! so its first turn answers exactly as fast as a later one: no connect, handshake or warm-up
//! on the first turn's path.
//!
//! The providers are the simulator's mocks with fixed latencies and slow set-ups: the STT
//! connect takes 300 ms, the TTS connect 2.5 s, and the LLM's `warm()` 1 s (a wrapper that
//! records when it finished). The caller starts to speak after a lead of 3 s, so every set-up
//! has finished before the first word, and the first turn's response latency (end of speech to
//! the first bot audio, from the engine's own timeline) equals the second turn's. The checker
//! can fail: with a 0.5 s lead the TTS connect is still running when the first answer is ready,
//! and the first turn is measurably slower.
//!
//! Covered elsewhere: the Smart Turn and VAD warm-ups on every thread of a real pool
//! (`warm_pool.rs`), each provider's warm-up and connect finishing before the first VAD stop
//! with failing and hanging warm-ups (`hybrid_prewarm.rs`), every stage warming at start
//! (`run_call.rs`).
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{SharedClock, SimClock, TimelineRecorder, TurnEvent, TurnRecord};
use brisvox_engine::sim::{FixedTurnModel, LoopbackCall, energy_vad_setup, open_loopback};
use brisvox_engine::{EngineConfig, InterruptionConfig, Offload, SessionBuilder, run_call};
use brisvox_providers::{LlmProvider, LlmRequest, LlmStream};
use brisvox_sim::input::{InputSink, InputTarget, SendFuture};
use brisvox_sim::latency::LatencyProfile;
use brisvox_sim::mock_llm::{LlmScript, MockLlm};
use brisvox_sim::scenario::{Scenario, play_input};
use brisvox_sim::session::SimSession;
use tokio::time::Instant;

const MS: Duration = Duration::from_millis(1);
const STT_CONNECT: Duration = Duration::from_millis(300);
const TTS_CONNECT: Duration = Duration::from_millis(2_500);
const LLM_WARM: Duration = Duration::from_millis(1_000);

/// The mock LLM with a slow `warm()` that records when it finished.
struct SlowWarmLlm {
    inner: MockLlm,
    clock: SharedClock,
    warmed: Arc<Mutex<Option<Instant>>>,
}

impl LlmProvider for SlowWarmLlm {
    fn name(&self) -> &'static str {
        "slow-warm-llm"
    }

    async fn stream(&self, request: LlmRequest) -> brisvox_providers::Result<LlmStream> {
        self.inner.stream(request).await
    }

    async fn warm(&self) -> brisvox_providers::Result<()> {
        self.clock.sleep(LLM_WARM).await;
        *self.warmed.lock().unwrap() = Some(self.clock.now());
        Ok(())
    }
}

/// The caller's side of the loopback call.
struct Caller {
    client: tokio::sync::Mutex<brisvox_transport::LoopbackClient>,
}

impl InputTarget for Caller {
    fn send(&self, frame: brisvox_core::frame::AudioFrame) -> SendFuture<'_> {
        Box::pin(async move {
            self.client
                .lock()
                .await
                .send_audio(frame.data().clone(), frame.sample_rate())
                .await
                .is_ok()
        })
    }
}

/// What one call showed.
struct Ran {
    /// Finished turns that got bot audio, in order.
    turns: Vec<TurnRecord>,
    /// When the LLM warm-up finished, and when the call started.
    warmed: Option<Instant>,
    origin: Instant,
    connects: (u64, u64),
    warms: [u64; 4],
}

/// Two questions after `lead` of silence, answered on the production assembly.
async fn call(lead: Duration) -> Ran {
    let clock: SharedClock = Arc::new(SimClock::new());
    let sim = SimSession::new(clock.clone());
    let fixed = LatencyProfile::Fixed;
    let stt = sim
        .stt
        .clone()
        .with_latency(fixed(STT_CONNECT), fixed(100 * MS), fixed(150 * MS), 1);
    let tts = sim
        .tts
        .clone()
        .with_latency(fixed(TTS_CONNECT), fixed(150 * MS), fixed(20 * MS), 1);
    let llm = sim
        .llm
        .clone()
        .with_latency(fixed(350 * MS), fixed(30 * MS), 1);
    let scenario = Scenario::new()
        .silence(lead)
        .speak("what time is it", 800 * MS)
        .silence(3_000 * MS)
        .speak("and what is the date", 800 * MS)
        .with_tail(2_000 * MS);
    for utterance in scenario.utterances() {
        stt.push(utterance);
    }
    for _ in 0..2 {
        llm.push(LlmScript::text(["Sure thing."]));
    }
    let warmed = Arc::new(Mutex::new(None));
    let LoopbackCall {
        session,
        client,
        cancel,
    } = open_loopback(&clock, 65_536).await.unwrap();
    let mut config = EngineConfig::DEFAULT;
    config.interruption = InterruptionConfig::OFF;
    let offload = Offload::Inline(clock.clone());
    let timeline = TimelineRecorder::new();
    let builder = SessionBuilder::new(
        config,
        clock.clone(),
        timeline.clone(),
        offload.clone(),
        Arc::new(stt),
        Arc::new(SlowWarmLlm {
            inner: llm,
            clock: clock.clone(),
            warmed: Arc::clone(&warmed),
        }),
        Arc::new(tts),
    )
    .unwrap()
    .with_vad(energy_vad_setup(offload, &sim.vad, &config))
    .with_smart_turn(Arc::new(FixedTurnModel(0.9)));
    let origin = clock.now();
    let mut running = run_call(session, builder, &cancel).await.unwrap();
    let caller = Arc::new(Caller {
        client: tokio::sync::Mutex::new(client),
    });
    play_input(
        &scenario,
        clock.clone(),
        origin,
        SampleRate::Hz16000,
        InputSink::target(caller.clone()),
        &cancel,
    )
    .await
    .unwrap();
    clock.sleep(2_000 * MS).await;
    let h = running.handles().clone();
    running.stop().await.unwrap();
    timeline.finish();
    let turns = timeline
        .history()
        .into_iter()
        .filter(|t| t.at(TurnEvent::FirstAudioOut).is_some())
        .collect();
    let warmed = *warmed.lock().unwrap();
    Ran {
        turns,
        warmed,
        origin,
        connects: (h.stt.snapshot().connects, h.tts.snapshot().connects),
        warms: [
            h.stt.snapshot().warms,
            h.tts.snapshot().warms,
            h.llm.snapshot().warms,
            h.driver.snapshot().warms,
        ],
    }
}

fn response(turn: &TurnRecord) -> Duration {
    turn.between(TurnEvent::UserSpeechEnd, TurnEvent::FirstAudioOut)
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn the_first_turn_pays_no_connect_or_warm_up() {
    let lead = 3_000 * MS;
    let ran = call(lead).await;
    assert_eq!(ran.turns.len(), 2, "both questions were answered");
    // Every set-up ran once, at the start of the call, and finished before the first word.
    assert_eq!(
        ran.connects,
        (1, 1),
        "one STT and one TTS session for the call"
    );
    assert_eq!(
        ran.warms,
        [1, 1, 1, 1],
        "stt, tts, llm, smart turn warmed once"
    );
    let first_word = ran.turns[0].at(TurnEvent::UserSpeechStart).unwrap();
    let warmed = ran.warmed.unwrap();
    assert!(
        warmed < first_word,
        "the LLM warm-up finished before the caller spoke"
    );
    assert!(
        ran.origin + TTS_CONNECT < first_word,
        "the slowest connect finished before the caller spoke"
    );
    // So the first turn answers as fast as the second (the same fixed provider latencies; the
    // output's 20 ms frame grid is the only allowed difference).
    let (first, second) = (response(&ran.turns[0]), response(&ran.turns[1]));
    assert!(
        first.abs_diff(second) <= 20 * MS,
        "first turn {first:?}, second turn {second:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_check_sees_a_connect_on_the_first_turns_path() {
    // The caller speaks after 0.5 s: the first answer is ready long before the 2.5 s TTS
    // connect finishes, so the first turn is slower than the second by the rest of it (about
    // 390 ms here).
    let ran = call(500 * MS).await;
    assert_eq!(ran.turns.len(), 2);
    let (first, second) = (response(&ran.turns[0]), response(&ran.turns[1]));
    assert!(
        first > second + 250 * MS,
        "first turn {first:?}, second turn {second:?}"
    );
}
