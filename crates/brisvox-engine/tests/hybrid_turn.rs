//! Hybrid gate 2 (`docs/PIPELINE.md` section 9): the engine's input chain (pump,
//! `AudioIn`, VAD worker) plus the engine's `SttStage` and `TurnDriver` in front of the untouched
//! reference LLM, TTS, pacer and output. The scripted caller's audio goes through a real
//! `TransportSession` over the loopback transport and the pump; the shared suite runs under the
//! Budget profile on a paused clock. The turn driver is built three ways: fixed silence, and the
//! adaptive policy with a mock Smart Turn model that always says "complete", each with the
//! recommended barge-in guards and with every guard off.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_agent::TurnPolicy;
use brisvox_core::{Pipeline, SimClock, TurnEvent};
use brisvox_engine::{
    AudioIn, EnergyModel, EngineConfig, InterruptionConfig, Offload, SttStage, SttStageConfig,
    SttStats, TurnDriver, TurnDriverConfig, TurnDriverStats, TurnProbe, VadSetup, VadWorkerConfig,
};
use brisvox_inference::VadParams;
use brisvox_inference::smart_turn::TurnModel;
use brisvox_sim::factory::{BuiltSession, ReferenceFactory, SessionFactory, SessionParts};
use brisvox_sim::input::{InputSink, InputTarget};
use brisvox_sim::leak::TaskCounter;
use brisvox_sim::mock_llm::LlmScript;
use brisvox_sim::output::OutputLog;
use brisvox_sim::reference::{LlmStage, OutputPacer, PlayLog, SessionProbes, TtsStage, WordLog};
use brisvox_sim::scenario::{Scenario, SimTransportOut};
use brisvox_sim::session::SimSession;
use brisvox_sim::suite::{self, Profile};
use common::MS;
use common::loopback::{self, LoopbackInput};

/// A Smart Turn model with a fixed answer.
struct FixedModel(f32);

impl TurnModel for FixedModel {
    fn predict(&self, _features: &[f32]) -> brisvox_inference::Result<f32> {
        Ok(self.0)
    }
}

/// How the turn driver decides.
#[derive(Clone, Copy, Debug)]
enum Policy {
    /// Fixed silence of 450 ms after the (backdated) VAD stop: longer than the 340 ms gap of
    /// s03, so a user who resumes inside it continues the same turn.
    Fixed,
    /// Adaptive with a model that always answers `probability`.
    Smart(f32),
}

/// One built call's handles, for tests that look inside.
#[derive(Clone)]
struct CallHandles {
    input: Arc<LoopbackInput>,
    probe: TurnProbe,
    driver: TurnDriverStats,
    stt: SttStats,
}

#[derive(Clone)]
struct HybridTurn {
    config: EngineConfig,
    policy: Policy,
    calls: Arc<Mutex<Vec<CallHandles>>>,
}

impl HybridTurn {
    fn new(policy: Policy, interruption: InterruptionConfig) -> Self {
        let mut config = EngineConfig::DEFAULT;
        config.interruption = interruption;
        // The simulator's energy VAD reports a stop 300 ms after the speech.
        config.interruption.vad_hangover = if interruption == InterruptionConfig::OFF {
            Duration::ZERO
        } else {
            300 * MS
        };
        config.turn = match policy {
            Policy::Fixed => TurnPolicy::fixed(450 * MS),
            // A complete verdict waits for the STT final: in s03 the 450 ms final latency
            // outlasts the 340 ms gap, so the resumed speech continues the turn.
            Policy::Smart(_) => TurnPolicy::Adaptive {
                fallback_silence: Duration::from_secs(3),
                require_final: true,
                final_grace: 800 * MS,
            },
        };
        Self {
            config,
            policy,
            calls: Arc::default(),
        }
    }

    fn handles(&self) -> Vec<CallHandles> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl SessionFactory for HybridTurn {
    async fn build(&self, parts: SessionParts) -> brisvox_sim::Result<BuiltSession> {
        let SessionParts {
            clock,
            stt,
            llm: mock_llm,
            tts: mock_tts,
            vad,
            transport_delay,
            timeline,
            origin,
            input_rate: _,
            output_rate,
        } = parts;
        let (input, commands) = loopback::open(&clock, &self.config)
            .await
            .map_err(|e| brisvox_sim::Error::Assertion(format!("hybrid setup: {e}")))?;

        let vad_setup = VadSetup::new(
            Offload::Inline(clock.clone()),
            VadWorkerConfig {
                params: VadParams {
                    threshold: 0.5,
                    neg_threshold: 0.35,
                    min_speech: vad.start_after,
                    min_silence: vad.stop_after,
                    speech_pad: Duration::ZERO,
                },
                ..VadWorkerConfig::from_engine(&self.config)
            },
            move || {
                #[allow(clippy::cast_possible_truncation)] // a threshold of a few hundred
                Ok(EnergyModel::from_pcm_rms(vad.threshold as f32))
            },
        );
        let audio_in = AudioIn::new(&self.config).with_vad(vad_setup);

        // The engine's STT and turn driver.
        let stt_stage = SttStage::new(
            Arc::new(stt),
            SttStageConfig::from_engine(&self.config),
            clock.clone(),
        );
        let probe = TurnProbe::new(256);
        let mut driver = TurnDriver::new(
            TurnDriverConfig::from_engine(&self.config),
            clock.clone(),
            timeline.clone(),
        )
        .with_commands(commands)
        .with_probe(probe.clone());
        if let Policy::Smart(probability) = self.policy {
            driver = driver.with_smart_turn(
                Arc::new(FixedModel(probability)),
                Offload::Inline(clock.clone()),
            );
        }
        let handles = CallHandles {
            input: Arc::clone(&input),
            probe,
            driver: driver.stats(),
            stt: stt_stage.stats(),
        };

        // The reference chain behind them.
        let output = OutputLog::new(origin, Some(timeline.clone()));
        let tail = SimTransportOut::from_log(clock.clone(), output.clone())
            .with_consume_delay(transport_delay);
        let words = WordLog::default();
        let played = PlayLog::default();
        let llm_stage = LlmStage::new(
            Arc::new(mock_llm),
            clock.clone(),
            timeline.clone(),
            Arc::clone(&words),
            Arc::clone(&played),
        );
        let tts_stage = TtsStage::new(
            Arc::new(mock_tts),
            output_rate,
            clock.clone(),
            timeline.clone(),
            Arc::clone(&words),
        )
        .with_play_log(Arc::clone(&played));
        let probes = SessionProbes::new(
            llm_stage.history(),
            words,
            Arc::clone(&played),
            llm_stage.truncations(),
        );
        let pipeline = Pipeline::new("hybrid-turn")
            .then_with_queue(audio_in, self.config.queues.input)
            .then_with_queue(stt_stage, self.config.queues.user)
            .then_with_queue(driver, self.config.queues.user)
            .then(llm_stage)
            .then(tts_stage)
            .then(OutputPacer::new(clock, played))
            .then(tail);
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(handles);
        Ok(BuiltSession::new(
            pipeline,
            probes,
            InputSink::target(Arc::clone(&input) as Arc<dyn InputTarget>),
            output,
        )
        .with_guard(input))
    }
}

fn fixed_recommended() -> HybridTurn {
    HybridTurn::new(Policy::Fixed, InterruptionConfig::RECOMMENDED)
}

fn fixed_open() -> HybridTurn {
    HybridTurn::new(Policy::Fixed, InterruptionConfig::OFF)
}

fn smart_recommended() -> HybridTurn {
    HybridTurn::new(Policy::Smart(0.9), InterruptionConfig::RECOMMENDED)
}

fn smart_open() -> HybridTurn {
    HybridTurn::new(Policy::Smart(0.9), InterruptionConfig::OFF)
}

#[tokio::test(start_paused = true)]
async fn all_scenarios_pass_with_fixed_silence_and_the_recommended_guards() {
    suite::scenarios::all(&fixed_recommended(), Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn all_scenarios_pass_with_fixed_silence_and_every_guard_off() {
    suite::scenarios::all(&fixed_open(), Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn all_scenarios_pass_with_the_adaptive_policy_and_a_fixed_mock_model() {
    suite::scenarios::all(&smart_recommended(), Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_basic_suite_passes_with_fixed_silence() {
    // Every guard off: the suite's barge-in budget (an interrupt at the output at 2.4 s to 2.6 s)
    // is that of an immediate barge-in; the guards deliberately delay it.
    suite::basic::all(&fixed_open(), Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_basic_suite_passes_with_the_adaptive_policy() {
    suite::basic::all(&smart_open(), Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn real_speech_is_answered_after_each_utterance() {
    suite::fixtures::a_real_speech_call_is_answered_after_each_utterance(
        &fixed_recommended(),
        Profile::Budget,
    )
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_engine_turn_driver_marks_speech_and_turns_like_the_reference() {
    let ms = |n: u64| Duration::from_millis(n);
    let scenario = Scenario::new()
        .silence(ms(100))
        .speak("tell me a story", ms(900))
        .silence(ms(1_800))
        .speak("okay stop", ms(600))
        .with_tail(ms(500));
    let marks = |run: &brisvox_sim::session::SessionRun| -> Vec<_> {
        run.turns
            .iter()
            .map(|t| {
                (
                    t.at(TurnEvent::UserSpeechStart).map(|at| at - run.origin),
                    t.at(TurnEvent::UserSpeechEnd).map(|at| at - run.origin),
                )
            })
            .collect()
    };
    let run = |hybrid: bool| {
        let scenario = scenario.clone();
        async move {
            let session = SimSession::new(Arc::new(SimClock::new()));
            session
                .llm
                .push(LlmScript::text(["A very long story that goes on and on."]));
            session.llm.push(LlmScript::text(["Sure."]));
            if hybrid {
                session
                    .run_with(&fixed_open(), &scenario, ms(2_000))
                    .await
                    .unwrap()
            } else {
                session
                    .run_with(&ReferenceFactory, &scenario, ms(2_000))
                    .await
                    .unwrap()
            }
        }
    };
    let reference = run(false).await;
    let hybrid = run(true).await;
    assert_eq!(hybrid.outcome, Ok(()));
    assert_eq!(marks(&hybrid), marks(&reference));
    assert_eq!(hybrid.turns.len(), reference.turns.len());
    assert_eq!(
        hybrid.probes.history().len(),
        reference.probes.history().len()
    );
}

#[tokio::test(start_paused = true)]
async fn the_driver_counts_what_a_barge_in_scenario_does() {
    let factory = fixed_open();
    suite::scenarios::s02_barge_in(&factory, Profile::Budget)
        .await
        .unwrap();
    let handles = factory.handles();
    assert!(!handles.is_empty());
    for call in &handles {
        let driver = call.driver.snapshot();
        assert_eq!(driver.interrupts, 1, "{driver:?}");
        assert_eq!(driver.clear_errors, 0, "{driver:?}");
        assert_eq!(driver.clear_timeouts, 0, "{driver:?}");
        assert_eq!(
            driver.clears_sent, 1,
            "the fast-path clear was sent: {driver:?}"
        );
        assert_eq!(driver.gate_timeouts, 0, "{driver:?}");
        // Every VAD stop got exactly one UtteranceFinal and no synthetic one was needed.
        let stt = call.stt.snapshot();
        assert_eq!(
            stt.finalizes_requested, driver.vad_stops,
            "{stt:?} {driver:?}"
        );
        assert_eq!(stt.finals_synthetic, 0, "{stt:?}");
        assert_eq!(stt.finals_real, stt.finalizes_requested, "{stt:?}");
        assert!(call.probe.machine().is_some());
    }
}

#[tokio::test(start_paused = true)]
async fn nothing_outlives_a_hybrid_call() {
    let clock = SimClock::new();
    let counter = TaskCounter::start_settled(&clock, 20 * MS).await.unwrap();
    suite::scenarios::s02_barge_in(&smart_recommended(), Profile::Budget)
        .await
        .unwrap();
    counter
        .wait_for_baseline(&clock, Duration::from_secs(1))
        .await
        .expect("the pump, the STT session task, the timer or an analysis task leaked");
}

#[tokio::test(start_paused = true)]
async fn the_caller_audio_really_travels_through_the_transport_and_the_pump() {
    let factory = fixed_recommended();
    suite::scenarios::s01_happy_path_with_realistic_latency(&factory, Profile::Budget)
        .await
        .unwrap();
    for call in factory.handles() {
        let pump = call.input.pump.lock().unwrap();
        let handle = pump.as_ref().expect("the sink was bound: the pump started");
        let stats = handle.stats();
        assert!(stats.audio > 100, "{stats:?}");
        assert_eq!(stats.hangups, 0, "the call was never hung up: {stats:?}");
        let stt = call.stt.snapshot();
        assert!(stt.audio_sent > 100, "audio reached the provider: {stt:?}");
        assert_eq!(stt.audio_dropped_queue, 0, "{stt:?}");
    }
}
