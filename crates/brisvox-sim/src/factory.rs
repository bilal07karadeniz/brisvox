//! The seam between the simulator and the pipeline under test.
//!
//! A [`SessionFactory`] turns a [`SessionParts`] (the mock providers, the clock, the timeline and
//! the call's rates) into a [`BuiltSession`]: the pipeline to start, the probes to read the
//! conversation back, the [`InputSink`] the scripted caller feeds, and the [`OutputLog`] that
//! records what the bot sent. [`ReferenceFactory`] builds exactly the reference chain of
//! [`crate::reference`]; the production engine implements the trait for its own chain, and the
//! whole suite in [`crate::suite`] then runs against it unchanged.
//!
//! [`SimSession::run_with`](crate::session::SimSession::run_with) is the runner: it builds,
//! starts the pipeline, binds the input, plays the caller, lingers, stops and collects a
//! [`SessionRun`](crate::session::SessionRun).

use std::any::Any;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{Pipeline, SharedClock, TimelineRecorder};
use tokio::time::Instant;

use crate::error::Result;
use crate::input::InputSink;
use crate::mock_llm::MockLlm;
use crate::mock_stt::MockStt;
use crate::mock_tts::MockTts;
use crate::output::OutputLog;
use crate::reference::{
    LlmStage, OutputPacer, PlayLog, SessionProbes, SttStage, TtsStage, TurnManager, WordLog,
};
use crate::vad::{EnergyVad, VadConfig};

/// Everything a factory needs to build one simulated call's pipeline.
///
/// The mocks are shared handles: the factory builds its providers from clones, the scenario's
/// utterances were already queued on `stt`, and tests keep reading counters from their own
/// copies.
#[derive(Debug, Clone)]
pub struct SessionParts {
    /// The call's clock (the paused-time `SimClock` in simulations).
    pub clock: SharedClock,
    /// Mock STT, already scripted with the caller's utterances.
    pub stt: MockStt,
    /// Mock LLM, scripted by the test.
    pub llm: MockLlm,
    /// Mock TTS.
    pub tts: MockTts,
    /// VAD settings (for pipelines that use the simulator's energy VAD).
    pub vad: VadConfig,
    /// Time the output transport takes per frame (a slow consumer); zero normally. A factory
    /// whose output goes through a real transport slows its client's consumption instead.
    pub transport_delay: Duration,
    /// The call's timeline recorder; the pipeline marks turn events on it.
    pub timeline: TimelineRecorder,
    /// The call's start (scenario time zero): times in the output log are relative to it.
    pub origin: Instant,
    /// Sample rate of the caller's audio.
    pub input_rate: SampleRate,
    /// Sample rate of the bot's audio.
    pub output_rate: SampleRate,
}

/// The future of a [`BuiltSession::with_drain`] hook.
pub type DrainFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// How long the runner waits for a drain hook (simulated time).
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

type DrainHook = Box<dyn FnOnce() -> DrainFuture + Send>;

/// A simulated call, built and ready to start.
pub struct BuiltSession {
    /// The pipeline to start (with default links).
    pub pipeline: Pipeline,
    /// Handles to read the conversation, word log and playout back after the run.
    pub probes: SessionProbes,
    /// Where the scripted caller's audio goes. A pipeline-input sink
    /// ([`InputSink::pipeline`]) is bound by the runner right after the start.
    pub input: InputSink,
    /// What the bot sent back (a recording tap on the pipeline's output, plus wire arrivals
    /// for factories that have a wire).
    pub output: OutputLog,
    /// Resources that must live exactly as long as the call (a loopback client and its reader
    /// task, for example). The runner drops them after the pipeline has stopped, the drain hook
    /// has run and the output log has been read; they are also dropped if the run's future is.
    guard: Option<Box<dyn Any + Send>>,
    /// Runs after the pipeline stopped and before the logs are read.
    drain: Option<DrainHook>,
}

impl BuiltSession {
    /// A built session without guarded resources.
    pub fn new(
        pipeline: Pipeline,
        probes: SessionProbes,
        input: InputSink,
        output: OutputLog,
    ) -> Self {
        Self {
            pipeline,
            probes,
            input,
            output,
            guard: None,
            drain: None,
        }
    }

    /// Keeps `guard` alive until the run is over: the runner drops it after the pipeline
    /// stopped, the [drain hook](Self::with_drain) completed and the output log was read.
    #[must_use]
    pub fn with_guard<T: Send + 'static>(mut self, guard: T) -> Self {
        self.guard = Some(Box::new(guard));
        self
    }

    /// A hook the runner awaits (at most [`DRAIN_TIMEOUT`]) after the pipeline has stopped and
    /// before it reads the output log and drops the guard. A factory whose wire records come from
    /// a client reader task uses it to let the reader see the last events (the final clear and
    /// the hang-up) before the records are taken.
    #[must_use]
    pub fn with_drain<Fut>(mut self, drain: impl FnOnce() -> Fut + Send + 'static) -> Self
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.drain = Some(Box::new(move || Box::pin(drain())));
        self
    }

    /// Takes the drain hook out of the session (the runner runs it once).
    pub fn take_drain(&mut self) -> Option<DrainFuture> {
        self.drain.take().map(|hook| hook())
    }

    /// Takes the guard out of the session (the runner holds it to the end of the call).
    pub fn take_guard(&mut self) -> Option<Box<dyn Any + Send>> {
        self.guard.take()
    }
}

impl fmt::Debug for BuiltSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuiltSession")
            .field("input", &self.input)
            .field("output", &self.output)
            .field("guard", &self.guard.is_some())
            .field("drain", &self.drain.is_some())
            .finish_non_exhaustive()
    }
}

/// Builds the pipeline under test for one simulated call.
///
/// The factory is cloned into concurrent calls (chaos runs build a hundred sessions at once), so
/// it is a cheap, shareable value: put heavy shared state behind an `Arc`.
pub trait SessionFactory: Clone + Send + Sync + 'static {
    /// Builds one call's session from `parts`. Must not advance time: the runner measures the
    /// call from `parts.origin` and starts the caller right after the pipeline has started.
    ///
    /// # Errors
    /// If the pipeline cannot be assembled (a failing setup, never a test expectation).
    fn build(&self, parts: SessionParts) -> impl Future<Output = Result<BuiltSession>> + Send;
}

/// Builds the reference chain of [`crate::reference`], exactly:
///
/// ```text
/// EnergyVad -> SttStage -> TurnManager -> LlmStage -> TtsStage -> OutputPacer -> SimTransportOut
/// ```
///
/// The caller's audio goes straight into the first stage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReferenceFactory;

impl SessionFactory for ReferenceFactory {
    async fn build(&self, parts: SessionParts) -> Result<BuiltSession> {
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
        let output = OutputLog::new(origin, Some(timeline.clone()));
        let transport = crate::scenario::SimTransportOut::from_log(clock.clone(), output.clone())
            .with_consume_delay(transport_delay);
        let words = WordLog::default();
        let played = PlayLog::default();
        let llm = LlmStage::new(
            Arc::new(mock_llm),
            clock.clone(),
            timeline.clone(),
            Arc::clone(&words),
            Arc::clone(&played),
        );
        let tts = TtsStage::new(
            Arc::new(mock_tts),
            output_rate,
            clock.clone(),
            timeline.clone(),
            Arc::clone(&words),
        )
        .with_play_log(Arc::clone(&played));
        let probes =
            SessionProbes::new(llm.history(), words, Arc::clone(&played), llm.truncations());
        let pipeline = Pipeline::new("sim-session")
            .then(EnergyVad::new(vad))
            .then(SttStage::new(Arc::new(stt), input_rate, clock.clone()))
            .then(TurnManager::new(clock.clone(), timeline))
            .then(llm)
            .then(tts)
            .then(OutputPacer::new(clock, played))
            .then(transport);
        Ok(BuiltSession::new(
            pipeline,
            probes,
            InputSink::pipeline(),
            output,
        ))
    }
}
