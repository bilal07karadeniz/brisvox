//! Hybrid gate 1 (`docs/PIPELINE.md` section 9): the engine's input chain (transport
//! pump, `AudioIn`, VAD worker) feeding the untouched reference STT, turn manager, LLM, TTS,
//! pacer and output. The scripted caller's audio goes through a real `TransportSession` over the
//! loopback transport and the pump, exactly as in production; the shared suite runs under the
//! Budget profile on a paused clock.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_core::frame::AudioFrame;
use brisvox_core::{FrameSender, Pipeline, SharedClock, SimClock, TurnEvent};
use brisvox_engine::{
    AudioIn, EnergyModel, EngineConfig, Offload, PlayoutBook, Pump, PumpConfig, PumpHandle,
    PumpLinks, VadSetup, VadWorkerConfig,
};
use brisvox_inference::VadParams;
use brisvox_sim::factory::{BuiltSession, ReferenceFactory, SessionFactory, SessionParts};
use brisvox_sim::input::{InputSink, InputTarget, SendFuture};
use brisvox_sim::leak::TaskCounter;
use brisvox_sim::mock_llm::LlmScript;
use brisvox_sim::output::OutputLog;
use brisvox_sim::reference::{
    LlmStage, OutputPacer, PlayLog, SessionProbes, SttStage, TtsStage, TurnManager, WordLog,
};
use brisvox_sim::scenario::{Scenario, SimTransportOut};
use brisvox_sim::session::SimSession;
use brisvox_sim::suite::{self, Profile};
use brisvox_transport::{
    LoopbackClient, LoopbackConfig, LoopbackTransport, MarkAcks, SessionConfig, Transport,
    TransportSession,
};
use common::MS;
use tokio_util::sync::{CancellationToken, DropGuard};

/// The caller as the sim sees it: audio frames in, through a loopback client, the transport
/// session and the pump, into the pipeline's first stage. The pump starts when the runner binds
/// the sink to the started pipeline.
struct LoopbackInput {
    client: tokio::sync::Mutex<LoopbackClient>,
    session: Mutex<Option<TransportSession>>,
    pump: Mutex<Option<PumpHandle>>,
    book: Arc<PlayoutBook>,
    clock: SharedClock,
    config: PumpConfig,
    cancel: CancellationToken,
    _stop_on_drop: DropGuard,
}

impl InputTarget for LoopbackInput {
    fn send(&self, frame: AudioFrame) -> SendFuture<'_> {
        Box::pin(async move {
            let mut client = self.client.lock().await;
            client
                .send_audio(frame.data().clone(), frame.sample_rate())
                .await
                .is_ok()
        })
    }

    fn bind(&self, input: &FrameSender) {
        let session = self
            .session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(session) = session else { return };
        let handle = Pump::spawn(
            session,
            PumpLinks {
                input: input.clone(),
                // The reference output has no transport marks to echo.
                upstream: None,
            },
            Arc::clone(&self.book),
            self.clock.clone(),
            self.config,
            &self.cancel,
        );
        *self.pump.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
    }
}

/// `AudioIn` + VAD worker (energy model, inline offload) behind a real pump, then the reference
/// chain.
#[derive(Clone)]
struct HybridInput {
    config: EngineConfig,
    /// Every call's input, for the tests that look at the pump afterwards.
    inputs: Arc<Mutex<Vec<Arc<LoopbackInput>>>>,
}

impl HybridInput {
    fn new() -> Self {
        Self {
            config: EngineConfig::DEFAULT,
            inputs: Arc::default(),
        }
    }
}

impl SessionFactory for HybridInput {
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
            input_rate,
            output_rate,
        } = parts;
        let setup = |what: &str, e: &dyn std::fmt::Display| {
            brisvox_sim::Error::Assertion(format!("hybrid setup: {what}: {e}"))
        };

        // The caller's side: loopback client -> transport session -> pump (started at bind).
        let (mut transport, client) = LoopbackTransport::new(
            clock.clone(),
            LoopbackConfig {
                latency: Duration::ZERO,
                mark_acks: MarkAcks::Auto {
                    delay: Duration::ZERO,
                },
                ..LoopbackConfig::default()
            },
        );
        let cancel = CancellationToken::new();
        let link = transport
            .accept(&cancel)
            .await
            .map_err(|e| setup("accept", &e))?
            .ok_or_else(|| setup("accept", &"no call"))?;
        let session = TransportSession::open(
            clock.clone(),
            link,
            SessionConfig {
                position_interval: 20 * MS,
                ..SessionConfig::default()
            },
            &cancel,
        )
        .await
        .map_err(|e| setup("open", &e))?;
        let input = Arc::new(LoopbackInput {
            client: tokio::sync::Mutex::new(client),
            session: Mutex::new(Some(session)),
            pump: Mutex::new(None),
            book: PlayoutBook::shared(16),
            clock: clock.clone(),
            config: PumpConfig::from_engine(&self.config),
            cancel: cancel.clone(),
            _stop_on_drop: cancel.drop_guard(),
        });

        // The engine's input chain: the VAD gets the simulator's thresholds, so it hears speech
        // exactly where the reference energy VAD does.
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

        // The reference chain behind it.
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
        let pipeline = Pipeline::new("hybrid-input")
            .then_with_queue(audio_in, self.config.queues.input)
            .then(SttStage::new(Arc::new(stt), input_rate, clock.clone()))
            .then(TurnManager::new(clock.clone(), timeline))
            .then(llm_stage)
            .then(tts_stage)
            .then(OutputPacer::new(clock, played))
            .then(tail);
        self.inputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::clone(&input));
        Ok(BuiltSession::new(
            pipeline,
            probes,
            InputSink::target(Arc::clone(&input) as Arc<dyn InputTarget>),
            output,
        )
        .with_guard(input))
    }
}

#[tokio::test(start_paused = true)]
async fn s01_happy_path_with_realistic_latency() {
    suite::scenarios::s01_happy_path_with_realistic_latency(&HybridInput::new(), Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn s02_barge_in() {
    suite::scenarios::s02_barge_in(&HybridInput::new(), Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn s03_double_utterance_is_one_turn() {
    suite::scenarios::s03_double_utterance_is_one_turn(&HybridInput::new(), Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn s04_long_silence_stays_quiet() {
    suite::scenarios::s04_long_silence_stays_quiet(&HybridInput::new(), Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn s05_provider_stall_delays_but_completes() {
    suite::scenarios::s05_provider_stall_delays_but_completes(&HybridInput::new(), Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_basic_suite_passes_with_the_engine_input_chain() {
    suite::basic::all(&HybridInput::new(), Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn real_speech_is_answered_after_each_utterance() {
    suite::fixtures::a_real_speech_call_is_answered_after_each_utterance(
        &HybridInput::new(),
        Profile::Budget,
    )
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_engine_vad_marks_speech_exactly_where_the_reference_vad_does() {
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
                    .run_with(&HybridInput::new(), &scenario, ms(2_000))
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
    assert!(
        marks(&hybrid)
            .iter()
            .all(|(s, e)| s.is_some() && e.is_some()),
        "{:?}",
        marks(&hybrid)
    );
    // The same turns, the same conversation.
    assert_eq!(hybrid.turns.len(), reference.turns.len());
    assert_eq!(
        hybrid.probes.history().len(),
        reference.probes.history().len()
    );
}

#[tokio::test(start_paused = true)]
async fn nothing_outlives_a_hybrid_call() {
    let clock = SimClock::new();
    let counter = TaskCounter::start_settled(&clock, 20 * MS).await.unwrap();
    suite::scenarios::s01_happy_path_with_realistic_latency(&HybridInput::new(), Profile::Budget)
        .await
        .unwrap();
    counter
        .wait_for_baseline(&clock, Duration::from_secs(1))
        .await
        .expect("the pump, the transport session, the loopback or the VAD worker leaked");
}

#[tokio::test(start_paused = true)]
async fn the_caller_audio_really_travels_through_the_transport_and_the_pump() {
    let factory = HybridInput::new();
    suite::scenarios::s02_barge_in(&factory, Profile::Budget)
        .await
        .unwrap();
    let inputs = factory.inputs.lock().unwrap();
    assert!(!inputs.is_empty());
    for input in inputs.iter() {
        let pump = input.pump.lock().unwrap();
        let handle = pump.as_ref().expect("the sink was bound: the pump started");
        let stats = handle.stats();
        // A scenario of over 3 s is 150+ frames of 20 ms.
        assert!(stats.audio > 150, "{stats:?}");
        assert_eq!(stats.audio_discarded, 0, "{stats:?}");
        assert_eq!(stats.hangups, 0, "the call was never hung up: {stats:?}");
        assert!(handle.started(), "Started was read");
    }
}

#[tokio::test(start_paused = true)]
async fn the_rest_of_the_scenarios_also_pass_through_the_input_chain() {
    suite::scenarios::all(&HybridInput::new(), Profile::Budget)
        .await
        .unwrap();
}
