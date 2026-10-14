//! Turn-taking state machine.
//!
//! [`TurnMachine`] decides when a user turn starts, when it ends and when the bot may speak. It is
//! pure and synchronous: every input carries its time as a [`tokio::time::Instant`] (a reading of
//! the injected clock), it does no IO and reads no clock. Its only output is the list of
//! [`TurnAction`]s appended by [`TurnMachine::handle`]; the caller carries them out. The caller
//! owns the timer slot (at most one pending deadline, see [`TurnAction::ArmTimer`]), runs the
//! Smart Turn analysis on the inference pool ([`TurnAction::RequestAnalysis`]) and emits the
//! Control and System frames.
//!
//! A `VadStart` while the bot is thinking or speaking is a *candidate* barge-in: the
//! machine asks its [`InterruptionPolicy`] whether it is real. See [`BargeInCandidate`].
//!
//! The state diagram and the policies are documented in `docs/ARCHITECTURE.md`, section
//! "Turn-taking".

use std::time::Duration;

use brisvox_core::TurnEpoch;
use tokio::time::Instant;

use crate::interrupt::{
    Assessment, InterruptionPolicy, Observation, TextKind, VetoReason, count_words,
};

/// Offset used when a deadline would overflow the clock's range (about 100 years).
const FAR: Duration = Duration::from_secs(100 * 365 * 24 * 3600);

/// Default silence after which an incomplete Smart Turn verdict ends the turn anyway. Matches
/// Pipecat's `SmartTurnParams.stop_secs` (see `docs/providers/smart-turn.md`).
pub const DEFAULT_FALLBACK_SILENCE: Duration = Duration::from_secs(3);

/// Default wait for an STT final after a complete verdict (when required), or after a final with
/// sentence-final punctuation while the verdict is incomplete.
pub const DEFAULT_FINAL_GRACE: Duration = Duration::from_millis(800);

/// `at + d`, clamped instead of panicking when the clock range would overflow: `d` itself, else
/// [`FAR`] (about 100 years), else progressively smaller offsets down to one hour. Only when even
/// that does not fit (a clock reading at the very end of its range) is `at` returned.
fn after(at: Instant, d: Duration) -> Instant {
    const LADDER: [Duration; 4] = [
        FAR,
        Duration::from_secs(10 * 365 * 24 * 3600),
        Duration::from_secs(24 * 3600),
        Duration::from_secs(3600),
    ];
    if let Some(deadline) = at.checked_add(d) {
        return deadline;
    }
    LADDER
        .iter()
        .find_map(|offset| at.checked_add(*offset))
        .unwrap_or(at)
}

/// How the end of a user turn is decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnPolicy {
    /// The turn ends once VAD has been silent for `silence` after the VAD stop. The VAD stop
    /// time is already backdated by the VAD's minimum silence, so `silence` counts from the
    /// start of the silence, not from the moment VAD reported it.
    FixedSilence {
        /// Silence after the VAD stop that ends the turn.
        silence: Duration,
    },
    /// Smart Turn decides. On every VAD stop a Smart Turn analysis is requested.
    ///
    /// * A complete verdict ends the turn at once; with `require_final` it waits for the STT final
    ///   of this pause (up to `final_grace` after the verdict) first.
    /// * An incomplete verdict (or none yet) keeps the pause open until `fallback_silence` after
    ///   the VAD stop, which then ends the turn.
    /// * The STT final of this pause, when its text ends with sentence-final punctuation (`. ? !
    ///   …`, trailing whitespace ignored) and the verdict is incomplete, shortens the wait to
    ///   `final_grace` after that final, when that is earlier than the fallback deadline. An
    ///   unpunctuated final never shortens the wait. Only one final per pause counts: the one
    ///   answering this pause's VAD stop (see [`TurnInput::SttFinal`]).
    /// * A verdict or final for an analysis or pause that is no longer current is ignored. A
    ///   shortened wait never extends past the fallback deadline.
    Adaptive {
        /// Silence after the VAD stop that ends the turn when Smart Turn does not complete it.
        fallback_silence: Duration,
        /// Whether a complete verdict also waits for an STT final.
        require_final: bool,
        /// How long the STT signals may take to arrive (see the variant docs above).
        final_grace: Duration,
    },
}

impl TurnPolicy {
    /// A fixed-silence policy.
    pub const fn fixed(silence: Duration) -> Self {
        Self::FixedSilence { silence }
    }

    /// The adaptive policy with the default fallback (3 s), no required STT final and an 800 ms
    /// final grace.
    pub const fn adaptive() -> Self {
        Self::Adaptive {
            fallback_silence: DEFAULT_FALLBACK_SILENCE,
            require_final: false,
            final_grace: DEFAULT_FINAL_GRACE,
        }
    }
}

/// Identifies one Smart Turn analysis. The caller echoes it back in [`TurnInput::Verdict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AnalysisId(u64);

impl AnalysisId {
    /// An id with the given number (the machine hands out 1, 2, ... itself).
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The number of this id.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Why a user turn ended. Carried by [`TurnAction::EndUserTurn`] for timelines and metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The fixed silence elapsed.
    FixedSilence,
    /// Smart Turn completed the turn (and, when required, the STT final arrived in time).
    Verdict,
    /// The verdict completed the turn, but the required STT final did not arrive within
    /// `final_grace` (or the fallback deadline came first).
    FinalGrace,
    /// The fallback silence elapsed with no complete verdict.
    Fallback,
    /// An STT final with sentence-final punctuation and an incomplete verdict; `final_grace`
    /// elapsed after that final.
    SttPunctuation,
    /// The session hung up while the user turn was open. The caller emits `UserTurnEnded` and
    /// does not start a response.
    Hangup,
    /// The user typed a message ([`TurnInput::UserText`]): the turn ended as soon as it
    /// started. The caller emits `UserTurnEnded` with the typed text and starts the response.
    Text,
}

/// An STT final that arrived during the current pause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FinalSeen {
    /// When the final was received.
    pub at: Instant,
    /// Whether its text ends with sentence-final punctuation.
    pub ends_sentence: bool,
}

/// Smart Turn progress within one pause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Analysis {
    /// Fixed-silence policy: no analysis is requested.
    NotRequested,
    /// Requested; this id is the only one whose verdict is accepted.
    Pending(AnalysisId),
    /// The verdict was incomplete (at this time).
    Incomplete {
        /// When the verdict was observed.
        at: Instant,
    },
    /// The verdict was complete, and the turn waits for the required STT final.
    Complete {
        /// When the verdict was observed.
        at: Instant,
    },
}

/// Data of [`TurnState::UserPaused`]: the candidate end of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pause {
    vad_stop_at: Instant,
    analysis: Analysis,
    stt: Option<FinalSeen>,
}

impl Pause {
    /// When VAD stopped (backdated by the VAD's minimum silence).
    pub fn vad_stop_at(&self) -> Instant {
        self.vad_stop_at
    }

    /// The Smart Turn progress of this pause.
    pub fn analysis(&self) -> Analysis {
        self.analysis
    }

    /// The STT final that answered this pause, if one arrived.
    pub fn final_seen(&self) -> Option<FinalSeen> {
        self.stt
    }

    /// The deadline that ends this pause if nothing else happens, and the reason it would give.
    fn deadline(&self, policy: TurnPolicy) -> (Instant, EndReason) {
        match policy {
            TurnPolicy::FixedSilence { silence } => {
                (after(self.vad_stop_at, silence), EndReason::FixedSilence)
            }
            TurnPolicy::Adaptive {
                fallback_silence,
                final_grace,
                ..
            } => {
                let base = after(self.vad_stop_at, fallback_silence);
                let (base_reason, shortened) = match self.analysis {
                    Analysis::Complete { at } => (
                        EndReason::FinalGrace,
                        Some((after(at, final_grace), EndReason::FinalGrace)),
                    ),
                    Analysis::Incomplete { .. } => (
                        EndReason::Fallback,
                        self.stt
                            .filter(|f| f.ends_sentence)
                            .map(|f| (after(f.at, final_grace), EndReason::SttPunctuation)),
                    ),
                    Analysis::NotRequested | Analysis::Pending(_) => (EndReason::Fallback, None),
                };
                match shortened {
                    Some((deadline, reason)) if deadline < base => (deadline, reason),
                    _ => (base, base_reason),
                }
            }
        }
    }
}

/// The turn machine's state. Data that only exists in a state is carried by that state, so an
/// analysis cannot be pending outside a pause, and a verdict is matched to its pause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    /// No user turn is open and the bot is silent.
    Idle,
    /// The user is speaking (VAD active).
    UserSpeaking,
    /// VAD stopped: a candidate end of turn, open until the caller's deadline or a verdict.
    UserPaused(Pause),
    /// The user turn ended; waiting for the bot's first audio (or for the bot to report none).
    AwaitingResponse,
    /// The bot is playing audio of the current epoch.
    BotSpeaking,
    /// The session hung up. Terminal: every input is ignored.
    Closed,
}

impl TurnState {
    /// The state without its data, for tables and statistics.
    pub fn kind(&self) -> StateKind {
        match self {
            Self::Idle => StateKind::Idle,
            Self::UserSpeaking => StateKind::UserSpeaking,
            Self::UserPaused(_) => StateKind::UserPaused,
            Self::AwaitingResponse => StateKind::AwaitingResponse,
            Self::BotSpeaking => StateKind::BotSpeaking,
            Self::Closed => StateKind::Closed,
        }
    }
}

/// A state without its data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StateKind {
    /// See [`TurnState::Idle`].
    Idle,
    /// See [`TurnState::UserSpeaking`].
    UserSpeaking,
    /// See [`TurnState::UserPaused`].
    UserPaused,
    /// See [`TurnState::AwaitingResponse`].
    AwaitingResponse,
    /// See [`TurnState::BotSpeaking`].
    BotSpeaking,
    /// See [`TurnState::Closed`].
    Closed,
}

impl StateKind {
    /// Every state kind.
    pub const ALL: [StateKind; 6] = [
        StateKind::Idle,
        StateKind::UserSpeaking,
        StateKind::UserPaused,
        StateKind::AwaitingResponse,
        StateKind::BotSpeaking,
        StateKind::Closed,
    ];
}

/// Inputs to the machine. Every input carries its time (or the time it refers to).
#[derive(Debug, Clone, PartialEq)]
pub enum TurnInput {
    /// VAD detected speech. `at` is the speech start.
    VadStart {
        /// When speech started.
        at: Instant,
    },
    /// VAD detected the end of speech. `at` is the backdated stop (start of the silence).
    VadStop {
        /// When the silence started.
        at: Instant,
    },
    /// An STT interim transcript of the current utterance (cumulative: each interim is the whole
    /// text so far). Accepted while the user speaks or pauses, where it has no effect, and
    /// while a barge-in candidate is pending, where the interruption policy reads it (word
    /// count, backchannels, echo guard). Otherwise ignored and counted.
    SttInterim {
        /// The interim text.
        text: String,
        /// When it was received.
        at: Instant,
    },
    /// The STT final of one VAD stop. The caller sends exactly one per [`TurnInput::VadStop`],
    /// in the same order (the STT stage finalizes on every VAD stop, as the reference
    /// `TurnManager` does). A final answers the oldest VAD stop it has not answered yet. Only the
    /// final of the current pause can shorten or end that pause; a final with no unanswered VAD
    /// stop is ignored and counted.
    SttFinal {
        /// The final text.
        text: String,
        /// When it was received.
        at: Instant,
    },
    /// The result of a [`TurnAction::RequestAnalysis`]. Ignored unless `request` is the current
    /// analysis of the current pause.
    Verdict {
        /// The analysis this verdict answers.
        request: AnalysisId,
        /// Whether Smart Turn judged the turn complete (`probability > threshold`).
        complete: bool,
        /// The probability (informational; the decision is `complete`).
        probability: f32,
        /// When the verdict was received.
        at: Instant,
    },
    /// A timer fired: the caller delivers this at (or after) the armed deadline. A tick earlier
    /// than the deadline re-arms it.
    Tick {
        /// The clock reading when the timer fired.
        now: Instant,
    },
    /// The bot's audio of `epoch` started playing. Send it at the start of the response's audio
    /// and again for audio that resumes after a gap (a new TTS round after a tool call or a
    /// filler). With the echo guard on, a repeat restarts the echo window only when it comes at
    /// least `echo_guard` after the previous `BotAudioStarted` of the epoch; closer together
    /// (for example one per audio chunk) or not later than the previous one it is ignored
    /// without effect, so per-chunk reporting is harmless. The window counts from the latest
    /// restart. With the echo guard off, repeats are harmless too.
    BotAudioStarted {
        /// The epoch the audio belongs to.
        epoch: TurnEpoch,
        /// When it started.
        at: Instant,
    },
    /// The bot's audio of `epoch` ended. Also sent while awaiting a response when the response
    /// produced no audio at all (for example an empty transcript, where no LLM request is made):
    /// without it the machine waits in `AwaitingResponse` and a later `VadStart` is a barge-in.
    BotAudioEnded {
        /// The epoch the audio belonged to.
        epoch: TurnEpoch,
        /// When it ended.
        at: Instant,
    },
    /// The session hung up. Moves to the terminal [`TurnState::Closed`].
    Hangup {
        /// When the hang-up happened.
        at: Instant,
    },
    /// The user sent a typed message (an RTVI `send-text`). The caller holds the text;
    /// the machine only decides the turn:
    /// - `Idle`, `AwaitingResponse`, `BotSpeaking` without a pending barge-in: a new user turn
    ///   that ends at once (`StartUserTurn` then `EndUserTurn` with [`EndReason::Text`], and an
    ///   `Interrupt` first while the bot is thinking or speaking). The state becomes
    ///   `AwaitingResponse`.
    /// - the same states with a pending barge-in candidate: the user is talking, so the
    ///   candidate is confirmed (a barge-in, or an ordinary turn when detached) and the state
    ///   becomes `UserSpeaking`; the caller adds the text to that turn, which ends as usual.
    /// - `UserSpeaking`, `UserPaused`, `Closed`: ignored (the caller adds the text to the open
    ///   turn, or drops it after the hang-up).
    UserText {
        /// When the message arrived.
        at: Instant,
    },
    /// The application wants the bot to speak a line of its own (an agent process's
    /// `say`). The caller holds the text; the machine only decides the turn:
    /// - `Idle` without a pending barge-in candidate: a bot turn starts in a new epoch
    ///   ([`TurnAction::StartBotTurn`]) and the state becomes `AwaitingResponse`, so the line's
    ///   audio moves the machine to `BotSpeaking` and the caller can barge in on it as on any
    ///   response.
    /// - `AwaitingResponse` or `BotSpeaking` without a candidate, with `interrupt`: the bot's
    ///   response is cut first (`Interrupt`, then `StartBotTurn` in the new epoch).
    /// - every other case (the user is speaking, a candidate is pending, the bot is busy and
    ///   `interrupt` is off, `Closed`): ignored; the caller keeps the line and offers it again
    ///   once the machine is `Idle`.
    BotTurn {
        /// Whether a running response may be cut for the line.
        interrupt: bool,
        /// When the line was asked for.
        at: Instant,
    },
}

/// Input kinds, for statistics and tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputKind {
    /// See [`TurnInput::VadStart`].
    VadStart,
    /// See [`TurnInput::VadStop`].
    VadStop,
    /// See [`TurnInput::SttInterim`].
    SttInterim,
    /// See [`TurnInput::SttFinal`].
    SttFinal,
    /// See [`TurnInput::Verdict`].
    Verdict,
    /// See [`TurnInput::Tick`].
    Tick,
    /// See [`TurnInput::BotAudioStarted`].
    BotAudioStarted,
    /// See [`TurnInput::BotAudioEnded`].
    BotAudioEnded,
    /// See [`TurnInput::Hangup`].
    Hangup,
    /// See [`TurnInput::UserText`].
    UserText,
    /// See [`TurnInput::BotTurn`].
    BotTurn,
}

impl InputKind {
    /// Every input kind.
    pub const ALL: [InputKind; 11] = [
        InputKind::VadStart,
        InputKind::VadStop,
        InputKind::SttInterim,
        InputKind::SttFinal,
        InputKind::Verdict,
        InputKind::Tick,
        InputKind::BotAudioStarted,
        InputKind::BotAudioEnded,
        InputKind::Hangup,
        InputKind::UserText,
        InputKind::BotTurn,
    ];

    /// The slot of this kind in [`TurnStats`]. Exhaustive, so a new variant cannot share a slot.
    fn index(self) -> usize {
        match self {
            Self::VadStart => 0,
            Self::VadStop => 1,
            Self::SttInterim => 2,
            Self::SttFinal => 3,
            Self::Verdict => 4,
            Self::Tick => 5,
            Self::BotAudioStarted => 6,
            Self::BotAudioEnded => 7,
            Self::Hangup => 8,
            Self::UserText => 9,
            Self::BotTurn => 10,
        }
    }
}

/// Number of [`InputKind`] slots in [`TurnStats`].
const INPUT_KINDS: usize = 11;

impl TurnInput {
    /// The kind of this input.
    pub fn kind(&self) -> InputKind {
        match self {
            Self::VadStart { .. } => InputKind::VadStart,
            Self::VadStop { .. } => InputKind::VadStop,
            Self::SttInterim { .. } => InputKind::SttInterim,
            Self::SttFinal { .. } => InputKind::SttFinal,
            Self::Verdict { .. } => InputKind::Verdict,
            Self::Tick { .. } => InputKind::Tick,
            Self::BotAudioStarted { .. } => InputKind::BotAudioStarted,
            Self::BotAudioEnded { .. } => InputKind::BotAudioEnded,
            Self::Hangup { .. } => InputKind::Hangup,
            Self::UserText { .. } => InputKind::UserText,
            Self::BotTurn { .. } => InputKind::BotTurn,
        }
    }
}

/// What the caller must do after a transition. Actions are appended in the order they must be
/// carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnAction {
    /// A user turn started in `epoch`: emit `Control::UserTurnStarted`.
    StartUserTurn {
        /// The epoch of the new turn.
        epoch: TurnEpoch,
    },
    /// Run a Smart Turn analysis on the inference pool and feed the result back as
    /// [`TurnInput::Verdict`] with this id. The machine does not cancel it: a late verdict is
    /// ignored.
    RequestAnalysis {
        /// The id to echo back.
        id: AnalysisId,
    },
    /// The user turn in `epoch` ended for `reason`: emit `Control::UserTurnEnded` and start the
    /// response. For [`EndReason::Hangup`] the caller emits the end but starts no response.
    EndUserTurn {
        /// The epoch of the turn that ended.
        epoch: TurnEpoch,
        /// Why it ended.
        reason: EndReason,
    },
    /// A barge-in: emit `System::Interrupt { new_epoch }`. Always followed by `StartUserTurn`
    /// (or, for [`TurnInput::BotTurn`], `StartBotTurn`) in the same epoch.
    Interrupt {
        /// The epoch the session moves to.
        new_epoch: TurnEpoch,
    },
    /// Make `deadline` the one pending timer, replacing any earlier one. Deliver
    /// [`TurnInput::Tick`] at (or after) it.
    ArmTimer {
        /// When the timer must fire.
        deadline: Instant,
    },
    /// Cancel the pending timer (there is one, by construction).
    CancelTimer,
    /// A bot turn started in `epoch` ([`TurnInput::BotTurn`]): speak the line the caller holds
    /// as the response of this epoch. Its audio is reported as for any response
    /// ([`TurnInput::BotAudioStarted`], [`TurnInput::BotAudioEnded`]).
    StartBotTurn {
        /// The epoch of the bot turn.
        epoch: TurnEpoch,
    },
}

/// Counters the machine keeps for the caller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnStats {
    ignored: [u64; INPUT_KINDS],
}

impl TurnStats {
    /// How many inputs of `kind` were ignored (illegal in the state they arrived in, stale, or
    /// vetoed). Accepted inputs that have no effect (for example an interim during speech) are
    /// not counted.
    pub fn ignored(&self, kind: InputKind) -> u64 {
        self.ignored.get(kind.index()).copied().unwrap_or(0)
    }

    /// The sum of [`TurnStats::ignored`] over all kinds.
    pub fn total_ignored(&self) -> u64 {
        self.ignored
            .iter()
            .fold(0u64, |sum, count| sum.saturating_add(*count))
    }
}

/// Counters of barge-in decisions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InterruptionStats {
    candidates: u64,
    confirmed: u64,
    promoted: u64,
    vetoed: [u64; 5],
}

impl InterruptionStats {
    /// Barge-in candidates seen (every `VadStart` while the bot was thinking or speaking).
    pub fn candidates(&self) -> u64 {
        self.candidates
    }

    /// Candidates confirmed as interruptions (an `Interrupt` was emitted).
    pub fn confirmed(&self) -> u64 {
        self.confirmed
    }

    /// Candidates that became an ordinary user turn because the bot's audio ended while they
    /// were pending and the policy then confirmed them with the bot silent (no `Interrupt`:
    /// there was nothing left to cut). The guards still apply on this path.
    pub fn promoted(&self) -> u64 {
        self.promoted
    }

    /// Candidates dropped for `reason` (no `Interrupt`, no epoch bump).
    pub fn vetoed(&self, reason: VetoReason) -> u64 {
        self.vetoed.get(reason.index()).copied().unwrap_or(0)
    }

    /// The sum of [`InterruptionStats::vetoed`] over all reasons.
    pub fn total_vetoed(&self) -> u64 {
        self.vetoed
            .iter()
            .fold(0u64, |sum, count| sum.saturating_add(*count))
    }
}

/// A pending barge-in: the user started speaking while the bot was thinking or speaking, and the
/// [`InterruptionPolicy`] has not confirmed it yet.
///
/// The machine does not enter a state for it: while a candidate exists, [`TurnMachine::state`]
/// is still `AwaitingResponse` or `BotSpeaking` (or `Idle`, see below), no user turn is open,
/// the epoch is unchanged and no action has been emitted (a vetoed candidate leaves no trace
/// but a counter). A candidate is resolved by
///
/// * the policy confirming it on the `VadStart` itself, an `SttInterim`, a `Tick`, a
///   `BotAudioStarted` or the `VadStop` (`Interrupt` + `StartUserTurn`, as without guards);
/// * the `VadStop` while a guard still blocks it (dropped: the stop is consumed and counted
///   as an owed STT final, so later finals stay aligned). At the stop the policy uses
///   [`InterruptionPolicy::assess_at_stop`], which waives the minimum speech
///   duration for an interim with a substantive word (a short "stop" is not noise);
/// * the end of the bot's audio (`BotAudioEnded` of the current epoch), after which the
///   candidate is *detached*: the state is `Idle`, the bot is silent (no echo guard) and the
///   candidate stays pending. The other guards (backchannel, minimum speech and words,
///   transcript wait) keep judging it on interims, ticks and the `VadStop`. Once the policy
///   confirms it, it becomes an ordinary user turn without an `Interrupt` (there is nothing
///   left to cut); if it is still blocked at the stop it is dropped like any other;
/// * `Hangup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BargeInCandidate {
    started_at: Instant,
    words: u32,
    text: TextKind,
    recheck: Option<Instant>,
}

impl BargeInCandidate {
    /// When VAD started.
    pub fn started_at(&self) -> Instant {
        self.started_at
    }

    /// Words in the longest interim seen so far.
    pub fn words(&self) -> u32 {
        self.words
    }

    /// What the latest interim looks like.
    pub fn text(&self) -> TextKind {
        self.text
    }

    /// When the machine will assess the candidate again without new input (the armed timer).
    pub fn recheck_at(&self) -> Option<Instant> {
        self.recheck
    }
}

/// The actions of a confirmed barge-in into `epoch`.
fn out_barge_in(epoch: TurnEpoch, out: &mut Vec<TurnAction>) {
    out.push(TurnAction::Interrupt { new_epoch: epoch });
    out.push(TurnAction::StartUserTurn { epoch });
}

/// Whether `text` ends with sentence-final punctuation (trailing whitespace ignored).
fn ends_sentence(text: &str) -> bool {
    text.trim_end()
        .chars()
        .last()
        .is_some_and(|c| matches!(c, '.' | '?' | '!' | '…'))
}

/// The pure turn-taking state machine. See the module docs and `docs/ARCHITECTURE.md`.
#[derive(Debug)]
pub struct TurnMachine {
    policy: TurnPolicy,
    state: TurnState,
    epoch: TurnEpoch,
    /// Whether a user turn has started yet (the first one uses [`TurnEpoch::ZERO`]).
    started_any: bool,
    next_analysis: u64,
    /// The deadline the caller was last told to arm, if any.
    armed: Option<Instant>,
    /// VAD stops whose STT final has not arrived yet, oldest first (a count: finals arrive in
    /// the order of their stops). The final of the current pause is the one that arrives when
    /// it is the only stop still owed.
    finals_owed: u32,
    stats: TurnStats,
    interruption: InterruptionPolicy,
    /// The pending barge-in, if any (only in `AwaitingResponse` / `BotSpeaking`).
    candidate: Option<BargeInCandidate>,
    /// When the current echo window started: the bot's audio of the current epoch began, or
    /// resumed after a gap of at least `echo_guard` (for the echo guard).
    bot_audio_at: Option<Instant>,
    /// The latest `BotAudioStarted` time accepted for the current epoch. A new one restarts the
    /// echo window only when it comes at least `echo_guard` after this one (a real gap), so a
    /// sender that reports every chunk cannot keep the window open for the whole response.
    bot_audio_seen: Option<Instant>,
    istats: InterruptionStats,
}

impl TurnMachine {
    /// A machine in [`TurnState::Idle`] at [`TurnEpoch::ZERO`], with the interruption policy
    /// disabled: every barge-in interrupts at once (the plain always-interrupt behaviour).
    pub fn new(policy: TurnPolicy) -> Self {
        Self::with_interruption(policy, InterruptionPolicy::disabled())
    }

    /// A machine that judges barge-ins with `interruption` (see [`InterruptionPolicy`]).
    pub fn with_interruption(policy: TurnPolicy, interruption: InterruptionPolicy) -> Self {
        Self {
            policy,
            state: TurnState::Idle,
            epoch: TurnEpoch::ZERO,
            started_any: false,
            next_analysis: 1,
            armed: None,
            finals_owed: 0,
            stats: TurnStats::default(),
            interruption,
            candidate: None,
            bot_audio_at: None,
            bot_audio_seen: None,
            istats: InterruptionStats::default(),
        }
    }

    /// The policy this machine decides with.
    pub fn policy(&self) -> TurnPolicy {
        self.policy
    }

    /// The current state.
    pub fn state(&self) -> TurnState {
        self.state
    }

    /// The current epoch.
    pub fn epoch(&self) -> TurnEpoch {
        self.epoch
    }

    /// The deadline of the pending timer, if one is armed.
    pub fn armed_deadline(&self) -> Option<Instant> {
        self.armed
    }

    /// The counters of ignored inputs.
    pub fn stats(&self) -> TurnStats {
        self.stats
    }

    /// The interruption policy this machine judges barge-ins with.
    pub fn interruption(&self) -> &InterruptionPolicy {
        &self.interruption
    }

    /// The pending barge-in, if one is waiting for the policy's confirmation.
    pub fn barge_in_candidate(&self) -> Option<BargeInCandidate> {
        self.candidate
    }

    /// The counters of barge-in decisions.
    pub fn interruption_stats(&self) -> InterruptionStats {
        self.istats
    }

    /// Applies `input`, appending the actions the caller must carry out to `out`.
    ///
    /// Never panics. An input with no legal transition in the current state leaves the state
    /// unchanged, appends nothing, is counted in [`TurnStats`] and logged at debug level.
    pub fn handle(&mut self, input: TurnInput, out: &mut Vec<TurnAction>) {
        let kind = input.kind();
        let accepted = match input {
            TurnInput::VadStart { at } => self.vad_start(at, out),
            TurnInput::VadStop { at } => self.vad_stop(at, out),
            TurnInput::SttInterim { text, at } => self.stt_interim(&text, at, out),
            TurnInput::SttFinal { text, at } => self.stt_final(&text, at, out),
            TurnInput::Verdict {
                request,
                complete,
                at,
                ..
            } => self.verdict(request, complete, at, out),
            TurnInput::Tick { now } => self.tick(now, out),
            TurnInput::BotAudioStarted { epoch, at } => self.bot_audio_started(epoch, at, out),
            TurnInput::BotAudioEnded { epoch, at } => self.bot_audio_ended(epoch, at, out),
            TurnInput::Hangup { .. } => self.hangup(out),
            TurnInput::UserText { at } => self.user_text(at, out),
            TurnInput::BotTurn { interrupt, at } => self.bot_turn(interrupt, at, out),
        };
        if !accepted {
            if let Some(slot) = self.stats.ignored.get_mut(kind.index()) {
                *slot = slot.saturating_add(1);
            }
            tracing::debug!(
                input = ?kind,
                state = ?self.state.kind(),
                "turn: input ignored"
            );
        }
    }

    /// What the policy sees of `candidate` at `now`.
    fn observe(&self, candidate: &BargeInCandidate, now: Instant) -> Observation {
        Observation {
            speech: now.saturating_duration_since(candidate.started_at),
            words: candidate.words,
            text: candidate.text,
            bot_audio_age: self
                .bot_audio_at
                .map(|started| now.saturating_duration_since(started)),
        }
    }

    /// Assesses the pending candidate at `now` (this is the barge-in guard hook).
    /// Confirmed: the barge-in happens. Blocked: the candidate stays pending and the timer is
    /// set for the policy's next time-driven recheck.
    fn evaluate_candidate(&mut self, now: Instant, out: &mut Vec<TurnAction>) {
        let Some(mut candidate) = self.candidate else {
            return;
        };
        // A stale input (for example a tick of an earlier timer) can carry a time before the
        // candidate began: judge it as of the start, so the recheck deadline stays correct.
        let now = now.max(candidate.started_at);
        let observation = self.observe(&candidate, now);
        match self.interruption.assess(&observation) {
            Assessment::Confirmed => {
                self.confirm_candidate(out);
                self.sync_timer(out);
            }
            Assessment::Blocked(_) => {
                candidate.recheck = self
                    .interruption
                    .recheck_in(&observation)
                    .map(|d| after(now, d));
                self.candidate = Some(candidate);
                self.sync_timer(out);
            }
        }
    }

    /// Turns the pending candidate into a user turn and appends its actions. While the bot is
    /// thinking or speaking this is the barge-in (`Interrupt` + `StartUserTurn`); for a
    /// detached candidate (state `Idle`, the bot's audio already ended) it is an ordinary turn
    /// (`StartUserTurn` only, counted as promoted). The epoch moves on either way. The caller
    /// syncs the timer.
    fn confirm_candidate(&mut self, out: &mut Vec<TurnAction>) {
        self.candidate = None;
        self.clear_bot_audio();
        let detached = self.state == TurnState::Idle;
        let epoch = self.advance_epoch();
        if detached {
            self.istats.promoted = self.istats.promoted.saturating_add(1);
            out.push(TurnAction::StartUserTurn { epoch });
        } else {
            self.istats.confirmed = self.istats.confirmed.saturating_add(1);
            out_barge_in(epoch, out);
        }
        self.state = TurnState::UserSpeaking;
    }

    /// Drops the pending candidate: no `Interrupt`, no epoch bump.
    fn veto_candidate(&mut self, reason: VetoReason, out: &mut Vec<TurnAction>) {
        self.candidate = None;
        if let Some(slot) = self.istats.vetoed.get_mut(reason.index()) {
            *slot = slot.saturating_add(1);
        }
        tracing::debug!(?reason, state = ?self.state.kind(), "turn: barge-in vetoed");
        self.sync_timer(out);
    }

    fn stt_interim(&mut self, text: &str, at: Instant, out: &mut Vec<TurnAction>) -> bool {
        match self.state {
            TurnState::UserSpeaking | TurnState::UserPaused(_) => true,
            TurnState::Idle | TurnState::AwaitingResponse | TurnState::BotSpeaking => {
                let Some(mut candidate) = self.candidate else {
                    return false;
                };
                let words = count_words(text);
                if words > 0 {
                    candidate.words = candidate.words.max(words);
                    candidate.text = self.interruption.classify(text);
                    self.candidate = Some(candidate);
                }
                self.evaluate_candidate(at, out);
                true
            }
            TurnState::Closed => false,
        }
    }

    /// Forgets the bot's audio (it ended, a turn started, or the call closed): no echo window.
    fn clear_bot_audio(&mut self) {
        self.bot_audio_at = None;
        self.bot_audio_seen = None;
    }

    /// Moves to the next epoch (a new user turn after one has started, or a barge-in).
    fn advance_epoch(&mut self) -> TurnEpoch {
        self.epoch = self.epoch.next();
        self.started_any = true;
        self.epoch
    }

    fn next_analysis_id(&mut self) -> AnalysisId {
        let id = AnalysisId::new(self.next_analysis);
        self.next_analysis = self.next_analysis.saturating_add(1);
        id
    }

    fn vad_start(&mut self, at: Instant, out: &mut Vec<TurnAction>) -> bool {
        match self.state {
            TurnState::Idle => {
                if self.candidate.is_some() {
                    // A detached candidate: VAD is already active, this is a duplicate.
                    return false;
                }
                let epoch = if self.started_any {
                    self.advance_epoch()
                } else {
                    self.started_any = true;
                    self.epoch
                };
                out.push(TurnAction::StartUserTurn { epoch });
                self.state = TurnState::UserSpeaking;
                true
            }
            TurnState::UserPaused(_) => {
                // The same turn resumes; the pending analysis is no longer current.
                self.state = TurnState::UserSpeaking;
                self.sync_timer(out);
                true
            }
            TurnState::AwaitingResponse | TurnState::BotSpeaking => {
                if self.candidate.is_some() {
                    // VAD is already active: a duplicate.
                    return false;
                }
                self.candidate = Some(BargeInCandidate {
                    started_at: at,
                    words: 0,
                    text: TextKind::None,
                    recheck: None,
                });
                self.istats.candidates = self.istats.candidates.saturating_add(1);
                self.evaluate_candidate(at, out);
                true
            }
            TurnState::UserSpeaking | TurnState::Closed => false,
        }
    }

    fn vad_stop(&mut self, at: Instant, out: &mut Vec<TurnAction>) -> bool {
        if let Some(candidate) = self.candidate {
            // The user stopped while a barge-in was still pending: decide with what is known
            // at the stop. The stop is owed a final in either case.
            let observation = self.observe(&candidate, at);
            match self.interruption.assess_at_stop(&observation) {
                Assessment::Confirmed => {
                    // Confirmed by the clock (a Tick not yet delivered) or by a short but
                    // substantive word: start the turn (a barge-in unless the candidate is
                    // detached), then the stop is an ordinary one of the new turn.
                    self.confirm_candidate(out);
                }
                Assessment::Blocked(reason) => {
                    self.finals_owed = self.finals_owed.saturating_add(1);
                    self.veto_candidate(reason, out);
                    return true;
                }
            }
        }
        if self.state != TurnState::UserSpeaking {
            return false;
        }
        self.finals_owed = self.finals_owed.saturating_add(1);
        let analysis = match self.policy {
            TurnPolicy::FixedSilence { .. } => Analysis::NotRequested,
            TurnPolicy::Adaptive { .. } => {
                let id = self.next_analysis_id();
                out.push(TurnAction::RequestAnalysis { id });
                Analysis::Pending(id)
            }
        };
        self.state = TurnState::UserPaused(Pause {
            vad_stop_at: at,
            analysis,
            stt: None,
        });
        self.sync_timer(out);
        true
    }

    fn stt_final(&mut self, text: &str, at: Instant, out: &mut Vec<TurnAction>) -> bool {
        if self.state == TurnState::Closed || self.finals_owed == 0 {
            // Nothing is waiting for a final: a duplicate or a stray one.
            return false;
        }
        // The oldest owed stop is answered. It is the current pause's stop only when it is the
        // last one owed (the current pause's stop is the newest, and finals arrive in order).
        let answers_current_pause = self.finals_owed == 1;
        self.finals_owed = self.finals_owed.saturating_sub(1);
        match self.state {
            TurnState::UserPaused(mut pause) if answers_current_pause => {
                pause.stt = Some(FinalSeen {
                    at,
                    ends_sentence: ends_sentence(text),
                });
                let verdict_waits_for_final = matches!(pause.analysis, Analysis::Complete { .. });
                self.state = TurnState::UserPaused(pause);
                if verdict_waits_for_final {
                    self.finish_turn(EndReason::Verdict, out);
                } else {
                    self.sync_timer(out);
                }
                true
            }
            // A final for an earlier stop (an abandoned pause, or a turn that already ended), or
            // one that arrives during speech or while the response runs: consumed, no effect.
            _ => true,
        }
    }

    fn verdict(
        &mut self,
        request: AnalysisId,
        complete: bool,
        at: Instant,
        out: &mut Vec<TurnAction>,
    ) -> bool {
        let TurnState::UserPaused(mut pause) = self.state else {
            return false;
        };
        if pause.analysis != Analysis::Pending(request) {
            return false;
        }
        if !complete {
            pause.analysis = Analysis::Incomplete { at };
            self.state = TurnState::UserPaused(pause);
            self.sync_timer(out);
            return true;
        }
        let require_final = matches!(
            self.policy,
            TurnPolicy::Adaptive {
                require_final: true,
                ..
            }
        );
        if require_final && pause.stt.is_none() {
            pause.analysis = Analysis::Complete { at };
            self.state = TurnState::UserPaused(pause);
            self.sync_timer(out);
        } else {
            self.finish_turn(EndReason::Verdict, out);
        }
        true
    }

    fn tick(&mut self, now: Instant, out: &mut Vec<TurnAction>) -> bool {
        if self.candidate.is_some() {
            // The timer that delivered this tick is consumed.
            self.armed = None;
            self.evaluate_candidate(now, out);
            return true;
        }
        let TurnState::UserPaused(pause) = self.state else {
            return false;
        };
        // The timer that delivered this tick is consumed: nothing is left to cancel.
        self.armed = None;
        let (deadline, reason) = pause.deadline(self.policy);
        if now < deadline {
            self.sync_timer(out);
            return true;
        }
        self.finish_turn(reason, out);
        true
    }

    fn bot_audio_started(
        &mut self,
        epoch: TurnEpoch,
        at: Instant,
        out: &mut Vec<TurnAction>,
    ) -> bool {
        if epoch != self.epoch {
            return false;
        }
        match self.state {
            TurnState::AwaitingResponse => {
                self.state = TurnState::BotSpeaking;
                self.bot_audio_at = Some(at);
                self.bot_audio_seen = Some(at);
                // The echo window starts now; a pending candidate is judged again.
                self.evaluate_candidate(at, out);
                true
            }
            // Audio that resumes after a gap (a new TTS round): the echo can resume with it, so
            // the window restarts, but only after a real gap: `at` must come at least
            // `echo_guard` after the previous `BotAudioStarted`. Reports for the chunks of
            // continuous playback (closer together than that) and out-of-order or repeated ones
            // (`at` not after the previous) never move the window. With the echo guard off this
            // has no effect (repeats are harmless and not counted).
            TurnState::BotSpeaking => {
                let guard = self.interruption.echo_guard;
                if !guard.is_zero()
                    && let Some(seen) = self.bot_audio_seen
                    && at > seen
                {
                    self.bot_audio_seen = Some(at);
                    if at.saturating_duration_since(seen) >= guard {
                        self.bot_audio_at = Some(at);
                        self.evaluate_candidate(at, out);
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn bot_audio_ended(
        &mut self,
        epoch: TurnEpoch,
        at: Instant,
        out: &mut Vec<TurnAction>,
    ) -> bool {
        match self.state {
            // Also covers a response that produced no audio at all.
            TurnState::BotSpeaking | TurnState::AwaitingResponse if epoch == self.epoch => {
                self.state = TurnState::Idle;
                self.clear_bot_audio();
                if self.candidate.is_some() {
                    // The user is speaking over the end of the bot's audio. The candidate is
                    // judged again with the bot silent (no echo window): the backchannel,
                    // minimum speech and words guards still apply, so a "mm-hmm" or a cough
                    // at the end of a sentence does not become a turn. It is confirmed as an
                    // ordinary user turn, or stays pending (detached) until the policy decides.
                    self.evaluate_candidate(at, out);
                }
                true
            }
            _ => false,
        }
    }

    /// Terminal. An open user turn is closed (`EndUserTurn` with [`EndReason::Hangup`]) so that
    /// every `StartUserTurn` is paired with an end; the pending timer is cancelled.
    fn hangup(&mut self, out: &mut Vec<TurnAction>) -> bool {
        if self.state == TurnState::Closed {
            return false;
        }
        let open_turn = matches!(
            self.state,
            TurnState::UserSpeaking | TurnState::UserPaused(_)
        );
        self.candidate = None;
        self.clear_bot_audio();
        self.state = TurnState::Closed;
        self.sync_timer(out);
        if open_turn {
            out.push(TurnAction::EndUserTurn {
                epoch: self.epoch,
                reason: EndReason::Hangup,
            });
        }
        true
    }

    /// A typed message (see [`TurnInput::UserText`]).
    fn user_text(&mut self, at: Instant, out: &mut Vec<TurnAction>) -> bool {
        match self.state {
            TurnState::Idle | TurnState::AwaitingResponse | TurnState::BotSpeaking => {
                if self.candidate.is_some() {
                    // The user is talking already: the typed text joins the turn the candidate
                    // becomes, which ends as a spoken turn does.
                    self.confirm_candidate(out);
                    self.sync_timer(out);
                    return true;
                }
                let bot_busy = self.state != TurnState::Idle;
                let epoch = if self.started_any {
                    self.advance_epoch()
                } else {
                    self.started_any = true;
                    self.epoch
                };
                self.clear_bot_audio();
                if bot_busy {
                    out_barge_in(epoch, out);
                } else {
                    out.push(TurnAction::StartUserTurn { epoch });
                }
                tracing::debug!(epoch = epoch.get(), ?at, "turn: typed user message");
                self.finish_turn(EndReason::Text, out);
                true
            }
            TurnState::UserSpeaking | TurnState::UserPaused(_) | TurnState::Closed => false,
        }
    }

    /// A line of the bot's own (see [`TurnInput::BotTurn`]).
    fn bot_turn(&mut self, interrupt: bool, at: Instant, out: &mut Vec<TurnAction>) -> bool {
        if self.candidate.is_some() {
            // The user is talking (or VAD is active): the line waits.
            return false;
        }
        let cut = match self.state {
            TurnState::Idle => false,
            TurnState::AwaitingResponse | TurnState::BotSpeaking if interrupt => true,
            _ => return false,
        };
        let epoch = if self.started_any {
            self.advance_epoch()
        } else {
            self.started_any = true;
            self.epoch
        };
        self.clear_bot_audio();
        if cut {
            out.push(TurnAction::Interrupt { new_epoch: epoch });
        }
        out.push(TurnAction::StartBotTurn { epoch });
        self.state = TurnState::AwaitingResponse;
        self.sync_timer(out);
        tracing::debug!(epoch = epoch.get(), ?at, cut, "turn: bot line");
        true
    }

    /// Ends the user turn: the state becomes `AwaitingResponse`, the timer is cancelled.
    fn finish_turn(&mut self, reason: EndReason, out: &mut Vec<TurnAction>) {
        self.state = TurnState::AwaitingResponse;
        self.clear_bot_audio();
        self.sync_timer(out);
        out.push(TurnAction::EndUserTurn {
            epoch: self.epoch,
            reason,
        });
    }

    /// Brings the caller's single timer slot in line with the state: arms the pause deadline,
    /// or cancels a timer that is no longer needed. Emits nothing when nothing changed.
    fn sync_timer(&mut self, out: &mut Vec<TurnAction>) {
        let desired = match self.state {
            TurnState::UserPaused(pause) => Some(pause.deadline(self.policy).0),
            TurnState::Idle | TurnState::AwaitingResponse | TurnState::BotSpeaking => {
                self.candidate.and_then(|c| c.recheck)
            }
            TurnState::UserSpeaking | TurnState::Closed => None,
        };
        if desired == self.armed {
            return;
        }
        match desired {
            Some(deadline) => out.push(TurnAction::ArmTimer { deadline }),
            None => out.push(TurnAction::CancelTimer),
        }
        self.armed = desired;
    }
}
