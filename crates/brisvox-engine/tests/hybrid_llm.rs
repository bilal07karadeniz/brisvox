//! Hybrid gate 3 (`docs/PIPELINE.md` section 9): the engine's input chain (pump,
//! `AudioIn`, VAD worker), `SttStage` and `TurnDriver`, then the engine's `LlmRunner`, in front of
//! the untouched reference TTS, pacer and output. The scripted caller's audio goes through a real
//! `TransportSession` over the loopback transport and the pump; the shared suite runs under the
//! Budget profile on a paused clock. The runner cuts interrupted responses to the played words
//! through a `TruncationSource` over the reference pipeline's word and playout logs (the same
//! evidence the reference LLM stage uses), so the structural invariants (context equals the played
//! prefix, roles, turn counts, interrupt counts) are the suite's own.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_agent::context::Message;
use brisvox_agent::played::{PlayedConfig, PlayedText};
use brisvox_agent::tools::{
    EngineConfig as ToolEngineConfig, SessionId, Tool, ToolCallCtx, ToolEngine, ToolFuture,
    ToolKind, ToolOutput, ToolRegistry, ToolSpec,
};
use brisvox_agent::{Truncation, TurnPolicy};
use brisvox_core::{Pipeline, SharedClock, SimClock, TurnEvent};
use brisvox_engine::{
    AudioIn, BoxFuture, Cut, CutRequest, EnergyModel, EngineConfig, InterruptionConfig, LlmRunner,
    LlmStats, Offload, SttStage, SttStageConfig, SttStats, TruncationSource, TurnDriver,
    TurnDriverConfig, TurnDriverStats, TurnProbe, VadSetup, VadWorkerConfig,
};
use brisvox_inference::VadParams;
use brisvox_inference::smart_turn::TurnModel;
use brisvox_sim::factory::{BuiltSession, SessionFactory, SessionParts};
use brisvox_sim::input::{InputSink, InputTarget};
use brisvox_sim::leak::TaskCounter;
use brisvox_sim::mock_llm::LlmScript;
use brisvox_sim::output::OutputLog;
use brisvox_sim::reference::{OutputPacer, PlayLog, SessionProbes, TtsStage, WordLog};
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
    /// Fixed silence of 300 ms: shorter than the 400 ms gap before the barge-in of the played
    /// text tests, so that a caller who speaks again after it starts a new turn.
    Quick,
    /// Adaptive with a model that always answers `probability`.
    Smart(f32),
}

/// One built call's handles, for tests that look inside.
#[derive(Clone)]
struct CallHandles {
    input: Arc<LoopbackInput>,
    driver: TurnDriverStats,
    stt: SttStats,
    llm: LlmStats,
}

/// What the caller heard of a response, from the reference pipeline's evidence: the words the TTS
/// timed and the audio the pacer released, judged at the instant of the barge-in (or now, when the
/// response ended by itself). A response whose audio played out in full is kept as sent.
struct ReferenceSource {
    words: WordLog,
    played: PlayLog,
    clock: SharedClock,
}

impl TruncationSource for ReferenceSource {
    fn cut<'a>(&'a self, request: &'a CutRequest) -> BoxFuture<'a, Cut> {
        let epoch = request.key.epoch;
        let at = request.interrupted_at.unwrap_or_else(|| self.clock.now());
        let (position, complete, total) = {
            let played = self.played.lock().unwrap_or_else(PoisonError::into_inner);
            played
                .get(&epoch)
                .map_or((Duration::ZERO, false, Duration::ZERO), |playout| {
                    (playout.position(at), playout.is_complete(), playout.total())
                })
        };
        let cut = if complete && position >= total {
            let text = request.sent.trim_end();
            Cut::HeardInFull(Truncation {
                text: Arc::from(text),
                estimated: false,
                words_heard: text.split_whitespace().count(),
                unmatched_words: 0,
                gaps: 0,
                played: position,
            })
        } else {
            let mut tracker = PlayedText::new();
            tracker.push_sent(&request.sent);
            let words = self.words.lock().unwrap_or_else(PoisonError::into_inner);
            for (word_epoch, word, start) in words.iter() {
                if *word_epoch == epoch {
                    tracker.push_word(Arc::clone(word), *start);
                }
            }
            Cut::Heard(tracker.heard(position, &PlayedConfig::default()))
        };
        Box::pin(async move { cut })
    }
}

/// A ReadOnly tool that answers at once (the suite's s08 calls `time_in`).
struct TimeIn;

impl Tool for TimeIn {
    fn call<'a>(&'a self, _args: serde_json::Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async { Ok(ToolOutput::Json(serde_json::json!({"time": "09:00"}))) })
    }
}

fn time_in_tools(clock: &SharedClock) -> (Arc<ToolRegistry>, Arc<ToolEngine>) {
    let mut registry = ToolRegistry::new();
    registry
        .register(
            ToolSpec::new(
                "time_in",
                "The local time in a city.",
                serde_json::json!({
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"],
                }),
                ToolKind::ReadOnly,
            ),
            TimeIn,
        )
        .unwrap();
    let registry = Arc::new(registry);
    let engine = Arc::new(ToolEngine::new(
        Arc::clone(&registry),
        SessionId::new(1),
        clock.clone(),
        ToolEngineConfig::default(),
    ));
    (registry, engine)
}

#[derive(Clone)]
struct HybridLlm {
    config: EngineConfig,
    policy: Policy,
    tools: bool,
    calls: Arc<Mutex<Vec<CallHandles>>>,
}

impl HybridLlm {
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
            Policy::Quick => TurnPolicy::fixed(300 * MS),
            // A complete verdict waits for the STT final: in s03 the 450 ms final latency
            // outlasts the 340 ms gap, so the resumed speech continues the turn.
            Policy::Smart(_) => TurnPolicy::Adaptive {
                fallback_silence: Duration::from_secs(3),
                require_final: true,
                final_grace: 800 * MS,
            },
        };
        // The suite's s06 scripts one failing response and then the answer of the NEXT turn: a
        // retry inside the first turn would consume that answer. The retry itself is tested in
        // `llm_runner.rs`.
        config.llm.retries = 0;
        Self {
            config,
            policy,
            tools: false,
            calls: Arc::default(),
        }
    }

    /// The production default for LLM retries (one) instead of the suite accommodation (zero).
    fn with_default_retries(mut self) -> Self {
        self.config.llm.retries = EngineConfig::DEFAULT.llm.retries;
        self
    }

    /// The runner has the `time_in` tool (the suite's s08 calls it).
    fn with_tools(mut self) -> Self {
        self.tools = true;
        self
    }

    fn handles(&self) -> Vec<CallHandles> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl SessionFactory for HybridLlm {
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
        let mut driver = TurnDriver::new(
            TurnDriverConfig::from_engine(&self.config),
            clock.clone(),
            timeline.clone(),
        )
        .with_commands(commands)
        .with_probe(TurnProbe::new(256));
        if let Policy::Smart(probability) = self.policy {
            driver = driver.with_smart_turn(
                Arc::new(FixedModel(probability)),
                Offload::Inline(clock.clone()),
            );
        }

        // The engine's LLM runner, cutting by the reference pipeline's evidence.
        let words = WordLog::default();
        let played = PlayLog::default();
        let mut builder = LlmRunner::builder(
            Arc::new(mock_llm),
            &self.config,
            clock.clone(),
            timeline.clone(),
        )
        .with_truncation(Arc::new(ReferenceSource {
            words: Arc::clone(&words),
            played: Arc::clone(&played),
            clock: clock.clone(),
        }));
        if self.tools {
            let (registry, engine) = time_in_tools(&clock);
            builder = builder.with_tools(registry, engine);
        }
        let llm_stage = builder.build();
        let handles = CallHandles {
            input: Arc::clone(&input),
            driver: driver.stats(),
            stt: stt_stage.stats(),
            llm: llm_stage.stats(),
        };
        let probes = SessionProbes::new(
            llm_stage.context(),
            Arc::clone(&words),
            Arc::clone(&played),
            llm_stage.truncations(),
        );

        // The reference chain behind them.
        let output = OutputLog::new(origin, Some(timeline.clone()));
        let tail = SimTransportOut::from_log(clock.clone(), output.clone())
            .with_consume_delay(transport_delay);
        let tts_stage = TtsStage::new(
            Arc::new(mock_tts),
            output_rate,
            clock.clone(),
            timeline.clone(),
            Arc::clone(&words),
        )
        .with_play_log(Arc::clone(&played));
        let pipeline = Pipeline::new("hybrid-llm")
            .then_with_queue(audio_in, self.config.queues.input)
            .then_with_queue(stt_stage, self.config.queues.user)
            .then_with_queue(driver, self.config.queues.user)
            .then_with_queue(llm_stage, self.config.queues.bot)
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

fn fixed_recommended() -> HybridLlm {
    HybridLlm::new(Policy::Fixed, InterruptionConfig::RECOMMENDED)
}

fn fixed_open() -> HybridLlm {
    HybridLlm::new(Policy::Fixed, InterruptionConfig::OFF)
}

fn smart_recommended() -> HybridLlm {
    HybridLlm::new(Policy::Smart(0.9), InterruptionConfig::RECOMMENDED)
}

fn smart_open() -> HybridLlm {
    HybridLlm::new(Policy::Smart(0.9), InterruptionConfig::OFF)
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
async fn every_scenario_but_s06_passes_with_the_default_retry_configuration() {
    // Gate 3's other tests set `retries = 0` because s06 scripts a failing response followed by the
    // NEXT turn's answer: with the default (one retry) the retry would consume that answer. So a
    // gate-3 pass with `retries = 0` says nothing about the default configuration; this test runs
    // every other scenario with it. s06 itself is covered only with `retries = 0`; the retry is
    // tested in `llm_runner.rs` (`a_failed_request_is_retried_once_before_any_output`).
    use brisvox_sim::suite::scenarios as s;
    let factory = fixed_recommended().with_default_retries();
    let profile = Profile::Budget;
    macro_rules! each {
        ($($name:ident),+ $(,)?) => {$(
            s::$name(&factory, profile).await.unwrap_or_else(|e| {
                panic!("{}: {e}", stringify!($name))
            });
        )+};
    }
    each!(
        s01_happy_path_with_realistic_latency,
        s02_barge_in,
        s03_double_utterance_is_one_turn,
        s04_long_silence_stays_quiet,
        s05_provider_stall_delays_but_completes,
        s07_stt_disconnect_then_reconnect,
        s08_tool_call_with_text,
        s09_back_to_back_turns,
        s10_interrupt_during_tts_first_byte_wait,
        s11_tts_disconnect_mid_answer_then_recovery,
    );
}

#[tokio::test(start_paused = true)]
async fn all_scenarios_pass_with_a_registered_tool() {
    // s08's response calls `time_in`: the runner executes it and asks the model again (the
    // suite scripts one response, so the second round gets the mock's empty fallback answer).
    suite::scenarios::all(&fixed_recommended().with_tools(), Profile::Budget)
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
async fn the_played_text_tests_pass_with_the_engine_runner() {
    // The tests place the barge-in 0.4 s to 7 s after the first utterance: with the 450 ms
    // silence of the other tests a barge-in at 0.4 s would continue the first turn (the caller
    // "resumed"), which is correct but not what these tests are about.
    suite::played_text::all(
        &HybridLlm::new(Policy::Quick, InterruptionConfig::OFF),
        Profile::Budget,
    )
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_tool_turn_with_a_second_script_runs_the_tool_and_speaks_both_rounds() {
    let ms = |n: u64| Duration::from_millis(n);
    let scenario = Scenario::new()
        .silence(ms(100))
        .speak("what time is it in Tokyo", ms(900))
        .with_tail(ms(500));
    let factory = fixed_recommended().with_tools();
    let session = SimSession::new(Arc::new(SimClock::new()));
    session
        .llm
        .push(LlmScript::text(["Let me check."]).with_tool_call(
            "call_1",
            "time_in",
            r#"{"city":"Tokyo"}"#,
        ));
    session
        .llm
        .push(LlmScript::text(["It is nine in the morning."]));
    let run = session
        .run_with(&factory, &scenario, ms(2_500))
        .await
        .unwrap();
    assert_eq!(run.outcome, Ok(()));

    let context = run.probes.context.lock().unwrap().clone();
    context.validate().unwrap();
    let kinds: Vec<&str> = context
        .messages()
        .iter()
        .map(|m| match m {
            Message::System { .. } => "system",
            Message::User { .. } => "user",
            Message::Assistant { tool_calls, .. } if tool_calls.is_empty() => "assistant",
            Message::Assistant { .. } => "assistant+calls",
            Message::Tool { .. } => "tool",
        })
        .collect();
    assert_eq!(kinds, ["user", "assistant+calls", "tool", "assistant"]);
    assert_eq!(context.messages()[3].text(), "It is nine in the morning.");
    // The first round's text was spoken, then the second's, in one epoch of audio.
    let played = run.played_text(brisvox_core::TurnEpoch::ZERO);
    assert!(played.starts_with("Let me check."), "played: {played:?}");
    // The second round is played too: a dropped or never flushed second round would leave only
    // the first (the context assertion above cannot tell: the runner pushes that message either
    // way).
    assert!(
        played.contains("It is nine in the morning."),
        "the second round was not played: {played:?}"
    );
    assert!(
        played.find("Let me check.") < played.find("It is nine in the morning."),
        "rounds play in order: {played:?}"
    );
    // Two timed rounds in the first turn's record.
    assert_eq!(run.turns[0].llm_round_count(), 2);
    for call in factory.handles() {
        let llm = call.llm.snapshot();
        assert_eq!((llm.tool_rounds, llm.rounds), (1, 2), "{llm:?}");
        assert_eq!(llm.validate_failures, 0, "{llm:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn the_engine_turn_driver_and_runner_mark_speech_and_turns_like_the_reference() {
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
                    t.at(TurnEvent::LlmRequest).is_some(),
                    t.at(TurnEvent::LlmFirstToken).is_some(),
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
            let run = if hybrid {
                session
                    .run_with(&fixed_open(), &scenario, ms(2_000))
                    .await
                    .unwrap()
            } else {
                session
                    .run_with(
                        &brisvox_sim::factory::ReferenceFactory,
                        &scenario,
                        ms(2_000),
                    )
                    .await
                    .unwrap()
            };
            (run, session.llm.requests())
        }
    };
    let (reference, _) = run(false).await;
    let (hybrid, hybrid_requests) = run(true).await;
    assert_eq!(hybrid.outcome, Ok(()));
    assert_eq!(marks(&hybrid), marks(&reference));
    assert_eq!(hybrid.turns.len(), reference.turns.len());
    assert_eq!(
        hybrid.probes.history().len(),
        reference.probes.history().len()
    );
    // The barge-in cut the story to words that were played: a non-empty word prefix of it. (The
    // exact cut differs from the reference's by the engine turn driver's own timing.)
    let story = "A very long story that goes on and on.";
    let history = hybrid.probes.history();
    let first = &history[1].content;
    assert!(
        !first.is_empty() && story.starts_with(&**first),
        "cut story: {first:?}"
    );
    assert_eq!(&*history[3].content, "Sure.");
    // And the request of the second turn was built from the cut context: the provider was sent
    // the cut text, never the full story.
    assert_eq!(hybrid_requests.len(), 2);
    let assistant: Vec<&str> = hybrid_requests[1]
        .messages
        .iter()
        .filter(|m| m.role == brisvox_providers::Role::Assistant)
        .map(|m| &*m.content)
        .collect();
    assert_eq!(
        assistant,
        [&**first],
        "the second request carries the cut story"
    );
}

#[tokio::test(start_paused = true)]
async fn the_runner_counts_what_a_barge_in_scenario_does() {
    let factory = fixed_open();
    suite::scenarios::s02_barge_in(&factory, Profile::Budget)
        .await
        .unwrap();
    let handles = factory.handles();
    assert!(!handles.is_empty());
    for call in &handles {
        let driver = call.driver.snapshot();
        assert_eq!(driver.interrupts, 1, "{driver:?}");
        let llm = call.llm.snapshot();
        assert_eq!(llm.turns, 2, "{llm:?}");
        assert!(llm.cut_requests >= 1, "the barge-in was cut: {llm:?}");
        assert_eq!(llm.validate_failures, 0, "{llm:?}");
        assert_eq!(llm.join_timeouts, 0, "{llm:?}");
        assert_eq!(llm.cut_errors, 0, "{llm:?}");
        let stt = call.stt.snapshot();
        assert_eq!(stt.finals_synthetic, 0, "{stt:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn nothing_outlives_a_hybrid_call() {
    let clock = SimClock::new();
    let counter = TaskCounter::start_settled(&clock, 20 * MS).await.unwrap();
    suite::scenarios::s02_barge_in(&smart_recommended(), Profile::Budget)
        .await
        .unwrap();
    suite::scenarios::s08_tool_call_with_text(&smart_recommended().with_tools(), Profile::Budget)
        .await
        .unwrap();
    counter
        .wait_for_baseline(&clock, Duration::from_secs(1))
        .await
        .expect("the pump, a stage task, the round task or a tool task leaked");
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
        let llm = call.llm.snapshot();
        assert_eq!((llm.turns, llm.failed_rounds), (1, 0), "{llm:?}");
    }
}
