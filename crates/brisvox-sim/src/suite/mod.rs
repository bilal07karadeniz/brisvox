//! The reusable simulator suite: whole-call scenarios written once, run against any pipeline a
//! [`SessionFactory`](crate::factory::SessionFactory) builds.
//!
//! Every function is `async fn name<F: SessionFactory>(factory: &F, profile: Profile) ->
//! Result<()>` (some take an extra argument) and returns [`Error::Assertion`] describing the first
//! failed check. A test wraps it: `suite::scenarios::s02_barge_in(&factory, Profile::Exact)
//! .await.unwrap()`. The calls run on a fresh paused-time `SimClock` created by the suite, so the
//! caller must be on a `start_paused` runtime.
//!
//! # Profiles
//!
//! The structural invariants are identical under both profiles and always asserted exactly:
//! the pipeline outcome is `Ok`; two runs of the same call have identical fingerprints
//! (determinism); no audio of an older turn reaches the output after an interrupt; the context
//! holds what was played; roles and turn counts; interrupt counts (from `Interrupt` frames at the
//! output tap, not from client clears); recovery after faults; isolation versus a solo run; no
//! leaked tasks; flat heap.
//!
//! Timing and position values are where the profiles differ:
//! - [`Profile::Exact`] is for the reference pipeline, whose latency is a closed-form function of
//!   the mocks' latencies and the 20 ms frame clock: the values are asserted to the millisecond.
//! - [`Profile::Budget`] is for production pipelines, which add scheduling hops, a settle wait and
//!   different endpointing: values only have to fall inside the ranges of the table below.
//!
//! Production fingerprints are only ever compared to production fingerprints, never to the
//! reference's.
//!
//! | check | Exact | Budget |
//! |---|---|---|
//! | one turn: first bot audio | 1480 ms | 1480 to 1700 ms |
//! | one turn: speech start and end marks | exact | within 20 ms |
//! | one turn: end-of-turn delay | 280 ms | 280 to 470 ms |
//! | one turn: response latency | 280 ms | 280 to 470 ms |
//! | s01 first audio after the end of speech, lower bound | 580 ms | 580 ms |
//! | s02 and barge-in: time from barge-in to silence at the output | 0 | 0 to 50 ms (a slow consumer's frame) |
//! | barge-in: interrupt at the output | 2420 ms | 2400 to 2440 ms |
//! | s05 LLM time to first token behind a 2 s stall | 2000 ms | 2000 to 2020 ms |
//! | words heard at a mid-answer cut (s11, played text) | exact | +-1 word |
//! | context vs heard text of a turn a barge-in cut (every scenario) | exact | up to 1 word fewer, never more |
//! | context vs heard text of a turn that played to its end | exact | exact |
//! | playout log vs frames at the output tap | equal | within 20 ms |
//! | chaos: gap between a slow consumer's audio frames | at least 30 ms | at least 20 ms |
//! | s08 tool call | ignored: no tool messages, no leak | ignored, or the full tool loop (call, result, second round, both spoken) |
//!
//! Checks that are bounds in both profiles keep their value: bot audio within 1.5 s of the end of
//! speech (s01, s09, chaos SLO), fixtures within 600 ms.
//!
//! The Budget numbers were set from the measured values of the production factory
//! (`brisvox_engine::sim::EngineFactory`, fixed 450 ms silence and the adaptive policy) and of
//! the hybrid chains, plus one 20 ms frame of margin; the reference's exact values stay inside
//! every range (a sim test checks it). The measurements are in `docs/LATENCY.md`. The 450 ms end
//! of turn of the fixed policy is the widest case: 170 ms over the reference's 280 ms.

use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{SharedClock, SimClock};

use crate::error::{Error, Result};

pub mod basic;
pub mod chaos;
pub mod fixtures;
pub mod played_text;
pub mod scenarios;

/// Which timing expectations a suite run applies; see the [module docs](self).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Profile {
    /// Closed-form timings of the reference pipeline, asserted exactly.
    Exact,
    /// Ranges for production pipelines.
    Budget,
}

/// Slack of the Budget profile for the playout log versus frames seen at the output tap.
pub const BUDGET_PLAYED_SLACK: Duration = Duration::from_millis(20);

/// Words of slack of the Budget profile when a heard position decides how much text counts as
/// played.
pub const BUDGET_WORD_SLACK: usize = 1;

impl Profile {
    /// Whether this is [`Profile::Exact`].
    pub const fn is_exact(self) -> bool {
        matches!(self, Self::Exact)
    }

    /// Words of slack when the conversation is compared with the heard text
    /// ([`SessionRun::check_context_matches_played_text_within`]): 0 under Exact,
    /// [`BUDGET_WORD_SLACK`] under Budget (fewer words only, never more, and only for turns a
    /// barge-in cut).
    ///
    /// [`SessionRun::check_context_matches_played_text_within`]:
    ///     crate::session::SessionRun::check_context_matches_played_text_within
    pub const fn word_slack(self) -> usize {
        match self {
            Self::Exact => 0,
            Self::Budget => BUDGET_WORD_SLACK,
        }
    }

    /// The structural invariant "the context holds what was played" under this profile: exact
    /// under Exact; under Budget the text of a turn a barge-in cut may lack up to
    /// [`BUDGET_WORD_SLACK`] trailing words of what was heard (never more), and every other turn
    /// must match exactly.
    ///
    /// # Errors
    /// [`Error::Assertion`] on the first mismatch.
    pub fn context_holds_played(self, run: &crate::session::SessionRun) -> Result<()> {
        run.check_context_matches_played_text_within(self.word_slack())
    }

    /// Checks a measured duration: equal to `exact` under Exact, inside `budget` under Budget.
    ///
    /// # Errors
    /// [`Error::Assertion`] naming `what`.
    pub fn duration(
        self,
        what: &str,
        got: Duration,
        exact: Duration,
        budget: RangeInclusive<Duration>,
    ) -> Result<()> {
        match self {
            Self::Exact if got == exact => Ok(()),
            Self::Exact => Err(assertion(format!("{what}: {got:?}, expected {exact:?}"))),
            Self::Budget if budget.contains(&got) => Ok(()),
            Self::Budget => Err(assertion(format!(
                "{what}: {got:?}, expected within {:?} to {:?}",
                budget.start(),
                budget.end()
            ))),
        }
    }

    /// Checks a lower bound: at least `exact_min` under Exact, at least `budget_min` under
    /// Budget.
    ///
    /// # Errors
    /// [`Error::Assertion`] naming `what`.
    pub fn at_least(
        self,
        what: &str,
        got: Duration,
        exact_min: Duration,
        budget_min: Duration,
    ) -> Result<()> {
        let min = if self.is_exact() {
            exact_min
        } else {
            budget_min
        };
        if got >= min {
            Ok(())
        } else {
            Err(assertion(format!(
                "{what}: {got:?}, expected at least {min:?}"
            )))
        }
    }

    /// Checks heard text: equal to `exact` under Exact; under Budget a word prefix of `full`
    /// whose word count is within [`BUDGET_WORD_SLACK`] of `exact`'s.
    ///
    /// # Errors
    /// [`Error::Assertion`] naming `what`.
    pub fn heard_text(self, what: &str, got: &str, exact: &str, full: &str) -> Result<()> {
        if self.is_exact() {
            return ensure_text(what, got, exact);
        }
        let heard: Vec<&str> = got.split_whitespace().collect();
        let all: Vec<&str> = full.split_whitespace().collect();
        let want = exact.split_whitespace().count();
        let close = heard.len().abs_diff(want) <= BUDGET_WORD_SLACK;
        if all.starts_with(&heard) && close {
            Ok(())
        } else {
            Err(assertion(format!(
                "{what}: {got:?} is not a word prefix of {full:?} within {BUDGET_WORD_SLACK} word \
                 of {exact:?}"
            )))
        }
    }
}

fn ensure_text(what: &str, got: &str, expected: &str) -> Result<()> {
    if got == expected {
        Ok(())
    } else {
        Err(assertion(format!("{what}: {got:?}, expected {expected:?}")))
    }
}

/// An [`Error::Assertion`] with `message`.
pub(crate) fn assertion(message: String) -> Error {
    Error::Assertion(message)
}

/// A fresh paused-time clock for one suite run.
pub(crate) fn sim_clock() -> SharedClock {
    Arc::new(SimClock::new())
}

/// `Some(value)` or an assertion error naming `what`.
pub(crate) fn some<T>(value: Option<T>, what: &str) -> Result<T> {
    value.ok_or_else(|| assertion(format!("missing: {what}")))
}

/// `slice[index]` or an assertion error naming `what`.
pub(crate) fn nth<'a, T>(slice: &'a [T], index: usize, what: &str) -> Result<&'a T> {
    slice
        .get(index)
        .ok_or_else(|| assertion(format!("missing {what} #{index} (have {})", slice.len())))
}

/// Fails with an [`Error::Assertion`] unless the condition holds.
macro_rules! ensure {
    ($cond:expr, $($arg:tt)+) => {
        if !$cond {
            return Err($crate::suite::assertion(format!($($arg)+)));
        }
    };
}

/// Fails with an [`Error::Assertion`] unless both sides are equal.
macro_rules! ensure_eq {
    ($left:expr, $right:expr, $($arg:tt)+) => {{
        let (left, right) = (&$left, &$right);
        if left != right {
            return Err($crate::suite::assertion(format!(
                "{}: {left:?} != {right:?}",
                format!($($arg)+)
            )));
        }
    }};
}

pub(crate) use {ensure, ensure_eq};
