//! Pause, then decide: what happens to a barge-in candidate while the bot is paused.
//!
//! With [`PausePolicy`] set on the [`TurnMachine`](crate::turn::TurnMachine), a barge-in
//! candidate does not wait for the interruption policy with the bot still talking. The machine
//! asks the transport to *pause* the bot's playout at once (`TurnAction::PausePlayout`): the
//! caller hears silence as early as the interruption would have stopped the bot. The candidate
//! then has a bounded decision window, [`PausePolicy::decide`], to end in one of two ways:
//!
//! * **commit**: exactly today's interruption (`Interrupt`, epoch + 1, the unplayed audio is
//!   dropped, the LLM context is cut to what was played);
//! * **resume**: the bot continues the same response from the paused point
//!   (`TurnAction::ResumePlayout`); the epoch does not change and the context is untouched.
//!
//! The decision is a [`PauseRule`]: a pure function of the [`PauseEvidence`] the machine gathers
//! (time, words the speech recogniser heard, command words, the bot's echo).
//! Two rules ship here: [`NeverRule`] (never resumes on evidence, the plain interruption
//! policy decides) and [`WordsRule`] (resume only when no recognised words arrive: the
//! behaviour of LiveKit's `resume_false_interruption`). Another rule plugs in through the same
//! trait.
//!
//! Everything uncertain commits: the window ends in a commit unless the rule says `Resume`, a
//! full hold buffer commits, and after [`PausePolicy::tighten_after`] resumed pauses within
//! [`PausePolicy::tighten_window`] the machine stops pausing on VAD alone and waits for one
//! recognised word (the bot keeps talking meanwhile), so a noise that keeps starting the VAD does not
//! make the bot stutter.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::interrupt::TextKind;

/// How a pause ended, for metrics (`brisvox_barge_in_pauses_total{outcome}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PauseOutcome {
    /// The bot resumed the response.
    Resumed,
    /// The rule (words, a command word, the interruption policy) committed the interruption
    /// before the window ended.
    Committed,
    /// The hold buffer filled up while paused: committed.
    Overflow,
    /// The decision window ended without a `Resume`: committed.
    Deadline,
    /// The bot's audio ended while paused (everything had played before the pause took hold):
    /// nothing was held, nothing to resume.
    Ended,
}

impl PauseOutcome {
    /// All outcomes, in label order.
    pub const ALL: [Self; 5] = [
        Self::Resumed,
        Self::Committed,
        Self::Overflow,
        Self::Deadline,
        Self::Ended,
    ];

    /// The metric label value.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Resumed => "resumed",
            Self::Committed => "committed",
            Self::Overflow => "overflow",
            Self::Deadline => "deadline",
            Self::Ended => "ended",
        }
    }

    /// Index into [`Self::ALL`].
    pub const fn index(self) -> usize {
        match self {
            Self::Resumed => 0,
            Self::Committed => 1,
            Self::Overflow => 2,
            Self::Deadline => 3,
            Self::Ended => 4,
        }
    }
}

/// What a [`PauseRule`] says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseVerdict {
    /// No decision yet: stay paused.
    Wait,
    /// Interrupt the bot, as without the pause.
    Commit,
    /// Continue the paused response.
    Resume,
}

/// What the machine knows about a barge-in candidate when it asks the rule. All durations are
/// derived from input timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PauseEvidence {
    /// Time since the pause began (zero at the moment the candidate begins).
    pub elapsed: Duration,
    /// The decision window has ended: the rule must answer for good. `Wait` is read as `Commit`.
    pub deadline: bool,
    /// The VAD is still active (the candidate's speech has not stopped).
    pub vad_active: bool,
    /// How long the VAD has been active since the candidate began.
    pub speech: Duration,
    /// Words of the candidate the recogniser heard that are the caller's: the longest interim or
    /// final, and zero when the words are the bot's own echo or only a backchannel.
    pub words: u32,
    /// The latest text is a short command ("stop", "wait", "dur").
    pub command: bool,
    /// The words heard are the bot's own voice coming back.
    pub echo: bool,
    /// The latest text is only backchannel phrases.
    pub backchannel: bool,
    /// The interruption policy would confirm the candidate now (its guards all pass).
    pub policy_confirms: bool,
    /// The recogniser's final for the candidate's speech has arrived.
    pub final_seen: bool,
    /// What the latest text looks like.
    pub text: TextKind,
}

/// The decision hook of a pause. Pure and deterministic: it reads nothing but the evidence.
pub trait PauseRule: fmt::Debug + Send + Sync {
    /// Decides. Called on every event of the candidate (start, interim, final, stop,
    /// timer). `Wait` before the deadline keeps the bot paused; at the deadline
    /// (`evidence.deadline`) anything but `Resume` commits.
    fn decide(&self, evidence: &PauseEvidence) -> PauseVerdict;
}

/// Never resumes on evidence: the interruption policy decides when to commit, a candidate the
/// policy vetoes at the VAD stop resumes (the bot was never really interrupted), and the window
/// ends in a commit. For tests and as the neutral rule.
#[derive(Debug, Clone, Copy, Default)]
pub struct NeverRule;

impl PauseRule for NeverRule {
    fn decide(&self, e: &PauseEvidence) -> PauseVerdict {
        if e.policy_confirms {
            PauseVerdict::Commit
        } else if !e.vad_active && !e.deadline {
            // The speech ended and the guards still block it: a cough, a backchannel, an echo.
            PauseVerdict::Resume
        } else {
            PauseVerdict::Wait
        }
    }
}

/// LiveKit's rule, made safe for a lagging recogniser: any recognised word commits at once (a
/// command word too). The bot's own echo and backchannels are not caller words. Resume needs
/// the speech to have stopped *and* no words: at the end of the window, or when the
/// recogniser's final of that speech has no words. While the VAD is still active the rule
/// waits, and the window's end commits (uncertain: the speech goes on and nothing says it is not
/// the caller's).
#[derive(Debug, Clone, Copy, Default)]
pub struct WordsRule;

impl PauseRule for WordsRule {
    fn decide(&self, e: &PauseEvidence) -> PauseVerdict {
        if e.command && !e.echo {
            return PauseVerdict::Commit;
        }
        if e.words >= 1 {
            return PauseVerdict::Commit;
        }
        if e.vad_active {
            return PauseVerdict::Wait;
        }
        if e.deadline || e.final_seen {
            return PauseVerdict::Resume;
        }
        PauseVerdict::Wait
    }
}

/// The settings of pause-then-decide.
#[derive(Debug, Clone)]
pub struct PausePolicy {
    /// The decision window: at most this long after the pause began, the rule must answer.
    pub decide: Duration,
    /// The decision hook.
    pub rule: Arc<dyn PauseRule>,
    /// After this many resumed pauses within [`Self::tighten_window`], a candidate is not paused
    /// until it has one recognised caller word. Zero: never tighten.
    pub tighten_after: u32,
    /// The window `tighten_after` counts in.
    pub tighten_window: Duration,
    /// How long a candidate's speech has to last before the bot is paused for it (LiveKit's
    /// `min_interruption_duration`). Until then the bot keeps talking, a recognised word or a
    /// command word still commits at once, and a candidate whose speech ends first waits for
    /// its final (an empty one drops it): a click or a door never pauses the bot. Zero: pause at
    /// the first moment of speech.
    pub after: Duration,
}

/// Default decision window.
pub const DEFAULT_DECIDE: Duration = Duration::from_millis(1500);
/// Default number of resumed pauses that tighten the rule.
pub const DEFAULT_TIGHTEN_AFTER: u32 = 2;
/// Default window of the tightening count.
pub const DEFAULT_TIGHTEN_WINDOW: Duration = Duration::from_secs(60);

impl PausePolicy {
    /// A policy with the default window and tightening around `rule`.
    pub fn new(rule: Arc<dyn PauseRule>) -> Self {
        Self {
            decide: DEFAULT_DECIDE,
            rule,
            tighten_after: DEFAULT_TIGHTEN_AFTER,
            tighten_window: DEFAULT_TIGHTEN_WINDOW,
            after: Duration::ZERO,
        }
    }

    /// The words rule with the default settings.
    pub fn words() -> Self {
        Self::new(Arc::new(WordsRule))
    }

    /// The never rule with the default settings.
    pub fn never() -> Self {
        Self::new(Arc::new(NeverRule))
    }
}

/// Counters of the pauses a machine made.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PauseStats {
    /// Pauses begun (`PausePlayout` emitted).
    pub pauses: u64,
    /// Pauses by outcome, indexed by [`PauseOutcome::index`].
    pub outcomes: [u64; 5],
    /// Candidates that began while the rule was tightened and were not paused.
    pub tightened_skips: u64,
    /// Pauses a hang-up ended.
    pub abandoned: u64,
}

impl PauseStats {
    /// Pauses that ended in `outcome`.
    pub fn outcome(&self, outcome: PauseOutcome) -> u64 {
        self.outcomes.get(outcome.index()).copied().unwrap_or(0)
    }
}
