//! `SimSession`: a whole simulated call (scripted caller -> pipeline under test over mock
//! providers -> recording output) on the paused clock.
//!
//! The pipeline under test comes from a [`SessionFactory`]; [`SimSession::run`] uses the
//! [`ReferenceFactory`] chain, [`SimSession::run_with`] any other.

use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{Links, SharedClock, TimelineRecorder, TurnRecord};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::factory::{DRAIN_TIMEOUT, ReferenceFactory, SessionFactory, SessionParts};
use crate::fixture::Fixture;
use crate::mock_llm::MockLlm;
use crate::mock_stt::MockStt;
use crate::mock_tts::MockTts;
use crate::output::{OutputRecord, WireRecord};
use crate::reference::SessionProbes;
use crate::scenario::{Scenario, play_input};
use crate::vad::VadConfig;

/// Input audio rate of simulated calls.
pub const INPUT_RATE: SampleRate = SampleRate::Hz16000;
/// Output audio rate of simulated calls.
pub const OUTPUT_RATE: SampleRate = SampleRate::Hz24000;

/// A simulated call's configuration: the mocks (configure them before `run`) and the VAD.
#[derive(Debug, Clone)]
pub struct SimSession {
    clock: SharedClock,
    /// Mock STT (its utterances are filled from the scenario by `run`).
    pub stt: MockStt,
    /// Mock LLM (queue one script per expected response).
    pub llm: MockLlm,
    /// Mock TTS.
    pub tts: MockTts,
    /// VAD settings.
    pub vad: VadConfig,
    /// Time the output transport takes per frame (a slow consumer, e.g. a congested socket);
    /// zero by default.
    pub transport_delay: Duration,
}

/// The caller's side of a run.
#[derive(Debug, Clone, Copy)]
pub enum CallerInput<'a> {
    /// A synthetic script.
    Scenario(&'a Scenario),
    /// Real speech from a fixture (its transcripts script the mock STT).
    Fixture(&'a Fixture),
}

impl<'a> From<&'a Scenario> for CallerInput<'a> {
    fn from(scenario: &'a Scenario) -> Self {
        Self::Scenario(scenario)
    }
}

impl<'a> From<&'a Fixture> for CallerInput<'a> {
    fn from(fixture: &'a Fixture) -> Self {
        Self::Fixture(fixture)
    }
}

/// What happened in a finished simulated call.
#[derive(Debug)]
pub struct SessionRun {
    /// When the call started (scenario time zero).
    pub origin: Instant,
    /// Every frame that reached the pipeline's output tap (pipeline-sourced; interrupts are
    /// counted here).
    pub output: Vec<OutputRecord>,
    /// Finished turns' timelines.
    pub turns: Vec<TurnRecord>,
    /// Conversation and word logs.
    pub probes: SessionProbes,
    /// The pipeline's outcome (`Ok` unless a stage failed).
    pub outcome: brisvox_core::Result<()>,
    /// Transport frames not recorded because the recorder's limit was reached.
    pub dropped_output: usize,
    /// What a client on the wire received, with arrival times (empty for pipelines without a
    /// wire, like the reference one).
    pub wire: Vec<WireRecord>,
}

impl SessionRun {
    /// A run from its parts (a run without wire records; set [`Self::wire`] for those).
    pub fn new(
        origin: Instant,
        output: Vec<OutputRecord>,
        turns: Vec<TurnRecord>,
        probes: SessionProbes,
        outcome: brisvox_core::Result<()>,
        dropped_output: usize,
    ) -> Self {
        Self {
            origin,
            output,
            turns,
            probes,
            outcome,
            dropped_output,
            wire: Vec::new(),
        }
    }
}

impl SimSession {
    /// Zero-latency mocks on `clock` (use the paused-time `SimClock`).
    pub fn new(clock: SharedClock) -> Self {
        Self {
            stt: MockStt::new(clock.clone()),
            llm: MockLlm::new(clock.clone()),
            tts: MockTts::new(clock.clone()),
            vad: VadConfig::default(),
            transport_delay: Duration::ZERO,
            clock,
        }
    }

    /// The call's clock.
    pub fn clock(&self) -> SharedClock {
        self.clock.clone()
    }

    /// Plays `scenario`, keeps the call open `linger` longer for the bot to finish, then stops.
    ///
    /// The scenario's utterances are queued on the (shared) mock STT: use a fresh `SimSession`
    /// per run.
    ///
    /// # Errors
    /// Only if the pipeline cannot be started.
    pub async fn run(&self, scenario: &Scenario, linger: Duration) -> Result<SessionRun> {
        self.run_with(&ReferenceFactory, scenario, linger).await
    }

    /// Like [`Self::run`], with real speech from a fixture as the caller (its transcripts
    /// script the mock STT).
    ///
    /// # Errors
    /// Only if the pipeline cannot be started.
    pub async fn run_fixture(&self, fixture: &Fixture, linger: Duration) -> Result<SessionRun> {
        self.run_with(&ReferenceFactory, fixture, linger).await
    }

    /// Like [`Self::run`] / [`Self::run_fixture`] (`input` is a `&Scenario` or a `&Fixture`),
    /// against the pipeline `factory` builds.
    ///
    /// The runner builds the session, starts its pipeline, binds the input sink to the pipeline's
    /// input, plays the caller, waits `linger`, then stops the pipeline (30 s bound) and collects
    /// the run.
    ///
    /// # Errors
    /// If the factory cannot build the session or the pipeline cannot be started.
    pub async fn run_with<'a, F: SessionFactory>(
        &self,
        factory: &F,
        input: impl Into<CallerInput<'a>>,
        linger: Duration,
    ) -> Result<SessionRun> {
        let input = input.into();
        let utterances = match input {
            CallerInput::Scenario(scenario) => scenario.utterances(),
            CallerInput::Fixture(fixture) => fixture.utterances(),
        };
        for utterance in utterances {
            self.stt.push(utterance);
        }
        let clock = self.clock.clone();
        let timeline = TimelineRecorder::new();
        let origin = clock.now();
        let mut built = factory
            .build(SessionParts {
                clock: clock.clone(),
                stt: self.stt.clone(),
                llm: self.llm.clone(),
                tts: self.tts.clone(),
                vad: self.vad,
                transport_delay: self.transport_delay,
                timeline: timeline.clone(),
                origin,
                input_rate: INPUT_RATE,
                output_rate: OUTPUT_RATE,
            })
            .await?;
        let guard = built.take_guard();
        let drain = built.take_drain();
        let cancel = CancellationToken::new();
        let mut running = built.pipeline.start(Links::default(), &cancel).await?;
        built.input.bind(running.input().clone());
        built.input.bind_upstream(running.upstream_input().clone());
        match input {
            CallerInput::Scenario(scenario) => {
                play_input(
                    scenario,
                    clock.clone(),
                    origin,
                    INPUT_RATE,
                    built.input.clone(),
                    &cancel,
                )
                .await?;
            }
            CallerInput::Fixture(fixture) => {
                fixture
                    .play(clock.clone(), origin, built.input.clone(), &cancel)
                    .await?;
            }
        }
        // The script is over: the caller no longer holds the pipeline's input open.
        built.input.unbind();
        clock.sleep(linger).await;
        let outcome = running.stop(Duration::from_secs(30)).await;
        // The wire records come from the guard's reader task: let it see the last events, read
        // the logs, and only then drop the guard (dropping it first would abort the reader and
        // lose them).
        if let Some(drain) = drain {
            tokio::select! {
                () = drain => {}
                () = clock.sleep(DRAIN_TIMEOUT) => {}
            }
        }
        timeline.finish();
        let mut run = SessionRun::new(
            origin,
            built.output.records(),
            timeline.history(),
            built.probes,
            outcome,
            built.output.dropped_records(),
        );
        run.wire = built.output.wire_records();
        drop(guard);
        Ok(run)
    }
}
