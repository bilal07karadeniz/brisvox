//! [`S2sStage`]: a speech-to-speech model as one stage of the call graph
//! (`docs/S2S.md`). It replaces `SttStage`, `TurnDriver`, `LlmRunner`, `SegmenterStage` and
//! `TtsStage`; `AudioIn` stays in front of it and `TransportOut` behind it.
//!
//! ```text
//! AudioIn --16 kHz audio, local VAD events--> S2sStage --UtteranceStarted, Audio, TtsEnded,
//!   (VAD worker optional)                       |  ^          Interrupt, Error--> TransportOut
//!                                               |  +---- TtsStarted / TtsEnded (upstream) ----+
//!                                               +--audio, control--> session task --> provider
//!                                                      ^--------S2sEvent----------------+
//! ```
//!
//! # What the stage keeps from the classic pipeline
//! - **Turn epochs.** The stage is the sole epoch authority (like `TurnDriver`): the epoch moves
//!   at every start of caller speech. When the bot is busy at that moment (a response is being
//!   generated, a tool runs, or audio of it is still playing) it is a barge-in: the stage clears
//!   the transport through the same clear hook as the turn driver (`note_clear` on the
//!   [`PlayoutBook`], then `Clear`), pushes `Interrupt{new_epoch}`
//!   downstream, drops its own queued audio and cancels the tools of the old turn. Provider
//!   events of a response that is no longer current are dropped (counted as `stale_events`).
//! - **What was played.** The model keeps its own history, so the invariant "after an
//!   interruption the context contains only what was played" is enforced there:
//!   `S2sControl::Truncate` with the duration the caller **heard**, computed from the
//!   `PlayoutBook` exactly as the played-text truncation does (released audio minus the client
//!   lead, unless the end mark was echoed). The job waits for the book to settle (bounded by
//!   `EngineConfig::settle_wait`) and never blocks the barge-in itself. The local
//!   [`Context`] (`SessionHandles::context`) mirrors the conversation
//!   (caller transcripts, spoken text cut to the heard share, tool calls and results).
//! - **Tools** run through the call's `ToolEngine` (read-only tools are cancelled by a barge-in;
//!   side-effect tools never run twice). A call runs when its response is done and the
//!   turn is still current; the outputs go back as `function_call_output` items and, only when
//!   the turn is still current, a new response is requested. Results of a superseded turn are
//!   added to the model's history without starting a response.
//! - **Time** comes from the injected clock, every network wait has a timeout, every queue is
//!   bounded and the session task is a [`TaskGuard`] that dies with the stage.
//!
//! # Who ends the caller's turn
//! - [`S2sTurnMode::Provider`] (default): the provider's VAD. It reports speech start and stop,
//!   creates the response and cancels a running one. Caller audio is streamed continuously (the
//!   provider needs silence to find the end of speech). The barge-in decision is the provider's
//!   `SpeechStarted` event; the local VAD events are ignored. No local VAD is needed.
//! - [`S2sTurnMode::Local`]: the provider's VAD is off. The call's local VAD
//!   (`SessionBuilder::with_vad`) starts and ends turns: `UserStartedSpeaking` is the barge-in
//!   decision, `UserStoppedSpeaking` commits the audio buffer and creates a response. Audio is
//!   sent only while the caller speaks (plus a 300 ms pre-roll), so the provider's buffer holds
//!   one utterance. Smart Turn and the finals gate are not used (follow-up).
//!
//! Barge-in policy is VAD-only in both modes: the transcript-based echo guard and backchannel
//! rules of `InterruptionPolicy` need STT words and do not apply here.
//!
//! # Concurrency
//! `process` only forwards: caller audio goes to the task through a bounded drop-newest queue,
//! everything else through a small bounded command queue. The task owns the provider session,
//! the epoch, the response state and every frame the stage emits, so they are strictly ordered.
//! It never awaits the downstream under a barge-in: queued output audio is a `VecDeque` the
//! select loop drains when the downstream has room, and a barge-in empties it first.
//!
//! # Failure
//! A dead session (error, closed channels, a full control queue) is replaced with a fixed
//! backoff, at most `max_reconnects` consecutive attempts (the budget returns after a session
//! that lived ten seconds). The model's history is lost with the session, so the new session is
//! SEEDED from the local mirror ([`S2sConfig::history`], see `docs/S2S.md` section 7). When the
//! budget is spent the stage emits one fatal error frame and stays silent.
//!
//! # Rotation
//! A provider session has a limit (OpenAI Realtime: 60 minutes; Gemini Live: about 10 minutes per
//! connection, announced by `goAway`). [`S2sProvider::session_limit`] reports it; a margin before
//! it ([`S2sStageConfig::rotate_margin`]), or when the provider announces the end
//! ([`brisvox_providers::S2sEvent::SessionEnding`]), the task connects a replacement seeded from
//! the mirror while the old session keeps serving, and switches over at the first moment the bot is idle (no response,
//! tool or playback, the caller silent, the mirror unchanged since the seed was taken). A
//! replacement that arrives at a busy moment is discarded and retried, so the call never hears a
//! gap and no in-flight response is cut.

mod seed;
mod task;

#[cfg(test)]
mod backlog_tests;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_agent::Context;
use brisvox_agent::tools::{ToolEngine, ToolRegistry};
use brisvox_core::frame::{ControlFrame, ControlKind, DataFrame, Frame, SampleRate, SystemFrame};
use brisvox_core::{
    Direction, EpochPolicy, Processor, ProcessorContext, Result as CoreResult, SharedClock,
    TimelineRecorder,
};
use brisvox_providers::s2s::{S2sConfig, S2sProvider, S2sSession};
use brisvox_providers::{LlmProvider, SttProvider, TtsProvider};
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::config::EngineConfig;
use crate::events::EngineEvents;
use crate::guard::TaskGuard;
use crate::offload::Offload;
use crate::playout::PlayoutBook;
use crate::rates::CallRatesCell;
use crate::turn_driver::ClearHook;

/// Capacity of the audio queue between the stage and its task (about 1.3 s of 20 ms frames).
pub const AUDIO_QUEUE: usize = 64;

/// Capacity of the command queue between the stage and its task. Commands are rare (VAD events
/// and playback notices, a few per turn); a full queue is counted in
/// [`S2sStatsSnapshot::commands_dropped`].
pub const COMMAND_QUEUE: usize = 64;

/// Default [`S2sStageConfig::rotate_margin`]: the replacement session is attempted two minutes
/// before the provider's session limit and retried (while the bot is busy) until the limit.
pub const DEFAULT_ROTATE_MARGIN: Duration = Duration::from_secs(120);

/// A boxed future, so the provider can be held as a trait object.
pub type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// An [`S2sProvider`] as a trait object (the builder holds the provider without a type
/// parameter). Implemented for every [`S2sProvider`].
pub trait S2sConnect: Send + Sync + 'static {
    /// Provider name.
    fn name(&self) -> &'static str;

    /// [`S2sProvider::connect`].
    fn connect(&self, config: S2sConfig) -> Boxed<'_, brisvox_providers::Result<S2sSession>>;

    /// [`S2sProvider::warm`].
    fn warm(&self) -> Boxed<'_, brisvox_providers::Result<()>>;

    /// [`S2sProvider::session_limit`].
    fn session_limit(&self) -> Option<Duration>;
}

impl<P: S2sProvider> S2sConnect for P {
    fn name(&self) -> &'static str {
        S2sProvider::name(self)
    }

    fn connect(&self, config: S2sConfig) -> Boxed<'_, brisvox_providers::Result<S2sSession>> {
        Box::pin(S2sProvider::connect(self, config))
    }

    fn warm(&self) -> Boxed<'_, brisvox_providers::Result<()>> {
        Box::pin(S2sProvider::warm(self))
    }

    fn session_limit(&self) -> Option<Duration> {
        S2sProvider::session_limit(self)
    }
}

/// Who ends the caller's turn, see the [module docs](self).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum S2sTurnMode {
    /// The provider's VAD (default).
    #[default]
    Provider,
    /// The call's local VAD; the provider's VAD is off.
    Local,
}

/// Options of the S2S hook, [`SessionBuilder::with_s2s`](crate::SessionBuilder::with_s2s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct S2sOptions {
    /// Who ends the caller's turn.
    pub turn: S2sTurnMode,
    /// Ask the provider for a transcript of the caller's speech (feeds the local context and the
    /// `UserTranscript` events).
    pub input_transcription: bool,
    /// Longest stretch of model audio held in the stage's output queue; audio beyond it is
    /// dropped (and the utterance counts as lost). The model generates faster than real time,
    /// so this is the longest response the stage can buffer ahead of playback.
    pub max_buffered_audio: Duration,
}

impl Default for S2sOptions {
    fn default() -> Self {
        Self {
            turn: S2sTurnMode::Provider,
            input_transcription: false,
            max_buffered_audio: Duration::from_secs(60),
        }
    }
}

/// Settings of the [`S2sStage`].
#[derive(Debug, Clone)]
pub struct S2sStageConfig {
    /// Instructions of the agent.
    pub instructions: Arc<str>,
    /// Voice, or `None` for the provider's default.
    pub voice: Option<Arc<str>>,
    /// BCP-47 language hint, or `None`.
    pub language: Option<Arc<str>>,
    /// Options of the hook.
    pub options: S2sOptions,
    /// Rate of the audio the stage receives ([`AudioIn`](crate::AudioIn)'s model rate).
    pub model_rate: SampleRate,
    /// Rate of the audio exchanged with the provider.
    pub provider_rate: SampleRate,
    /// Length of one audio frame sent downstream and to the provider.
    pub frame: Duration,
    /// Deadline of a connect (and of the warm-up).
    pub connect_timeout: Duration,
    /// Deadline of a send downstream that must not wait for ever (the interrupt, the clear).
    pub send_timeout: Duration,
    /// Wait before the first reconnect; doubles per attempt (cap 5 s).
    pub reconnect_backoff: Duration,
    /// Consecutive failed sessions before the stage gives up.
    pub max_reconnects: u32,
    /// Deadline of a pool job (the resampler build).
    pub offload_timeout: Duration,
    /// How long after the caller's turn ended a response may take to start before the wait is
    /// given up (counted, the bot is no longer treated as busy).
    pub response_deadline: Duration,
    /// Margin assumed between the transport's released audio and the caller's ear when the
    /// transport reports no lead ([`EngineConfig::lead_margin`]).
    pub lead_margin: Duration,
    /// Longest wait for the playout book to settle before truncating
    /// ([`EngineConfig::settle_wait`]).
    pub settle_wait: Duration,
    /// Tool-loop rounds one turn may spend ([`EngineConfig::llm`] `max_tool_rounds`).
    pub max_tool_rounds: u32,
    /// A provider session is replaced (seeded from the local mirror, switched over while the bot
    /// is idle) this long before the provider's [`S2sProvider::session_limit`] ends it. Default
    /// [`DEFAULT_ROTATE_MARGIN`].
    pub rotate_margin: Duration,
}

impl S2sStageConfig {
    /// Settings that follow `engine`, with the given agent text and options.
    pub fn from_engine(engine: &EngineConfig, instructions: Arc<str>, options: S2sOptions) -> Self {
        Self {
            instructions,
            voice: None,
            language: None,
            options,
            model_rate: engine.rates.model,
            provider_rate: SampleRate::Hz24000,
            frame: engine.rates.frame,
            connect_timeout: engine.timeouts.connect,
            send_timeout: engine.timeouts.send,
            reconnect_backoff: engine.timeouts.reconnect_backoff,
            max_reconnects: engine.timeouts.max_reconnects,
            offload_timeout: engine.timeouts.join,
            response_deadline: engine.response_deadline,
            lead_margin: engine.lead_margin,
            settle_wait: engine.settle_wait,
            max_tool_rounds: engine.llm.max_tool_rounds,
            rotate_margin: DEFAULT_ROTATE_MARGIN,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) audio_in: AtomicU64,
    pub(crate) audio_dropped_queue: AtomicU64,
    pub(crate) audio_dropped_no_session: AtomicU64,
    pub(crate) audio_to_provider: AtomicU64,
    pub(crate) audio_dropped_provider: AtomicU64,
    pub(crate) commands_dropped: AtomicU64,
    pub(crate) upstream_dropped: AtomicU64,
    pub(crate) provider_audio_chunks: AtomicU64,
    pub(crate) stale_events: AtomicU64,
    pub(crate) utterances: AtomicU64,
    pub(crate) frames_out: AtomicU64,
    pub(crate) output_overflow: AtomicU64,
    pub(crate) queued_discarded: AtomicU64,
    pub(crate) barge_ins: AtomicU64,
    pub(crate) truncations_sent: AtomicU64,
    pub(crate) truncations_skipped: AtomicU64,
    pub(crate) truncation_failures: AtomicU64,
    pub(crate) tool_calls: AtomicU64,
    pub(crate) tool_rounds: AtomicU64,
    pub(crate) tool_calls_stale: AtomicU64,
    pub(crate) tool_results_stale: AtomicU64,
    pub(crate) tool_round_limit: AtomicU64,
    pub(crate) sessions_opened: AtomicU64,
    pub(crate) session_failures: AtomicU64,
    pub(crate) rotations: AtomicU64,
    pub(crate) rotations_deferred: AtomicU64,
    pub(crate) rotation_failures: AtomicU64,
    pub(crate) connect_failures: AtomicU64,
    pub(crate) response_requests: AtomicU64,
    pub(crate) response_timeouts: AtomicU64,
    pub(crate) warnings: AtomicU64,
    pub(crate) clear_failures: AtomicU64,
    pub(crate) resampler_failures: AtomicU64,
}

pub(crate) fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Live counters of an [`S2sStage`]; cheap to clone, clones share the values.
#[derive(Debug, Clone, Default)]
pub struct S2sStats {
    pub(crate) counters: Arc<Counters>,
}

/// A copy of the counters at one moment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct S2sStatsSnapshot {
    /// Caller audio frames the stage received.
    pub audio_in: u64,
    /// Caller frames dropped because the task's audio queue was full.
    pub audio_dropped_queue: u64,
    /// Caller frames dropped because no session was open (connecting, reconnecting, gave up).
    pub audio_dropped_no_session: u64,
    /// Frames sent to the provider.
    pub audio_to_provider: u64,
    /// Frames the provider's input queue could not take (dropped).
    pub audio_dropped_provider: u64,
    /// Commands lost because the command queue was full (a bug if non-zero).
    pub commands_dropped: u64,
    /// Upstream frames dropped because the upstream lane was full.
    pub upstream_dropped: u64,
    /// Audio chunks received from the provider.
    pub provider_audio_chunks: u64,
    /// Provider events dropped because their response was no longer current.
    pub stale_events: u64,
    /// Utterances announced downstream.
    pub utterances: u64,
    /// Audio frames queued downstream.
    pub frames_out: u64,
    /// Chunks dropped because the output queue was at its limit.
    pub output_overflow: u64,
    /// Audio frames discarded from the output queue by a barge-in (never sent downstream).
    pub queued_discarded: u64,
    /// Barge-ins (clear plus interrupt).
    pub barge_ins: u64,
    /// `Truncate` commands sent.
    pub truncations_sent: u64,
    /// Items not truncated because the caller heard all of them.
    pub truncations_skipped: u64,
    /// Truncations that could not be sent.
    pub truncation_failures: u64,
    /// Tool calls received from the model.
    pub tool_calls: u64,
    /// Tool rounds run (one per response that called tools).
    pub tool_rounds: u64,
    /// Tool calls of a superseded response (answered `interrupted`, not run).
    pub tool_calls_stale: u64,
    /// Tool results that arrived after their turn was superseded (added, no response requested).
    pub tool_results_stale: u64,
    /// Tool rounds refused because the turn spent its round limit.
    pub tool_round_limit: u64,
    /// Sessions opened.
    pub sessions_opened: u64,
    /// Sessions that died.
    pub session_failures: u64,
    /// Sessions replaced by a fresh, re-seeded one before the provider's limit (the call went
    /// on without a gap).
    pub rotations: u64,
    /// Replacement sessions that were opened but not used because the bot or the caller became
    /// active, or the conversation moved on, meanwhile (retried).
    pub rotations_deferred: u64,
    /// Replacement sessions that could not be opened (retried).
    pub rotation_failures: u64,
    /// Connect attempts that failed.
    pub connect_failures: u64,
    /// Responses requested (`CreateResponse`).
    pub response_requests: u64,
    /// Waits for a response that gave up after the deadline.
    pub response_timeouts: u64,
    /// Provider warnings.
    pub warnings: u64,
    /// Transport clears that failed or timed out.
    pub clear_failures: u64,
    /// Output resamplers that could not be built (the utterance's audio is lost).
    pub resampler_failures: u64,
}

impl S2sStats {
    /// A copy of the counters.
    pub fn snapshot(&self) -> S2sStatsSnapshot {
        let c = &self.counters;
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        S2sStatsSnapshot {
            audio_in: get(&c.audio_in),
            audio_dropped_queue: get(&c.audio_dropped_queue),
            audio_dropped_no_session: get(&c.audio_dropped_no_session),
            audio_to_provider: get(&c.audio_to_provider),
            audio_dropped_provider: get(&c.audio_dropped_provider),
            commands_dropped: get(&c.commands_dropped),
            upstream_dropped: get(&c.upstream_dropped),
            provider_audio_chunks: get(&c.provider_audio_chunks),
            stale_events: get(&c.stale_events),
            utterances: get(&c.utterances),
            frames_out: get(&c.frames_out),
            output_overflow: get(&c.output_overflow),
            queued_discarded: get(&c.queued_discarded),
            barge_ins: get(&c.barge_ins),
            truncations_sent: get(&c.truncations_sent),
            truncations_skipped: get(&c.truncations_skipped),
            truncation_failures: get(&c.truncation_failures),
            tool_calls: get(&c.tool_calls),
            tool_rounds: get(&c.tool_rounds),
            tool_calls_stale: get(&c.tool_calls_stale),
            tool_results_stale: get(&c.tool_results_stale),
            tool_round_limit: get(&c.tool_round_limit),
            sessions_opened: get(&c.sessions_opened),
            session_failures: get(&c.session_failures),
            rotations: get(&c.rotations),
            rotations_deferred: get(&c.rotations_deferred),
            rotation_failures: get(&c.rotation_failures),
            connect_failures: get(&c.connect_failures),
            response_requests: get(&c.response_requests),
            response_timeouts: get(&c.response_timeouts),
            warnings: get(&c.warnings),
            clear_failures: get(&c.clear_failures),
            resampler_failures: get(&c.resampler_failures),
        }
    }
}

/// What the stage tells its task besides audio.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Cmd {
    /// The local VAD heard the caller start speaking. The second field is the number of audio
    /// chunks the stage had queued for the task when the command was issued: the task handles
    /// the audio queue up to there first, so the command sees the same audio the VAD saw.
    UserStarted(Instant, u64),
    /// The local VAD heard the caller stop speaking (second field as for `UserStarted`).
    UserStopped(Instant, u64),
    /// The client played the end of the response of `epoch`.
    BotEnded(brisvox_core::TurnEpoch),
}

/// Everything the task needs, handed over when it starts.
pub(crate) struct TaskParts {
    pub(crate) config: S2sStageConfig,
    pub(crate) provider: Arc<dyn S2sConnect>,
    pub(crate) clock: SharedClock,
    pub(crate) timeline: TimelineRecorder,
    pub(crate) book: Arc<PlayoutBook>,
    pub(crate) rates: CallRatesCell,
    pub(crate) offload: Offload,
    pub(crate) hook: ClearHook,
    pub(crate) tools: Option<(Arc<ToolRegistry>, Arc<ToolEngine>)>,
    pub(crate) events: Option<EngineEvents>,
    pub(crate) context: Arc<Mutex<Context>>,
    pub(crate) stats: S2sStats,
    pub(crate) audio_rx: mpsc::Receiver<Bytes>,
    pub(crate) cmd_rx: mpsc::Receiver<Cmd>,
}

/// The speech-to-speech stage; see the [module docs](self).
pub struct S2sStage {
    stats: S2sStats,
    audio_tx: mpsc::Sender<Bytes>,
    /// Audio chunks accepted by `audio_tx` so far (the sequence stamped on the commands).
    audio_queued: u64,
    cmd_tx: mpsc::Sender<Cmd>,
    parts: Option<TaskParts>,
    task: Option<TaskGuard>,
}

impl std::fmt::Debug for S2sStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S2sStage")
            .field("started", &self.task.is_some())
            .finish_non_exhaustive()
    }
}

/// The collaborators of the stage that the assembly shares with other stages.
pub struct S2sWiring {
    /// The call's clock.
    pub clock: SharedClock,
    /// The call's timeline.
    pub timeline: TimelineRecorder,
    /// The call's playout book.
    pub book: Arc<PlayoutBook>,
    /// The call's audio rates (published by the pump).
    pub rates: CallRatesCell,
    /// Where heavy work runs.
    pub offload: Offload,
    /// How a barge-in clears the transport.
    pub hook: ClearHook,
    /// The conversation mirror.
    pub context: Arc<Mutex<Context>>,
    /// The tools of the call.
    pub tools: Option<(Arc<ToolRegistry>, Arc<ToolEngine>)>,
    /// Conversation events for the application.
    pub events: Option<EngineEvents>,
}

impl S2sStage {
    /// A stage over `provider`. It connects when the pipeline starts.
    pub fn new(provider: Arc<dyn S2sConnect>, config: S2sStageConfig, wiring: S2sWiring) -> Self {
        let (audio_tx, audio_rx) = mpsc::channel(AUDIO_QUEUE);
        let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_QUEUE);
        let stats = S2sStats::default();
        Self {
            parts: Some(TaskParts {
                config,
                provider,
                clock: wiring.clock,
                timeline: wiring.timeline,
                book: wiring.book,
                rates: wiring.rates,
                offload: wiring.offload,
                hook: wiring.hook,
                tools: wiring.tools,
                events: wiring.events,
                context: wiring.context,
                stats: stats.clone(),
                audio_rx,
                cmd_rx,
            }),
            stats,
            audio_tx,
            audio_queued: 0,
            cmd_tx,
            task: None,
        }
    }

    /// The live counters.
    pub fn stats(&self) -> S2sStats {
        self.stats.clone()
    }

    fn start_task(&mut self, ctx: &ProcessorContext) -> CoreResult<()> {
        let Some(parts) = self.parts.take() else {
            return Ok(());
        };
        let Some(downstream) = ctx.sender(Direction::Downstream) else {
            // The stage needs a neighbour to send to; keep the parts for a later try.
            self.parts = Some(parts);
            return Ok(());
        };
        self.task = Some(TaskGuard::spawn("s2s", task::run(parts, downstream)));
        Ok(())
    }

    fn command(&self, cmd: Cmd) {
        if self.cmd_tx.try_send(cmd).is_err() {
            bump(&self.stats.counters.commands_dropped);
        }
    }
}

impl Processor for S2sStage {
    fn name(&self) -> &'static str {
        "s2s"
    }

    /// The stage owns the epoch: it must see (and emit) every frame of every epoch.
    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        frame: Frame,
        direction: Direction,
        ctx: &mut ProcessorContext,
    ) -> CoreResult<()> {
        if direction == Direction::Upstream {
            match &frame {
                // Observed by the wrapper of the call (`BotStartedSpeaking`); nothing to do.
                Frame::Control(ControlFrame {
                    kind: ControlKind::TtsStarted,
                    ..
                }) => {}
                Frame::Control(ControlFrame {
                    epoch,
                    kind: ControlKind::TtsEnded,
                }) => self.command(Cmd::BotEnded(*epoch)),
                // Never awaited from `process`: the neighbour upstream may be blocked pushing
                // into this stage's inbox.
                _ => {
                    if let Some(upstream) = ctx.sender(Direction::Upstream)
                        && upstream.try_send(frame).is_err()
                    {
                        bump(&self.stats.counters.upstream_dropped);
                    }
                }
            }
            return Ok(());
        }
        match &frame {
            Frame::Data(DataFrame::Audio(audio)) => {
                self.start_task(ctx)?;
                bump(&self.stats.counters.audio_in);
                if self.audio_tx.try_send(audio.data().clone()).is_err() {
                    bump(&self.stats.counters.audio_dropped_queue);
                } else {
                    self.audio_queued = self.audio_queued.saturating_add(1);
                }
                Ok(())
            }
            Frame::System(SystemFrame::Start) => {
                self.start_task(ctx)?;
                ctx.forward(frame, Direction::Downstream).await
            }
            Frame::System(SystemFrame::UserStartedSpeaking { at }) => {
                self.command(Cmd::UserStarted(*at, self.audio_queued));
                Ok(())
            }
            Frame::System(SystemFrame::UserStoppedSpeaking { at }) => {
                self.command(Cmd::UserStopped(*at, self.audio_queued));
                Ok(())
            }
            // A hint for stages that analyse early; this one has no use for it.
            Frame::System(SystemFrame::UserSpeechFalling { .. } | SystemFrame::Wake { .. }) => {
                Ok(())
            }
            _ => ctx.forward(frame, Direction::Downstream).await,
        }
    }
}

/// Providers for the three classic stages when a call runs on a speech-to-speech model: they
/// are never used (the builder's STT, LLM and TTS stages are not part of that pipeline).
#[derive(Debug, Clone, Copy, Default)]
pub struct Unused;

impl SttProvider for Unused {
    fn name(&self) -> &'static str {
        "unused"
    }

    async fn connect(
        &self,
        _config: brisvox_providers::SttConfig,
    ) -> brisvox_providers::Result<brisvox_providers::SttSession> {
        Err(brisvox_providers::Error::Unavailable {
            provider: "unused",
            reason: "speech-to-speech call",
        })
    }
}

impl LlmProvider for Unused {
    fn name(&self) -> &'static str {
        "unused"
    }

    async fn stream(
        &self,
        _request: brisvox_providers::LlmRequest,
    ) -> brisvox_providers::Result<brisvox_providers::LlmStream> {
        Err(brisvox_providers::Error::Unavailable {
            provider: "unused",
            reason: "speech-to-speech call",
        })
    }
}

impl TtsProvider for Unused {
    fn name(&self) -> &'static str {
        "unused"
    }

    async fn connect(
        &self,
        _config: brisvox_providers::TtsConfig,
    ) -> brisvox_providers::Result<brisvox_providers::TtsSession> {
        Err(brisvox_providers::Error::Unavailable {
            provider: "unused",
            reason: "speech-to-speech call",
        })
    }
}
