//! Speculative generation in a hybrid simulation: the engine's input chain, `SttStage`,
//! `TurnDriver` (offering candidates) and `LlmRunner` (running them) in front of the untouched
//! reference TTS, pacer and output, on a paused clock. A speculative run and a non-speculative run
//! of the same call end with the identical context and the same words on the wire; the
//! speculative one answers sooner. Misses (an interim that lags the final), a caller who resumes,
//! and a tool turn are covered, and the structural scenarios of the shared suite pass with the
//! feature on (with speculations started and committed in the barge-in and tool calls).
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
    LlmStats, Offload, SpeculationConfig, SpeculationStats, SttStage, SttStageConfig,
    TruncationSource, TurnDriver, TurnDriverConfig, TurnDriverStats, VadSetup, VadWorkerConfig,
};
use brisvox_inference::VadParams;
use brisvox_inference::smart_turn::TurnModel;
use brisvox_sim::factory::{BuiltSession, SessionFactory, SessionParts};
use brisvox_sim::input::{InputSink, InputTarget};
use brisvox_sim::latency::LatencyProfile;
use brisvox_sim::mock_llm::LlmScript;
use brisvox_sim::output::OutputLog;
use brisvox_sim::reference::{OutputPacer, PlayLog, SessionProbes, TtsStage, WordLog};
use brisvox_sim::scenario::{Scenario, SimTransportOut};
use brisvox_sim::session::{SessionRun, SimSession};
use brisvox_sim::suite::{self, Profile};
use common::loopback;

/// What the caller heard of a response, from the reference pipeline's evidence (as in the
/// hybrid gate 3).
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

/// A ReadOnly tool that answers at once.
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

/// A Smart Turn model with a fixed answer.
struct FixedModel(f32);

impl TurnModel for FixedModel {
    fn predict(&self, _features: &[f32]) -> brisvox_inference::Result<f32> {
        Ok(self.0)
    }
}

#[derive(Clone)]
struct CallHandles {
    driver: TurnDriverStats,
    llm: LlmStats,
    spec: SpeculationStats,
}

/// The engine's input chain, STT, driver and runner behind the reference output.
#[derive(Clone)]
struct Hybrid {
    config: EngineConfig,
    tools: bool,
    /// The Smart Turn model's answer, for the adaptive policy.
    smart: Option<f32>,
    calls: Arc<Mutex<Vec<CallHandles>>>,
}

impl Hybrid {
    /// Fixed silence of `silence`, every barge-in guard off, speculation as given.
    fn new(silence: Duration, speculation: SpeculationConfig) -> Self {
        let mut config = EngineConfig::DEFAULT;
        config.interruption = InterruptionConfig::OFF;
        config.turn = TurnPolicy::fixed(silence);
        // s06 of the suite scripts a failing response followed by the next turn's answer.
        config.llm.retries = 0;
        config.speculation = speculation;
        Self {
            config,
            tools: false,
            smart: None,
            calls: Arc::default(),
        }
    }

    /// The adaptive policy with a model that always answers `probability` (a complete verdict
    /// waits for the STT final, as in the hybrid gates).
    fn adaptive(probability: f32, speculation: SpeculationConfig) -> Self {
        let mut hybrid = Self::new(ms(450), speculation);
        hybrid.config.turn = TurnPolicy::Adaptive {
            fallback_silence: Duration::from_secs(3),
            require_final: true,
            final_grace: ms(800),
        };
        hybrid.smart = Some(probability);
        hybrid
    }

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

impl SessionFactory for Hybrid {
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
        .with_commands(commands);
        if let Some(probability) = self.smart {
            driver = driver.with_smart_turn(
                Arc::new(FixedModel(probability)),
                Offload::Inline(clock.clone()),
            );
        }
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
            driver: driver.stats(),
            llm: llm_stage.stats(),
            spec: llm_stage.speculation_stats(),
        };
        let probes = SessionProbes::new(
            llm_stage.context(),
            Arc::clone(&words),
            Arc::clone(&played),
            llm_stage.truncations(),
        );
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
        let pipeline = Pipeline::new("hybrid-speculation")
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

const fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn finals_only() -> SpeculationConfig {
    SpeculationConfig {
        on_partial: false,
        ..SpeculationConfig::ENABLED
    }
}

/// A session with realistic provider latencies: the STT final comes 100 ms after the finalize,
/// the LLM answers 400 ms after the request.
fn session(scripts: &[LlmScript]) -> SimSession {
    session_with(scripts, ms(400))
}

/// [`session`] with the LLM's time to first token chosen.
fn session_with(scripts: &[LlmScript], first_token: Duration) -> SimSession {
    let session = SimSession::new(Arc::new(SimClock::new()));
    let stt = session.stt.clone().with_latency(
        LatencyProfile::ZERO,
        LatencyProfile::Fixed(ms(100)),
        LatencyProfile::Fixed(ms(100)),
        1,
    );
    drop(stt);
    let llm = session.llm.clone().with_latency(
        LatencyProfile::Fixed(first_token),
        LatencyProfile::Fixed(ms(10)),
        1,
    );
    drop(llm);
    for script in scripts {
        session.llm.push(script.clone());
    }
    session
}

fn answer() -> LlmScript {
    LlmScript::text(["It is sunny", " today."])
}

fn weather() -> Scenario {
    Scenario::new()
        .silence(ms(100))
        .speak("what is the weather like today", ms(900))
        .with_tail(ms(500))
}

/// What a run answered: the context and the response latency of the first turn.
struct Outcome {
    messages: Vec<Message>,
    response_latency: Duration,
    requests: usize,
    first_audio: Duration,
}

async fn run(factory: &Hybrid, session: &SimSession, scenario: &Scenario) -> (SessionRun, Outcome) {
    let run = session
        .run_with(factory, scenario, ms(6_000))
        .await
        .unwrap();
    assert_eq!(run.outcome, Ok(()));
    let messages = run.probes.context.lock().unwrap().messages().to_vec();
    let latency = run.turns[0].metrics().response_latency.unwrap();
    let first_audio = run.turns[0].at(TurnEvent::FirstAudioOut).unwrap() - run.origin;
    let outcome = Outcome {
        messages,
        response_latency: latency,
        requests: session.llm.requests().len(),
        first_audio,
    };
    (run, outcome)
}

#[tokio::test(start_paused = true)]
async fn speculation_answers_sooner_and_leaves_the_same_context() {
    let scenario = weather();
    let plain = Hybrid::new(ms(700), SpeculationConfig::OFF);
    let (plain_run, plain_out) = run(&plain, &session(&[answer()]), &scenario).await;
    let speculative = Hybrid::new(ms(700), finals_only());
    let (spec_run, spec_out) = run(&speculative, &session(&[answer()]), &scenario).await;

    // The same conversation, word for word, and the same words reached the caller.
    assert_eq!(spec_out.messages, plain_out.messages);
    assert_eq!(spec_out.messages.len(), 2);
    assert_eq!(spec_out.messages[1].text(), "It is sunny today.");
    assert_eq!(
        spec_run.played_text(brisvox_core::TurnEpoch::ZERO),
        plain_run.played_text(brisvox_core::TurnEpoch::ZERO)
    );
    // One request in both: the speculation WAS the request.
    assert_eq!((plain_out.requests, spec_out.requests), (1, 1));

    // And it is faster: the LLM started when the final arrived (400 ms after the end of speech)
    // instead of when the turn ended (700 ms), so its 400 ms of thinking mostly happened while the
    // machine was still waiting.
    let gain = plain_out.response_latency - spec_out.response_latency;
    assert!(
        gain >= ms(250),
        "plain {:?}, speculative {:?}",
        plain_out.response_latency,
        spec_out.response_latency
    );
    assert!(spec_out.first_audio < plain_out.first_audio);

    // The metrics say what happened.
    let handles = speculative.handles();
    assert_eq!(handles.len(), 1);
    let spec = handles[0].spec.snapshot();
    assert_eq!(
        (spec.started, spec.hits, spec.discarded()),
        (1, 1, 0),
        "{spec:?}"
    );
    assert_eq!(spec.hit_rate(), Some(1.0));
    assert_eq!(spec.wasted_tokens(), 0);
    assert!(spec.head_start_ms >= 250, "{spec:?}");
    let driver = handles[0].driver.snapshot();
    assert_eq!(
        (driver.speculations_offered, driver.speculations_withdrawn),
        (1, 0)
    );
    let llm = handles[0].llm.snapshot();
    assert_eq!((llm.turns, llm.rounds, llm.requests), (1, 1, 1), "{llm:?}");
    // The plain run never speculated.
    assert_eq!(plain.handles()[0].spec.snapshot().started, 0);
    assert_eq!(plain.handles()[0].driver.snapshot().speculations_offered, 0);
}

#[tokio::test(start_paused = true)]
async fn a_lagging_interim_costs_a_wasted_request_but_not_the_result() {
    // The mock STT's interims lag the final by a word, so the partial candidate at the VAD stop
    // is wrong: it is replaced at the final. The conversation must still be the plain one.
    let scenario = weather();
    let plain = Hybrid::new(ms(700), SpeculationConfig::OFF);
    let (_, plain_out) = run(&plain, &session_with(&[answer()], ms(50)), &scenario).await;
    let speculative = Hybrid::new(ms(700), SpeculationConfig::ENABLED);
    // A fast model: the partial run has answered before the final replaces it.
    let (_, spec_out) = run(
        &speculative,
        &session_with(&[answer(), answer()], ms(50)),
        &scenario,
    )
    .await;

    assert_eq!(spec_out.messages, plain_out.messages);
    assert_eq!(
        spec_out.requests, 2,
        "the partial run and the run of the final"
    );
    let spec = speculative.handles()[0].spec.snapshot();
    assert_eq!(
        (spec.started, spec.superseded, spec.hits),
        (2, 1, 1),
        "{spec:?}"
    );
    assert_eq!(spec.hit_rate(), Some(0.5));
    assert!(spec.wasted_tokens() > 0, "{spec:?}");
    // The answer was ready before the turn ended, so it is released the moment the turn ends:
    // the whole 50 ms of the model is saved (a slower model would save more).
    assert!(
        plain_out.response_latency - spec_out.response_latency >= ms(40),
        "plain {:?}, speculative {:?}",
        plain_out.response_latency,
        spec_out.response_latency
    );
    let driver = speculative.handles()[0].driver.snapshot();
    assert_eq!(driver.speculations_offered, 2, "{driver:?}");
}

#[tokio::test(start_paused = true)]
async fn a_caller_who_goes_on_speaking_is_answered_once_after_everything_they_said() {
    // Two stretches of speech 600 ms apart: one turn (the fixed silence is 700 ms). The VAD stops
    // 300 ms into the gap and the STT final of the first stretch arrives 100 ms later, so the
    // driver offers "book a table" as a candidate (the runner starts it) 200 ms before the
    // second stretch begins and withdraws it there.
    let scenario = Scenario::new()
        .silence(ms(100))
        .speak("book a table", ms(700))
        .silence(ms(600))
        .speak("for four people", ms(700))
        .with_tail(ms(500));
    let plain = Hybrid::new(ms(700), SpeculationConfig::OFF);
    let (_, plain_out) = run(&plain, &session(&[answer()]), &scenario).await;
    let speculative = Hybrid::new(ms(700), finals_only());
    let (_, spec_out) = run(&speculative, &session(&[answer(), answer()]), &scenario).await;

    assert_eq!(spec_out.messages, plain_out.messages);
    assert_eq!(spec_out.messages[0].text(), "book a table for four people");
    let handles = speculative.handles();
    let spec = handles[0].spec.snapshot();
    let driver = handles[0].driver.snapshot();
    // The first stretch produced a candidate that the second one withdrew, and the whole
    // utterance was answered by one speculation that hit.
    assert_eq!(
        (driver.speculations_offered, driver.speculations_withdrawn),
        (2, 1),
        "{driver:?}"
    );
    assert_eq!(
        (spec.started, spec.withdrawn, spec.hits),
        (2, 1, 1),
        "{spec:?}"
    );
    assert_eq!(spec.discarded(), 1, "{spec:?}");
    assert_eq!(handles[0].llm.snapshot().turns, 1);
    assert_eq!(
        spec_out.requests, 2,
        "the withdrawn run and the run that hit"
    );
    assert!(
        spec_out.response_latency < plain_out.response_latency,
        "plain {:?}, speculative {:?}",
        plain_out.response_latency,
        spec_out.response_latency
    );
}

#[tokio::test(start_paused = true)]
async fn a_tool_turn_has_the_same_context_with_and_without_speculation() {
    let scenario = Scenario::new()
        .silence(ms(100))
        .speak("what time is it in Tokyo", ms(900))
        .with_tail(ms(500));
    let scripts = [
        LlmScript::text(["Let me check."]).with_tool_call(
            "call_1",
            "time_in",
            r#"{"city":"Tokyo"}"#,
        ),
        LlmScript::text(["It is nine in the morning."]),
    ];
    let plain = Hybrid::new(ms(700), SpeculationConfig::OFF).with_tools();
    let (_, plain_out) = run(&plain, &session(&scripts), &scenario).await;
    let speculative = Hybrid::new(ms(700), finals_only()).with_tools();
    let (_, spec_out) = run(&speculative, &session(&scripts), &scenario).await;

    assert_eq!(spec_out.messages, plain_out.messages);
    assert_eq!(
        spec_out.messages.len(),
        4,
        "user, assistant+calls, tool, assistant: {:?}",
        spec_out.messages
    );
    assert_eq!((plain_out.requests, spec_out.requests), (2, 2));
    let spec = speculative.handles()[0].spec.snapshot();
    assert_eq!((spec.hits, spec.tool_phases), (1, 1), "{spec:?}");
    assert!(
        plain_out.response_latency > spec_out.response_latency,
        "plain {:?}, speculative {:?}",
        plain_out.response_latency,
        spec_out.response_latency
    );
    let llm = speculative.handles()[0].llm.snapshot();
    assert_eq!(
        (llm.tool_rounds, llm.rounds, llm.validate_failures),
        (1, 2, 0),
        "{llm:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_structural_scenarios_pass_with_speculation_on() {
    // Barge-in (the cut context is the played prefix), back-to-back turns, a stalled provider,
    // an STT disconnect, a tool turn and the rest of the suite's structural invariants hold with
    // candidates flying. The suite scripts ONE response per turn, so a speculation that is
    // discarded would consume the script of the answer: that is why only the candidates at the
    // final transcript are on here (the mock's interims lag the final by a word, which makes the
    // partial candidate wrong every time) and why s03 (resumption) and s06 (a scripted failure)
    // are left to the dedicated tests above and in `speculation_runner.rs`.
    use suite::scenarios as s;
    let factory = Hybrid::new(ms(450), finals_only()).with_tools();
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
        s04_long_silence_stays_quiet,
        s05_provider_stall_delays_but_completes,
        s07_stt_disconnect_then_reconnect,
        s08_tool_call_with_text,
        s09_back_to_back_turns,
        s10_interrupt_during_tts_first_byte_wait,
        s11_tts_disconnect_mid_answer_then_recovery,
    );
    let handles = factory.handles();
    assert!(!handles.is_empty());
    let offered: u64 = handles
        .iter()
        .map(|h| h.driver.snapshot().speculations_offered)
        .sum();
    let started: u64 = handles.iter().map(|h| h.spec.snapshot().started).sum();
    let hits: u64 = handles.iter().map(|h| h.spec.snapshot().hits).sum();
    assert!(
        offered > 0 && started > 0 && hits > 0,
        "the feature was exercised: offered {offered}, started {started}, hits {hits}"
    );
    // A call with a barge-in whose speculation was committed (s02, s10 and the like), and a call
    // with a tool phase whose speculation was committed (s08).
    assert!(
        handles
            .iter()
            .any(|h| h.driver.snapshot().interrupts > 0 && h.spec.snapshot().hits > 0),
        "no committed speculation in a call with a barge-in"
    );
    assert!(
        handles.iter().any(|h| {
            let spec = h.spec.snapshot();
            spec.tool_phases > 0 && spec.hits > 0
        }),
        "no committed speculation in a tool turn"
    );
    for call in &handles {
        let llm = call.llm.snapshot();
        assert_eq!(llm.validate_failures, 0, "{llm:?}");
        assert_eq!(llm.join_timeouts, 0, "{llm:?}");
        // No speculation is lost track of: each one ended as a hit or as a counted discard.
        let spec = call.spec.snapshot();
        assert!(spec.hits + spec.discarded() <= spec.started, "{spec:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn with_the_adaptive_policy_the_context_is_the_same_and_nothing_is_slower() {
    // A complete Smart Turn verdict ends the turn when the final arrives, so a candidate at the
    // final adds nothing (the turn ends in the same call and no candidate is offered). The
    // partial candidate at the stop is offered, but the mock STT's last interim lags the final by
    // a word ("what is the weather like" against "... today"), so it is a miss: one wasted
    // request, the same conversation, the same latency. (A hit on a partial needs an interim
    // equal to the final, which the mock never produces; the runner tests cover that path.)
    let scenario = weather();
    let plain = Hybrid::adaptive(0.9, SpeculationConfig::OFF);
    let (_, plain_out) = run(&plain, &session_with(&[answer()], ms(300)), &scenario).await;
    assert_eq!(plain_out.requests, 1);
    for speculation in [finals_only(), SpeculationConfig::ENABLED] {
        let speculative = Hybrid::adaptive(0.9, speculation);
        let (_, spec_out) = run(
            &speculative,
            &session_with(&[answer(), answer(), answer()], ms(300)),
            &scenario,
        )
        .await;
        assert_eq!(spec_out.messages, plain_out.messages, "{speculation:?}");
        let handles = speculative.handles();
        let spec = handles[0].spec.snapshot();
        let driver = handles[0].driver.snapshot();
        assert_eq!(handles[0].llm.snapshot().turns, 1);
        assert_eq!(handles[0].llm.snapshot().validate_failures, 0);
        if speculation.on_partial {
            // The lagging partial candidate was offered, started and missed at the final.
            assert_eq!(driver.speculations_offered, 1, "{driver:?}");
            assert_eq!(
                (spec.started, spec.hits, spec.misses, spec.superseded),
                (1, 0, 1, 0),
                "{spec:?}"
            );
            assert_eq!(spec.discarded(), 1, "{spec:?}");
            assert_eq!(
                spec_out.requests, 2,
                "the wasted partial run and the real one"
            );
        } else {
            assert_eq!(
                (driver.speculations_offered, spec.started),
                (0, 0),
                "the turn ended at the final: no candidate: {spec:?} {driver:?}"
            );
            assert_eq!(spec_out.requests, 1);
        }
        // The wasted request costs money, not time: nothing is slower than without the feature.
        assert_eq!(
            spec_out.response_latency, plain_out.response_latency,
            "{speculation:?}"
        );
    }
}
