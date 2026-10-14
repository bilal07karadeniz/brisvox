//! [`RequestBuilder`]: the single place where an LLM request is made from the conversation
//! (`docs/PIPELINE.md` section 4; the insertion point of the context window).
//!
//! Every request of the runner goes through [`RequestBuilder::build`]:
//!
//! 1. [`build_window`] derives the window the LLM sees from the full [`Context`] with the
//!    session's [`WindowPolicy`] (`EngineConfig::llm.window`). The full transcript is never
//!    modified; the leading system messages (the prompt, or the flow node's prompt) are always
//!    kept; older turns are dropped and covered by the summary when there is one.
//! 2. A turn with many tool rounds is bounded too. The window keeps the latest turn
//!    whole, and each message of it is clipped to a quarter of the budget, so ten large tool
//!    results can still add up to far more than the budget. When the window is over budget, the
//!    tool results of that latest turn are clipped, oldest first, to
//!    [`LlmConfig::tool_result_floor_tokens`](crate::LlmConfig::tool_result_floor_tokens)
//!    until the request fits (tool calls and their results stay paired, so the request stays
//!    valid). The full transcript keeps the whole text.
//! 3. The window is converted to the provider form ([`crate::convert`]) and paired with the
//!    tools of the round (name-ordered, so a request is byte-identical across rounds).
//!
//! # Summaries
//! A [`Summarizer`] (usually an LLM call) keeps the summary of the dropped turns up to date, and
//! it does so **off the request path**: a request is never delayed by a summary. The runner
//! calls [`RequestBuilder::refresh_ahead`] from a task of its own after each response (see
//! [`crate::llm`]); a request that is built before the summary caught up goes out with the
//! summary it has, and the dropped turns the summary does not cover yet are counted
//! (`uncovered_requests`).
//!
//! Because the next user message is not there yet when the refresh runs, `refresh_ahead`
//! summarizes as the window will look **once the next user turn has been added** (a placeholder
//! user turn the size of the last one), so in steady state the summary is ready before the turn
//! that needs it. [`RequestBuilder::refresh`] summarizes the window as it is now.
//!
//! `refresh_ahead` never covers the turn that has just finished: its last assistant message is
//! not final until the played-text cut ran (a barge-in right after the response changes or
//! removes it), and late notes and interrupted-call results are still appended to it. The summary
//! of an ahead refresh therefore ends at the start of the last real user turn
//! (`refresh_summary_until`), which stays a turn start whatever happens to that turn afterwards.
//! The price: when the just-finished turn alone is too large for the window (its tool results
//! exceed the budget), the request after it goes out without that turn (counted as
//! `uncovered_requests`) and the next refresh covers it. The invariant "the LLM context holds
//! only what was played" wins over covering the heavy turn one request earlier.
//!
//! A refresh runs under a clock-based timeout (`LlmConfig::summary_timeout`, a bound for the
//! background call, not for a request) and never stores a summary the window rejects. A
//! summarizer that keeps returning a summary that is too large cannot freeze the summary
//! silently: every rejection is counted ([`RequestStatsSnapshot`]: `summary_too_large`,
//! `consecutive_summary_failures`), logged at `warn` on the tracing target `brisvox::engine`,
//! and the previous summary stays in use. After [`SUMMARY_DEGRADED_AFTER`] failures in a row a
//! circuit breaker backs the attempts off exponentially (the timeout, doubled per further
//! failure, at most [`MAX_SUMMARY_BACKOFF`]); skipped attempts are counted (`summary_backoffs`),
//! so a hanging or broken summarizer costs a bounded number of billed calls, not one per turn.
//!
//! A summary computed for a context that was replaced meanwhile (a flow `Reset`, see
//! [`RequestBuilder::reset_summary`]) is discarded: every reset bumps a generation the refresh
//! checks before it stores. The check and the store happen under the summary mutex, and the
//! reset bumps the generation and clears the summary under the same mutex, so a refresh can never
//! store the summary of the old conversation after the reset.
//!
//! # Cost
//! `build` walks the full context once for the cut (turn starts, validation: O(messages), a few
//! ns each, the full transcript is never trimmed) and clips only the messages of the window.
//! Clipping ([`clipped`]) gallops up from a small prefix, so a 64 KiB tool result costs
//! O(`max_message_tokens`) bytes of copying, not O(64 KiB) per bisection step; the over-budget
//! rule uses the same function. A request in a long call, or with large messages in the latest
//! turn, is **not** built on the tokio worker: [`RequestBuilder::build_offloaded`] hands a build
//! to the [`Offload`] when the context passes [`INLINE_MAX_MESSAGES`] messages, the latest
//! turn passes [`INLINE_MAX_TURN_BYTES`] bytes of text or the whole context passes
//! [`INLINE_MAX_CONTEXT_BYTES`] (large results of older turns), and the snapshot of a summary refresh
//! (which clones the context) follows the same rule. Below those sizes the work is bounded by a
//! few tens of microseconds (measured by `tests/llm_window.rs`, see `docs/PIPELINE.md`). A pool
//! that refuses or times out the job falls back to tokio's blocking threads (counted:
//! `offload_fallbacks`), because a request must not fail on a busy pool; never to the worker
//! itself, where the heavy build would break the 50 us rule and could block on the context lock
//! the timed-out pool job still holds.
//!
//! The builder is shared by reference between the round tasks of a session (one is alive at a
//! time); its only mutable parts are the stored summary and the backoff instant, behind mutexes
//! that are never held across an await.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_agent::context::{CharsPerToken, Context, Message, TokenEstimator};
use brisvox_agent::window::{
    Summarizer, Summary, Window, WindowError, WindowPolicy, build_window, clipped, needs_summary,
    refresh_summary_until,
};
use brisvox_core::Clock;
use brisvox_providers::{LlmRequest, ToolSpec};
use serde_json::Value;
use tokio::time::Instant;

use crate::convert::to_provider_messages;
use crate::offload::Offload;

/// A context with more messages than this is not built or cloned on a tokio worker.
pub const INLINE_MAX_MESSAGES: usize = 384;

/// A context whose latest turn holds more bytes of text than this is not built or cloned on a
/// tokio worker.
pub const INLINE_MAX_TURN_BYTES: usize = 16 * 1024;

/// A context that holds more bytes of text than this in all (older turns included: a large tool
/// result of an earlier turn is copied by a snapshot and scanned by a window build too) is not
/// built or cloned on a tokio worker.
pub const INLINE_MAX_CONTEXT_BYTES: usize = 256 * 1024;

/// Whether work on `context` (a window build, a snapshot clone) may exceed the ~50 us a tokio
/// worker is allowed: the transcript is long, the latest turn holds a lot of text (large tool
/// results), or the whole context does (large results of older turns). O(messages), at most
/// [`INLINE_MAX_MESSAGES`] steps; stops early.
pub fn is_heavy(context: &Context) -> bool {
    let messages = context.messages();
    if messages.len() > INLINE_MAX_MESSAGES {
        return true;
    }
    let mut bytes = 0usize;
    let mut in_latest_turn = true;
    for message in messages.iter().rev() {
        bytes = bytes.saturating_add(message.text().len());
        if (in_latest_turn && bytes > INLINE_MAX_TURN_BYTES) || bytes > INLINE_MAX_CONTEXT_BYTES {
            return true;
        }
        if matches!(message, Message::User { .. }) {
            in_latest_turn = false;
        }
    }
    false
}

/// A request made by [`RequestBuilder::build`] and what the window did to produce it.
#[derive(Debug, Clone, PartialEq)]
pub struct BuiltRequest {
    /// The request to send.
    pub request: LlmRequest,
    /// Estimated tokens of the messages in the request (summary included).
    pub estimated_tokens: usize,
    /// Whole turns kept verbatim.
    pub kept_turns: usize,
    /// The request still exceeds the budget: the prompt or the clipped latest turn is larger
    /// than the budget by itself.
    pub over_budget: bool,
    /// Tool results of the latest turn that were clipped further to fit (see the module docs).
    pub clipped_tool_results: usize,
    /// Messages dropped from the window that no summary covers (lost to the model).
    pub uncovered: usize,
}

#[derive(Debug, Default)]
struct Counters {
    windows: AtomicU64,
    over_budget: AtomicU64,
    uncovered_requests: AtomicU64,
    clipped_tool_results: AtomicU64,
    window_errors: AtomicU64,
    invalid_summaries: AtomicU64,
    summaries: AtomicU64,
    summary_too_large: AtomicU64,
    summary_failures: AtomicU64,
    summary_timeouts: AtomicU64,
    summary_backoffs: AtomicU64,
    summaries_discarded: AtomicU64,
    builds_offloaded: AtomicU64,
    snapshots_offloaded: AtomicU64,
    offload_fallbacks: AtomicU64,
    consecutive_summary_failures: AtomicU64,
    max_estimated_tokens: AtomicUsize,
    last_estimated_tokens: AtomicUsize,
    summary_covers_until: AtomicUsize,
    summary_degraded: AtomicBool,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Counters of a [`RequestBuilder`], shared and readable while the session runs.
#[derive(Debug, Clone, Default)]
pub struct RequestStats {
    counters: Arc<Counters>,
}

/// A copy of the [`RequestStats`] counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestStatsSnapshot {
    /// Requests built (each through `build_window`).
    pub windows: u64,
    /// Of those, requests still over budget after clipping.
    pub over_budget: u64,
    /// Requests whose window dropped messages that no summary covers.
    pub uncovered_requests: u64,
    /// Tool results of a latest turn clipped to fit the budget.
    pub clipped_tool_results: u64,
    /// Windows that could not be built (the context was invalid).
    pub window_errors: u64,
    /// Stored summaries the window rejected as misaligned and the builder dropped (the context
    /// was replaced under it).
    pub invalid_summaries: u64,
    /// Summary refreshes that stored a new summary.
    pub summaries: u64,
    /// Summaries rejected because they exceed `max_summary_tokens` (the summary is not updated).
    pub summary_too_large: u64,
    /// Summarizer failures other than a too-large summary.
    pub summary_failures: u64,
    /// Refreshes that hit the clock-based timeout.
    pub summary_timeouts: u64,
    /// Refresh attempts skipped because the circuit breaker is backing off after repeated
    /// failures.
    pub summary_backoffs: u64,
    /// Refreshes whose result was discarded because the history was replaced meanwhile.
    pub summaries_discarded: u64,
    /// Requests built on the [`Offload`] because the context was heavy (see [`is_heavy`]).
    pub builds_offloaded: u64,
    /// Summary snapshots (context clone and range computation) made on the [`Offload`].
    pub snapshots_offloaded: u64,
    /// Offloaded jobs the pool refused or timed out, done inline instead.
    pub offload_fallbacks: u64,
    /// Failed refreshes in a row (reset by a success).
    pub consecutive_summary_failures: u64,
    /// Largest estimated request so far.
    pub max_estimated_tokens: usize,
    /// Estimated size of the latest request.
    pub last_estimated_tokens: usize,
    /// Index in the full context one past the last message the stored summary covers (0: there is
    /// no summary).
    pub summary_covers_until: usize,
    /// Three or more failed refreshes in a row: the summary is not being kept up to date.
    pub summary_degraded: bool,
}

/// Failed refreshes in a row after which [`RequestStatsSnapshot::summary_degraded`] is set and
/// the circuit breaker starts backing attempts off.
pub const SUMMARY_DEGRADED_AFTER: u64 = 3;

/// The longest the circuit breaker waits before the next refresh attempt.
pub const MAX_SUMMARY_BACKOFF: Duration = Duration::from_secs(60);

/// Estimated bytes of a JSON value's compact text, without serializing it.
fn json_len(value: &Value) -> usize {
    match value {
        Value::Null => 4,
        Value::Bool(_) => 5,
        Value::Number(_) => 8,
        Value::String(text) => text.len().saturating_add(2),
        Value::Array(items) => items.iter().fold(2usize, |sum, item| {
            sum.saturating_add(json_len(item)).saturating_add(1)
        }),
        Value::Object(map) => map.iter().fold(2usize, |sum, (key, item)| {
            sum.saturating_add(key.len())
                .saturating_add(json_len(item))
                .saturating_add(4)
        }),
    }
}

/// The estimator the window uses by default: [`CharsPerToken`], plus the opaque `provider_data`
/// of assistant messages and of their tool calls (a thinking signature or reasoning state is
/// echoed in every later request, so it counts against the budget; `CharsPerToken` alone ignores
/// it).
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowEstimator;

impl TokenEstimator for WindowEstimator {
    fn estimate(&self, message: &Message) -> usize {
        let base = CharsPerToken.estimate(message);
        let Message::Assistant {
            provider_data,
            tool_calls,
            ..
        } = message
        else {
            return base;
        };
        let bytes = provider_data.as_ref().map_or(0, json_len).saturating_add(
            tool_calls
                .iter()
                .filter_map(|call| call.provider_data.as_ref())
                .map(json_len)
                .sum::<usize>(),
        );
        base.saturating_add(bytes.div_ceil(4))
    }
}

impl RequestStats {
    /// A copy of the counters.
    pub fn snapshot(&self) -> RequestStatsSnapshot {
        let c = &*self.counters;
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        RequestStatsSnapshot {
            windows: get(&c.windows),
            over_budget: get(&c.over_budget),
            uncovered_requests: get(&c.uncovered_requests),
            clipped_tool_results: get(&c.clipped_tool_results),
            window_errors: get(&c.window_errors),
            invalid_summaries: get(&c.invalid_summaries),
            summaries: get(&c.summaries),
            summary_too_large: get(&c.summary_too_large),
            summary_failures: get(&c.summary_failures),
            summary_timeouts: get(&c.summary_timeouts),
            summary_backoffs: get(&c.summary_backoffs),
            summaries_discarded: get(&c.summaries_discarded),
            builds_offloaded: get(&c.builds_offloaded),
            snapshots_offloaded: get(&c.snapshots_offloaded),
            offload_fallbacks: get(&c.offload_fallbacks),
            consecutive_summary_failures: get(&c.consecutive_summary_failures),
            max_estimated_tokens: c.max_estimated_tokens.load(Ordering::Relaxed),
            last_estimated_tokens: c.last_estimated_tokens.load(Ordering::Relaxed),
            summary_covers_until: c.summary_covers_until.load(Ordering::Relaxed),
            summary_degraded: c.summary_degraded.load(Ordering::Relaxed),
        }
    }

    /// Counts a failed refresh; returns the failures in a row.
    fn failed_refresh(&self) -> u64 {
        let c = &*self.counters;
        let failures = c
            .consecutive_summary_failures
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if failures >= SUMMARY_DEGRADED_AFTER {
            c.summary_degraded.store(true, Ordering::Relaxed);
        }
        failures
    }

    fn successful_refresh(&self) {
        let c = &*self.counters;
        c.consecutive_summary_failures.store(0, Ordering::Relaxed);
        c.summary_degraded.store(false, Ordering::Relaxed);
    }
}

/// Builds the LLM requests of a session. See the [module docs](self).
pub struct RequestBuilder {
    policy: WindowPolicy,
    estimator: Arc<dyn TokenEstimator>,
    summarizer: Option<Arc<dyn Summarizer>>,
    summary: Mutex<Option<Summary>>,
    /// Bumped by [`Self::reset_summary`]: a refresh that started before a reset must not store.
    generation: AtomicU64,
    /// The circuit breaker: no refresh attempt before this instant.
    not_before: Mutex<Option<Instant>>,
    summary_timeout: Duration,
    tool_result_floor_tokens: usize,
    /// Where heavy builds and snapshots run, and the bound for such a job.
    offload: Option<(Offload, Duration)>,
    stats: RequestStats,
}

impl std::fmt::Debug for RequestBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestBuilder")
            .field("policy", &self.policy)
            .field("summarizer", &self.summarizer.is_some())
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl RequestBuilder {
    /// A builder for `policy`, estimating tokens with [`WindowEstimator`], without a summarizer.
    /// `summary_timeout` bounds each [`Self::refresh`] (a background call); `tool_result_floor_tokens` is the clip
    /// size of the over-budget rule (see the module docs).
    pub fn new(
        policy: WindowPolicy,
        summary_timeout: Duration,
        tool_result_floor_tokens: usize,
    ) -> Self {
        Self {
            policy,
            estimator: Arc::new(WindowEstimator),
            summarizer: None,
            summary: Mutex::new(None),
            generation: AtomicU64::new(0),
            not_before: Mutex::new(None),
            summary_timeout,
            tool_result_floor_tokens,
            offload: None,
            stats: RequestStats::default(),
        }
    }

    /// Keeps the summary of dropped turns up to date with `summarizer`.
    #[must_use]
    pub fn with_summarizer(mut self, summarizer: Arc<dyn Summarizer>) -> Self {
        self.summarizer = Some(summarizer);
        self
    }

    /// Runs heavy builds and summary snapshots (see [`is_heavy`]) on `offload`, each bounded by
    /// `timeout`. Without it everything runs inline.
    #[must_use]
    pub fn with_offload(mut self, offload: Offload, timeout: Duration) -> Self {
        self.offload = Some((offload, timeout));
        self
    }

    /// Estimates tokens with `estimator` instead of [`WindowEstimator`].
    #[must_use]
    pub fn with_estimator(mut self, estimator: Arc<dyn TokenEstimator>) -> Self {
        self.estimator = estimator;
        self
    }

    /// The window policy.
    pub const fn policy(&self) -> &WindowPolicy {
        &self.policy
    }

    /// Shared counters.
    pub fn stats(&self) -> RequestStats {
        self.stats.clone()
    }

    /// The summarizer, if there is one.
    pub fn summarizer(&self) -> Option<Arc<dyn Summarizer>> {
        self.summarizer.clone()
    }

    /// The stored summary of the dropped turns.
    pub fn summary(&self) -> Option<Summary> {
        lock(&self.summary).clone()
    }

    /// Forgets the summary. Required when the history was replaced (a flow `Reset`): its
    /// `covers_until` index would point into a different conversation.
    pub fn reset_summary(&self) {
        // Under the summary mutex, like the refresh's check-and-store: a refresh either stored
        // before this (and is cleared here) or sees the new generation and discards.
        let mut stored = lock(&self.summary);
        self.generation.fetch_add(1, Ordering::SeqCst);
        *stored = None;
        self.stats
            .counters
            .summary_covers_until
            .store(0, Ordering::Relaxed);
    }

    /// The request for the next LLM round: the window of `context` converted to the provider
    /// form, with `tools` (already name-ordered). This is the one place requests are made.
    ///
    /// # Errors
    /// [`WindowError::InvalidContext`] when `context` breaks the ordering rules (a bug of the
    /// caller; counted as `window_errors`). A misaligned stored summary is dropped and counted,
    /// not an error.
    pub fn build(
        &self,
        context: &Context,
        tools: &[ToolSpec],
    ) -> Result<BuiltRequest, WindowError> {
        let estimator: &dyn TokenEstimator = &*self.estimator;
        let summary = lock(&self.summary).clone();
        let built = match build_window(context, &self.policy, estimator, summary.as_ref()) {
            Err(WindowError::InvalidSummary { covers_until }) => {
                tracing::warn!(
                    target: "brisvox::engine",
                    covers_until,
                    "stored summary no longer matches the context; dropping it"
                );
                bump(&self.stats.counters.invalid_summaries);
                self.reset_summary();
                build_window(context, &self.policy, estimator, None)
            }
            other => other,
        };
        let mut window = match built {
            Ok(window) => window,
            Err(error) => {
                bump(&self.stats.counters.window_errors);
                return Err(error);
            }
        };
        let clipped = if window.over_budget {
            self.bound_latest_turn(&mut window)
        } else {
            0
        };
        let c = &*self.stats.counters;
        bump(&c.windows);
        if window.over_budget {
            bump(&c.over_budget);
        }
        let uncovered = window.uncovered.len();
        if uncovered > 0 {
            bump(&c.uncovered_requests);
        }
        c.clipped_tool_results.fetch_add(
            u64::try_from(clipped).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        c.max_estimated_tokens
            .fetch_max(window.estimated_tokens, Ordering::Relaxed);
        c.last_estimated_tokens
            .store(window.estimated_tokens, Ordering::Relaxed);
        Ok(BuiltRequest {
            request: LlmRequest {
                messages: to_provider_messages(&window.context),
                tools: tools.to_vec(),
            },
            estimated_tokens: window.estimated_tokens,
            kept_turns: window.kept_turns,
            over_budget: window.over_budget,
            clipped_tool_results: clipped,
            uncovered,
        })
    }

    /// [`Self::build`] for the live context of a session. A heavy context (see [`is_heavy`]) is
    /// built on the [`Offload`] (the build locks the context there), anything else inline. If the
    /// pool refuses or times out the job, the request is built on tokio's blocking threads: it
    /// must not fail on a busy pool (counted as `offload_fallbacks`), and the heavy build must
    /// not run on the worker, where it could also wait for the context lock a timed-out pool job
    /// still holds. Only if that fails too (the build panicked, or the runtime is shutting down)
    /// is it built inline.
    ///
    /// # Errors
    /// The same as [`Self::build`].
    pub async fn build_offloaded(
        self: &Arc<Self>,
        context: &Arc<Mutex<Context>>,
        tools: &[ToolSpec],
    ) -> Result<BuiltRequest, WindowError> {
        if let Some((offload, timeout)) = &self.offload
            && is_heavy(&lock(context))
        {
            let (this, ctx, specs) = (Arc::clone(self), Arc::clone(context), tools.to_vec());
            match offload
                .run(*timeout, move || this.build(&lock(&ctx), &specs))
                .await
            {
                Ok(done) => {
                    bump(&self.stats.counters.builds_offloaded);
                    return done.value;
                }
                Err(error) => {
                    tracing::warn!(
                        target: "brisvox::engine",
                        %error,
                        "offloaded request build failed; building on a blocking thread"
                    );
                    bump(&self.stats.counters.offload_fallbacks);
                    let (this, ctx, specs) =
                        (Arc::clone(self), Arc::clone(context), tools.to_vec());
                    // Bounded CPU work (the build of one window): it ends by itself, so the
                    // await needs no timer. Cancelling the round detaches it.
                    match tokio::task::spawn_blocking(move || this.build(&lock(&ctx), &specs)).await
                    {
                        Ok(built) => return built,
                        Err(error) => tracing::error!(
                            target: "brisvox::engine",
                            %error,
                            "request build failed on a blocking thread; building inline"
                        ),
                    }
                }
            }
        }
        self.build(&lock(context), tools)
    }

    /// Counts a failed refresh and, once degraded, backs the next attempts off.
    fn failed_refresh(&self, clock: &dyn Clock) {
        let failures = self.stats.failed_refresh();
        if failures < SUMMARY_DEGRADED_AFTER {
            return;
        }
        let doublings = u32::try_from(failures - SUMMARY_DEGRADED_AFTER)
            .unwrap_or(u32::MAX)
            .min(16);
        let delay = self
            .summary_timeout
            .saturating_mul(1u32 << doublings)
            .min(MAX_SUMMARY_BACKOFF);
        *lock(&self.not_before) = Some(clock.now() + delay);
    }

    fn successful_refresh(&self) {
        self.stats.successful_refresh();
        *lock(&self.not_before) = None;
    }

    /// Clips the tool results of the latest turn, oldest first, to the floor until the window
    /// fits. Returns the number of messages clipped.
    fn bound_latest_turn(&self, window: &mut Window) -> usize {
        let estimator: &dyn TokenEstimator = &*self.estimator;
        let messages = window.context.messages_mut();
        let turn_start = messages
            .iter()
            .rposition(|m| matches!(m, Message::User { .. }))
            .unwrap_or(0);
        let mut total = window.estimated_tokens;
        let mut count = 0usize;
        for index in turn_start..messages.len() {
            if total <= self.policy.max_tokens {
                break;
            }
            let Some(message @ Message::Tool { .. }) = messages.get_mut(index) else {
                continue;
            };
            let before = estimator.estimate(message);
            let smaller = clipped(message, self.tool_result_floor_tokens, estimator);
            let after = estimator.estimate(&smaller);
            if after < before {
                *message = smaller.into_owned();
                total = total.saturating_sub(before.saturating_sub(after));
                count = count.saturating_add(1);
            }
        }
        window.estimated_tokens = window.context.estimate_tokens(estimator);
        window.over_budget = window.estimated_tokens > self.policy.max_tokens;
        count
    }

    /// Brings the summary of the dropped turns up to date with the summarizer, if there is one
    /// and the window drops turns it does not cover. Bounded by the summary timeout on `clock`.
    ///
    /// The context is read once under its lock (a copy is summarized, so no lock is held while
    /// the summarizer runs) and nothing is ever written to it. Failures are counted and the
    /// previous summary stays in use (see the module docs for the too-large case and the circuit
    /// breaker). Cancel-safe.
    pub async fn refresh(&self, context: &Arc<Mutex<Context>>, clock: &dyn Clock) {
        self.refresh_inner(context, clock, false).await;
    }

    /// [`Self::refresh`] for the window of the **next** request: the copy that is summarized has
    /// a placeholder user turn (the size of the last user message) at its end, so the turns the
    /// next user message will push out of the window are covered before that message arrives.
    /// The turn that has just finished is never covered (see the module docs). Meant to run
    /// after a response, off the request path.
    pub async fn refresh_ahead(&self, context: &Arc<Mutex<Context>>, clock: &dyn Clock) {
        self.refresh_inner(context, clock, true).await;
    }

    /// The snapshot step of a refresh, inline or (for a heavy context) on the offload.
    async fn snapshot(
        &self,
        context: &Arc<Mutex<Context>>,
        summary: Option<Summary>,
        ahead: bool,
    ) -> Option<Snapshot> {
        if let Some((offload, timeout)) = &self.offload
            && is_heavy(&lock(context))
        {
            let (ctx, estimator, policy) = (
                Arc::clone(context),
                Arc::clone(&self.estimator),
                self.policy,
            );
            let owned = summary.clone();
            match offload
                .run(*timeout, move || {
                    take_snapshot(&ctx, &policy, &*estimator, owned, ahead)
                })
                .await
            {
                Ok(done) => {
                    bump(&self.stats.counters.snapshots_offloaded);
                    return done.value;
                }
                Err(error) => {
                    tracing::warn!(
                        target: "brisvox::engine",
                        %error,
                        "offloaded summary snapshot failed; taking it inline"
                    );
                    bump(&self.stats.counters.offload_fallbacks);
                }
            }
        }
        take_snapshot(context, &self.policy, &*self.estimator, summary, ahead)
    }

    async fn refresh_inner(&self, context: &Arc<Mutex<Context>>, clock: &dyn Clock, ahead: bool) {
        let Some(summarizer) = &self.summarizer else {
            return;
        };
        if let Some(not_before) = *lock(&self.not_before)
            && clock.now() < not_before
        {
            bump(&self.stats.counters.summary_backoffs);
            return;
        }
        let estimator: &dyn TokenEstimator = &*self.estimator;
        let generation = self.generation.load(Ordering::SeqCst);
        let stored = lock(&self.summary).clone();
        let Some(Snapshot {
            context: snapshot,
            mut summary,
            limit,
        }) = self.snapshot(context, stored, ahead).await
        else {
            return;
        };
        let result = {
            let work = refresh_summary_until(
                &snapshot,
                &self.policy,
                estimator,
                &**summarizer,
                &mut summary,
                limit,
            );
            tokio::select! {
                biased;
                result = work => Some(result),
                () = clock.sleep(self.summary_timeout) => None,
            }
        };
        let c = &*self.stats.counters;
        {
            // The generation check and the store are one step under the summary mutex (see
            // `reset_summary`).
            let mut stored = lock(&self.summary);
            if self.generation.load(Ordering::SeqCst) != generation {
                // The history was replaced while the summarizer ran: the result describes
                // another conversation.
                bump(&c.summaries_discarded);
                return;
            }
            // Rounds that completed before a failure or the timeout stay: each is a valid
            // summary.
            let advanced =
                summary.as_ref().map(|s| s.covers_until) != stored.as_ref().map(|s| s.covers_until);
            if advanced {
                c.summary_covers_until.store(
                    summary.as_ref().map_or(0, |s| s.covers_until),
                    Ordering::Relaxed,
                );
                *stored = summary;
            }
        }
        match result {
            Some(Ok(calls)) => {
                c.summaries
                    .fetch_add(u64::try_from(calls).unwrap_or(u64::MAX), Ordering::Relaxed);
                self.successful_refresh();
            }
            Some(Err(WindowError::SummaryTooLarge {
                estimated_tokens,
                limit,
            })) => {
                bump(&c.summary_too_large);
                self.failed_refresh(clock);
                tracing::warn!(
                    target: "brisvox::engine",
                    estimated_tokens,
                    limit,
                    consecutive = self.stats.snapshot().consecutive_summary_failures,
                    "summarizer returned a summary over the limit; keeping the previous summary"
                );
            }
            Some(Err(error)) => {
                bump(&c.summary_failures);
                self.failed_refresh(clock);
                tracing::warn!(target: "brisvox::engine", %error, "summarizer failed");
            }
            None => {
                bump(&c.summary_timeouts);
                self.failed_refresh(clock);
                tracing::warn!(
                    target: "brisvox::engine",
                    timeout = ?self.summary_timeout,
                    "summarizer timed out"
                );
            }
        }
    }
}

/// What a refresh summarizes: a copy of the context, the summary it extends, and the index the
/// summary must not cover (see `refresh_summary_until`).
struct Snapshot {
    context: Context,
    summary: Option<Summary>,
    limit: usize,
}

/// Copies `context` for a refresh (under its lock; the summarizer then runs without it) and says
/// whether the window drops turns `summary` does not cover. `None`: nothing to do.
///
/// With `ahead` the copy gets a placeholder user turn (the size of the last user message), and
/// the limit is the start of the last real user turn: the turn that has just finished is not
/// final yet (see the module docs) and is never covered.
fn take_snapshot(
    context: &Mutex<Context>,
    policy: &WindowPolicy,
    estimator: &dyn TokenEstimator,
    summary: Option<Summary>,
    ahead: bool,
) -> Option<Snapshot> {
    let mut snapshot = lock(context).clone();
    let mut limit = usize::MAX;
    if ahead {
        let last_user = snapshot
            .messages()
            .iter()
            .rposition(|m| matches!(m, Message::User { .. }));
        limit = last_user.unwrap_or(0);
        if snapshot.pending_tool_calls().is_empty() {
            let next = last_user
                .and_then(|index| snapshot.messages().get(index))
                .map_or_else(|| Arc::from(""), |m| Arc::from(m.text()));
            snapshot.push(Message::User { text: next });
        }
    }
    match needs_summary(&snapshot, policy, estimator, summary.as_ref()) {
        Ok(Some(range)) if range.start < limit => Some(Snapshot {
            context: snapshot,
            summary,
            limit,
        }),
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(target: "brisvox::engine", %error, "summary refresh skipped");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use brisvox_agent::context::{ToolCall, ToolCallId};
    use brisvox_agent::window::{BoxFuture, CLIP_MARKER};
    use brisvox_core::{SimClock, TurnEpoch};
    use serde_json::json;

    use super::*;

    const T1: TurnEpoch = TurnEpoch::new(1);

    fn builder(max_tokens: usize, keep: usize) -> RequestBuilder {
        RequestBuilder::new(
            WindowPolicy::new(max_tokens, keep),
            Duration::from_secs(1),
            16,
        )
    }

    fn tokens(request: &LlmRequest) -> usize {
        let estimator = CharsPerToken;
        let context = crate::convert::from_provider_messages(&request.messages, T1).unwrap();
        context.estimate_tokens(&estimator)
    }

    fn chat(turns: usize, words: usize) -> Context {
        let mut ctx = Context::with_system("You are a voice agent.");
        for turn in 0..turns {
            ctx.push(Message::user(format!(
                "{turn} {}",
                "question ".repeat(words)
            )));
            ctx.push(Message::assistant(format!(
                "{turn} {}",
                "answer ".repeat(words)
            )));
        }
        ctx
    }

    /// A turn of `rounds` tool rounds, each with a large result.
    fn tool_heavy_turn(ctx: &mut Context, rounds: usize, bytes: usize) {
        ctx.push(Message::user("please look everything up"));
        for round in 0..rounds {
            let id = ToolCallId::new(T1, u32::try_from(round).unwrap());
            ctx.push(Message::assistant_with_calls(
                "",
                vec![ToolCall {
                    id: id.clone(),
                    provider_id: None,
                    name: Arc::from("lookup"),
                    arguments: json!({"n": round}),
                    provider_data: None,
                }],
            ));
            ctx.push(Message::tool_result(id, "x".repeat(bytes)));
        }
    }

    #[test]
    fn a_short_conversation_is_sent_whole() {
        let b = builder(4_000, 8);
        let ctx = chat(3, 5);
        let built = b.build(&ctx, &[]).unwrap();
        assert_eq!(built.request.messages.len(), ctx.len());
        assert_eq!(built.kept_turns, 3);
        assert!(!built.over_budget && built.uncovered == 0);
        assert_eq!(tokens(&built.request), built.estimated_tokens);
        assert_eq!(b.stats().snapshot().windows, 1);
    }

    #[test]
    fn a_long_conversation_is_bounded_and_the_dropped_turns_are_counted_as_uncovered() {
        let b = builder(300, 8);
        let ctx = chat(100, 20);
        let built = b.build(&ctx, &[]).unwrap();
        assert!(built.estimated_tokens <= 300, "{}", built.estimated_tokens);
        assert!(built.request.messages.len() < ctx.len() / 4);
        assert!(
            built.uncovered > 0,
            "no summary: the dropped turns are lost"
        );
        let stats = b.stats().snapshot();
        assert_eq!(stats.uncovered_requests, 1);
        assert_eq!(stats.max_estimated_tokens, built.estimated_tokens);
        // The prompt is first and the latest turn is last.
        assert_eq!(
            built.request.messages[0].role,
            brisvox_providers::Role::System
        );
        assert!(
            built
                .request
                .messages
                .last()
                .unwrap()
                .content
                .starts_with("99 ")
        );
    }

    #[test]
    fn ten_large_tool_results_in_one_turn_are_bounded() {
        let b = builder(1_000, 8);
        let mut ctx = Context::with_system("You are a voice agent.");
        tool_heavy_turn(&mut ctx, 12, 20_000);
        ctx.validate().unwrap();
        // Without the rule: 12 results clipped to a quarter of the budget each is 3x the budget.
        let window = build_window(&ctx, b.policy(), &CharsPerToken, None).unwrap();
        assert!(window.over_budget, "the unbounded window is over budget");
        assert!(window.estimated_tokens > 2_000);

        let built = b.build(&ctx, &[]).unwrap();
        assert!(!built.over_budget, "{built:?}");
        assert!(
            built.estimated_tokens <= 1_000,
            "{}",
            built.estimated_tokens
        );
        assert!(built.clipped_tool_results >= 1);
        // Pairs are intact: the request is a valid context.
        let back = crate::convert::from_provider_messages(&built.request.messages, T1).unwrap();
        back.validate().unwrap();
        // The newest results keep more text than the oldest (clipping goes oldest first).
        let results: Vec<usize> = built
            .request
            .messages
            .iter()
            .filter(|m| m.role == brisvox_providers::Role::Tool)
            .map(|m| m.content.len())
            .collect();
        assert_eq!(results.len(), 12);
        assert!(
            results.first().unwrap() <= results.last().unwrap(),
            "{results:?}"
        );
        // The full transcript is untouched.
        assert!(ctx.messages().iter().any(|m| m.text().len() == 20_000));
        let stats = b.stats().snapshot();
        assert_eq!((stats.windows, stats.over_budget), (1, 0));
        assert_eq!(
            stats.clipped_tool_results,
            built.clipped_tool_results as u64
        );
    }

    #[test]
    fn a_prompt_larger_than_the_budget_is_reported_not_cut() {
        let b = builder(100, 2);
        let mut ctx = Context::with_system("p".repeat(2_000));
        ctx.push(Message::user("hi"));
        let built = b.build(&ctx, &[]).unwrap();
        assert!(built.over_budget);
        assert_eq!(built.request.messages[0].content.len(), 2_000);
        assert_eq!(b.stats().snapshot().over_budget, 1);
    }

    #[test]
    fn tools_are_passed_through_in_order() {
        let b = builder(4_000, 8);
        let tools = vec![
            brisvox_providers::ToolSpec {
                name: Arc::from("a"),
                description: Arc::from("d"),
                parameters_schema: Arc::from("{}"),
            },
            brisvox_providers::ToolSpec {
                name: Arc::from("b"),
                description: Arc::from("d"),
                parameters_schema: Arc::from("{}"),
            },
        ];
        let built = b.build(&chat(1, 1), &tools).unwrap();
        assert_eq!(built.request.tools, tools);
    }

    #[test]
    fn an_invalid_context_is_an_error_and_counted() {
        let b = builder(4_000, 8);
        let mut ctx = Context::with_system("p");
        ctx.push(Message::user("q"));
        ctx.push(Message::tool_result(ToolCallId::new(T1, 0), "orphan"));
        assert!(matches!(
            b.build(&ctx, &[]),
            Err(WindowError::InvalidContext(_))
        ));
        assert_eq!(b.stats().snapshot().window_errors, 1);
    }

    #[test]
    fn clipping_keeps_the_pair_and_cuts_at_a_character_boundary() {
        let id = ToolCallId::new(T1, 0);
        let long = Message::tool_error(id.clone(), "çağrı sonucu ".repeat(200));
        let cut = clipped(&long, 20, &CharsPerToken).into_owned();
        let Message::Tool {
            call_id,
            content,
            is_error,
        } = &cut
        else {
            panic!("tool expected");
        };
        assert_eq!(call_id, &id);
        assert!(*is_error);
        assert!(content.ends_with(CLIP_MARKER), "{content}");
        assert!(CharsPerToken.estimate(&cut) <= 20);
        // Short results are untouched.
        let short = Message::tool_result(id, "ok");
        assert_eq!(*clipped(&short, 20, &CharsPerToken), short);
        // Not even the marker fits: empty.
        let tiny = clipped(&long, 4, &CharsPerToken).into_owned();
        assert_eq!(tiny.text(), "");
    }

    // ---- summaries ----

    /// A summarizer that answers with `text`.
    struct Fixed {
        text: String,
        calls: AtomicUsize,
    }

    impl Fixed {
        fn new(text: impl Into<String>) -> Arc<Self> {
            Arc::new(Self {
                text: text.into(),
                calls: AtomicUsize::new(0),
            })
        }
    }

    impl Summarizer for Fixed {
        fn summarize<'a>(
            &'a self,
            _previous: Option<&'a Summary>,
            _messages: &'a [Message],
            _max_tokens: usize,
        ) -> BoxFuture<'a, Result<String, WindowError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(self.text.clone()) })
        }
    }

    struct Never;

    impl Summarizer for Never {
        fn summarize<'a>(
            &'a self,
            _previous: Option<&'a Summary>,
            _messages: &'a [Message],
            _max_tokens: usize,
        ) -> BoxFuture<'a, Result<String, WindowError>> {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_stores_a_summary_that_the_next_request_carries() {
        let summarizer = Fixed::new("the caller asked about the clinic");
        let b = builder(300, 2).with_summarizer(summarizer.clone());
        let ctx = Arc::new(Mutex::new(chat(20, 15)));
        let clock = SimClock::new();
        b.refresh(&ctx, &clock).await;
        let summary = b.summary().expect("summary stored");
        assert!(summary.covers_until > 1);
        assert!(summarizer.calls.load(Ordering::SeqCst) >= 1);
        let built = b.build(&lock(&ctx), &[]).unwrap();
        assert_eq!(built.uncovered, 0, "the dropped turns are covered");
        assert!(
            built.request.messages[1]
                .content
                .contains("the caller asked about the clinic")
        );
        assert!(built.estimated_tokens <= 300);
        let stats = b.stats().snapshot();
        assert!(stats.summaries >= 1);
        assert_eq!((stats.summary_too_large, stats.summary_failures), (0, 0));
        // Nothing left to summarize: no further call.
        let calls = summarizer.calls.load(Ordering::SeqCst);
        b.refresh(&ctx, &clock).await;
        assert_eq!(summarizer.calls.load(Ordering::SeqCst), calls);
    }

    #[tokio::test(start_paused = true)]
    async fn a_summarizer_that_returns_too_large_summaries_is_detected_not_frozen_silently() {
        // 300 tokens: max_summary_tokens is 75; the answer is about 250.
        let b = builder(300, 2).with_summarizer(Fixed::new("long ".repeat(200)));
        let ctx = Arc::new(Mutex::new(chat(20, 15)));
        let clock = SimClock::new();
        for expected in 1..=3u64 {
            b.refresh(&ctx, &clock).await;
            let stats = b.stats().snapshot();
            assert_eq!(stats.summary_too_large, expected);
            assert_eq!(stats.consecutive_summary_failures, expected);
            assert_eq!(stats.summary_degraded, expected >= SUMMARY_DEGRADED_AFTER);
            assert!(b.summary().is_none(), "a rejected summary is never stored");
        }
        // Degraded: the circuit breaker skips attempts for a while (no billed call) ...
        b.refresh(&ctx, &clock).await;
        let stats = b.stats().snapshot();
        assert_eq!((stats.summary_too_large, stats.summary_backoffs), (3, 1));
        // ... and retries once the backoff (the timeout, 1 s here) has passed.
        clock.sleep(Duration::from_millis(1_100)).await;
        b.refresh(&ctx, &clock).await;
        let stats = b.stats().snapshot();
        assert_eq!(stats.summary_too_large, 4);
        // The next backoff is twice as long.
        clock.sleep(Duration::from_millis(1_100)).await;
        b.refresh(&ctx, &clock).await;
        assert_eq!(b.stats().snapshot().summary_backoffs, 2);
        // The requests still fit the budget, and say they lost context.
        let built = b.build(&lock(&ctx), &[]).unwrap();
        assert!(built.estimated_tokens <= 300);
        assert!(built.uncovered > 0);
        assert_eq!(b.stats().snapshot().uncovered_requests, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hanging_summarizer_is_bounded_by_the_clock() {
        let b = builder(300, 2).with_summarizer(Arc::new(Never));
        let ctx = Arc::new(Mutex::new(chat(20, 15)));
        let clock = SimClock::new();
        let started = clock.now();
        b.refresh(&ctx, &clock).await;
        assert_eq!(clock.now() - started, Duration::from_secs(1));
        let stats = b.stats().snapshot();
        assert_eq!(stats.summary_timeouts, 1);
        assert_eq!(stats.consecutive_summary_failures, 1);
        assert!(b.summary().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_misaligned_summary_is_dropped_when_the_history_was_replaced() {
        let b = builder(300, 2).with_summarizer(Fixed::new("short summary"));
        let ctx = Arc::new(Mutex::new(chat(20, 15)));
        let clock = SimClock::new();
        b.refresh(&ctx, &clock).await;
        assert!(b.summary().is_some());
        // A flow Reset replaces the history: the stored index means nothing any more.
        let replaced = chat(1, 2);
        let built = b.build(&replaced, &[]).unwrap();
        assert_eq!(built.request.messages.len(), replaced.len());
        assert!(b.summary().is_none());
        assert_eq!(b.stats().snapshot().invalid_summaries, 1);
        b.reset_summary();
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_without_a_summarizer_or_a_need_does_nothing() {
        let clock = SimClock::new();
        let none = builder(300, 2);
        none.refresh(&Arc::new(Mutex::new(chat(20, 15))), &clock)
            .await;
        assert!(none.summary().is_none());
        let summarizer = Fixed::new("s");
        let b = builder(4_000, 8).with_summarizer(summarizer.clone());
        b.refresh(&Arc::new(Mutex::new(chat(2, 2))), &clock).await;
        assert_eq!(summarizer.calls.load(Ordering::SeqCst), 0);
    }

    // ---- refreshing ahead, resets, the estimator ----

    #[tokio::test(start_paused = true)]
    async fn refreshing_ahead_covers_what_the_next_user_message_pushes_out() {
        let clock = SimClock::new();
        let summarizer = Fixed::new("earlier turns");
        // A saturated window: 20 turns, only the last 2 kept.
        let now_only = builder(300, 2).with_summarizer(summarizer.clone());
        let ahead = builder(300, 2).with_summarizer(summarizer);
        let ctx = Arc::new(Mutex::new(chat(20, 15)));
        now_only.refresh(&ctx, &clock).await;
        ahead.refresh_ahead(&ctx, &clock).await;
        // The next user message arrives.
        lock(&ctx).push(Message::user(format!("20 {}", "question ".repeat(15))));
        let late = now_only.build(&lock(&ctx), &[]).unwrap();
        assert!(
            late.uncovered > 0,
            "the turn the message pushed out is lost"
        );
        let early = ahead.build(&lock(&ctx), &[]).unwrap();
        assert_eq!(early.uncovered, 0, "it was covered before the message came");
        assert!(early.estimated_tokens <= 300);
        // And the real context was not touched by the placeholder.
        assert_eq!(lock(&ctx).len(), 1 + 2 * 20 + 1);
    }

    struct Sleepy(SimClock);

    impl Summarizer for Sleepy {
        fn summarize<'a>(
            &'a self,
            _previous: Option<&'a Summary>,
            _messages: &'a [Message],
            _max_tokens: usize,
        ) -> BoxFuture<'a, Result<String, WindowError>> {
            Box::pin(async move {
                self.0.sleep(Duration::from_millis(100)).await;
                Ok("a summary".to_owned())
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_summary_made_for_a_replaced_history_is_discarded() {
        let clock = SimClock::new();
        let b = builder(300, 2).with_summarizer(Arc::new(Sleepy(clock)));
        let ctx = Arc::new(Mutex::new(chat(20, 15)));
        let reset = async {
            clock.sleep(Duration::from_millis(10)).await;
            *lock(&ctx) = chat(1, 2);
            b.reset_summary();
        };
        tokio::join!(b.refresh(&ctx, &clock), reset);
        assert!(
            b.summary().is_none(),
            "the old conversation's summary is not stored"
        );
        let stats = b.stats().snapshot();
        assert_eq!((stats.summaries_discarded, stats.summaries), (1, 0));
        assert_eq!(stats.consecutive_summary_failures, 0, "not a failure");
    }

    // ---- the ahead refresh, recovery of the circuit breaker, offload ----

    /// Records the text of every message it is asked to summarize.
    struct Recording {
        seen: Mutex<Vec<String>>,
    }

    impl Summarizer for Recording {
        fn summarize<'a>(
            &'a self,
            _previous: Option<&'a Summary>,
            messages: &'a [Message],
            _max_tokens: usize,
        ) -> BoxFuture<'a, Result<String, WindowError>> {
            lock(&self.seen).extend(messages.iter().map(|m| m.text().to_owned()));
            Box::pin(async { Ok("earlier turns".to_owned()) })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_ahead_refresh_never_summarizes_the_turn_that_just_finished() {
        let clock = SimClock::new();
        let recording = Arc::new(Recording {
            seen: Mutex::new(Vec::new()),
        });
        let b = builder(300, 2).with_summarizer(recording.clone());
        // Older turns, then one tool-heavy turn (6 results, more than the whole budget) and its
        // final answer: the window of the next request keeps nothing of it.
        let mut ctx = chat(10, 15);
        let last_user = ctx.len();
        tool_heavy_turn(&mut ctx, 6, 3_000);
        ctx.push(Message::assistant("Here is everything I found."));
        let ctx = Arc::new(Mutex::new(ctx));
        b.refresh_ahead(&ctx, &clock).await;

        let summary = b.summary().expect("the older turns are summarized");
        assert_eq!(
            summary.covers_until, last_user,
            "the summary ends where the just-finished turn starts"
        );
        assert!(
            lock(&recording.seen)
                .iter()
                .all(|text| !text.contains("Here is everything")),
            "the unplayed final answer was summarized"
        );

        // The caller barged in before hearing any of the answer: the answer goes away, a late
        // note lands in the finished turn, and the next user message follows.
        {
            let mut context = lock(&ctx);
            context.messages_mut().pop();
            context.push(Message::system("Late result of a tool call"));
            context.push(Message::user("never mind"));
        }
        let built = b.build(&lock(&ctx), &[]).unwrap();
        let stats = b.stats().snapshot();
        assert_eq!(
            stats.invalid_summaries, 0,
            "the summary is still aligned: {stats:?}"
        );
        assert!(
            b.summary().is_some(),
            "the summary of the older turns survived the change to the last turn"
        );
        assert!(
            built
                .request
                .messages
                .iter()
                .any(|m| m.content.contains("earlier turns")),
            "the request carries the summary"
        );
    }

    /// Answers by mode: a short summary, one over the limit, or a failure.
    struct Switch {
        mode: std::sync::atomic::AtomicU8,
    }

    impl Switch {
        const OK: u8 = 0;
        const TOO_LARGE: u8 = 1;
        const FAIL: u8 = 2;

        fn set(&self, mode: u8) {
            self.mode.store(mode, Ordering::SeqCst);
        }
    }

    impl Summarizer for Switch {
        fn summarize<'a>(
            &'a self,
            _previous: Option<&'a Summary>,
            messages: &'a [Message],
            _max_tokens: usize,
        ) -> BoxFuture<'a, Result<String, WindowError>> {
            let mode = self.mode.load(Ordering::SeqCst);
            Box::pin(async move {
                match mode {
                    Self::OK => Ok(format!("summary of {} messages", messages.len())),
                    Self::TOO_LARGE => Ok("long ".repeat(200)),
                    _ => Err(WindowError::Summarizer {
                        reason: "down".into(),
                    }),
                }
            })
        }
    }

    /// Collects the events of the tracing macros on this thread.
    #[derive(Clone, Default)]
    struct Capture {
        events: Arc<Mutex<Vec<(tracing::Level, String, String)>>>,
    }

    struct MessageField(String);

    impl tracing::field::Visit for MessageField {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
    }

    impl tracing::Subscriber for Capture {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _attributes: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut message = MessageField(String::new());
            event.record(&mut message);
            lock(&self.events).push((
                *event.metadata().level(),
                event.metadata().target().to_owned(),
                message.0,
            ));
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    impl Capture {
        fn warnings(&self) -> Vec<String> {
            lock(&self.events)
                .iter()
                .filter(|(level, target, _)| {
                    *level == tracing::Level::WARN && target == "brisvox::engine"
                })
                .map(|(_, _, message)| message.clone())
                .collect()
        }
    }

    fn push_turn(ctx: &Arc<Mutex<Context>>, tag: &str) {
        let mut ctx = lock(ctx);
        ctx.push(Message::user(format!("{tag} {}", "question ".repeat(15))));
        ctx.push(Message::assistant(format!(
            "{tag} {}",
            "answer ".repeat(15)
        )));
    }

    #[tokio::test(start_paused = true)]
    async fn the_circuit_breaker_recovers_after_a_success_and_keeps_the_previous_summary_meanwhile()
    {
        let capture = Capture::default();
        let _guard = tracing::subscriber::set_default(capture.clone());
        let clock = SimClock::new();
        let switch = Arc::new(Switch {
            mode: std::sync::atomic::AtomicU8::new(Switch::OK),
        });
        let b = builder(300, 2).with_summarizer(switch.clone());
        let ctx = Arc::new(Mutex::new(chat(20, 15)));
        b.refresh(&ctx, &clock).await;
        let good = b.summary().expect("a first summary");
        assert!(capture.warnings().is_empty());

        // The window moves on, and the summarizer starts returning summaries over the limit.
        push_turn(&ctx, "21");
        push_turn(&ctx, "22");
        switch.set(Switch::TOO_LARGE);
        for expected in 1..=3u64 {
            b.refresh(&ctx, &clock).await;
            let stats = b.stats().snapshot();
            assert_eq!(stats.consecutive_summary_failures, expected);
            assert_eq!(
                b.summary().as_ref(),
                Some(&good),
                "a rejected summary never replaces the stored one"
            );
        }
        let stats = b.stats().snapshot();
        assert!(
            stats.summary_degraded && stats.summary_too_large == 3,
            "{stats:?}"
        );
        assert!(lock(&b.not_before).is_some(), "the breaker is open");
        let warnings = capture.warnings();
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(
            warnings.iter().all(|w| w.contains("over the limit")),
            "{warnings:?}"
        );

        // The summarizer recovers; after the backoff the next attempt succeeds and closes the
        // breaker.
        switch.set(Switch::OK);
        clock.sleep(Duration::from_millis(1_100)).await;
        b.refresh(&ctx, &clock).await;
        let stats = b.stats().snapshot();
        assert_eq!(stats.consecutive_summary_failures, 0, "{stats:?}");
        assert!(!stats.summary_degraded, "{stats:?}");
        assert!(lock(&b.not_before).is_none(), "the breaker is closed");
        let advanced = b.summary().unwrap();
        assert!(advanced.covers_until > good.covers_until);
        // And the next failure starts counting from zero again.
        push_turn(&ctx, "23");
        push_turn(&ctx, "24");
        switch.set(Switch::FAIL);
        b.refresh(&ctx, &clock).await;
        let stats = b.stats().snapshot();
        assert_eq!(
            (stats.consecutive_summary_failures, stats.summary_degraded),
            (1, false)
        );
        assert_eq!(b.summary().as_ref(), Some(&advanced));
        assert!(
            capture
                .warnings()
                .last()
                .is_some_and(|w| w.contains("summarizer failed")),
            "{:?}",
            capture.warnings()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeout_is_logged_and_the_backoff_is_capped() {
        let capture = Capture::default();
        let _guard = tracing::subscriber::set_default(capture.clone());
        let clock = SimClock::new();
        let b = builder(300, 2).with_summarizer(Arc::new(Never));
        let ctx = Arc::new(Mutex::new(chat(20, 15)));
        // Twelve timeouts in a row, each attempt made after the previous backoff: the delay
        // doubles from 1 s and stops at MAX_SUMMARY_BACKOFF (60 s).
        for _ in 0..12 {
            clock
                .sleep(MAX_SUMMARY_BACKOFF + Duration::from_secs(1))
                .await;
            b.refresh(&ctx, &clock).await;
        }
        let stats = b.stats().snapshot();
        assert_eq!((stats.summary_timeouts, stats.summary_backoffs), (12, 0));
        assert!(
            capture.warnings().iter().all(|w| w.contains("timed out")),
            "{:?}",
            capture.warnings()
        );
        // Uncapped, the delay after the 12th failure would be 512 s. Capped: 60 s.
        clock.sleep(Duration::from_secs(59)).await;
        b.refresh(&ctx, &clock).await;
        let stats = b.stats().snapshot();
        assert_eq!(
            (stats.summary_timeouts, stats.summary_backoffs),
            (12, 1),
            "still backing off 59 s after the failure"
        );
        clock.sleep(Duration::from_secs(2)).await;
        b.refresh(&ctx, &clock).await;
        assert_eq!(
            b.stats().snapshot().summary_timeouts,
            13,
            "the attempt after 61 s is made: the backoff is capped at 60 s"
        );
    }

    fn heavy_context() -> Context {
        let mut ctx = Context::with_system("You are a voice agent.");
        tool_heavy_turn(&mut ctx, 2, 20_000);
        ctx
    }

    #[test]
    fn a_context_is_heavy_by_the_latest_turn_or_by_its_length() {
        assert!(!is_heavy(&chat(10, 15)));
        assert!(is_heavy(&heavy_context()));
        // Older large results do not make a small latest turn heavy.
        let mut ctx = heavy_context();
        ctx.push(Message::assistant("done"));
        ctx.push(Message::user("thanks"));
        assert!(!is_heavy(&ctx));
        assert!(is_heavy(&chat(INLINE_MAX_MESSAGES, 1)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_heavy_build_goes_to_the_offload_and_a_refused_job_falls_back_inline() {
        let clock: brisvox_core::SharedClock = Arc::new(SimClock::new());
        let ctx = Arc::new(Mutex::new(heavy_context()));
        let b = Arc::new(
            builder(1_000, 4).with_offload(Offload::Inline(clock), Duration::from_secs(1)),
        );
        let built = b.build_offloaded(&ctx, &[]).await.unwrap();
        let stats = b.stats().snapshot();
        assert_eq!((stats.builds_offloaded, stats.offload_fallbacks), (1, 0));
        assert!(!built.over_budget);
        // A small context is built inline.
        let small = Arc::new(Mutex::new(chat(3, 5)));
        b.build_offloaded(&small, &[]).await.unwrap();
        assert_eq!(b.stats().snapshot().builds_offloaded, 1);

        // A pool that is shut down refuses every job: the request is built inline anyway.
        let pool = brisvox_inference::InferencePool::new(
            &brisvox_inference::PoolConfig {
                threads: 1,
                queue_capacity: 2,
                name: "request-refused".to_owned(),
            },
            brisvox_core::SystemClock::shared(),
        )
        .unwrap();
        let handle = pool.handle();
        tokio::task::spawn_blocking(move || pool.shutdown())
            .await
            .unwrap();
        let refused =
            Arc::new(builder(1_000, 4).with_offload(Offload::Pool(handle), Duration::from_secs(1)));
        let again = refused.build_offloaded(&ctx, &[]).await.unwrap();
        let stats = refused.stats().snapshot();
        assert_eq!((stats.builds_offloaded, stats.offload_fallbacks), (0, 1));
        assert_eq!(again.request.messages, built.request.messages);
    }

    #[tokio::test(start_paused = true)]
    async fn a_heavy_summary_snapshot_goes_to_the_offload() {
        let clock = SimClock::new();
        let shared: brisvox_core::SharedClock = Arc::new(clock);
        let summarizer = Fixed::new("earlier turns");
        let b = builder(300, 2)
            .with_summarizer(summarizer.clone())
            .with_offload(Offload::Inline(shared), Duration::from_secs(1));
        // 400 messages: past INLINE_MAX_MESSAGES.
        let ctx = Arc::new(Mutex::new(chat(INLINE_MAX_MESSAGES / 2 + 10, 15)));
        b.refresh(&ctx, &clock).await;
        assert!(b.summary().is_some());
        assert_eq!(b.stats().snapshot().snapshots_offloaded, 1);
        // A short context is snapshotted inline.
        let short = Arc::new(Mutex::new(chat(20, 15)));
        let other = builder(300, 2)
            .with_summarizer(summarizer)
            .with_offload(Offload::Inline(Arc::new(clock)), Duration::from_secs(1));
        other.refresh(&short, &clock).await;
        assert_eq!(other.stats().snapshot().snapshots_offloaded, 0);
    }

    #[test]
    fn provider_data_counts_against_the_budget() {
        let plain = Message::assistant("hello");
        let with_data = Message::Assistant {
            text: Arc::from("hello"),
            tool_calls: Vec::new(),
            provider_data: Some(json!({"signature": "s".repeat(4_000)})),
        };
        let plain_cost = WindowEstimator.estimate(&plain);
        assert_eq!(plain_cost, CharsPerToken.estimate(&plain));
        let cost = WindowEstimator.estimate(&with_data);
        assert!(cost >= plain_cost + 1_000, "{cost} vs {plain_cost}");
        // Other roles are unchanged.
        let user = Message::user("hi");
        assert_eq!(
            WindowEstimator.estimate(&user),
            CharsPerToken.estimate(&user)
        );
    }
}
