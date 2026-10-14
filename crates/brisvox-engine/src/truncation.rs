//! [`TruncationSource`]: where the LLM runner learns what the caller actually heard of a
//! response (`docs/PIPELINE.md` section 5).
//!
//! After an interruption the LLM context must contain only what was played. The runner knows
//! which assistant message belongs to which round and when the response was interrupted; it does
//! not know how much audio the caller heard. That knowledge lives behind this trait:
//!
//! - The LLM runner defines the trait, its use of it and a stub, [`KeepAll`], that keeps every
//!   message as the LLM wrote it.
//! - The production pipeline implements it over the [`PlayoutBook`]: [`PlayoutTruncation`], a bounded wait on
//!   the injected clock for the transport's final position, then `PlayedText::heard` at
//!   `min(released, total) - lead` (see below).
//! - The hybrid test of gate 3 implements it over the reference pipeline's word and playout logs.
//!
//! # Protocol
//! At the next `UserTurnEnded` (and at `Stop`), before the user message is pushed, the runner
//! asks [`TruncationSource::cut`] once per round that has an assistant message in the context,
//! from the last round to the first (so a removed message does not shift the indices still to be
//! processed). `interrupted_at` is the instant the barge-in happened (the position is judged as
//! of then, not as of the later moment of the question); `None` means the response ended by
//! itself. The answer is applied with `apply_truncation`; whatever outcome it produces (unchanged,
//! shortened, removed) is the policy, and the runner adds no omission rule of its own.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_agent::{PlayedConfig, Truncation};
use brisvox_core::clock::deadline_after;
use brisvox_core::{SharedClock, TurnEpoch};
use tokio::time::Instant;

use crate::config::EngineConfig;
use crate::playout::{Heard, PlayoutBook, RoundKey};

/// A boxed future, as returned by [`TruncationSource::cut`] (the trait is used as a trait object).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One assistant message that may have to be cut to what was heard.
#[derive(Debug, Clone)]
pub struct CutRequest {
    /// The round the message belongs to: the turn's epoch and the round's ordinal within it.
    pub key: RoundKey,
    /// The message text as it is in the context now.
    pub sent: Arc<str>,
    /// When the caller barged in, judged on the injected clock; `None` when the response ended
    /// without an interruption.
    pub interrupted_at: Option<Instant>,
}

/// What to do with the message of a [`CutRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cut {
    /// Leave the message as the LLM wrote it.
    Keep,
    /// Replace the message text with the heard prefix and count it as a truncation
    /// ([`TruncationStats`](brisvox_agent::TruncationStats)).
    Heard(Truncation),
    /// The caller heard the whole message: apply the (whitespace-trimmed) text but do not count
    /// it as a cut.
    HeardInFull(Truncation),
}

/// Answers, per round, how much of its message the caller heard. See the [module docs](self).
pub trait TruncationSource: Send + Sync + 'static {
    /// The decision for `request`. May wait (bounded by the injected clock) for the transport's
    /// final playback position; the runner calls it from its own task only, never while it holds
    /// the context lock.
    fn cut<'a>(&'a self, request: &'a CutRequest) -> BoxFuture<'a, Cut>;
}

/// The stub source: every message stays as the LLM wrote it.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeepAll;

impl TruncationSource for KeepAll {
    fn cut<'a>(&'a self, _request: &'a CutRequest) -> BoxFuture<'a, Cut> {
        Box::pin(async { Cut::Keep })
    }
}

/// How [`PlayoutTruncation`] decides.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayoutTruncationConfig {
    /// Subtracted from the released audio when the transport reports no playback lead (audio
    /// queued in the wire queue and the client buffer). Zero where the oracle is wire arrival.
    pub lead_margin: Duration,
    /// How long to wait for the final `PlaybackPosition` after a `Clear` before using the last
    /// known one. It must fit inside the runner's bound for the whole pass,
    /// [`Timeouts::analysis_job`](crate::Timeouts::analysis_job).
    pub settle_wait: Duration,
    /// How the heard text is decided (word start, estimate rate).
    pub played: PlayedConfig,
}

impl PlayoutTruncationConfig {
    /// The `lead_margin` and `settle_wait` of `config`, the default [`PlayedConfig`].
    pub fn from_engine(config: &EngineConfig) -> Self {
        Self {
            lead_margin: config.lead_margin,
            // `EngineConfig::validate` rejects a longer wait; clamp for an unvalidated config.
            settle_wait: config.settle_wait.min(config.timeouts.analysis_job / 2),
            played: PlayedConfig::default(),
        }
    }
}

/// A settle wait that timed out, remembered for the rest of the pass.
#[derive(Debug, Clone, Copy)]
struct TimedOut {
    through: TurnEpoch,
    until: Instant,
}

/// The production [`TruncationSource`]: what the caller heard, from the [`PlayoutBook`] the
/// pump, TTS stage and transport stage write (`docs/PIPELINE.md` section 5).
///
/// For one round:
/// 1. When the response was interrupted, wait until the transport has reported its final
///    position for every utterance up to the round's epoch
///    ([`PlayoutBook::wait_settled_through`]): at most `settle_wait` on the injected clock
///    counted from the barge-in, so nothing is waited for when the question comes later. A
///    timeout is counted (`settle_timeouts`) and the last known positions are used. After a
///    timeout the pass does not wait again: the rounds of one pass are asked one after the
///    other, and every one would otherwise add a full wait.
/// 2. An utterance played to its end (its mark, or the mark of a later utterance, was echoed)
///    is kept whole as sent: [`Cut::HeardInFull`].
/// 3. Otherwise [`Cut::Heard`] with `PlayedText::heard` at
///    `min(released, total_audio) - lead`, never below zero. The released position is an upper
///    bound of playback. `lead` is the client's reported playback lead
///    ([`PlayoutBook::client_lead`], Twilio) when the transport has one, else
///    [`PlayoutTruncationConfig::lead_margin`] less the time between the last audio released to
///    the wire and the barge-in ([`RoundSnapshot::margin_at`](crate::RoundSnapshot::margin_at):
///    after a tool or filler wait the client's buffer is empty and nothing is subtracted); the
///    audio of later utterances in the client's buffer is taken off it first. A response that
///    ended without an interruption has played out: no lead is subtracted, neither the margin nor
///    a reported client lead (also when its end mark was never echoed).
///
/// A round the book does not know (evicted) or with no text is kept as it is.
#[derive(Debug)]
pub struct PlayoutTruncation {
    book: Arc<PlayoutBook>,
    clock: SharedClock,
    config: PlayoutTruncationConfig,
    timed_out: Mutex<Option<TimedOut>>,
    unnoted_clear_cuts: AtomicU64,
}

impl PlayoutTruncation {
    /// A source over `book`, waiting on `clock`. The clock is also attached to the book
    /// ([`PlayoutBook::attach_clock`]), which stamps when audio last went to the wire.
    pub fn new(
        book: Arc<PlayoutBook>,
        clock: SharedClock,
        config: PlayoutTruncationConfig,
    ) -> Self {
        book.attach_clock(clock.clone());
        Self {
            book,
            clock,
            config,
            timed_out: Mutex::new(None),
            unnoted_clear_cuts: AtomicU64::new(0),
        }
    }

    /// Interrupted responses asked about while the book had never been told of a clear
    /// ([`BookStats::clears_noted`](crate::BookStats::clears_noted) was zero). Non-zero means
    /// the turn driver was wired with `with_commands` instead of [`clear_hook`](crate::clear_hook):
    /// every mid-utterance barge-in then waits out `settle_wait` and the client lead is not tied
    /// to its clear. The assembly asserts it stays zero; the first occurrence is also logged as
    /// an error.
    pub fn unnoted_clear_cuts(&self) -> u64 {
        self.unnoted_clear_cuts.load(Ordering::Relaxed)
    }

    /// The book this source reads.
    pub fn book(&self) -> &Arc<PlayoutBook> {
        &self.book
    }

    /// Waits for the final position of every utterance up to `epoch`, once per pass. The wait
    /// counts from the barge-in (`interrupted_at`): the transport flushes the final position
    /// within moments of the clear, so a position that is still missing `settle_wait` after it
    /// is not coming, and a question asked later (the caller talks on for a while before the
    /// turn ends) is answered without waiting at all.
    async fn settle(&self, epoch: TurnEpoch, interrupted_at: Instant) {
        let now = self.clock.now();
        let budget = self
            .config
            .settle_wait
            .saturating_sub(now.saturating_duration_since(interrupted_at));
        let memo = *self
            .timed_out
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if memo.is_some_and(|m| epoch <= m.through && now < m.until) {
            return;
        }
        let settled = self
            .book
            .wait_settled_through(epoch, &*self.clock, budget)
            .await;
        let mut slot = self
            .timed_out
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *slot = (!settled).then(|| TimedOut {
            through: epoch,
            until: deadline_after(self.clock.now(), self.config.settle_wait),
        });
    }

    fn decide(&self, request: &CutRequest) -> Cut {
        let Some(mut snapshot) = self.book.snapshot(request.key) else {
            return Cut::Keep;
        };
        let sent = request.sent.trim_end();
        if sent.is_empty() {
            return Cut::Keep;
        }
        // A response that ended by itself has played out by the time the next turn starts: no
        // margin, and the client's last reported lead (audio it held at some earlier moment) is
        // not subtracted either, even when the end mark was never echoed.
        let margin = if let Some(at) = request.interrupted_at {
            // The fixed margin assumes audio was still going out; it shrinks by the time since
            // the last release (a tool or filler wait after the utterance drained the buffer).
            snapshot.margin_at(self.config.lead_margin, at)
        } else {
            snapshot.client_lead = None;
            Duration::ZERO
        };
        match snapshot.heard(margin, &self.config.played) {
            Heard::Whole => Cut::HeardInFull(Truncation {
                text: Arc::from(sent),
                estimated: false,
                words_heard: sent.split_whitespace().count(),
                unmatched_words: 0,
                gaps: 0,
                played: snapshot.heard_position(margin),
            }),
            Heard::Cut(truncation) => Cut::Heard(truncation),
        }
    }
}

impl TruncationSource for PlayoutTruncation {
    fn cut<'a>(&'a self, request: &'a CutRequest) -> BoxFuture<'a, Cut> {
        Box::pin(async move {
            if let Some(at) = request.interrupted_at {
                if self.book.stats().clears_noted == 0
                    && self.unnoted_clear_cuts.fetch_add(1, Ordering::Relaxed) == 0
                {
                    tracing::error!(
                        target: "brisvox::engine",
                        "a barge-in is cut but the playout book never saw a clear:                          the turn driver must use playout::clear_hook"
                    );
                }
                self.settle(request.key.epoch, at).await;
            }
            self.decide(request)
        })
    }
}
