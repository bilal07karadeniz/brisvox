//! [`TurnDriver`]: the production turn-taking stage and the sole epoch authority
//! (`docs/PIPELINE.md` sections 2 and 3).
//!
//! ```text
//! SttStage --audio, VAD events, Transcript, UtteranceFinal--> TurnDriver --Interrupt, UserTurnStarted,
//!                                                              |            Transcript (joined final),
//!                                                              |            UserTurnEnded--> LlmRunner
//!    ^----------- TtsStarted / TtsEnded (upstream) ------------+--Clear--> transport CommandSender
//! ```
//!
//! The driver wraps the pure [`TurnMachine`]: it turns frames into
//! [`TurnInput`]s, carries out the [`TurnAction`]s, and adds what the machine leaves to its caller:
//! the timer, the Smart Turn analysis, the finals gate, the response backstop and the frames.
//! Everything runs in one task. Helper tasks (the timer, a Smart Turn job) hand their results
//! back through an [`EventSlot`] and a `SystemFrame::Wake`, so the machine sees one ordered stream
//! of inputs and a paused-clock simulation is deterministic. All times come from the injected
//! clock.
//!
//! # Action mapping
//! - `StartUserTurn` pushes `UserTurnStarted` and marks `UserSpeechStart` on the timeline.
//! - `RequestAnalysis{id}` copies the Smart Turn ring ([`SmartTurn::snapshot_into`] a recycled
//!   owned buffer, so the ring keeps filling and no pause after the first allocates) and runs the analysis on the [`Offload`] in a helper task; its
//!   verdict comes back as a `Wake` event. A verdict of a pause that is no longer current is
//!   ignored by the machine; a failed or timed-out analysis (or no model at all) is only
//!   counted: the machine's fallback deadline (`fallback_silence`) ends the turn.
//!   The VAD stop travels on the system lane and can overtake caller audio still queued in this
//!   driver's data lane. A snapshot taken at that moment would lack the last frames before the
//!   pause (the model would see a truncated tail), so while audio is flowing but the newest
//!   frame in the ring is stamped before the stop, the analysis is DEFERRED
//!   (`analyses_deferred`) and starts as soon as a frame stamped at or after the stop has been
//!   pushed, or after at most [`ANALYSIS_DEFER_MAX`] on the driver's timer (a leg that suppresses
//!   silence sends no frame after the stop; the model then sees the audio there is). A deferred
//!   analysis is dropped when the user resumes, the turn ends or the call hangs up. Before any
//!   audio has arrived (nothing to lag behind) there is no deferral.
//! - `ArmTimer` / `CancelTimer` set the machine's one deadline. The driver has three deadlines
//!   (the machine's, the finals gate's and the response backstop's) and one timer task for all
//!   of them ([`DeadlineTimer`]); on a wake it handles what is due.
//! - `Interrupt` (a confirmed barge-in): `timeline.interrupt`, then the transport `Clear` for the
//!   new epoch through the [`CommandSender`] (applied before it returns, bypassing the command
//!   queue, bounded by [`TurnDriverConfig::send_timeout`]), then `SystemFrame::Interrupt`
//!   downstream, then (the next action) `UserTurnStarted`. Interrupt always precedes the turn
//!   start.
//! - `EndUserTurn` goes through the finals gate.
//!
//! # Early analysis (opt-in)
//! With [`EngineConfig::early_analysis`](crate::EngineConfig::early_analysis) enabled the VAD
//! worker sends `SystemFrame::UserSpeechFalling` when the probability has stayed below the
//! negative threshold for [`EarlyAnalysisConfig::fall_delay`](crate::EarlyAnalysisConfig), before
//! the VAD's minimum silence confirms the stop. In an open user turn under an adaptive policy
//! the driver then snapshots the Smart Turn ring and runs the analysis at once (after the same
//! wait for the ring to reach the silence start as the normal path), so that the verdict is
//! ready, or at least running, when the stop arrives.
//!
//! **Reuse rule.** When the stop is confirmed and the machine requests its analysis, the early
//! verdict answers that request (fed as the machine's `Verdict`, so the machine cannot tell the
//! difference) if ALL of these hold; otherwise the early work is dropped (`early_wasted`) and
//! the normal analysis runs on the current ring:
//! 1. the stop is the stop OF THIS FALL: its time equals the hint's (the detector dates the hint
//!    to the exact time the stop will carry, `UserSpeechFalling`; it sends none when
//!    `speech_pad > min_silence`, where that time cannot be known). Speech that cancels a
//!    pending stop sends nothing, so a stop with another time belongs to a later pause and the
//!    VAD saw speech in between; a newer hint or a speech start drops the early analysis on the
//!    spot. Together this is what "the audio since the snapshot is silence only" means: the VAD
//!    decides what is speech, the driver does not re-measure it. **Exception, by design:** the
//!    VAD's own definition of silence is "no window at or above its threshold"; a window in the
//!    hysteresis band (between the negative and the positive threshold) neither cancels the
//!    pending stop nor restarts its silence, so a stop can still be confirmed with the hint's
//!    time after band-level audio (breath, a faint syllable) arrived following the snapshot.
//!    That verdict is reused all the same: the Smart Turn model sees the audio up to the
//!    silence start, band-level audio is by the VAD's judgement not speech, and the normal path
//!    would have analysed the same stop (the test `a_band_level_window_between_the_hint_and_the_stop_still_reuses`
//!    pins this);
//! 2. at most [`EarlyAnalysisConfig::max_tail`](crate::EarlyAnalysisConfig) of audio arrived
//!    after the snapshot (a VAD that lags that far behind the audio is not trusted);
//! 3. the analysis started (it was not still waiting for the ring) and was not already adopted.
//!
//! If the early job is still running at the stop, the machine's request waits for it instead of
//! starting a second job (one analysis per pause either way); if that job fails the normal
//! analysis is started then. Verdict agreement on the fixtures, the saved latency and the extra
//! analyses per turn are measured in `docs/LATENCY.md`. Nothing here changes when the feature
//! is off: the hint is never sent and every counter stays zero.
//!
//! **Budget.** At most [`EarlyAnalysisConfig::max_per_turn`](crate::EarlyAnalysisConfig) early
//! analyses start per user turn (`early_capped` counts the hints beyond it): every early
//! analysis that is not reused is an extra Smart Turn job, and this bounds what a speaker who
//! pauses a lot can cost (`docs/RISKS.md`, inference capacity).
//!
//! # Barge-in policy
//! The machine judges candidate barge-ins with the [`InterruptionPolicy`] built from
//! [`EngineConfig::interruption`](crate::EngineConfig::interruption) (or one given to
//! [`TurnDriver::with_interruption`]): STT interims are fed as `SttInterim` inputs (they are not
//! forwarded), so the word and backchannel guards see them, and the driver reports the bot's
//! audio from the upstream `TtsStarted` / `TtsEnded` frames (`BotAudioStarted` / `BotAudioEnded`),
//! which opens and closes the echo window. A vetoed candidate emits nothing and bumps no epoch.
//!
//! # The finals gate
//! STT answers every VAD stop with exactly one `UtteranceFinal` ([`SttStage`](crate::SttStage)),
//! in order, preceded by the final segments of that stretch of speech. The driver keeps a ledger
//! of the stops it has not seen answered (`owed finals`: one entry per `UserStoppedSpeaking`,
//! recording the turn it belonged to). The segments between two `UtteranceFinal`s are joined into
//! exactly one text and fed to the machine as one `SttFinal`; the text is also appended to the
//! turn it belongs to.
//!
//! When the machine ends a turn while one of that turn's stops is still unanswered, the end is
//! HELD: nothing is emitted until the finals arrive or [`TurnDriverConfig::final_wait`] passes.
//! Finalize is sent at the VAD stop and Smart Turn runs concurrently, so the added wait is the
//! slower of the two, not the sum. At release the driver pushes one `Transcript` (final, the
//! joined text of all of the turn's stops) and `UserTurnEnded`, marks `TurnEnd`, and clears the
//! Smart Turn ring.
//! - A gate that expires with no text at all (no final text and no interim) is extended ONCE by
//!   [`TurnDriverConfig::late_final_wait`] (`gate_extensions`): the STT stage answers every stop
//!   itself at its own timeout, so a final that is only a little late is still used instead of
//!   dropping the caller's words and leaving them unanswered.
//! - A gate that expires counts `gate_timeouts` and proceeds with what it has; if no final text
//!   arrived but the provider had sent interims, the latest interim is used (so a lost final
//!   does not lose the user's words). The interim never replaces the confirmed text of an
//!   earlier stop of the turn.
//! - An empty transcript emits NO `UserTurnEnded` (nothing to answer). The machine is waiting for
//!   the bot, so the driver feeds it a synthesized `BotAudioEnded`, which is the caller obligation
//!   documented on [`TurnInput::BotAudioEnded`].
//! - If the user starts speaking again while an end is held, it is a RESUME and not a barge-in:
//!   the machine is already waiting for a response that was never requested and no bot audio
//!   exists, so judging the speech by the barge-in guards (minimum speech, backchannel veto) could
//!   veto the continuation and drop its words, or send a spurious transport `Clear` and
//!   `Interrupt`. The driver therefore closes the machine's wait first (the same synthesized
//!   `BotAudioEnded` as for an empty turn) so the `VadStart` opens a new turn with no guard. The
//!   held end is dropped: the text collected so far is carried into the new turn and the finals
//!   still owed for it are re-attributed to the new turn, so nothing the user said is lost and the
//!   bot answers once, after the whole utterance.
//! - A late `UtteranceFinal` of a turn that already ended (gate timeout) feeds the machine's
//!   bookkeeping and is otherwise dropped.
//!
//! # Response backstop
//! After `UserTurnEnded` the driver arms [`TurnDriverConfig::response_deadline`]: the bound on
//! "the bot never produced audio" (a stalled LLM or TTS). The first upstream `TtsStarted` of that
//! epoch (sent once, at the first audio handoff) cancels it, and so does `TtsEnded`:
//! once audio is playing, a long answer must never trip the backstop (a synthetic
//! `BotAudioEnded` mid-speech would make the user's next speech a plain new turn with no
//! `Interrupt` and no transport `Clear`). The end of playback is then guaranteed by the
//! transport's own mark timeout. If the backstop expires the driver feeds a synthetic
//! `BotAudioEnded` (counted), so a response that never produces audio cannot leave the machine
//! in `AwaitingResponse` forever.
//!
//! # Hang-up
//! `SystemFrame::Stop` (the pump's hang-up) closes the machine: an open turn is dropped without a
//! `UserTurnEnded` (no response is wanted, and the pipeline is stopping), a held end is dropped,
//! the timer and any analysis are cancelled, and `Stop` is forwarded.
//!
//! # Speculation (opt-in)
//! With [`EngineConfig::speculation`](crate::EngineConfig::speculation) enabled the driver offers
//! the [`LlmRunner`](crate::LlmRunner) a candidate end of turn before the turn is confirmed: a
//! non-final `Transcript` downstream ([`crate::speculation`]). Candidates are offered
//! - at the final transcript of a stop (`UtteranceFinal`) while the machine is paused and no
//!   other stop of the turn is still owed: the turn's final text so far;
//! - with [`SpeculationConfig::on_partial`](crate::SpeculationConfig::on_partial), at the VAD stop
//!   that paused the machine, with the best partial text (final segments heard so far, else the
//!   latest interim). With a non-zero
//!   [`SpeculationConfig::partial_stable_for`](crate::SpeculationConfig::partial_stable_for) (the
//!   stable-partial gate) this candidate waits: it is offered that long after the stop
//!   reached the driver, only if the caller stayed silent (a `VadStart` clears the wait: no
//!   candidate, not even a withdrawn one) and no final came first (the final's candidate
//!   supersedes it). A change of the text after the stop restarts the wait (changes before the
//!   stop do not count: the stop itself is the start of the silence being measured).
//!
//! A candidate equal (normalised) to the one already offered is not sent again; at most
//! [`SpeculationConfig::max_per_turn`](crate::SpeculationConfig::max_per_turn) are offered per
//! turn. If the caller resumes while a candidate is out, or the held end is released without a
//! `UserTurnEnded`, the candidate is withdrawn (an empty non-final `Transcript`). The turn's real
//! end is the unchanged final `Transcript` + `UserTurnEnded`; the runner decides hit or miss.
//! Nothing here changes when the feature is off.
//!
//! # Bot lines
//! With [`TurnDriver::with_say_input`] the application can make the bot speak a line of its own
//! ([`BotLine`], [`crate::agent_input`]). The driver holds the lines in order (at most
//! [`MAX_HELD_LINES`], newer ones refused and counted; an
//! interrupting line drops the lines that wait before it, counted the same way) and
//! offers the first to the machine ([`TurnInput::BotTurn`]) whenever it handled a frame and the
//! machine is idle (an interrupting line: also while a response runs), no barge-in candidate is
//! pending and no held end waits. When the machine starts the bot turn
//! ([`TurnAction::StartBotTurn`]) the driver sends the line downstream as an `LlmText` of the new
//! epoch: the runner speaks it as that epoch's response. The response backstop is armed as for
//! a user turn; the line's audio is reported upstream like any response, so the caller can barge
//! in on it. A line raises no user events and no `UserTurnStarted`/`UserTurnEnded`.
//!
//! # Conversation events
//! With [`TurnDriver::with_events`] the driver publishes the events it decides, at the moment it
//! decides them, so they keep its order: [`EngineEvent::BotInterrupted`] with the `Interrupt`,
//! [`EngineEvent::UserStartedSpeaking`] with `UserTurnStarted`, [`EngineEvent::UserText`] with
//! the turn's final `Transcript`, and [`EngineEvent::UserStoppedSpeaking`] whenever the user turn
//! that started closes: with `UserTurnEnded`, and also without it — an empty turn (noise, an
//! empty final), a held end that a new turn supersedes, and the hang-up. Every started turn is
//! stopped exactly once. A typed turn (a [`TextTurn`] that starts a turn of its own) raises no
//! speaking events, only its `UserText` (Pipecat's `send-text` neither).
//!
//! # Other frames
//! Caller audio is consumed here (it feeds the Smart Turn ring and goes no further). DTMF, which
//! the pump tags with epoch zero, is re-tagged with the current epoch and forwarded.
//! Other downstream frames are forwarded; upstream `TtsStarted` / `TtsEnded` are consumed,
//! other upstream frames are forwarded with a non-blocking send.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_agent::{
    InterruptionPolicy, InterruptionStats, StateKind, TurnAction, TurnInput, TurnMachine,
    TurnPolicy, TurnState, TurnStats,
};
use brisvox_core::frame::{
    ControlFrame, ControlKind, DataFrame, DtmfFrame, Frame, SampleRate, SystemFrame, TextFrame,
    TranscriptFrame,
};
use brisvox_core::{
    Direction, EpochPolicy, FrameSender, Processor, ProcessorContext, Result as CoreResult,
    SharedClock, TimelineRecorder, TurnEpoch, TurnEvent,
};
use brisvox_inference::smart_turn::{SmartTurn, THRESHOLD, TurnModel};
use brisvox_transport::{CommandSender, TransportCommand};
use tokio::time::Instant;
use tracing::Instrument;

use crate::agent_input::{BotLine, MAX_HELD_LINES, SayReceiver};
use crate::config::{EarlyAnalysisConfig, EngineConfig, InterruptionConfig, SpeculationConfig};
use crate::denoise::DenoiseFeed;
use crate::event_slot::{EventSlot, PushOutcome, SlotOverflow};
use crate::events::{EngineEvent, EngineEvents};
use crate::guard::TaskGuard;
use crate::offload::Offload;
use crate::speculation::{candidate_frame, normalize, withdrawal_frame};
use crate::text_input::{TextTurn, TextTurnReceiver};
use crate::timer::DeadlineTimer;

/// How many pool jobs prepare the Smart Turn sessions of a pool of `threads` threads at `Start`:
/// half the threads, at least one. The model creates the sessions in parallel (about 100 ms
/// each, two per job), and half the pool stays free for the running calls' VAD windows
/// (one job creating them in a row needs about 5 s for 50 threads and holds a thread).
pub const fn smart_turn_warm_jobs(threads: usize) -> usize {
    let half = threads.div_ceil(2);
    if half == 0 { 1 } else { half }
}

/// The `Wake` token of the driver's event slot.
const WAKE_TOKEN: u32 = 1;

/// Pending events of the driver's slot (timer fires and verdicts; at most one analysis is live).
const EVENT_CAPACITY: usize = 16;

/// Typed messages waiting in the driver's text slot (newest rejected beyond it, counted).
const TEXT_CAPACITY: usize = 8;

/// Bot lines waiting in the driver's line slot (newest rejected beyond it, counted).
const LINE_SLOT_CAPACITY: usize = 8;

/// Stops whose final the driver still waits for; beyond this the oldest is forgotten (counted).
/// The STT stage answers every stop, so a backlog this long means a stuck pipeline.
const MAX_OWED_STOPS: usize = 64;

/// Longest text collected for one turn or one group, in bytes. Longer text is dropped (counted):
/// an LLM request must stay bounded.
const MAX_TEXT_BYTES: usize = 64 * 1024;

/// How much of the bot's recent text the echo match looks at.
const ECHO_TEXT_CHARS: usize = 400;

/// The longest a Smart Turn analysis waits for the ring to catch up with the VAD stop (see the
/// module docs).
pub const ANALYSIS_DEFER_MAX: Duration = Duration::from_millis(200);

/// The future of one transport clear: `Err` carries the (credential-free) reason.
pub type ClearFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// How the driver clears the transport on a barge-in: called with the new epoch.
pub type ClearHook = Arc<dyn Fn(TurnEpoch) -> ClearFuture + Send + Sync>;

/// Settings of the [`TurnDriver`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnDriverConfig {
    /// How the end of a user turn is decided. Default: the engine's
    /// ([`EngineConfig::turn`](crate::EngineConfig::turn)).
    pub policy: TurnPolicy,
    /// Which barge-ins are real. Default: the engine's
    /// ([`EngineConfig::interruption`](crate::EngineConfig::interruption)).
    pub interruption: InterruptionConfig,
    /// How long a turn end is held for the STT finals. Default: the engine's `final_wait`.
    pub final_wait: Duration,
    /// How much longer a held end waits ONCE when `final_wait` expires with no text at all (no
    /// final text and no interim): the STT stage answers every stop itself at
    /// [`Timeouts::finalize`](crate::Timeouts::finalize), and a turn that gives up before that
    /// would lose a late answer's words and leave the caller unanswered. Default: the engine's
    /// `timeouts.finalize` minus `final_wait` (the validated configuration keeps it positive);
    /// zero disables the extension.
    pub late_final_wait: Duration,
    /// Backstop after `UserTurnEnded` before a synthetic `BotAudioEnded`. Default: the engine's
    /// `response_deadline`.
    pub response_deadline: Duration,
    /// Timeout of one Smart Turn analysis. Default:
    /// [`Timeouts::analysis_job`](crate::Timeouts::analysis_job).
    pub analysis_timeout: Duration,
    /// Timeout of the transport `Clear`. Default: [`Timeouts::send`](crate::Timeouts::send).
    pub send_timeout: Duration,
    /// Timeout of the Smart Turn model warm-up at `Start` (it prepares one analysis session per
    /// inference pool thread, about 100 ms each). Default:
    /// [`Timeouts::connect`](crate::Timeouts::connect).
    pub warm_timeout: Duration,
    /// The rate of the audio the driver receives and the Smart Turn ring expects (the engine's
    /// model rate, 16 kHz). Audio at another rate is dropped and counted.
    pub model_rate: SampleRate,
    /// Speculative generation: when and how often candidates are offered. Default: the engine's
    /// (off).
    pub speculation: SpeculationConfig,
    /// Speculative Smart Turn analysis at the VAD's falling hint. Default: the engine's (off).
    pub early: EarlyAnalysisConfig,
}

impl TurnDriverConfig {
    /// Settings derived from the engine configuration.
    pub fn from_engine(config: &EngineConfig) -> Self {
        Self {
            policy: config.turn,
            interruption: config.interruption,
            final_wait: config.final_wait,
            late_final_wait: config.timeouts.finalize.saturating_sub(config.final_wait),
            response_deadline: config.response_deadline,
            analysis_timeout: config.timeouts.analysis_job,
            send_timeout: config.timeouts.send,
            warm_timeout: config.timeouts.connect,
            model_rate: config.rates.model,
            speculation: config.speculation,
            early: config.early_analysis,
        }
    }
}

impl Default for TurnDriverConfig {
    fn default() -> Self {
        Self::from_engine(&EngineConfig::DEFAULT)
    }
}

#[derive(Debug, Default)]
struct Counters {
    audio_frames: AtomicU64,
    audio_wrong_rate: AtomicU64,
    vad_starts: AtomicU64,
    vad_stops: AtomicU64,
    interrupts: AtomicU64,
    turns_started: AtomicU64,
    turns_ended: AtomicU64,
    empty_turns: AtomicU64,
    ends_held: AtomicU64,
    ends_superseded: AtomicU64,
    gate_timeouts: AtomicU64,
    gate_extensions: AtomicU64,
    interim_fallbacks: AtomicU64,
    late_finals: AtomicU64,
    stray_finals: AtomicU64,
    owed_overflow: AtomicU64,
    text_dropped: AtomicU64,
    analyses_started: AtomicU64,
    analyses_ok: AtomicU64,
    analyses_failed: AtomicU64,
    analyses_unavailable: AtomicU64,
    analyses_deferred: AtomicU64,
    response_timeouts: AtomicU64,
    clears_sent: AtomicU64,
    clear_errors: AtomicU64,
    clear_timeouts: AtomicU64,
    dtmf: AtomicU64,
    upstream_dropped: AtomicU64,
    speculations_offered: AtomicU64,
    speculations_withdrawn: AtomicU64,
    speculations_capped: AtomicU64,
    early_hints: AtomicU64,
    early_started: AtomicU64,
    early_reused: AtomicU64,
    early_ready: AtomicU64,
    early_wasted: AtomicU64,
    early_capped: AtomicU64,
    early_failed: AtomicU64,
    warms: AtomicU64,
    warm_failures: AtomicU64,
    warm_skips: AtomicU64,
    verdicts_timed: AtomicU64,
    verdict_latency_us: AtomicU64,
    text_turns: AtomicU64,
    text_joined: AtomicU64,
    text_held: AtomicU64,
    text_rejected: AtomicU64,
    lines_started: AtomicU64,
    lines_rejected: AtomicU64,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// A point-in-time copy of the [`TurnDriver`] counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnDriverStatsSnapshot {
    /// Audio frames consumed (fed to the Smart Turn ring).
    pub audio_frames: u64,
    /// Audio frames dropped because their rate is not the model rate.
    pub audio_wrong_rate: u64,
    /// `UserStartedSpeaking` events seen.
    pub vad_starts: u64,
    /// `UserStoppedSpeaking` events seen (each owes one `UtteranceFinal`).
    pub vad_stops: u64,
    /// Confirmed barge-ins (an `Interrupt` was emitted).
    pub interrupts: u64,
    /// `UserTurnStarted` frames emitted.
    pub turns_started: u64,
    /// `UserTurnEnded` frames emitted.
    pub turns_ended: u64,
    /// Turns that ended with an empty transcript (no `UserTurnEnded`).
    pub empty_turns: u64,
    /// Turn ends that had to wait for STT finals.
    pub ends_held: u64,
    /// Held ends dropped because the user resumed (their text carried into the new turn).
    pub ends_superseded: u64,
    /// Held ends released by `final_wait` instead of by the finals.
    pub gate_timeouts: u64,
    /// Held ends whose wait was extended once by `late_final_wait` because `final_wait` expired
    /// with no text at all.
    pub gate_extensions: u64,
    /// Gate timeouts that used the latest interim as the transcript.
    pub interim_fallbacks: u64,
    /// `UtteranceFinal`s that arrived for a turn that had already ended; their text was dropped.
    pub late_finals: u64,
    /// `UtteranceFinal`s with no stop owed.
    pub stray_finals: u64,
    /// Owed stops forgotten because the backlog hit its bound.
    pub owed_overflow: u64,
    /// Transcript segments dropped by the text bound.
    pub text_dropped: u64,
    /// Smart Turn analyses started.
    pub analyses_started: u64,
    /// Analyses whose verdict was fed to the machine.
    pub analyses_ok: u64,
    /// Analyses that failed or timed out (the fallback deadline decides).
    pub analyses_failed: u64,
    /// Analyses requested with no model or pool configured.
    pub analyses_unavailable: u64,
    /// Analyses that had to wait for the ring to catch up with the VAD stop.
    pub analyses_deferred: u64,
    /// Response backstops that expired (synthetic `BotAudioEnded`).
    pub response_timeouts: u64,
    /// Transport `Clear`s applied.
    pub clears_sent: u64,
    /// Transport `Clear`s that failed (the session ended).
    pub clear_errors: u64,
    /// Transport `Clear`s that timed out.
    pub clear_timeouts: u64,
    /// DTMF frames re-tagged and forwarded.
    pub dtmf: u64,
    /// Upstream frames dropped because the upstream lane was full.
    pub upstream_dropped: u64,
    /// Speculation candidates sent to the runner.
    pub speculations_offered: u64,
    /// Candidates withdrawn because the caller resumed or the end was released empty.
    pub speculations_withdrawn: u64,
    /// Candidates not offered because the turn's budget (`max_per_turn`) was spent.
    pub speculations_capped: u64,
    /// `UserSpeechFalling` hints received while early analysis is enabled.
    pub early_hints: u64,
    /// Early analyses started (hints that found an open user turn and a model). Not part of
    /// `analyses_started`.
    pub early_started: u64,
    /// Early verdicts that answered the machine's request at the stop (`analyses_ok` counts
    /// them too).
    pub early_reused: u64,
    /// Of `early_reused`, the verdicts that were already there when the stop arrived (the
    /// stop-to-verdict wait was zero).
    pub early_ready: u64,
    /// Early analyses that were not used: the user resumed or a newer hint came (the extra
    /// analysis per fall that did not become a stop), the stop did not meet the reuse rule, or
    /// the turn ended, was cancelled or hung up first. Disjoint from `early_failed` and
    /// `early_reused`: `early_started == early_reused + early_wasted + early_failed` when no
    /// early analysis is in flight.
    pub early_wasted: u64,
    /// Hints ignored because the turn's budget (`max_per_turn`) was spent.
    pub early_capped: u64,
    /// Early analyses whose job failed or timed out (not counted in `early_wasted`; the normal
    /// analysis runs instead when the machine was already waiting for it).
    pub early_failed: u64,
    /// Smart Turn warm-ups that succeeded at `Start` (0 without a model, and 0 when an earlier
    /// call already warmed this model on the same pool; a model with nothing to warm still
    /// counts as warmed).
    pub warms: u64,
    /// Smart Turn warm-ups that failed or timed out (the first analysis pays the cold start).
    pub warm_failures: u64,
    /// Smart Turn warm-ups not submitted because the pool's queue was deep (they cool down and
    /// a later call warms; see `brisvox_engine::offload::WARM_COOLDOWN`).
    pub warm_skips: u64,
    /// Verdicts fed to the machine that were timed from the VAD stop's arrival at the driver
    /// (one per stop at most).
    pub verdicts_timed: u64,
    /// Total microseconds from the stop's arrival to the verdict being fed, over
    /// `verdicts_timed`: divide for the mean VAD-stop-to-verdict latency.
    pub verdict_latency_us: u64,
    /// Typed messages ([`crate::text_input`]) fed to the machine as a turn of their own.
    pub text_turns: u64,
    /// Typed messages added to a user turn that was open (the user was speaking).
    pub text_joined: u64,
    /// Typed messages with `run_immediately: false`, held for the next user turn.
    pub text_held: u64,
    /// Typed messages dropped: empty, after the hang-up, too long for the text bound, or
    /// refused by the full text slot.
    pub text_rejected: u64,
    /// Bot lines ([`crate::agent_input`]) started as a bot turn.
    pub lines_started: u64,
    /// Bot lines dropped: refused by the full slot or the full hold
    /// ([`MAX_HELD_LINES`]), superseded by an interrupting
    /// line, or still waiting at the hang-up.
    pub lines_rejected: u64,
}

/// Shared handle on the counters of one [`TurnDriver`].
#[derive(Debug, Clone)]
pub struct TurnDriverStats {
    counters: Arc<Counters>,
}

impl TurnDriverStats {
    /// The counters now.
    pub fn snapshot(&self) -> TurnDriverStatsSnapshot {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        TurnDriverStatsSnapshot {
            audio_frames: get(&c.audio_frames),
            audio_wrong_rate: get(&c.audio_wrong_rate),
            vad_starts: get(&c.vad_starts),
            vad_stops: get(&c.vad_stops),
            interrupts: get(&c.interrupts),
            turns_started: get(&c.turns_started),
            turns_ended: get(&c.turns_ended),
            empty_turns: get(&c.empty_turns),
            ends_held: get(&c.ends_held),
            ends_superseded: get(&c.ends_superseded),
            gate_timeouts: get(&c.gate_timeouts),
            gate_extensions: get(&c.gate_extensions),
            interim_fallbacks: get(&c.interim_fallbacks),
            late_finals: get(&c.late_finals),
            stray_finals: get(&c.stray_finals),
            owed_overflow: get(&c.owed_overflow),
            text_dropped: get(&c.text_dropped),
            analyses_started: get(&c.analyses_started),
            analyses_ok: get(&c.analyses_ok),
            analyses_failed: get(&c.analyses_failed),
            analyses_unavailable: get(&c.analyses_unavailable),
            analyses_deferred: get(&c.analyses_deferred),
            response_timeouts: get(&c.response_timeouts),
            clears_sent: get(&c.clears_sent),
            clear_errors: get(&c.clear_errors),
            clear_timeouts: get(&c.clear_timeouts),
            dtmf: get(&c.dtmf),
            upstream_dropped: get(&c.upstream_dropped),
            speculations_offered: get(&c.speculations_offered),
            speculations_withdrawn: get(&c.speculations_withdrawn),
            speculations_capped: get(&c.speculations_capped),
            early_hints: get(&c.early_hints),
            early_started: get(&c.early_started),
            early_reused: get(&c.early_reused),
            early_ready: get(&c.early_ready),
            early_wasted: get(&c.early_wasted),
            early_capped: get(&c.early_capped),
            early_failed: get(&c.early_failed),
            warms: get(&c.warms),
            warm_failures: get(&c.warm_failures),
            warm_skips: get(&c.warm_skips),
            verdicts_timed: get(&c.verdicts_timed),
            verdict_latency_us: get(&c.verdict_latency_us),
            text_turns: get(&c.text_turns),
            text_joined: get(&c.text_joined),
            text_held: get(&c.text_held),
            text_rejected: get(&c.text_rejected),
            lines_started: get(&c.lines_started),
            lines_rejected: get(&c.lines_rejected),
        }
    }
}

/// What a [`TurnProbe`] knows about the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineSnapshot {
    /// The machine's state.
    pub state: StateKind,
    /// The machine's epoch.
    pub epoch: TurnEpoch,
    /// Inputs the machine ignored.
    pub ignored: TurnStats,
    /// Barge-in decisions.
    pub interruption: InterruptionStats,
}

#[derive(Debug)]
struct ProbeInner {
    actions: VecDeque<TurnAction>,
    capacity: usize,
    dropped: u64,
    machine: Option<MachineSnapshot>,
}

/// A window on the turn machine of a [`TurnDriver`] for tests and diagnostics: the last actions
/// the machine returned, in order, and a snapshot of its state and counters after the latest
/// input. Bounded: the oldest action is forgotten beyond `capacity`.
#[derive(Debug, Clone)]
pub struct TurnProbe {
    inner: Arc<Mutex<ProbeInner>>,
}

impl TurnProbe {
    /// A probe keeping the last `capacity` actions (at least 1).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            inner: Arc::new(Mutex::new(ProbeInner {
                actions: VecDeque::with_capacity(capacity),
                capacity,
                dropped: 0,
                machine: None,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, ProbeInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn record(&self, action: TurnAction) {
        let mut inner = self.lock();
        if inner.actions.len() >= inner.capacity {
            inner.actions.pop_front();
            inner.dropped = inner.dropped.saturating_add(1);
        }
        inner.actions.push_back(action);
    }

    fn set_machine(&self, snapshot: MachineSnapshot) {
        self.lock().machine = Some(snapshot);
    }

    /// The recorded actions, oldest first.
    pub fn actions(&self) -> Vec<TurnAction> {
        self.lock().actions.iter().copied().collect()
    }

    /// Actions forgotten because the probe was full.
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    /// The machine after the latest input (`None` before the first one).
    pub fn machine(&self) -> Option<MachineSnapshot> {
        self.lock().machine
    }
}

/// An event the driver's helper tasks hand to the driver.
#[derive(Debug)]
enum Event {
    /// The timer task reached its deadline.
    Timer,
    /// A Smart Turn job ended: `Some(probability)` or `None` when it failed.
    Verdict {
        id: brisvox_agent::AnalysisId,
        probability: Option<f32>,
        /// The snapshot buffer, handed back for reuse (`None` when the job did not finish).
        samples: Option<Vec<i16>>,
    },
    /// The early (speculative) Smart Turn job ended: the same payload as `Verdict`, keyed by the
    /// early analysis' sequence number instead of a machine id.
    EarlyVerdict {
        seq: u64,
        probability: Option<f32>,
        samples: Option<Vec<i16>>,
    },
}

/// A VAD stop the driver is waiting for the final of.
#[derive(Debug, Clone, Copy)]
struct OwedStop {
    /// The user turn the stop belongs to; `None` for a stop the machine did not take into a turn
    /// (a vetoed barge-in candidate): its text is dropped.
    turn: Option<TurnEpoch>,
}

/// The user turn in progress (speaking, paused, or ended and held).
#[derive(Debug)]
struct OpenTurn {
    epoch: TurnEpoch,
    text: String,
}

/// A turn end waiting for STT finals.
#[derive(Debug, Clone, Copy)]
struct Held {
    epoch: TurnEpoch,
    deadline: Instant,
    /// The wait was already extended by `late_final_wait`.
    extended: bool,
}

/// The response the bot owes after a `UserTurnEnded`.
#[derive(Debug, Clone, Copy)]
struct Response {
    epoch: TurnEpoch,
    deadline: Instant,
}

/// Where an early analysis is.
#[derive(Debug, Clone, Copy)]
enum EarlyStage {
    /// The ring has not reached the silence start yet (the audio lane lags the system lane);
    /// the job starts when it does, or at `deadline`.
    Waiting { deadline: Instant },
    /// The job runs.
    Running,
    /// The job returned this probability and nobody asked for it yet.
    Done(f32),
}

/// A speculative Smart Turn analysis started at a falling hint (see the module docs).
struct Early {
    seq: u64,
    /// The hint's time: the time the stop of this fall will carry.
    hint_at: Instant,
    stage: EarlyStage,
    /// Samples pushed into the ring since the snapshot.
    tail: usize,
    /// The machine's request this job answers when it ends.
    adopted: Option<brisvox_agent::AnalysisId>,
    /// The job was spawned (and counted in `early_started`), so dropping the analysis without
    /// reusing it is counted exactly once, as `early_wasted` (see `discard_early`).
    started: bool,
    _task: Option<TaskGuard>,
}

/// Smart Turn: the ring of the user's audio, and where analyses run.
struct Analyzer {
    ring: SmartTurn,
    offload: Offload,
    /// The snapshot buffer of the previous analysis, handed back with its verdict and reused by
    /// the next one: after the first pause of a session no pause allocates on the async worker.
    /// `None` before the first analysis and after a job that did not return it (timed out).
    spare: Option<Vec<i16>>,
}

/// Appends `src` (trimmed, skipped when empty) to `dst` separated by a space. `false` when the
/// text bound would be exceeded (nothing is appended).
fn append_text(dst: &mut String, src: &str) -> bool {
    let src = src.trim();
    if src.is_empty() {
        return true;
    }
    let separator = usize::from(!dst.is_empty());
    if dst
        .len()
        .saturating_add(separator)
        .saturating_add(src.len())
        > MAX_TEXT_BYTES
    {
        return false;
    }
    if separator == 1 {
        dst.push(' ');
    }
    dst.push_str(src);
    true
}

/// The duration of `samples` at the model rate (16 kHz).
fn frames_duration(samples: usize) -> Duration {
    let samples = u64::try_from(samples).unwrap_or(u64::MAX);
    Duration::from_micros(samples.saturating_mul(1_000_000) / 16_000)
}

fn control(epoch: TurnEpoch, kind: ControlKind) -> Frame {
    ControlFrame { epoch, kind }.into()
}

/// The helper that moves typed messages from the application's channel into the driver's text
/// slot and wakes the driver (it never touches the driver's state). Ends with the channel or the
/// driver.
async fn text_pump(
    mut input: TextTurnReceiver,
    slot: EventSlot<TextTurn>,
    me: FrameSender,
    counters: Arc<Counters>,
) {
    while let Some(typed) = input.rx.recv().await {
        match slot.push_and_wake(typed, &me, WAKE_TOKEN).await {
            Ok(PushOutcome::Queued) => {}
            Ok(PushOutcome::Rejected(_) | PushOutcome::Evicted(_)) => {
                bump(&counters.text_rejected);
            }
            Err(_) => return,
        }
    }
}

/// Moves bot lines from the application's channel into the driver's line slot with a `Wake`.
async fn line_pump(
    mut input: SayReceiver,
    slot: EventSlot<BotLine>,
    me: FrameSender,
    counters: Arc<Counters>,
) {
    while let Some(line) = input.rx.recv().await {
        match slot.push_and_wake(line, &me, WAKE_TOKEN).await {
            Ok(PushOutcome::Queued) => {}
            Ok(PushOutcome::Rejected(_) | PushOutcome::Evicted(_)) => {
                bump(&counters.lines_rejected);
            }
            Err(_) => return,
        }
    }
}

/// The turn driver stage; see the module docs.
pub struct TurnDriver {
    config: TurnDriverConfig,
    clock: SharedClock,
    timeline: TimelineRecorder,
    machine: TurnMachine,
    analyzer: Option<Analyzer>,
    clear: Option<ClearHook>,
    probe: Option<TurnProbe>,
    slot: EventSlot<Event>,
    timer: Option<DeadlineTimer>,
    me: Option<FrameSender>,
    analysis: Option<(brisvox_agent::AnalysisId, TaskGuard)>,
    /// The Smart Turn model warm-up started at `Start`.
    warm: Option<TaskGuard>,
    /// Scratch for the machine's actions and for decoded audio (reused: no allocation per frame).
    actions: Vec<TurnAction>,
    scratch: Vec<i16>,
    /// Inputs the driver feeds itself while carrying out actions (processed in order).
    followups: VecDeque<TurnInput>,
    /// The epoch of the latest turn started, the `old` side of the next `timeline.interrupt`.
    epoch: TurnEpoch,
    /// Whether the `StartUserTurn` being applied follows an `Interrupt` (the timeline has the
    /// speech start already).
    after_interrupt: bool,
    /// The epoch of the latest upstream `TtsStarted`: whose bot audio reached the transport. A
    /// barge-in on that turn marks `AudioOutStopped` once the fast-path clear is applied.
    audio_epoch: Option<TurnEpoch>,
    machine_deadline: Option<Instant>,
    vad_start_at: Option<Instant>,
    /// Time of the latest VAD stop, and of the newest audio frame pushed into the Smart Turn ring.
    stop_at: Option<Instant>,
    last_audio_at: Option<Instant>,
    /// Time just after the newest sample in the Smart Turn ring.
    ring_end: Option<Instant>,
    /// Noise-suppressed audio that replaces the plain samples in the ring (`crate::denoise`).
    denoised: Option<Arc<DenoiseFeed>>,
    /// The book the bot's recent text is read from, to recognise its own voice in the caller's
    /// interims (`InterruptionConfig::echo_text_match`).
    bot_text: Option<Arc<crate::playout::PlayoutBook>>,
    /// An analysis waiting for the ring to reach `stop_at` (see the module docs).
    deferred_analysis: Option<(brisvox_agent::AnalysisId, Instant)>,
    /// The speculative analysis of the current fall, if any.
    early: Option<Early>,
    /// An early verdict that answers the request the machine made while it handled the stop being
    /// processed: fed once the stop's bookkeeping is done (see `vad_stop`).
    early_ready: Option<(brisvox_agent::AnalysisId, f32)>,
    early_seq: u64,
    /// Early analyses started for the turn `early_epoch` (the per-turn budget).
    early_in_turn: u32,
    early_epoch: TurnEpoch,
    /// When the latest stop reached the driver, until its verdict is fed (latency statistics).
    stop_seen_at: Option<Instant>,
    owed: VecDeque<OwedStop>,
    /// Final segments since the last `UtteranceFinal`.
    group: String,
    /// The latest interim since the last `UtteranceFinal`.
    interim: String,
    turn: Option<OpenTurn>,
    held: Option<Held>,
    response: Option<Response>,
    /// The candidate the runner holds: its turn and normalised text.
    offered: Option<(TurnEpoch, String)>,
    /// Candidates offered for the turn `offers_epoch`.
    offers: u32,
    offers_epoch: TurnEpoch,
    /// The stable-partial gate: the normalised partial text of the open turn and when
    /// it last changed (only tracked while the gate is configured).
    partial_seen: Option<(String, Instant)>,
    /// A partial candidate waiting out the gate: the turn and when it becomes due.
    partial_gate: Option<(TurnEpoch, Instant)>,
    /// Typed messages: the channel until `Start`, then the helper that moves them into
    /// `text_slot` with a `Wake`.
    text_rx: Option<TextTurnReceiver>,
    text_slot: EventSlot<TextTurn>,
    _text_task: Option<TaskGuard>,
    /// Typed text with `run_immediately: false`, joined to the next user turn.
    text_hold: String,
    /// The typed text of the turn the machine is starting (taken by `start_turn`).
    text_pending: Option<String>,
    /// The turn being started is typed: its speech start is now, not the last VAD start.
    typed_start: bool,
    /// Bot lines: the channel until `Start`, then the helper that moves them into
    /// `line_slot` with a `Wake`.
    say_rx: Option<SayReceiver>,
    line_slot: EventSlot<BotLine>,
    _line_task: Option<TaskGuard>,
    /// Lines waiting for the machine, oldest first (at most [`MAX_HELD_LINES`]).
    lines: VecDeque<BotLine>,
    /// The text of the line the machine is starting (taken by `start_bot_turn`).
    line_pending: Option<Arc<str>>,
    /// Where the driver's conversation events go.
    events: Option<EngineEvents>,
    /// The spoken user turn whose `UserStartedSpeaking` is not yet matched by a stop.
    user_speaking: Option<TurnEpoch>,
    counters: Arc<Counters>,
}

impl TurnDriver {
    /// A driver deciding turns by `config.policy`, recording on `timeline`. Without a model
    /// ([`Self::with_smart_turn`]) an adaptive policy ends every turn at its fallback deadline.
    pub fn new(config: TurnDriverConfig, clock: SharedClock, timeline: TimelineRecorder) -> Self {
        let policy = config.interruption.policy();
        Self {
            config,
            clock,
            timeline,
            machine: TurnMachine::with_interruption(config.policy, policy),
            analyzer: None,
            clear: None,
            probe: None,
            slot: EventSlot::new(EVENT_CAPACITY, SlotOverflow::DropOldest),
            timer: None,
            me: None,
            analysis: None,
            warm: None,
            actions: Vec::with_capacity(8),
            scratch: Vec::new(),
            followups: VecDeque::new(),
            epoch: TurnEpoch::ZERO,
            after_interrupt: false,
            audio_epoch: None,
            machine_deadline: None,
            vad_start_at: None,
            stop_at: None,
            last_audio_at: None,
            ring_end: None,
            denoised: None,
            bot_text: None,
            deferred_analysis: None,
            early: None,
            early_ready: None,
            early_seq: 0,
            early_in_turn: 0,
            early_epoch: TurnEpoch::ZERO,
            stop_seen_at: None,
            owed: VecDeque::new(),
            group: String::new(),
            interim: String::new(),
            turn: None,
            held: None,
            response: None,
            offered: None,
            offers: 0,
            offers_epoch: TurnEpoch::ZERO,
            partial_seen: None,
            partial_gate: None,
            text_rx: None,
            text_slot: EventSlot::new(TEXT_CAPACITY, SlotOverflow::RejectNewest),
            _text_task: None,
            text_hold: String::new(),
            text_pending: None,
            typed_start: false,
            say_rx: None,
            line_slot: EventSlot::new(LINE_SLOT_CAPACITY, SlotOverflow::RejectNewest),
            _line_task: None,
            lines: VecDeque::new(),
            line_pending: None,
            events: None,
            user_speaking: None,
            counters: Arc::new(Counters::default()),
        }
    }

    /// Publishes the user turn events and barge-ins the driver decides to `events` (see
    /// "Conversation events" in the module docs).
    #[must_use]
    pub fn with_events(mut self, events: EngineEvents) -> Self {
        self.events = Some(events);
        self
    }

    /// Takes typed user messages from `input` (see [`crate::text_input`]): each one
    /// becomes a user turn, joins the open one, or waits for the next one.
    #[must_use]
    pub fn with_text_input(mut self, input: TextTurnReceiver) -> Self {
        self.text_rx = Some(input);
        self
    }

    /// Takes bot lines from `input` (see [`crate::agent_input`] and "Bot lines" in the
    /// module docs): each one becomes a bot turn once the machine is free for it.
    #[must_use]
    pub fn with_say_input(mut self, input: SayReceiver) -> Self {
        self.say_rx = Some(input);
        self
    }

    /// Judges barge-ins with `policy` instead of the one built from the configuration (for
    /// deployments with their own backchannel lists).
    #[must_use]
    pub fn with_interruption(mut self, policy: InterruptionPolicy) -> Self {
        self.machine = TurnMachine::with_interruption(self.config.policy, policy);
        self
    }

    /// Runs Smart Turn analyses of the user's audio with `model` on `offload` (allocates the
    /// 8 s ring of 128 000 samples, 256 KB; the first analysis adds one snapshot buffer of the
    /// same size, which later analyses reuse: 512 KB per session once it has paused).
    #[must_use]
    pub fn with_smart_turn(mut self, model: Arc<dyn TurnModel>, offload: Offload) -> Self {
        self.analyzer = Some(Analyzer {
            ring: SmartTurn::new(model),
            offload,
            spare: None,
        });
        self
    }

    /// Lets the Smart Turn analyses hear the noise-suppressed audio of `feed`: its chunks
    /// replace the plain samples already in the ring, just before each snapshot and with every
    /// audio frame. Where the feed has nothing (the suppressor was late, failed or dropped a
    /// chunk) the ring keeps the plain audio. Without a Smart Turn model it does nothing.
    #[must_use]
    pub(crate) fn with_denoised_feed(mut self, feed: Arc<DenoiseFeed>) -> Self {
        self.denoised = Some(feed);
        self
    }

    /// Recognises the bot's own voice coming back: an interim of a barge-in candidate that consists
    /// of words the bot has just said holds the candidate (`TurnMachine::set_echo`). Only has an
    /// effect with `min_words >= 1` in the interruption policy.
    #[must_use]
    pub fn with_bot_text(mut self, book: Arc<crate::playout::PlayoutBook>) -> Self {
        self.bot_text = Some(book);
        self
    }

    /// Whether `text`, the latest interim of the pending candidate received at `at`, is the bot's
    /// echo.
    fn is_echo(&self, text: &str, at: Instant) -> bool {
        let (Some(book), Some(candidate)) = (&self.bot_text, self.machine.barge_in_candidate())
        else {
            return false;
        };
        let age = at.saturating_duration_since(candidate.started_at());
        brisvox_agent::is_bot_echo(text, &book.recent_sent_text(ECHO_TEXT_CHARS), age)
    }

    /// Moves the feed's chunks into the Smart Turn ring (see [`Self::with_denoised_feed`]).
    fn apply_denoised(&mut self) {
        let (Some(feed), Some(analyzer), Some(ring_end)) = (
            self.denoised.as_ref(),
            self.analyzer.as_mut(),
            self.ring_end,
        ) else {
            return;
        };
        let hz = u128::from(self.config.model_rate.hz());
        let to_micros = |samples: usize| {
            u64::try_from(samples as u128 * 1_000_000 / hz.max(1)).unwrap_or(u64::MAX)
        };
        // A chunk whose audio the ring does not hold yet waits (the enhanced chunk of a frame can
        // be ready before the plain frame has reached this stage).
        let slack = Duration::from_millis(1);
        feed.drain_ready(
            |start, len| start + Duration::from_micros(to_micros(len)) <= ring_end + slack,
            |start, samples| {
                let end = start + Duration::from_micros(to_micros(samples.len()));
                let behind = ring_end.saturating_duration_since(end).as_micros();
                let back = usize::try_from(behind * hz / 1_000_000).unwrap_or(usize::MAX);
                analyzer.ring.overwrite(back, samples);
            },
        );
    }

    /// Sends the transport `Clear` of a barge-in through `commands` (the fast path that bypasses
    /// the command queue). Without it a barge-in only emits the downstream `Interrupt`.
    #[must_use]
    pub fn with_commands(self, commands: CommandSender) -> Self {
        self.with_clear_hook(Arc::new(move |epoch| {
            let commands = commands.clone();
            Box::pin(async move {
                commands
                    .send(TransportCommand::Clear { epoch })
                    .await
                    .map_err(|error| error.to_string())
            })
        }))
    }

    /// Clears the transport on a barge-in by calling `hook` with the new epoch (bounded by
    /// [`TurnDriverConfig::send_timeout`]) instead of through a [`CommandSender`]: for
    /// transports that are not a `TransportSession`, and for tests that need a clear that fails
    /// or never completes.
    #[must_use]
    pub fn with_clear_hook(mut self, hook: ClearHook) -> Self {
        self.clear = Some(hook);
        self
    }

    /// Records the machine's actions and state on `probe`.
    #[must_use]
    pub fn with_probe(mut self, probe: TurnProbe) -> Self {
        self.probe = Some(probe);
        self
    }

    /// Counters of this driver (usable after the driver moved into a pipeline).
    pub fn stats(&self) -> TurnDriverStats {
        TurnDriverStats {
            counters: Arc::clone(&self.counters),
        }
    }

    /// Prepares the Smart Turn model for the analyses of this call: a helper task asks it, on the
    /// offload, for one analysis session per worker thread (`TurnModel::warm(threads)`), as
    /// [`smart_turn_warm_jobs`] jobs at once (half the pool's threads, at least one: the model
    /// creates the sessions in parallel, about two per job, and half the pool stays free for
    /// the running calls), each bounded by [`TurnDriverConfig::warm_timeout`]. On a pool this
    /// runs once per pool and model for the whole process ([`Offload::claim_warm`], keyed by
    /// `TurnModel::warm_key`, else the instance's address): later calls skip it. A failure is
    /// counted, gives the claim back (after a cool-down when the pool was busy), and never fails
    /// the call. Traced as the span `smart_turn_warm` on `brisvox::engine`.
    fn start_warm(&mut self) {
        if self.warm.is_some() {
            return;
        }
        let Some(analyzer) = self.analyzer.as_ref() else {
            return;
        };
        let model = analyzer.ring.model();
        let offload = analyzer.offload.clone();
        let key = model.warm_key().map_or_else(
            || format!("smart_turn@{:p}", Arc::as_ptr(&model).cast::<()>()),
            |key| format!("smart_turn:{key}"),
        );
        let Some(claim) = offload.claim_warm(&key) else {
            return;
        };
        let timeout = self.config.warm_timeout;
        let counters = Arc::clone(&self.counters);
        let span = tracing::debug_span!(target: "brisvox::engine", "smart_turn_warm", key = %key);
        self.warm = Some(TaskGuard::spawn(
            "turn_warm",
            async move {
                let sessions = offload.threads();
                let jobs = smart_turn_warm_jobs(sessions);
                let report = offload
                    .warm(claim, timeout, jobs, move || model.warm(sessions))
                    .await;
                if report.skipped > 0 {
                    bump(&counters.warm_skips);
                    tracing::debug!(target: "brisvox::engine", "smart turn warm-up skipped: pool busy");
                } else if report.failed > 0 {
                    bump(&counters.warm_failures);
                    tracing::debug!(
                        target: "brisvox::engine",
                        failed = report.failed,
                        "smart turn warm-up failed"
                    );
                } else {
                    bump(&counters.warms);
                    tracing::debug!(target: "brisvox::engine", sessions, jobs, "smart turn warm");
                }
            }
            .instrument(span),
        ));
    }

    fn ensure_started(&mut self, ctx: &ProcessorContext) {
        if self.timer.is_some() {
            return;
        }
        let Some(me) = ctx.self_sender() else {
            return;
        };
        if let Some(input) = self.text_rx.take() {
            self._text_task = Some(TaskGuard::spawn(
                "turn_text",
                text_pump(
                    input,
                    self.text_slot.clone(),
                    me.clone(),
                    Arc::clone(&self.counters),
                ),
            ));
        }
        if let Some(input) = self.say_rx.take() {
            self._line_task = Some(TaskGuard::spawn(
                "turn_lines",
                line_pump(
                    input,
                    self.line_slot.clone(),
                    me.clone(),
                    Arc::clone(&self.counters),
                ),
            ));
        }
        self.timer = Some(DeadlineTimer::spawn(
            Arc::clone(&self.clock),
            self.slot.clone(),
            || Event::Timer,
            me.clone(),
            WAKE_TOKEN,
        ));
        self.me = Some(me);
    }

    /// Brings the timer in line with the earliest of the driver's deadlines.
    fn sync_timer(&mut self) {
        let mut next = self.machine_deadline;
        let others = [
            self.held.map(|held| held.deadline),
            self.response.map(|response| response.deadline),
            self.deferred_analysis.map(|(_, deadline)| deadline),
            self.partial_gate.map(|(_, deadline)| deadline),
            self.early.as_ref().and_then(|early| match early.stage {
                EarlyStage::Waiting { deadline } => Some(deadline),
                EarlyStage::Running | EarlyStage::Done(_) => None,
            }),
        ];
        for deadline in others.into_iter().flatten() {
            next = Some(next.map_or(deadline, |current| current.min(deadline)));
        }
        if let Some(timer) = self.timer.as_mut() {
            timer.set(next);
        }
    }

    fn note_machine(&self) {
        if let Some(probe) = &self.probe {
            probe.set_machine(MachineSnapshot {
                state: self.machine.state().kind(),
                epoch: self.machine.epoch(),
                ignored: self.machine.stats(),
                interruption: self.machine.interruption_stats(),
            });
        }
    }

    /// Feeds `input` to the machine and carries out its actions, then any input the actions
    /// caused in turn.
    async fn feed(&mut self, input: TurnInput, ctx: &mut ProcessorContext) -> CoreResult<()> {
        self.followups.push_back(input);
        while let Some(next) = self.followups.pop_front() {
            self.machine.handle(next, &mut self.actions);
            self.note_machine();
            let actions = std::mem::take(&mut self.actions);
            let mut result = Ok(());
            for action in &actions {
                if let Some(probe) = &self.probe {
                    probe.record(*action);
                }
                if result.is_ok() {
                    result = self.apply(*action, ctx).await;
                }
            }
            self.actions = actions;
            self.actions.clear();
            result?;
        }
        Ok(())
    }

    async fn apply(&mut self, action: TurnAction, ctx: &mut ProcessorContext) -> CoreResult<()> {
        match action {
            TurnAction::StartUserTurn { epoch } => self.start_turn(epoch, ctx).await,
            TurnAction::RequestAnalysis { id } => {
                self.request_analysis(id);
                Ok(())
            }
            TurnAction::EndUserTurn { epoch, reason } => {
                tracing::debug!(
                    target: "brisvox::engine",
                    epoch = epoch.get(),
                    ?reason,
                    "turn driver: the machine ended the user turn"
                );
                self.end_turn(epoch, reason, ctx).await
            }
            TurnAction::Interrupt { new_epoch } => self.interrupt(new_epoch, ctx).await,
            TurnAction::ArmTimer { deadline } => {
                self.machine_deadline = Some(deadline);
                Ok(())
            }
            TurnAction::CancelTimer => {
                self.machine_deadline = None;
                Ok(())
            }
            TurnAction::StartBotTurn { epoch } => self.start_bot_turn(epoch, ctx).await,
        }
    }

    /// Offers the oldest waiting bot line to the machine when it is free for it (see "Bot
    /// lines" in the module docs). At most one line starts per call.
    async fn offer_line(&mut self, ctx: &mut ProcessorContext) -> CoreResult<()> {
        let Some(line) = self.lines.front() else {
            return Ok(());
        };
        if self.machine.barge_in_candidate().is_some() || self.held.is_some() {
            return Ok(());
        }
        let free = match self.machine.state() {
            TurnState::Idle => true,
            TurnState::AwaitingResponse | TurnState::BotSpeaking => line.interrupt,
            TurnState::UserSpeaking | TurnState::UserPaused(_) | TurnState::Closed => false,
        };
        if !free {
            return Ok(());
        }
        let Some(line) = self.lines.pop_front() else {
            return Ok(());
        };
        let interrupt = line.interrupt;
        self.line_pending = Some(line.text);
        let result = self
            .feed(
                TurnInput::BotTurn {
                    interrupt,
                    at: self.clock.now(),
                },
                ctx,
            )
            .await;
        if let Some(text) = self.line_pending.take() {
            // The machine started no bot turn (not expected after the checks): the line waits.
            self.lines.push_front(BotLine { text, interrupt });
        }
        result
    }

    /// The machine started a bot turn: the held line goes downstream as the epoch's text, which
    /// the runner speaks as the response.
    async fn start_bot_turn(
        &mut self,
        epoch: TurnEpoch,
        ctx: &mut ProcessorContext,
    ) -> CoreResult<()> {
        let now = self.clock.now();
        self.offered = None;
        self.close_user();
        self.after_interrupt = false;
        self.epoch = epoch;
        let Some(text) = self.line_pending.take() else {
            // No line held (not expected): close the machine's wait for a response.
            self.followups
                .push_back(TurnInput::BotAudioEnded { epoch, at: now });
            return Ok(());
        };
        bump(&self.counters.lines_started);
        self.response = Some(Response {
            epoch,
            deadline: brisvox_core::clock::deadline_after(now, self.config.response_deadline),
        });
        tracing::debug!(target: "brisvox::engine", epoch = epoch.get(), "turn driver: bot line");
        ctx.push(
            DataFrame::LlmText(TextFrame { epoch, text }).into(),
            Direction::Downstream,
        )
        .await
    }

    /// Offers `text` to the runner as a speculation candidate of the open turn (see the module
    /// docs). Nothing is sent when speculation is off, the text is empty or equal (normalised) to
    /// the candidate already out, or the turn's budget is spent.
    async fn offer(
        &mut self,
        epoch: TurnEpoch,
        text: &str,
        ctx: &mut ProcessorContext,
    ) -> CoreResult<()> {
        if !self.config.speculation.enabled {
            return Ok(());
        }
        let norm = normalize(text);
        if norm.is_empty() {
            return Ok(());
        }
        if self
            .offered
            .as_ref()
            .is_some_and(|(offered, out)| *offered == epoch && *out == norm)
        {
            return Ok(());
        }
        if self.offers_epoch != epoch {
            self.offers_epoch = epoch;
            self.offers = 0;
        }
        if self.offers >= self.config.speculation.max_per_turn {
            bump(&self.counters.speculations_capped);
            return Ok(());
        }
        self.offers += 1;
        self.offered = Some((epoch, norm));
        bump(&self.counters.speculations_offered);
        ctx.push(
            candidate_frame(epoch, text.trim(), self.clock.now()),
            Direction::Downstream,
        )
        .await
    }

    fn publish(&self, event: EngineEvent) {
        if let Some(events) = &self.events {
            events.publish(event);
        }
    }

    /// Closes the spoken user turn that started, if any (`UserStoppedSpeaking`).
    fn close_user(&mut self) {
        if let Some(epoch) = self.user_speaking.take() {
            self.publish(EngineEvent::UserStoppedSpeaking { epoch });
        }
    }

    /// Withdraws the candidate the runner holds, if any.
    async fn withdraw(&mut self, ctx: &mut ProcessorContext) -> CoreResult<()> {
        let Some((epoch, _)) = self.offered.take() else {
            return Ok(());
        };
        bump(&self.counters.speculations_withdrawn);
        ctx.push(
            withdrawal_frame(epoch, self.clock.now()),
            Direction::Downstream,
        )
        .await
    }

    /// The best text for a candidate at the VAD stop: the open turn's final text, plus the final
    /// segments heard since the last `UtteranceFinal`, else the latest interim.
    fn partial_text(&self) -> Option<(TurnEpoch, String)> {
        let turn = self.turn.as_ref()?;
        let mut text = turn.text.clone();
        let tail = if self.group.trim().is_empty() {
            &self.interim
        } else {
            &self.group
        };
        append_text(&mut text, tail);
        Some((turn.epoch, text))
    }

    /// Whether the stable-partial gate is configured (speculation on, from a partial, with a
    /// non-zero wait).
    fn gate_on(&self) -> bool {
        let speculation = &self.config.speculation;
        speculation.enabled && speculation.on_partial && !speculation.partial_stable_for.is_zero()
    }

    /// Records when the partial text of the open turn last changed (the stable-partial gate's
    /// clock). A change while a candidate waits out the gate restarts the wait. One normalised
    /// copy of the text per transcript frame, only while the gate is configured.
    fn note_partial(&mut self) {
        if !self.gate_on() {
            return;
        }
        let Some((epoch, text)) = self.partial_text() else {
            return;
        };
        let norm = normalize(&text);
        if self
            .partial_seen
            .as_ref()
            .is_some_and(|(seen, _)| *seen == norm)
        {
            return;
        }
        let now = self.clock.now();
        self.partial_seen = Some((norm, now));
        if let Some((gated, deadline)) = self.partial_gate.as_mut()
            && *gated == epoch
        {
            *deadline = brisvox_core::clock::deadline_after(
                now,
                self.config.speculation.partial_stable_for,
            );
        }
    }

    /// At the VAD stop that paused the machine: offers the best partial text at once, or, with
    /// the stable-partial gate, schedules it for `partial_stable_for` after now, the stop
    /// ([`Self::on_deadlines`] offers it if the caller stayed silent and the text did not change;
    /// [`Self::note_partial`] restarts the wait on a change).
    async fn partial_at_stop(&mut self, ctx: &mut ProcessorContext) -> CoreResult<()> {
        let Some((epoch, text)) = self.partial_text() else {
            return Ok(());
        };
        if !self.gate_on() {
            return self.offer(epoch, &text, ctx).await;
        }
        self.note_partial();
        let norm = normalize(&text);
        if norm.is_empty() {
            return Ok(());
        }
        let now = self.clock.now();
        self.partial_gate = Some((
            epoch,
            brisvox_core::clock::deadline_after(now, self.config.speculation.partial_stable_for),
        ));
        Ok(())
    }

    /// The stable-partial gate elapsed: offers the partial text if the turn is still paused (or
    /// already ended and held for the final, which the adaptive policy does within a few
    /// milliseconds of the stop: the ungated partial is offered before that, so the gated one
    /// must be offered then too), unanswered by a final, and the caller has not resumed (a
    /// resume or a final clears the gate; a turn that was released is gone).
    async fn offer_stable_partial(
        &mut self,
        epoch: TurnEpoch,
        ctx: &mut ProcessorContext,
    ) -> CoreResult<()> {
        let held_here = self.held.is_some_and(|held| held.epoch == epoch);
        if !held_here
            && (self.held.is_some() || !matches!(self.machine.state(), TurnState::UserPaused(_)))
        {
            return Ok(());
        }
        match self.partial_text() {
            Some((turn_epoch, text)) if turn_epoch == epoch => self.offer(epoch, &text, ctx).await,
            _ => Ok(()),
        }
    }

    async fn start_turn(&mut self, epoch: TurnEpoch, ctx: &mut ProcessorContext) -> CoreResult<()> {
        // A candidate of the previous turn is withdrawn by the `UserTurnStarted` that follows.
        self.offered = None;
        self.partial_gate = None;
        self.partial_seen = None;
        // A held end superseded here (or any turn not closed yet) stops before the new turn.
        self.close_user();
        let now = self.clock.now();
        // A held end of the previous turn is superseded: its text and its owed finals move to
        // the new turn.
        let mut carried = String::new();
        if let Some(held) = self.held.take() {
            bump(&self.counters.ends_superseded);
            if let Some(prior) = self.turn.take() {
                carried = prior.text;
            }
            for owed in &mut self.owed {
                if owed.turn == Some(held.epoch) {
                    owed.turn = Some(epoch);
                }
            }
        }
        // Typed text held for this turn, then the typed text that starts it.
        let held_text = std::mem::take(&mut self.text_hold);
        let mut text = carried;
        for typed in [Some(held_text), self.text_pending.take()]
            .into_iter()
            .flatten()
        {
            if !append_text(&mut text, &typed) {
                bump(&self.counters.text_rejected);
            }
        }
        self.turn = Some(OpenTurn { epoch, text });
        self.interim.clear();
        self.response = None;
        self.epoch = epoch;
        let typed = std::mem::take(&mut self.typed_start);
        if !std::mem::take(&mut self.after_interrupt) {
            let speech_start = if typed {
                now
            } else {
                self.vad_start_at.unwrap_or(now)
            };
            self.timeline
                .mark(epoch, TurnEvent::UserSpeechStart, speech_start);
        }
        bump(&self.counters.turns_started);
        if !typed {
            self.user_speaking = Some(epoch);
            self.publish(EngineEvent::UserStartedSpeaking { epoch });
        }
        ctx.push(
            control(epoch, ControlKind::UserTurnStarted),
            Direction::Downstream,
        )
        .await
    }

    async fn interrupt(
        &mut self,
        new_epoch: TurnEpoch,
        ctx: &mut ProcessorContext,
    ) -> CoreResult<()> {
        let now = self.clock.now();
        bump(&self.counters.interrupts);
        let speech_start = if self.line_pending.is_some() {
            // A bot line cuts the response: nobody started speaking.
            None
        } else if self.typed_start {
            Some(now)
        } else {
            self.vad_start_at
        };
        let old = self.epoch;
        self.timeline.interrupt(old, new_epoch, now, speech_start);
        self.after_interrupt = true;
        self.response = None;
        // The clear is applied when `send_clear` returns: that is when the bot's audio stopped
        // (the transport stage marks it again when the `Interrupt` reaches it; first wins).
        if self.send_clear(new_epoch).await && self.audio_epoch == Some(old) {
            self.timeline
                .mark(old, TurnEvent::AudioOutStopped, self.clock.now());
        }
        // A turn still open (a held end a typed message overtakes) stops before the barge-in.
        self.close_user();
        self.publish(EngineEvent::BotInterrupted { epoch: new_epoch });
        ctx.push(
            SystemFrame::Interrupt { new_epoch }.into(),
            Direction::Downstream,
        )
        .await
    }

    /// The fast-path transport clear, bounded by the send timeout. Returns whether it was
    /// applied.
    async fn send_clear(&self, epoch: TurnEpoch) -> bool {
        let Some(clear) = &self.clear else {
            return false;
        };
        let sent = tokio::select! {
            sent = clear(epoch) => Some(sent),
            () = brisvox_core::clock::wait_for(&*self.clock, self.config.send_timeout) => None,
        };
        match sent {
            Some(Ok(())) => {
                bump(&self.counters.clears_sent);
                true
            }
            Some(Err(error)) => {
                bump(&self.counters.clear_errors);
                tracing::warn!(target: "brisvox::engine", %error, "transport clear failed");
                false
            }
            None => {
                bump(&self.counters.clear_timeouts);
                tracing::warn!(target: "brisvox::engine", "transport clear timed out");
                false
            }
        }
    }

    fn request_analysis(&mut self, id: brisvox_agent::AnalysisId) {
        if self.analyzer.is_none() || self.me.is_none() {
            bump(&self.counters.analyses_unavailable);
            return;
        }
        if self.reuse_early(id) {
            return;
        }
        self.request_fresh_analysis(id);
    }

    /// The normal path of a request: wait for the ring to catch up with the stop, then analyse.
    fn request_fresh_analysis(&mut self, id: brisvox_agent::AnalysisId) {
        // The stop can overtake queued audio: wait until the ring holds the audio up to it.
        if let (Some(stop), Some(last)) = (self.stop_at, self.last_audio_at)
            && last < stop
        {
            bump(&self.counters.analyses_deferred);
            let deadline =
                brisvox_core::clock::deadline_after(self.clock.now(), ANALYSIS_DEFER_MAX);
            self.deferred_analysis = Some((id, deadline));
            return;
        }
        self.deferred_analysis = None;
        self.start_analysis(id);
    }

    fn start_analysis(&mut self, id: brisvox_agent::AnalysisId) {
        let Some(task) = self.launch(move |probability, samples| Event::Verdict {
            id,
            probability,
            samples,
        }) else {
            return;
        };
        bump(&self.counters.analyses_started);
        // A new analysis supersedes the previous one: its verdict would be stale.
        self.analysis = Some((id, task));
    }

    /// Snapshots the ring and runs one analysis on the offload in a helper task; `done` builds
    /// the event that carries its outcome back to the driver. `None` without a model.
    fn launch(
        &mut self,
        done: impl FnOnce(Option<f32>, Option<Vec<i16>>) -> Event + Send + 'static,
    ) -> Option<TaskGuard> {
        // The suppressed audio up to the VAD stop is in the feed (the job that produced the stop
        // pushed it first): the snapshot must hold it.
        self.apply_denoised();
        let (Some(analyzer), Some(me)) = (self.analyzer.as_mut(), &self.me) else {
            return None;
        };
        // Recycle the previous analysis' buffer: the copy of the ring is then a plain memcpy
        // (about 2 us for 8 s) instead of a fresh 256 KB allocation on this worker.
        let mut samples = analyzer.spare.take().unwrap_or_default();
        analyzer.ring.snapshot_into(&mut samples);
        let model = analyzer.ring.model();
        let offload = analyzer.offload.clone();
        let timeout = self.config.analysis_timeout;
        let slot = self.slot.clone();
        let me = me.clone();
        Some(TaskGuard::spawn("turn.analysis", async move {
            let outcome = offload
                .run(timeout, move || {
                    let probability =
                        SmartTurn::analyze_snapshot_blocking(model.as_ref(), &samples);
                    (samples, probability)
                })
                .await;
            let (probability, samples) = match outcome {
                Ok(done) => {
                    let (samples, probability) = done.value;
                    (probability.ok(), Some(samples))
                }
                Err(error) => {
                    // A busy executor makes one of these per turn: debug, not a warning per
                    // call (the executor's refused and timed-out analyses are exported as
                    // `brisvox_smart_turn_*`, the pool's panics are still loud).
                    if matches!(error, brisvox_inference::Error::JobPanicked) {
                        tracing::warn!(target: "brisvox::engine", %error, "smart turn analysis panicked");
                    } else {
                        tracing::debug!(target: "brisvox::engine", %error, "smart turn analysis failed");
                    }
                    (None, None)
                }
            };
            let _ = slot
                .push_and_wake(done(probability, samples), &me, WAKE_TOKEN)
                .await;
        }))
    }

    /// A falling hint (`at` is when the silence began): starts the speculative analysis if an
    /// adaptive user turn is open (see the module docs).
    fn speech_falling(&mut self, at: Instant) {
        if !self.config.early.enabled {
            return;
        }
        bump(&self.counters.early_hints);
        // A newer fall supersedes the previous one (the user spoke again in between).
        self.discard_early();
        if self.analyzer.is_none()
            || self.me.is_none()
            || !matches!(self.config.policy, TurnPolicy::Adaptive { .. })
            || !matches!(self.machine.state(), TurnState::UserSpeaking)
        {
            return;
        }
        if self.early_epoch != self.epoch {
            self.early_epoch = self.epoch;
            self.early_in_turn = 0;
        }
        if self.early_in_turn >= self.config.early.max_per_turn {
            bump(&self.counters.early_capped);
            return;
        }
        self.early_in_turn += 1;
        self.early_seq = self.early_seq.wrapping_add(1);
        // The hint travels on the system lane and can overtake queued audio: the snapshot must
        // hold the audio up to the silence start (the same wait as the normal path).
        let waiting = self.last_audio_at.is_some_and(|last| last < at);
        let stage = if waiting {
            EarlyStage::Waiting {
                deadline: brisvox_core::clock::deadline_after(self.clock.now(), ANALYSIS_DEFER_MAX),
            }
        } else {
            EarlyStage::Running
        };
        self.early = Some(Early {
            seq: self.early_seq,
            hint_at: at,
            stage,
            tail: 0,
            adopted: None,
            started: false,
            _task: None,
        });
        if !waiting {
            self.start_early();
        }
    }

    /// Starts the job of the early analysis in hand (its stage becomes `Running`).
    fn start_early(&mut self) {
        let Some(seq) = self.early.as_ref().map(|early| early.seq) else {
            return;
        };
        let task = self.launch(move |probability, samples| Event::EarlyVerdict {
            seq,
            probability,
            samples,
        });
        if let Some(early) = self.early.as_mut() {
            if task.is_some() {
                bump(&self.counters.early_started);
                early.started = true;
            }
            early.stage = EarlyStage::Running;
            early.tail = 0;
            early._task = task;
        }
    }

    /// Drops the early analysis, counting it as wasted when its job had started. EVERY drop of
    /// an analysis that was not reused and did not fail goes through here (a newer hint, a
    /// speech start, a reuse rule that does not hold, the end of the turn, `Cancel`, hangup),
    /// so that `early_started == early_reused + early_wasted + early_failed` once no analysis
    /// is in flight.
    fn discard_early(&mut self) {
        if let Some(early) = self.early.take()
            && early.started
        {
            bump(&self.counters.early_wasted);
        }
    }

    /// At the machine's request `id`: answers it from the early analysis when the reuse rule
    /// holds (see the module docs). `true` when the request is taken care of.
    fn reuse_early(&mut self, id: brisvox_agent::AnalysisId) -> bool {
        let Some(early) = self.early.as_ref() else {
            return false;
        };
        let same_pause = self.stop_at == Some(early.hint_at);
        let tail = frames_duration(early.tail);
        let usable = same_pause
            && tail <= self.config.early.max_tail
            && early.adopted.is_none()
            && !matches!(early.stage, EarlyStage::Waiting { .. });
        if !usable {
            self.discard_early();
            return false;
        }
        match early.stage {
            EarlyStage::Done(probability) => {
                self.early = None;
                bump(&self.counters.early_reused);
                bump(&self.counters.early_ready);
                // Not fed from here: the request is made while the machine handles the stop, and
                // a verdict that completes the turn at once would end it before `vad_stop` has
                // attributed the stop (and the final it owes) to the turn.
                self.early_ready = Some((id, probability));
            }
            EarlyStage::Running | EarlyStage::Waiting { .. } => {
                if let Some(early) = self.early.as_mut() {
                    early.adopted = Some(id);
                }
            }
        }
        true
    }

    /// Queues the early probability as the machine's verdict for `id`.
    fn feed_early_verdict(&mut self, id: brisvox_agent::AnalysisId, probability: f32) {
        bump(&self.counters.analyses_ok);
        self.note_verdict_latency();
        self.followups.push_back(TurnInput::Verdict {
            request: id,
            complete: probability > THRESHOLD,
            probability,
            at: self.clock.now(),
        });
    }

    /// Adds the time since the latest stop reached the driver to the latency statistics (once
    /// per stop).
    fn note_verdict_latency(&mut self) {
        if let Some(seen) = self.stop_seen_at.take() {
            let waited = self.clock.now().saturating_duration_since(seen);
            bump(&self.counters.verdicts_timed);
            self.counters.verdict_latency_us.fetch_add(
                u64::try_from(waited.as_micros()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
    }

    /// The outcome of the early job `seq` (see [`Event::EarlyVerdict`]).
    async fn early_verdict(
        &mut self,
        seq: u64,
        probability: Option<f32>,
        ctx: &mut ProcessorContext,
    ) -> CoreResult<()> {
        let Some(early) = self.early.as_mut().filter(|early| early.seq == seq) else {
            return Ok(());
        };
        early._task = None;
        let adopted = early.adopted;
        match (probability, adopted) {
            (Some(probability), Some(id)) => {
                self.early = None;
                bump(&self.counters.early_reused);
                self.feed_early_verdict(id, probability);
                self.drain_followups(ctx).await
            }
            (Some(probability), None) => {
                early.stage = EarlyStage::Done(probability);
                Ok(())
            }
            (None, adopted) => {
                self.early = None;
                bump(&self.counters.early_failed);
                if let Some(id) = adopted {
                    // The machine is waiting for this one: run the normal analysis now.
                    self.request_fresh_analysis(id);
                }
                Ok(())
            }
        }
    }

    async fn end_turn(
        &mut self,
        epoch: TurnEpoch,
        reason: brisvox_agent::EndReason,
        ctx: &mut ProcessorContext,
    ) -> CoreResult<()> {
        if reason == brisvox_agent::EndReason::Hangup {
            self.turn = None;
            self.held = None;
            self.close_user();
            return Ok(());
        }
        let deadline =
            brisvox_core::clock::deadline_after(self.clock.now(), self.config.final_wait);
        self.held = Some(Held {
            epoch,
            deadline,
            extended: false,
        });
        if self.pending_finals(epoch) {
            bump(&self.counters.ends_held);
        }
        self.try_release(false, ctx).await
    }

    /// Whether a stop of the turn `epoch` still owes its final.
    fn pending_finals(&self, epoch: TurnEpoch) -> bool {
        self.owed.iter().any(|owed| owed.turn == Some(epoch))
    }

    /// Whether the held turn has neither final text nor an interim to answer with.
    fn no_text(&self, held: Held) -> bool {
        let final_text_empty = self
            .turn
            .as_ref()
            .filter(|turn| turn.epoch == held.epoch)
            .is_none_or(|turn| turn.text.trim().is_empty());
        final_text_empty && self.interim.trim().is_empty()
    }

    /// Releases the held turn end if its finals are in (or `timed_out`).
    async fn try_release(&mut self, timed_out: bool, ctx: &mut ProcessorContext) -> CoreResult<()> {
        let Some(held) = self.held else {
            return Ok(());
        };
        let pending = self.pending_finals(held.epoch);
        if pending && !timed_out {
            return Ok(());
        }
        if pending && !held.extended && !self.config.late_final_wait.is_zero() && self.no_text(held)
        {
            // Nothing to answer with yet, and the STT stage will still answer this stop (really
            // or synthetically) before its own timeout: wait for it once instead of ending the
            // turn empty and dropping the words of a final that is only a little late.
            bump(&self.counters.gate_extensions);
            self.held = Some(Held {
                deadline: brisvox_core::clock::deadline_after(
                    self.clock.now(),
                    self.config.late_final_wait,
                ),
                extended: true,
                ..held
            });
            return Ok(());
        }
        self.held = None;
        self.deferred_analysis = None;
        let now = self.clock.now();
        if pending {
            bump(&self.counters.gate_timeouts);
            // The finals that never came belong to nobody any more.
            for owed in &mut self.owed {
                if owed.turn == Some(held.epoch) {
                    owed.turn = None;
                }
            }
        }
        let mut text = if self
            .turn
            .as_ref()
            .is_some_and(|turn| turn.epoch == held.epoch)
        {
            self.turn.take().map(|turn| turn.text).unwrap_or_default()
        } else {
            String::new()
        };
        if pending && text.trim().is_empty() && !self.interim.trim().is_empty() {
            bump(&self.counters.interim_fallbacks);
            text = std::mem::take(&mut self.interim);
        }
        self.interim.clear();
        self.timeline.mark(held.epoch, TurnEvent::TurnEnd, now);
        self.discard_early();
        if let Some(analyzer) = self.analyzer.as_mut() {
            analyzer.ring.clear();
        }
        let text = text.trim();
        if text.is_empty() {
            self.withdraw(ctx).await?;
            // Nothing to answer, but the user did stop.
            self.close_user();
            bump(&self.counters.empty_turns);
            // No request is made, so the bot will never report audio: close the machine's
            // wait for it ourselves.
            self.followups.push_back(TurnInput::BotAudioEnded {
                epoch: held.epoch,
                at: now,
            });
            return Ok(());
        }
        bump(&self.counters.turns_ended);
        // The runner decides at `UserTurnEnded` whether its candidate answers this turn.
        self.offered = None;
        self.response = Some(Response {
            epoch: held.epoch,
            deadline: brisvox_core::clock::deadline_after(now, self.config.response_deadline),
        });
        let transcript = TranscriptFrame {
            epoch: held.epoch,
            text: Arc::from(text),
            is_final: true,
            received_at: now,
        };
        self.publish(EngineEvent::UserText {
            epoch: held.epoch,
            text: Arc::clone(&transcript.text),
        });
        ctx.push(
            DataFrame::Transcript(transcript).into(),
            Direction::Downstream,
        )
        .await?;
        self.close_user();
        ctx.push(
            control(held.epoch, ControlKind::UserTurnEnded),
            Direction::Downstream,
        )
        .await
    }

    async fn vad_start(&mut self, at: Instant, ctx: &mut ProcessorContext) -> CoreResult<()> {
        bump(&self.counters.vad_starts);
        self.vad_start_at = Some(at);
        // The user resumed: a pause analysis still waiting for audio is stale, and so is a
        // speculation on what they said before.
        self.deferred_analysis = None;
        self.discard_early();
        // A partial still waiting out the stable-partial gate is never offered: the caller
        // resumed.
        self.partial_gate = None;
        self.withdraw(ctx).await?;
        if self.held.is_some() {
            // The turn end is held, so the machine already waits for a response that was never
            // requested: no bot audio exists, and a VadStart would be judged as a barge-in
            // (minimum speech, backchannel veto, a spurious `Interrupt` and transport `Clear`).
            // It is a resumed turn: close the machine's wait (the same synthesized
            // `BotAudioEnded` as for an empty turn), so the VadStart opens a new turn that
            // supersedes the held end (`start_turn`).
            let epoch = self.machine.epoch();
            self.feed(TurnInput::BotAudioEnded { epoch, at }, ctx)
                .await?;
        }
        self.feed(TurnInput::VadStart { at }, ctx).await
    }

    async fn vad_stop(&mut self, at: Instant, ctx: &mut ProcessorContext) -> CoreResult<()> {
        bump(&self.counters.vad_stops);
        self.stop_at = Some(at);
        self.stop_seen_at = Some(self.clock.now());
        if self.owed.len() >= MAX_OWED_STOPS {
            self.owed.pop_front();
            bump(&self.counters.owed_overflow);
        }
        self.owed.push_back(OwedStop { turn: None });
        self.feed(TurnInput::VadStop { at }, ctx).await?;
        // The machine took the stop into a turn exactly when it is paused now.
        if matches!(self.machine.state(), TurnState::UserPaused(_)) {
            let epoch = self.machine.epoch();
            if let Some(last) = self.owed.back_mut() {
                last.turn = Some(epoch);
            }
            self.timeline.mark(epoch, TurnEvent::UserSpeechEnd, at);
            if self.config.speculation.enabled && self.config.speculation.on_partial {
                self.partial_at_stop(ctx).await?;
            }
        }
        // An early verdict that was ready answers the machine's request now that the stop is
        // attributed (it may end the turn, which then waits for the stop's final).
        if let Some((id, probability)) = self.early_ready.take() {
            self.feed_early_verdict(id, probability);
            self.drain_followups(ctx).await?;
        }
        Ok(())
    }

    async fn transcript(
        &mut self,
        frame: TranscriptFrame,
        ctx: &mut ProcessorContext,
    ) -> CoreResult<()> {
        if !frame.is_final {
            self.interim.clear();
            self.interim.push_str(&frame.text);
            if let Some(turn) = &self.turn {
                self.timeline
                    .mark(turn.epoch, TurnEvent::SttInterim, frame.received_at);
            }
            self.note_partial();
            if self.bot_text.is_some() {
                let echo = self.is_echo(&frame.text, frame.received_at);
                self.machine.set_echo(echo);
            }
            return self
                .feed(
                    TurnInput::SttInterim {
                        text: frame.text.to_string(),
                        at: frame.received_at,
                    },
                    ctx,
                )
                .await;
        }
        if !append_text(&mut self.group, &frame.text) {
            bump(&self.counters.text_dropped);
        }
        self.note_partial();
        Ok(())
    }

    async fn utterance_final(&mut self, ctx: &mut ProcessorContext) -> CoreResult<()> {
        let now = self.clock.now();
        let text = std::mem::take(&mut self.group);
        self.interim.clear();
        match self.owed.pop_front() {
            None => bump(&self.counters.stray_finals),
            Some(owed) => {
                let appended = match (owed.turn, self.turn.as_mut()) {
                    (Some(epoch), Some(turn)) if turn.epoch == epoch => {
                        if !append_text(&mut turn.text, &text) {
                            bump(&self.counters.text_dropped);
                        }
                        self.timeline.mark(epoch, TurnEvent::SttFinal, now);
                        true
                    }
                    _ => false,
                };
                if !appended && !text.trim().is_empty() {
                    bump(&self.counters.late_finals);
                }
            }
        }
        if self.owed.is_empty() {
            // The final of the last stop owed supersedes a partial that waits for the gate
            // (`offer_final` below offers the final text if the turn is still open).
            self.partial_gate = None;
            self.partial_seen = None;
        } else {
            // A late final of an earlier stop must not cancel the gate of a later stop of the
            // turn: that stop still owes its own final, which supersedes the gate when it comes.
            // The final text may have changed the partial text: that restarts the wait.
            self.note_partial();
        }
        self.feed(TurnInput::SttFinal { text, at: now }, ctx)
            .await?;
        self.offer_final(ctx).await?;
        self.try_release(false, ctx).await?;
        self.drain_followups(ctx).await
    }

    /// At a final transcript: if the machine is paused in an open turn whose stops are all
    /// answered, the turn's final text is a candidate (the end may follow at any moment).
    async fn offer_final(&mut self, ctx: &mut ProcessorContext) -> CoreResult<()> {
        if !self.config.speculation.enabled
            || self.held.is_some()
            || !matches!(self.machine.state(), TurnState::UserPaused(_))
        {
            return Ok(());
        }
        let Some(turn) = &self.turn else {
            return Ok(());
        };
        let epoch = turn.epoch;
        if self.pending_finals(epoch) {
            return Ok(());
        }
        let text = turn.text.clone();
        self.offer(epoch, &text, ctx).await
    }

    fn audio(&mut self, audio: &brisvox_core::frame::AudioFrame) {
        bump(&self.counters.audio_frames);
        let Some(analyzer) = self.analyzer.as_mut() else {
            return;
        };
        if audio.sample_rate() != self.config.model_rate {
            bump(&self.counters.audio_wrong_rate);
            return;
        }
        self.scratch.clear();
        self.scratch.extend(
            audio
                .data()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| i16::from_le_bytes(*pair)),
        );
        analyzer.ring.push(&self.scratch);
        let at = audio.timestamp();
        self.last_audio_at = Some(at);
        let pushed = self.scratch.len();
        if self.denoised.is_some() {
            // Only the enhanced audio needs to know where the ring ends.
            let hz = u64::from(self.config.model_rate.hz()).max(1);
            self.ring_end = Some(
                at + Duration::from_micros(u64::try_from(pushed).unwrap_or(0) * 1_000_000 / hz),
            );
            self.apply_denoised();
        }
        let mut start_early = false;
        if let Some(early) = self.early.as_mut() {
            match early.stage {
                // The audio caught up with the silence start: snapshot now.
                EarlyStage::Waiting { .. } if at >= early.hint_at => start_early = true,
                EarlyStage::Waiting { .. } => {}
                EarlyStage::Running | EarlyStage::Done(_) => {
                    early.tail = early.tail.saturating_add(pushed);
                }
            }
        }
        if start_early {
            self.start_early();
        }
        if let (Some((id, _)), Some(stop)) = (self.deferred_analysis, self.stop_at)
            && at >= stop
        {
            self.deferred_analysis = None;
            self.start_analysis(id);
        }
    }

    async fn wake(&mut self, ctx: &mut ProcessorContext) -> CoreResult<()> {
        for typed in self.text_slot.drain() {
            self.typed(typed, ctx).await?;
        }
        for line in self.line_slot.drain() {
            if line.interrupt {
                // An interrupting line overrides everything that waits (see `BotLine`).
                for _ in self.lines.drain(..) {
                    bump(&self.counters.lines_rejected);
                }
            }
            if self.lines.len() < MAX_HELD_LINES && self.machine.state() != TurnState::Closed {
                self.lines.push_back(line);
            } else {
                bump(&self.counters.lines_rejected);
            }
        }
        for event in self.slot.drain() {
            if let Event::EarlyVerdict {
                seq,
                probability,
                samples,
            } = event
            {
                if let (Some(analyzer), Some(samples)) = (self.analyzer.as_mut(), samples) {
                    analyzer.spare = Some(samples);
                }
                self.early_verdict(seq, probability, ctx).await?;
                continue;
            }
            if let Event::Verdict {
                id,
                probability,
                samples,
            } = event
            {
                if let (Some(analyzer), Some(samples)) = (self.analyzer.as_mut(), samples) {
                    analyzer.spare = Some(samples);
                }
                if self.analysis.as_ref().is_some_and(|(live, _)| *live == id) {
                    self.analysis = None;
                }
                match probability {
                    Some(probability) => {
                        bump(&self.counters.analyses_ok);
                        self.note_verdict_latency();
                        let input = TurnInput::Verdict {
                            request: id,
                            complete: probability > THRESHOLD,
                            probability,
                            at: self.clock.now(),
                        };
                        self.feed(input, ctx).await?;
                    }
                    None => bump(&self.counters.analyses_failed),
                }
            }
        }
        self.on_deadlines(ctx).await
    }

    /// Handles every deadline that has come. Level-triggered: it does not matter which event
    /// woke the driver.
    async fn on_deadlines(&mut self, ctx: &mut ProcessorContext) -> CoreResult<()> {
        let now = self.clock.now();
        if self
            .machine_deadline
            .is_some_and(|deadline| deadline <= now)
        {
            self.machine_deadline = None;
            self.feed(TurnInput::Tick { now }, ctx).await?;
        }
        if let Some((id, deadline)) = self.deferred_analysis
            && deadline <= now
        {
            self.deferred_analysis = None;
            self.start_analysis(id);
        }
        if let Some((epoch, deadline)) = self.partial_gate
            && deadline <= now
        {
            self.partial_gate = None;
            self.offer_stable_partial(epoch, ctx).await?;
        }
        if self.early.as_ref().is_some_and(
            |early| matches!(early.stage, EarlyStage::Waiting { deadline } if deadline <= now),
        ) {
            // A leg that suppresses silence sends no frame after the silence start: the model
            // sees the audio there is.
            self.start_early();
        }
        if self.held.is_some_and(|held| held.deadline <= now) {
            self.try_release(true, ctx).await?;
            // A released end may have queued a follow-up input (an empty turn's `BotAudioEnded`).
            self.drain_followups(ctx).await?;
        }
        if let Some(response) = self.response
            && response.deadline <= now
        {
            self.response = None;
            bump(&self.counters.response_timeouts);
            tracing::warn!(
                target: "brisvox::engine",
                epoch = response.epoch.get(),
                "no bot audio ended in time: closing the turn"
            );
            self.feed(
                TurnInput::BotAudioEnded {
                    epoch: response.epoch,
                    at: now,
                },
                ctx,
            )
            .await?;
        }
        Ok(())
    }

    /// Processes inputs queued by `try_release` outside a `feed` call.
    async fn drain_followups(&mut self, ctx: &mut ProcessorContext) -> CoreResult<()> {
        while let Some(input) = self.followups.pop_front() {
            self.feed(input, ctx).await?;
        }
        Ok(())
    }

    /// A typed user message (the rules are in [`crate::text_input`]).
    async fn typed(&mut self, typed: TextTurn, ctx: &mut ProcessorContext) -> CoreResult<()> {
        let text = typed.text.trim();
        let state = self.machine.state();
        if text.is_empty() || state == TurnState::Closed {
            bump(&self.counters.text_rejected);
            return Ok(());
        }
        if matches!(state, TurnState::UserSpeaking | TurnState::UserPaused(_)) {
            let joined = self
                .turn
                .as_mut()
                .is_some_and(|turn| append_text(&mut turn.text, text));
            bump(if joined {
                &self.counters.text_joined
            } else {
                &self.counters.text_rejected
            });
            return Ok(());
        }
        if !typed.run_immediately {
            let held = append_text(&mut self.text_hold, text);
            bump(if held {
                &self.counters.text_held
            } else {
                &self.counters.text_rejected
            });
            return Ok(());
        }
        // A pending barge-in candidate is speech: the machine confirms it and the text joins
        // that (spoken) turn; otherwise the turn is the typed message alone.
        let spoken = self.machine.barge_in_candidate().is_some();
        if spoken {
            bump(&self.counters.text_joined);
        } else {
            bump(&self.counters.text_turns);
        }
        self.text_pending = Some(text.to_owned());
        self.typed_start = !spoken;
        let result = self
            .feed(
                TurnInput::UserText {
                    at: self.clock.now(),
                },
                ctx,
            )
            .await;
        self.typed_start = false;
        if let Some(unused) = self.text_pending.take() {
            // The machine started no turn (not expected in these states): keep the text for
            // the next one.
            if !append_text(&mut self.text_hold, &unused) {
                bump(&self.counters.text_rejected);
            }
        }
        result
    }

    async fn hangup(&mut self, ctx: &mut ProcessorContext) -> CoreResult<()> {
        let now = self.clock.now();
        self.analysis = None;
        self.deferred_analysis = None;
        self.discard_early();
        self.early_ready = None;
        self.held = None;
        self.response = None;
        self.offered = None;
        self.partial_gate = None;
        self.partial_seen = None;
        self.machine_deadline = None;
        self.close_user();
        for _ in self.lines.drain(..) {
            bump(&self.counters.lines_rejected);
        }
        self.feed(TurnInput::Hangup { at: now }, ctx).await?;
        self.turn = None;
        self.machine_deadline = None;
        Ok(())
    }

    async fn downstream(&mut self, frame: Frame, ctx: &mut ProcessorContext) -> CoreResult<()> {
        match frame {
            Frame::Data(DataFrame::Audio(audio)) => {
                self.audio(&audio);
                Ok(())
            }
            Frame::Data(DataFrame::Transcript(transcript)) => {
                self.transcript(transcript, ctx).await
            }
            Frame::Data(DataFrame::Dtmf(dtmf)) => {
                bump(&self.counters.dtmf);
                let retagged = DtmfFrame {
                    epoch: self.machine.epoch(),
                    ..dtmf
                };
                ctx.push(DataFrame::Dtmf(retagged).into(), Direction::Downstream)
                    .await
            }
            Frame::Control(ControlFrame {
                kind: ControlKind::UtteranceFinal,
                ..
            }) => self.utterance_final(ctx).await,
            Frame::System(SystemFrame::UserStartedSpeaking { at }) => self.vad_start(at, ctx).await,
            Frame::System(SystemFrame::UserStoppedSpeaking { at }) => self.vad_stop(at, ctx).await,
            Frame::System(SystemFrame::UserSpeechFalling { at }) => {
                self.speech_falling(at);
                Ok(())
            }
            Frame::System(SystemFrame::Wake { .. }) => self.wake(ctx).await,
            Frame::System(SystemFrame::Start) => {
                self.start_warm();
                ctx.forward(SystemFrame::Start.into(), Direction::Downstream)
                    .await
            }
            Frame::System(SystemFrame::Stop) => {
                self.hangup(ctx).await?;
                ctx.forward(SystemFrame::Stop.into(), Direction::Downstream)
                    .await
            }
            Frame::System(SystemFrame::Cancel) => {
                self.analysis = None;
                self.deferred_analysis = None;
                self.discard_early();
                ctx.forward(SystemFrame::Cancel.into(), Direction::Downstream)
                    .await
            }
            other => ctx.forward(other, Direction::Downstream).await,
        }
    }

    async fn upstream(&mut self, frame: Frame, ctx: &mut ProcessorContext) -> CoreResult<()> {
        let now = self.clock.now();
        match frame {
            Frame::Control(ControlFrame {
                epoch,
                kind: ControlKind::TtsStarted,
            }) => {
                // Audio is playing: the "no audio" backstop has done its job. The end comes with
                // `TtsEnded` (the transport's mark timeout guards a client that never acks).
                self.audio_epoch = Some(epoch);
                if self
                    .response
                    .is_some_and(|response| response.epoch == epoch)
                {
                    self.response = None;
                }
                self.feed(TurnInput::BotAudioStarted { epoch, at: now }, ctx)
                    .await
            }
            Frame::Control(ControlFrame {
                epoch,
                kind: ControlKind::TtsEnded,
            }) => {
                if self
                    .response
                    .is_some_and(|response| response.epoch == epoch)
                {
                    self.response = None;
                }
                self.feed(TurnInput::BotAudioEnded { epoch, at: now }, ctx)
                    .await
            }
            other => {
                // Upstream traffic is never awaited from `process` (processor.rs, "Pushing
                // upstream without deadlock").
                if let Some(upstream) = ctx.sender(Direction::Upstream)
                    && upstream.try_send(other).is_err()
                {
                    bump(&self.counters.upstream_dropped);
                }
                Ok(())
            }
        }
    }
}

impl std::fmt::Debug for TurnDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnDriver")
            .field("state", &self.machine.state().kind())
            .field("epoch", &self.machine.epoch())
            .finish_non_exhaustive()
    }
}

impl Processor for TurnDriver {
    fn name(&self) -> &'static str {
        "turn_driver"
    }

    /// The driver originates epochs; user-side frames all carry epoch zero.
    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        frame: Frame,
        direction: Direction,
        ctx: &mut ProcessorContext,
    ) -> CoreResult<()> {
        self.ensure_started(ctx);
        let mut result = match direction {
            Direction::Downstream => self.downstream(frame, ctx).await,
            Direction::Upstream => self.upstream(frame, ctx).await,
        };
        if result.is_ok() && !self.lines.is_empty() {
            result = self.offer_line(ctx).await;
        }
        self.sync_timer();
        result
    }
}
