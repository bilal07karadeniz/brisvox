//! Resilience for provider stages: per-provider circuit breakers, retry budgets, an
//! optional secondary provider per stage, mid-call recovery and the proactive refresh of idle
//! TTS sockets.
//!
//! # Shape
//! [`ResilientStt`], [`ResilientLlm`] and [`ResilientTts`] implement the provider traits over a
//! primary [`Leg`] and an optional secondary leg of the same stage, so the engine uses them like
//! any adapter and nothing in the engine changes. A [`Leg`] is a provider instance plus its
//! [`CircuitBreaker`]; breakers and the process [`RetryBudget`] are process-wide (`Arc`s shared by
//! every call), while each wrapper instance is one call: it owns the call's session budget and
//! counters ([`ResilienceStats`]). Build the legs once per process and one wrapper per call:
//!
//! ```ignore
//! let resilience = Resilience::new(ResilienceConfig::DEFAULT, clock.clone());
//! let primary = resilience.leg("deepgram", Arc::new(deepgram));      // once per process
//! let secondary = resilience.leg("assemblyai", Arc::new(assemblyai));
//! let stt = ResilientStt::new(primary.clone(), &resilience)          // per call
//!     .with_secondary(secondary.clone());
//! ```
//!
//! Not wrapping a provider is today's behaviour; a wrapper without a secondary adds the breaker,
//! the budgets, in-place recovery (STT, TTS) and the idle refresh (TTS), and otherwise passes
//! everything through.
//!
//! # Attempts, breakers and budgets
//! - An *operation* is a `connect` (STT, TTS), a `stream` (LLM) or a mid-call recovery. It tries
//!   the legs in order, each at most once: the primary first (a recovery puts the leg that just
//!   failed last). A leg whose breaker refuses (open, or half-open with its probe taken) is
//!   skipped without cost. The first attempt of an operation is free; every further attempt (a
//!   failover, a recovery) takes one token from the call's session budget AND one from the
//!   process budget, so an outage cannot multiply the load on the providers (each bucket refills
//!   one token per [`BudgetConfig::refill_every`]).
//! - Every attempt that may be followed by another runs under
//!   [`ResilienceConfig::attempt_timeout`] on the injected clock; the last attempt of a `connect`
//!   or `stream` relies on the engine's own deadline (connect timeout, LLM first token), the last
//!   attempt of a recovery has the attempt timeout too (no engine deadline covers it).
//! - A breaker counts consecutive failures of its provider across all calls: a failed connect or
//!   request, or a session that dies before it proved healthy (a first event, or
//!   [`ResilienceConfig::healthy_after`] alive). [`BreakerConfig::failure_threshold`] failures
//!   open it for [`BreakerConfig::open_for`]; then one probe attempt is let through (half-open)
//!   and every other attempt is refused. Once the probe connected (STT, TTS) or its response
//!   produced a first event (LLM), the breaker closes *on probation*
//!   ([`Permit::probation`]): other calls are admitted again at once instead of waiting up to
//!   [`ResilienceConfig::healthy_after`] (or a whole LLM response) for the probe to prove
//!   healthy, and one more failure (the probe's own early death included) opens it again at
//!   once. A probe that fails before that opens it again. Request errors (HTTP 400, 404, 413,
//!   422) and cancellations are not provider failures: they neither count nor fail over.
//! - No leg allowed, or the budget spent: the operation fails with
//!   [`Error::Unavailable`](crate::Error::Unavailable) (or the last provider error), and the
//!   engine handles it as before (a failed connect, a dead session, a failed round).
//!
//! # What carries over to the other provider
//! - **STT** ([`ResilientStt`]): the audio of the open utterance (since the last final segment,
//!   plus [`ResilienceConfig::stt_replay_tail`] before it, at most
//!   [`ResilienceConfig::stt_replay`] in all) is replayed to the new session, followed by a
//!   `Finalize` the old session had not answered, so the turn's transcript is complete. The
//!   tail is kept because a provider sends a final segment some hundreds of ms after the audio
//!   it covers while the audio streams on: audio of that lag is not transcribed yet, and a word
//!   at the edge may be heard twice rather than lost. A segment that ends the utterance
//!   (`speech_final`) empties the window: the provider saw the speech end.
//! - **LLM** ([`ResilientLlm`]): a request that fails before any content was delivered (an
//!   error, a cut stream, or no event within [`ResilienceConfig::llm_first_event`] when a
//!   secondary exists) is re-issued to the secondary. After the first content event nothing is
//!   retried here (the engine's own rule: what was spoken stays spoken).
//! - **TTS** ([`ResilientTts`]): utterances that had produced no audio and no word timing yet
//!   are re-spoken on the new session (their text, and their flush), so the caller hears them
//!   whole; cancelled ones are acknowledged. An utterance that was partly heard cannot be resumed exactly (the
//!   provider's word timings may lag its audio), so then the session fails as before and the
//!   engine marks the utterance lost (the heard prefix stays in the context).
//!
//! # Idle refresh
//! A [`ResilientTts`] session that has been idle (no open utterance, nothing in flight) for
//! [`IdleRefresh`] (default: 80 % of [`TtsProvider::idle_limit`](crate::TtsProvider::idle_limit),
//! Cartesia's 5 minutes) opens a new session in the background and swaps it in while still idle,
//! dropping the old one: the next turn finds a fresh socket and never pays a connect, and no
//! audio is in flight at the swap, so nothing is heard.
//!
//! # Tasks and bounds
//! Each wrapped STT/TTS session and each wrapped LLM response runs one relay task. It ends when
//! the engine drops its handles (the provider's own cancellation contract), when the
//! session ends, or when a recovery fails. Channels have the adapters' capacities; at most one
//! item waits between the relay and the provider (the engine's input is not read meanwhile, so
//! backpressure reaches the engine); replay buffers are capped. No lock is held across an await.

mod attempt;
mod breaker;
mod budget;
mod llm;
mod stt;
mod tts;

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::SharedClock;

pub use attempt::{LegId, ResilienceProbe, ResilienceStats};
pub use breaker::{BreakerConfig, BreakerState, BreakerStats, CircuitBreaker, Permit};
pub use budget::{BudgetConfig, RetryBudget};
pub use llm::ResilientLlm;
pub use stt::ResilientStt;
pub use tts::ResilientTts;

/// When a [`ResilientTts`] replaces an idle session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleRefresh {
    /// Never.
    Off,
    /// After 80 % of the provider's [`idle_limit`](crate::TtsProvider::idle_limit) without
    /// traffic; never for a provider without a limit.
    Auto,
    /// After this long without traffic.
    After(Duration),
}

/// Settings of the resilience layer (one value per process; see the [module docs](self)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResilienceConfig {
    /// The circuit breaker of every leg built by [`Resilience::leg`].
    pub breaker: BreakerConfig,
    /// Retries one call may make (per wrapper instance).
    pub session_budget: BudgetConfig,
    /// Retries the whole process may make (shared by every call of a [`Resilience`]).
    pub process_budget: BudgetConfig,
    /// Deadline of an attempt that may be followed by another (a connect or a request with a
    /// leg left to try, and every recovery attempt). Default 2 s: within the engine's 5 s
    /// connect timeout a failover still has time.
    pub attempt_timeout: Duration,
    /// With a secondary LLM: a response with no event this long after its request started is
    /// abandoned and re-issued to the secondary. Default 4 s (the engine's first-token timeout
    /// is 8 s).
    pub llm_first_event: Duration,
    /// A session that lived this long (or delivered an event) proved its provider healthy.
    /// Default 10 s.
    pub healthy_after: Duration,
    /// Audio of the open utterance kept for an STT recovery (a rolling window, cut back to
    /// [`Self::stt_replay_tail`] at every final segment, emptied at the end of an utterance).
    /// Default 4 s: about 128 KiB per call at 16 kHz, held all the time while the caller is
    /// silent (the engine streams silence too), so keep it short.
    pub stt_replay: Duration,
    /// Audio kept for an STT recovery at a final segment that does not end the utterance: the
    /// provider's finals lag the audio they cover, so the most recent audio may not be in the
    /// final yet. Default 1 s (a word at the edge may be transcribed twice after a recovery;
    /// none is lost to a lag up to this long).
    pub stt_replay_tail: Duration,
    /// Text kept per utterance for a TTS recovery; a longer utterance is not replayed.
    /// Default 4 KiB.
    pub tts_replay_bytes: usize,
    /// Open utterances kept for a TTS recovery; older ones are not replayed. Default 8.
    pub tts_replay_contexts: usize,
    /// When an idle TTS session is replaced. Default [`IdleRefresh::Auto`].
    pub idle_refresh: IdleRefresh,
}

impl ResilienceConfig {
    /// The defaults documented on each field.
    pub const DEFAULT: Self = Self {
        breaker: BreakerConfig::DEFAULT,
        session_budget: BudgetConfig::SESSION,
        process_budget: BudgetConfig::PROCESS,
        attempt_timeout: Duration::from_secs(2),
        llm_first_event: Duration::from_secs(4),
        healthy_after: Duration::from_secs(10),
        stt_replay: Duration::from_secs(4),
        stt_replay_tail: Duration::from_secs(1),
        tts_replay_bytes: 4096,
        tts_replay_contexts: 8,
        idle_refresh: IdleRefresh::Auto,
    };
}

impl Default for ResilienceConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The process-wide part of the resilience layer: the configuration, the clock and the process
/// retry budget. Cheap to clone (the budget is shared).
#[derive(Debug, Clone)]
pub struct Resilience {
    config: ResilienceConfig,
    clock: SharedClock,
    process: Arc<RetryBudget>,
}

impl Resilience {
    /// A new process-wide context with its own process budget.
    pub fn new(config: ResilienceConfig, clock: SharedClock) -> Self {
        let process = Arc::new(RetryBudget::new(config.process_budget, clock.clone()));
        Self {
            config,
            clock,
            process,
        }
    }

    /// The configuration.
    pub const fn config(&self) -> &ResilienceConfig {
        &self.config
    }

    /// The clock everything runs on.
    pub fn clock(&self) -> &SharedClock {
        &self.clock
    }

    /// The process retry budget (shared by every call).
    pub fn process_budget(&self) -> &Arc<RetryBudget> {
        &self.process
    }

    /// A leg over `provider` with a new breaker named `name` (build one per provider instance
    /// and process, and clone it into every call).
    pub fn leg<P>(&self, name: &'static str, provider: Arc<P>) -> Leg<P> {
        Leg::new(
            provider,
            Arc::new(CircuitBreaker::new(
                name,
                self.config.breaker,
                self.clock.clone(),
            )),
        )
    }
}

/// One provider of a stage and its process-wide circuit breaker. Cloning shares both.
#[derive(Debug)]
pub struct Leg<P> {
    provider: Arc<P>,
    breaker: Arc<CircuitBreaker>,
}

impl<P> Clone for Leg<P> {
    fn clone(&self) -> Self {
        Self {
            provider: Arc::clone(&self.provider),
            breaker: Arc::clone(&self.breaker),
        }
    }
}

impl<P> Leg<P> {
    /// A leg over `provider` guarded by `breaker`.
    pub const fn new(provider: Arc<P>, breaker: Arc<CircuitBreaker>) -> Self {
        Self { provider, breaker }
    }

    /// The provider.
    pub const fn provider(&self) -> &Arc<P> {
        &self.provider
    }

    /// The breaker.
    pub const fn breaker(&self) -> &Arc<CircuitBreaker> {
        &self.breaker
    }
}

/// Whether `error` says the provider is unhealthy (counted by its breaker, worth a failover):
/// everything but a cancellation, the layer's own refusal and request-shaped HTTP errors.
pub(crate) fn is_provider_failure(error: &crate::Error) -> bool {
    match error {
        crate::Error::Core(brisvox_core::Error::Cancelled) | crate::Error::Unavailable { .. } => {
            false
        }
        crate::Error::Rejected { status, .. } => !matches!(status, 400 | 404 | 413 | 422),
        _ => true,
    }
}

#[cfg(test)]
mod tests;
