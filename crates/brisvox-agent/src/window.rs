//! Long-call context windowing.
//!
//! A call can run for an hour. The [`Context`] keeps the whole transcript, but the LLM input must
//! stay bounded. Before each request, [`build_window`] derives a new, smaller [`Context`] that
//! fits a [`WindowPolicy`]; the full transcript is never modified:
//!
//! - Leading system messages (the prompt) are always kept, first, and never clipped.
//! - A **turn** is a user message plus the messages after it, up to the next user message
//!   (assistant text, tool calls, their results, late notes). Turns are the unit of dropping, so
//!   a tool call is never separated from its results. Messages before the first user message
//!   form a turn of their own.
//! - The most recent turns are kept verbatim: up to [`WindowPolicy::keep_recent_turns`] of them,
//!   fewer when the budget needs it, never fewer than one (the latest turn). Older turns are
//!   dropped oldest first.
//! - A [`Summary`] (produced asynchronously, usually by an LLM; see [`Summarizer`]) covers the
//!   dropped turns, from the end of the leading system messages up to
//!   [`Summary::covers_until`]. The window inserts it as a system message after the prompt, and
//!   it counts against the budget.
//! - [`needs_summary`] says which message range still has to be summarized so that the summary
//!   covers exactly what the window drops. [`refresh_summary`] runs that loop, in rounds of at
//!   most [`WindowPolicy::max_summary_input_tokens`] each.
//! - Clipping: a message whose estimate exceeds [`WindowPolicy::max_message_tokens`] is sent
//!   clipped (its text is cut at a character boundary and [`CLIP_MARKER`] is appended). The full
//!   transcript keeps the whole text, and the summarizer sees the same clipped messages. Tool call
//!   arguments are not clipped.
//!
//! Summary size: [`refresh_summary`] never stores a summary larger than
//! [`WindowPolicy::max_summary_tokens`]; it returns [`WindowError::SummaryTooLarge`] and keeps the
//! previous summary. [`build_window`] clips a larger summary that the caller passes in.
//!
//! Budget: the window is at most [`WindowPolicy::max_tokens`] estimated tokens, unless a piece
//! that cannot be dropped is larger than the budget on its own: the prompt, or the latest turn
//! after clipping. That piece is still sent, and [`Window::over_budget`] reports it.

use std::borrow::Cow;
use std::future::Future;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;

use crate::context::{Context, ContextError, Message, TokenEstimator};

/// Start of the text of the system message that carries a [`Summary`].
pub const SUMMARY_PREFIX: &str =
    "Summary of the earlier part of this call (older turns were dropped from the context):\n";

/// Appended to text that was clipped to fit a limit (see [`WindowPolicy::max_message_tokens`]).
pub const CLIP_MARKER: &str = " [clipped]";

/// Limits of the window sent to the LLM. All fields are public so the policy can be configured
/// per session; [`WindowPolicy::new`] derives the other limits from the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowPolicy {
    /// Budget, in estimated tokens (see [`TokenEstimator`]), for the whole window: prompt,
    /// summary and kept turns.
    pub max_tokens: usize,
    /// How many of the most recent turns to keep verbatim when the budget allows. Fewer are kept
    /// when the budget requires it (shrinking down to one). `0` behaves as `1`.
    pub keep_recent_turns: usize,
    /// Largest summary message the window accepts, in estimated tokens (the prefix included).
    /// [`refresh_summary`] rejects a longer summary; [`build_window`] clips a longer one. It is
    /// also the size asked of the [`Summarizer`]. [`WindowPolicy::new`] sets a quarter of
    /// `max_tokens`.
    pub max_summary_tokens: usize,
    /// Most estimated tokens that one [`Summarizer`] call receives. A longer backlog of dropped
    /// turns is summarized in several rounds. A turn is never split, so a single turn larger than
    /// this is sent alone. [`WindowPolicy::new`] sets `max_tokens`.
    pub max_summary_input_tokens: usize,
    /// Largest estimated size of one message in the window and in summarizer input. Longer text
    /// is clipped (see [`CLIP_MARKER`]); the prompt is never clipped. [`WindowPolicy::new`] sets a
    /// quarter of `max_tokens`.
    pub max_message_tokens: usize,
}

impl WindowPolicy {
    /// A policy with the given budget and number of recent turns. The summary and message limits
    /// are derived from the budget (see the field docs); set the fields to change them.
    pub const fn new(max_tokens: usize, keep_recent_turns: usize) -> Self {
        Self {
            max_tokens,
            keep_recent_turns,
            max_summary_tokens: max_tokens / 4,
            max_summary_input_tokens: max_tokens,
            max_message_tokens: max_tokens / 4,
        }
    }
}

impl Default for WindowPolicy {
    /// 4000 tokens, 8 recent turns.
    fn default() -> Self {
        Self::new(4000, 8)
    }
}

/// A summary of the oldest turns of a conversation, produced by a [`Summarizer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// The summary text. [`build_window`] adds [`SUMMARY_PREFIX`] in front of it.
    pub text: Arc<str>,
    /// Index in the full context one past the last message the summary covers. The summary
    /// covers the messages from the end of the leading system messages up to this index. The
    /// index must be the start of a turn (see [`WindowError::InvalidSummary`]); it is not a
    /// message position inside a turn.
    pub covers_until: usize,
}

/// Why a window could not be built or a summary could not be produced.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WindowError {
    /// The full context breaks the ordering rules of [`Context::validate`].
    #[error(transparent)]
    InvalidContext(#[from] ContextError),
    /// The summary does not end at the start of a turn, or it lies beyond the context.
    #[error("summary covers up to message {covers_until}, which is not the start of a turn")]
    InvalidSummary {
        /// The rejected [`Summary::covers_until`].
        covers_until: usize,
    },
    /// The summarizer failed; the window keeps working without a new summary.
    #[error("summarizer failed: {reason}")]
    Summarizer {
        /// Why it failed (for logs; never user text).
        reason: String,
    },
    /// The summarizer returned a summary whose message estimates to more than
    /// [`WindowPolicy::max_summary_tokens`]. The previous summary is kept.
    #[error("summary of {estimated_tokens} estimated tokens exceeds the limit of {limit}")]
    SummaryTooLarge {
        /// Estimated tokens of the rejected summary message.
        estimated_tokens: usize,
        /// The limit it exceeded ([`WindowPolicy::max_summary_tokens`]).
        limit: usize,
    },
}

/// A boxed future, as returned by [`Summarizer::summarize`].
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Produces summaries asynchronously, usually with an LLM call. Implementations await the
/// client without blocking tokio workers and bound the call with a timeout.
pub trait Summarizer: Send + Sync {
    /// Summarizes `messages`, the next range of the conversation in order, extending `previous`
    /// when there is one. The returned text must cover the messages `previous` covers plus
    /// `messages`. `messages` are already clipped as the window would send them.
    ///
    /// `max_tokens` is the size limit for the result: the summary message (the text plus
    /// [`SUMMARY_PREFIX`]) must estimate to at most this many tokens, or [`refresh_summary`]
    /// rejects it. A summarizer should compress to fit it.
    fn summarize<'a>(
        &'a self,
        previous: Option<&'a Summary>,
        messages: &'a [Message],
        max_tokens: usize,
    ) -> BoxFuture<'a, Result<String, WindowError>>;
}

/// The window built for one LLM request.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// The messages to send. Always a valid context ([`Context::validate`]).
    pub context: Context,
    /// Estimated tokens of [`Self::context`].
    pub estimated_tokens: usize,
    /// Number of whole turns kept verbatim.
    pub kept_turns: usize,
    /// Index in the full context of the first kept message. Messages from the end of the leading
    /// system messages up to this index were dropped.
    pub kept_from: usize,
    /// Dropped messages that the summary does not cover yet. Empty when the summary is up to
    /// date. Non-empty only if the caller skipped [`needs_summary`]; those messages are then
    /// lost to the LLM.
    pub uncovered: Range<usize>,
    /// Whether [`Self::estimated_tokens`] exceeds the budget. This happens only when a piece that
    /// cannot be dropped is larger than the budget by itself (the prompt, or the latest turn after
    /// clipping).
    pub over_budget: bool,
}

/// Where the window cuts the full context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cut {
    /// Number of leading system messages.
    header: usize,
    /// Index one past the last message the summary covers (`header` without a summary).
    covered_end: usize,
    /// Index of the first kept message (`context.len()` when nothing is kept).
    kept_from: usize,
    /// Number of kept turns.
    kept_turns: usize,
}

/// Number of leading system messages.
fn header_len(messages: &[Message]) -> usize {
    messages
        .iter()
        .take_while(|m| matches!(m, Message::System { .. }))
        .count()
}

/// Turn starts: the first message after the header, and every user message after it.
fn turn_starts(messages: &[Message], header: usize) -> Vec<usize> {
    (header..messages.len())
        .filter(|&i| i == header || matches!(messages.get(i), Some(Message::User { .. })))
        .collect()
}

/// `message` with its text replaced by `text`, keeping its role, tool calls and ids.
fn with_text(message: &Message, text: &str) -> Message {
    match message {
        Message::System { .. } => Message::system(text),
        Message::User { .. } => Message::user(text),
        Message::Assistant {
            tool_calls,
            provider_data,
            ..
        } => Message::Assistant {
            text: Arc::from(text),
            tool_calls: tool_calls.clone(),
            provider_data: provider_data.clone(),
        },
        Message::Tool {
            call_id, is_error, ..
        } => Message::Tool {
            call_id: call_id.clone(),
            content: Arc::from(text),
            is_error: *is_error,
        },
    }
}

/// The largest index at most `len` that is a character boundary of `text` (never past the end).
fn boundary_at_most(text: &str, len: usize) -> usize {
    let mut len = len.min(text.len());
    while !text.is_char_boundary(len) {
        len = len.saturating_sub(1);
    }
    len
}

/// `message` as the window sends it: unchanged when its estimate is at most `limit` tokens.
/// Otherwise its text is cut at a character boundary, the longest prefix whose clipped message
/// (with [`CLIP_MARKER`] appended) fits. When not even the marker fits, the text is dropped, so
/// the result can still exceed `limit` (for example when tool call arguments are large).
///
/// The search gallops up from a small prefix and then bisects, so its cost grows with the size of
/// the **result** (about `limit`), not with the size of the text: clipping a 64 KiB tool result
/// to a quarter of a 4000-token window touches a few KiB per step, not 64 KiB. This runs for
/// every over-long message of every request, which is why it must not copy the whole text per
/// step.
pub fn clipped<'a>(
    message: &'a Message,
    limit: usize,
    estimator: &dyn TokenEstimator,
) -> Cow<'a, Message> {
    if estimator.estimate(message) <= limit {
        return Cow::Borrowed(message);
    }
    let text = message.text();
    let fits = |len: usize| -> bool {
        let cut = boundary_at_most(text, len);
        let kept = text.get(..cut).unwrap_or_default();
        let candidate = with_text(message, &format!("{kept}{CLIP_MARKER}"));
        estimator.estimate(&candidate) <= limit
    };
    // Estimates grow with the text, so the longest fitting prefix is found by search over byte
    // lengths. `lo` is the largest length known to fit (0 is the fallback), `hi` the largest
    // length that may still fit.
    let mut lo = 0usize;
    let mut hi = text.len();
    let mut probe = CLIP_GALLOP_START.min(text.len());
    while probe > 0 {
        if fits(probe) {
            lo = probe;
            if probe >= text.len() {
                break;
            }
            probe = probe.saturating_mul(2).min(text.len());
        } else {
            hi = probe.saturating_sub(1);
            break;
        }
    }
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid.saturating_sub(1);
        }
    }
    if lo == 0 {
        return Cow::Owned(with_text(message, ""));
    }
    let cut = boundary_at_most(text, lo);
    let kept = text.get(..cut).unwrap_or_default();
    Cow::Owned(with_text(message, &format!("{kept}{CLIP_MARKER}")))
}

/// First prefix length (bytes) [`clipped`] tries; each further try doubles it.
const CLIP_GALLOP_START: usize = 256;

/// Estimated tokens of `message` as the window sends it (clipped to the message limit).
fn sent_tokens(message: &Message, policy: &WindowPolicy, estimator: &dyn TokenEstimator) -> usize {
    estimator.estimate(&clipped(message, policy.max_message_tokens, estimator))
}

/// The system message that carries `summary`, with the prefix, before any clipping.
fn summary_text_message(summary: &Summary) -> Message {
    Message::system(format!("{SUMMARY_PREFIX}{}", summary.text))
}

/// The system message that carries `summary`, as the window sends it (clipped to the summary
/// limit).
fn summary_message(
    summary: &Summary,
    policy: &WindowPolicy,
    estimator: &dyn TokenEstimator,
) -> Message {
    clipped(
        &summary_text_message(summary),
        policy.max_summary_tokens,
        estimator,
    )
    .into_owned()
}

/// Computes where the window cuts `ctx`.
fn cut(
    ctx: &Context,
    policy: &WindowPolicy,
    estimator: &dyn TokenEstimator,
    summary: Option<&Summary>,
) -> Result<Cut, WindowError> {
    ctx.validate()?;
    let messages = ctx.messages();
    let len = messages.len();
    let header = header_len(messages);
    let starts = turn_starts(messages, header);
    let covered_end = match summary {
        None => header,
        Some(summary) => {
            let aligned = if starts.is_empty() {
                summary.covers_until == header
            } else {
                starts.contains(&summary.covers_until)
            };
            if !aligned {
                return Err(WindowError::InvalidSummary {
                    covers_until: summary.covers_until,
                });
            }
            summary.covers_until
        }
    };
    let header_tokens: usize = messages
        .iter()
        .take(header)
        .map(|m| estimator.estimate(m))
        .sum();
    let summary_tokens = summary.map_or(0, |s| {
        estimator.estimate(&summary_message(s, policy, estimator))
    });
    let budget = policy
        .max_tokens
        .saturating_sub(header_tokens.saturating_add(summary_tokens));

    // Turns not covered by the summary, oldest first (`first_eligible` indexes `starts`).
    let first_eligible = starts.partition_point(|&start| start < covered_end);
    let eligible = starts.len().saturating_sub(first_eligible);

    // Newest turns that fit the budget, counted from the end.
    let mut used = 0usize;
    let mut fit = 0usize;
    for turn in (first_eligible..starts.len()).rev() {
        let start = starts.get(turn).copied().unwrap_or(len);
        let end = starts.get(turn.saturating_add(1)).copied().unwrap_or(len);
        let cost: usize = messages
            .get(start..end)
            .unwrap_or_default()
            .iter()
            .map(|m| sent_tokens(m, policy, estimator))
            .sum();
        let total = used.saturating_add(cost);
        if total > budget {
            break;
        }
        used = total;
        fit = fit.saturating_add(1);
    }

    // Keep as many recent turns as the policy wants and the budget allows, but never fewer than
    // one: when even that does not fit, the latest turn is kept anyway and reported as over
    // budget.
    let kept_turns = if eligible == 0 {
        0
    } else {
        policy
            .keep_recent_turns
            .max(1)
            .min(fit)
            .max(1)
            .min(eligible)
    };
    let kept_from = starts
        .len()
        .checked_sub(kept_turns)
        .and_then(|turn| starts.get(turn))
        .copied()
        .unwrap_or(len);
    Ok(Cut {
        header,
        covered_end,
        kept_from,
        kept_turns,
    })
}

/// Builds the messages to send for one request from the full context, without modifying it.
///
/// `summary` covers the oldest turns (see [`Summary`]); pass the summary that
/// [`refresh_summary`] maintains, or `None` before any turn was summarized. A summary longer than
/// [`WindowPolicy::max_summary_tokens`] is clipped in the window.
///
/// # Errors
/// [`WindowError::InvalidContext`] if `ctx` breaks [`Context::validate`];
/// [`WindowError::InvalidSummary`] if `summary` does not end at a turn start.
pub fn build_window(
    ctx: &Context,
    policy: &WindowPolicy,
    estimator: &dyn TokenEstimator,
    summary: Option<&Summary>,
) -> Result<Window, WindowError> {
    let cut = cut(ctx, policy, estimator, summary)?;
    let messages = ctx.messages();
    let mut context = Context::new();
    for message in messages.iter().take(cut.header) {
        context.push(message.clone());
    }
    if let Some(summary) = summary {
        context.push(summary_message(summary, policy, estimator));
    }
    for message in messages.get(cut.kept_from..).unwrap_or_default() {
        context.push(clipped(message, policy.max_message_tokens, estimator).into_owned());
    }
    context.validate()?;
    let estimated_tokens = context.estimate_tokens(estimator);
    Ok(Window {
        context,
        estimated_tokens,
        kept_turns: cut.kept_turns,
        kept_from: cut.kept_from,
        uncovered: cut.covered_end.min(cut.kept_from)..cut.kept_from,
        over_budget: estimated_tokens > policy.max_tokens,
    })
}

/// End of the summarizer round that starts at `from`: whole turns from `from` up to `to` (both
/// turn starts, `from < to`) are taken while their sent size fits
/// [`WindowPolicy::max_summary_input_tokens`]. The first turn is always taken, so every round
/// advances `covers_until`.
fn round_end(
    messages: &[Message],
    starts: &[usize],
    from: usize,
    to: usize,
    policy: &WindowPolicy,
    estimator: &dyn TokenEstimator,
) -> usize {
    let mut end = from;
    let mut used = 0usize;
    for (turn, &start) in starts.iter().enumerate() {
        if start < from {
            continue;
        }
        if start >= to {
            break;
        }
        let next = starts
            .get(turn.saturating_add(1))
            .copied()
            .unwrap_or(messages.len());
        let cost: usize = messages
            .get(start..next)
            .unwrap_or_default()
            .iter()
            .map(|m| sent_tokens(m, policy, estimator))
            .sum();
        if end > from && used.saturating_add(cost) > policy.max_summary_input_tokens {
            break;
        }
        used = used.saturating_add(cost);
        end = next;
    }
    end
}

/// The message range that must be summarized next, or `None` when the summary already covers
/// everything the window drops.
///
/// The range starts where the summary ends (or after the leading system messages) and ends where
/// the window starts, but at most one summarizer round
/// ([`WindowPolicy::max_summary_input_tokens`]) past its start. `summary` is the summary the
/// caller currently holds: the range depends on it, because a summary takes budget and moves the
/// window's start. After summarizing the range, call again; [`refresh_summary`] loops.
///
/// # Errors
/// The same as [`build_window`].
pub fn needs_summary(
    ctx: &Context,
    policy: &WindowPolicy,
    estimator: &dyn TokenEstimator,
    summary: Option<&Summary>,
) -> Result<Option<Range<usize>>, WindowError> {
    let cut = cut(ctx, policy, estimator, summary)?;
    if cut.covered_end >= cut.kept_from {
        return Ok(None);
    }
    let messages = ctx.messages();
    let starts = turn_starts(messages, cut.header);
    let end = round_end(
        messages,
        &starts,
        cut.covered_end,
        cut.kept_from,
        policy,
        estimator,
    );
    Ok(Some(cut.covered_end..end))
}

/// Brings `summary` up to date with the window: while [`needs_summary`] asks for a range, the
/// summarizer summarizes it (extending the current summary) and the result replaces `summary`.
/// Returns the number of summarizer calls.
///
/// Each round moves `covers_until` forward by at least one turn, so the loop ends.
///
/// # Errors
/// A summarizer failure, or a summary larger than [`WindowPolicy::max_summary_tokens`]
/// ([`WindowError::SummaryTooLarge`]), leaves `summary` unchanged for that round and is returned.
/// The [`build_window`] result remains valid without a new summary (bounded by the limits), but its
/// [`Window::uncovered`] range is non-empty. Calling again retries the same round.
pub async fn refresh_summary(
    ctx: &Context,
    policy: &WindowPolicy,
    estimator: &dyn TokenEstimator,
    summarizer: &dyn Summarizer,
    summary: &mut Option<Summary>,
) -> Result<usize, WindowError> {
    refresh_summary_until(ctx, policy, estimator, summarizer, summary, usize::MAX).await
}

/// [`refresh_summary`] that never lets the summary cover a message at index `limit` or later:
/// `covers_until` stays at most the last turn start that is at most `limit`, and a round that
/// [`needs_summary`] asks for beyond that is cut short (or skipped when nothing before `limit`
/// is left to summarize). The turns beyond `limit` stay uncovered ([`Window::uncovered`]) until a
/// later call with a larger `limit`.
///
/// This is for a caller that summarizes a context whose newest turn is still changing (its last
/// assistant message may be cut to what was played, notes may be appended to it): pass the index
/// of that turn's first message as `limit`, and the summary only describes turns that are final.
///
/// # Errors
/// The same as [`refresh_summary`].
pub async fn refresh_summary_until(
    ctx: &Context,
    policy: &WindowPolicy,
    estimator: &dyn TokenEstimator,
    summarizer: &dyn Summarizer,
    summary: &mut Option<Summary>,
    limit: usize,
) -> Result<usize, WindowError> {
    let mut calls = 0usize;
    while let Some(range) = needs_summary(ctx, policy, estimator, summary.as_ref())? {
        let end = if range.end <= limit {
            range.end
        } else {
            // The largest turn start in (range.start, limit]; none: nothing final is left.
            let starts = turn_starts(ctx.messages(), header_len(ctx.messages()));
            match starts
                .iter()
                .rev()
                .copied()
                .find(|&start| start <= limit && start > range.start)
            {
                Some(end) => end,
                None => break,
            }
        };
        let range = range.start..end;
        // The summarizer sees the messages as the window would send them.
        let messages: Vec<Message> = ctx
            .messages()
            .get(range.clone())
            .unwrap_or_default()
            .iter()
            .map(|m| clipped(m, policy.max_message_tokens, estimator).into_owned())
            .collect();
        let text = summarizer
            .summarize(summary.as_ref(), &messages, policy.max_summary_tokens)
            .await?;
        let candidate = Summary {
            text: Arc::from(text),
            covers_until: range.end,
        };
        let estimated_tokens = estimator.estimate(&summary_text_message(&candidate));
        if estimated_tokens > policy.max_summary_tokens {
            return Err(WindowError::SummaryTooLarge {
                estimated_tokens,
                limit: policy.max_summary_tokens,
            });
        }
        *summary = Some(candidate);
        calls = calls.saturating_add(1);
    }
    Ok(calls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{CharsPerToken, ToolCall, ToolCallId};
    use brisvox_core::TurnEpoch;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PROMPT: &str = "You are a voice agent.";

    /// A user message whose estimate is `tokens` (4 bytes per token, plus 4 per message).
    fn user_of(tokens: usize) -> Message {
        Message::user("a".repeat(tokens.saturating_mul(4)))
    }

    /// `turns` turns of one user message and one assistant reply, each about `tokens` tokens.
    fn chat(turns: usize, tokens: usize) -> Context {
        let mut ctx = Context::with_system(PROMPT);
        for turn in 0..turns {
            ctx.push(Message::user(format!("{turn} {}", "u".repeat(tokens * 4))));
            ctx.push(Message::assistant(format!(
                "{turn} {}",
                "a".repeat(tokens * 4)
            )));
        }
        ctx
    }

    /// Polls a future that is ready on its first poll.
    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        for _ in 0..1_000 {
            if let std::task::Poll::Ready(out) = future.as_mut().poll(&mut cx) {
                return out;
            }
        }
        panic!("test future did not complete");
    }

    /// A policy with the given budget and recent turns, and no per-message clipping.
    fn unclipped(max_tokens: usize, keep: usize) -> WindowPolicy {
        WindowPolicy {
            max_message_tokens: usize::MAX,
            ..WindowPolicy::new(max_tokens, keep)
        }
    }

    struct Echo;

    impl Summarizer for Echo {
        fn summarize<'a>(
            &'a self,
            _previous: Option<&'a Summary>,
            messages: &'a [Message],
            _max_tokens: usize,
        ) -> BoxFuture<'a, Result<String, WindowError>> {
            Box::pin(async move { Ok(format!("{} messages", messages.len())) })
        }
    }

    struct Broken;

    impl Summarizer for Broken {
        fn summarize<'a>(
            &'a self,
            _previous: Option<&'a Summary>,
            _messages: &'a [Message],
            _max_tokens: usize,
        ) -> BoxFuture<'a, Result<String, WindowError>> {
            Box::pin(async {
                Err(WindowError::Summarizer {
                    reason: "timeout".into(),
                })
            })
        }
    }

    /// Extends the previous text with a fixed sentence every round, as a careless summarizer
    /// might.
    struct Growing;

    impl Summarizer for Growing {
        fn summarize<'a>(
            &'a self,
            previous: Option<&'a Summary>,
            _messages: &'a [Message],
            _max_tokens: usize,
        ) -> BoxFuture<'a, Result<String, WindowError>> {
            Box::pin(async move {
                let mut text = previous.map(|s| s.text.to_string()).unwrap_or_default();
                text.push_str(&"the caller asked about the clinic again. ".repeat(12));
                Ok(text)
            })
        }
    }

    /// Accepts inputs of at most `limit` estimated tokens and fails for larger ones, like a
    /// summarizer with an input limit. Records the largest input it was given.
    struct RejectLarge {
        limit: usize,
        largest: AtomicUsize,
    }

    impl Summarizer for RejectLarge {
        fn summarize<'a>(
            &'a self,
            _previous: Option<&'a Summary>,
            messages: &'a [Message],
            _max_tokens: usize,
        ) -> BoxFuture<'a, Result<String, WindowError>> {
            Box::pin(async move {
                let est = CharsPerToken;
                let tokens: usize = messages.iter().map(|m| est.estimate(m)).sum();
                self.largest.fetch_max(tokens, Ordering::Relaxed);
                if tokens > self.limit {
                    return Err(WindowError::Summarizer {
                        reason: "input too large".into(),
                    });
                }
                Ok(format!("{} messages", messages.len()))
            })
        }
    }

    #[test]
    fn an_empty_context_gives_an_empty_window() {
        let ctx = Context::new();
        let est = CharsPerToken;
        let window = build_window(&ctx, &WindowPolicy::default(), &est, None).unwrap();
        assert!(window.context.is_empty());
        assert_eq!((window.kept_turns, window.kept_from), (0, 0));
        assert_eq!(window.estimated_tokens, 0);
        assert!(window.uncovered.is_empty() && !window.over_budget);
        assert_eq!(
            needs_summary(&ctx, &WindowPolicy::default(), &est, None),
            Ok(None)
        );
    }

    #[test]
    fn a_context_with_only_system_messages_is_kept_whole() {
        let mut ctx = Context::with_system(PROMPT);
        ctx.push(Message::system("late note"));
        let est = CharsPerToken;
        let window = build_window(&ctx, &WindowPolicy::new(10, 1), &est, None).unwrap();
        assert_eq!(window.context, ctx, "the header is kept even over budget");
        assert_eq!(window.kept_turns, 0);
        assert!(
            window.over_budget,
            "the prompt alone exceeds the budget and is reported"
        );
    }

    #[test]
    fn a_message_larger_than_the_budget_is_kept_and_reported() {
        let est = CharsPerToken;
        let mut ctx = Context::with_system(PROMPT);
        ctx.push(Message::user("earlier"));
        ctx.push(Message::assistant("earlier reply"));
        ctx.push(user_of(1000));
        // Clipping is off here: the point is that an unclippable-by-policy latest turn is still
        // sent and reported.
        let policy = unclipped(100, 8);
        let window = build_window(&ctx, &policy, &est, None).unwrap();
        assert!(window.over_budget);
        assert_eq!(window.kept_turns, 1, "only the latest turn is kept");
        assert_eq!(
            window.context.len(),
            2,
            "prompt and the latest user message"
        );
        assert_eq!(window.context.messages()[1].text(), "a".repeat(4000));
        window.context.validate().unwrap();
        // The dropped turn is not summarized, so the caller is told to summarize it.
        assert_eq!(needs_summary(&ctx, &policy, &est, None), Ok(Some(1..3)));
    }

    #[test]
    fn the_budget_shrinks_the_kept_turns_down_to_one() {
        let est = CharsPerToken;
        // Three turns of 2500 tokens each: two of them do not fit in 4000 with the prompt.
        let mut ctx = Context::with_system(PROMPT);
        for _ in 0..3 {
            ctx.push(user_of(2500));
        }
        let policy = unclipped(4000, 3);
        let window = build_window(&ctx, &policy, &est, None).unwrap();
        assert_eq!(window.kept_turns, 1);
        assert_eq!(window.kept_from, 3);
        assert!(!window.over_budget);
        assert_eq!(window.context.len(), 2);
    }

    #[test]
    fn keep_recent_turns_caps_the_window_when_the_budget_allows_more() {
        let est = CharsPerToken;
        // Each turn costs about 1010 tokens: three fit in 4000, four do not.
        let ctx = chat(5, 500);
        let window = build_window(&ctx, &WindowPolicy::new(4000, 2), &est, None).unwrap();
        assert_eq!(window.kept_turns, 2);
        assert_eq!(window.kept_from, ctx.len() - 4);
        // Zero is treated as one.
        let zero = build_window(&ctx, &WindowPolicy::new(4000, 0), &est, None).unwrap();
        assert_eq!(zero.kept_turns, 1);
        // With room for everything, every turn is kept.
        let all = build_window(&ctx, &WindowPolicy::new(100_000, 100), &est, None).unwrap();
        assert_eq!((all.kept_turns, all.kept_from), (5, 1));
        assert_eq!(all.context, ctx);
    }

    #[test]
    fn the_summary_follows_the_prompt_and_takes_budget() {
        let est = CharsPerToken;
        let ctx = chat(6, 100);
        let policy = WindowPolicy::new(4000, 2);
        // Summary covers the first four turns (messages 1..9).
        let summary = Summary {
            text: Arc::from("four turns about the appointment"),
            covers_until: 9,
        };
        let window = build_window(&ctx, &policy, &est, Some(&summary)).unwrap();
        let messages = window.context.messages();
        assert_eq!(messages[0], Message::system(PROMPT));
        assert_eq!(
            messages[1],
            Message::system(format!("{SUMMARY_PREFIX}four turns about the appointment")),
            "the window carries the summary text itself"
        );
        assert_eq!(window.kept_from, 9);
        assert_eq!(window.kept_turns, 2);
        assert!(window.uncovered.is_empty());
        assert_eq!(messages.len(), 2 + 4);
        window.context.validate().unwrap();
        // Same kept turns without the summary; the summary message is the only difference.
        let without = build_window(&ctx, &policy, &est, None).unwrap();
        assert_eq!(without.kept_from, window.kept_from);
        assert_eq!(
            window.estimated_tokens,
            without.estimated_tokens + est.estimate(&summary_message(&summary, &policy, &est))
        );
    }

    #[test]
    fn a_summary_must_end_at_a_turn_start() {
        let est = CharsPerToken;
        let ctx = chat(3, 10);
        let policy = WindowPolicy::default();
        // Indices 2 and 4 are assistant messages inside turns; `len` is past the last turn.
        for covers_until in [2, 4, ctx.len()] {
            let summary = Summary {
                text: Arc::from("x"),
                covers_until,
            };
            assert_eq!(
                build_window(&ctx, &policy, &est, Some(&summary)).map(|_| ()),
                Err(WindowError::InvalidSummary { covers_until })
            );
        }
        // A summary of nothing (the prompt only) and one ending at the first turn are valid.
        let empty = Summary {
            text: Arc::from("nothing"),
            covers_until: 1,
        };
        assert!(build_window(&ctx, &policy, &est, Some(&empty)).is_ok());
        let no_turns = Context::with_system(PROMPT);
        let summary = Summary {
            text: Arc::from("x"),
            covers_until: 1,
        };
        assert!(build_window(&no_turns, &policy, &est, Some(&summary)).is_ok());
    }

    #[test]
    fn an_invalid_context_is_rejected_and_not_modified() {
        let est = CharsPerToken;
        let mut ctx = chat(2, 10);
        let orphan = ToolCallId::new(TurnEpoch::ZERO, 0);
        ctx.push(Message::tool_result(orphan, "{}"));
        let before = ctx.clone();
        let result = build_window(&ctx, &WindowPolicy::default(), &est, None);
        assert!(matches!(result, Err(WindowError::InvalidContext(_))));
        assert_eq!(ctx, before);
    }

    #[test]
    fn tool_calls_are_never_split_by_the_cut() {
        let est = CharsPerToken;
        let mut ctx = Context::with_system(PROMPT);
        for turn in 0..6_u32 {
            ctx.push(Message::user(format!("question {turn}")));
            let id = ctx.next_tool_call_id(TurnEpoch::new(turn));
            ctx.push(Message::assistant_with_calls(
                "",
                vec![ToolCall {
                    id: id.clone(),
                    provider_id: None,
                    name: "lookup".into(),
                    arguments: json!({ "turn": turn }),
                    provider_data: None,
                }],
            ));
            ctx.push(Message::tool_result(id, "{\"ok\":true}"));
            ctx.push(Message::assistant("answer"));
        }
        for keep in 1..=6 {
            let window = build_window(&ctx, &WindowPolicy::new(10_000, keep), &est, None).unwrap();
            window.context.validate().unwrap();
            assert_eq!(window.kept_turns, keep);
            let first = window.context.messages().get(1);
            assert!(
                matches!(first, Some(Message::User { .. })),
                "a kept window starts at a turn"
            );
        }
    }

    #[test]
    fn needs_summary_asks_for_the_dropped_range_and_then_nothing() {
        let est = CharsPerToken;
        let ctx = chat(10, 30);
        let policy = WindowPolicy {
            max_summary_input_tokens: usize::MAX,
            ..unclipped(400, 3)
        };
        let range = needs_summary(&ctx, &policy, &est, None).unwrap().unwrap();
        assert_eq!(range.start, 1, "dropping starts after the prompt");
        let window = build_window(&ctx, &policy, &est, None).unwrap();
        assert_eq!(range.end, window.kept_from);
        assert_eq!(window.uncovered, range);
        assert_eq!(window.kept_turns, 3);
        // Summarize the range; the window is then up to date.
        let mut summary = None;
        let calls = block_on(refresh_summary(&ctx, &policy, &est, &Echo, &mut summary)).unwrap();
        assert!(calls >= 1);
        let summary = summary.unwrap();
        assert_eq!(summary.covers_until, window.kept_from);
        assert_eq!(needs_summary(&ctx, &policy, &est, Some(&summary)), Ok(None));
        let up_to_date = build_window(&ctx, &policy, &est, Some(&summary)).unwrap();
        assert!(up_to_date.uncovered.is_empty());
        assert!(up_to_date.estimated_tokens <= policy.max_tokens);
    }

    #[test]
    fn a_summarizer_round_takes_at_most_the_input_limit_and_the_backlog_converges() {
        let est = CharsPerToken;
        // Each turn is about 70 tokens; the dropped backlog is about 4000 tokens.
        let ctx = chat(60, 30);
        let policy = WindowPolicy {
            max_summary_input_tokens: 200,
            ..WindowPolicy::new(400, 3)
        };
        let first_round = needs_summary(&ctx, &policy, &est, None).unwrap().unwrap();
        assert!(
            first_round.len() > 1,
            "the backlog is larger than one round"
        );
        let round_tokens: usize = ctx.messages()[first_round.clone()]
            .iter()
            .map(|m| est.estimate(m))
            .sum();
        assert!(round_tokens <= 200, "one round sends {round_tokens} tokens");

        // An outage: every call fails, nothing is summarized, the window reports the gap.
        let mut summary = None;
        assert!(block_on(refresh_summary(&ctx, &policy, &est, &Broken, &mut summary)).is_err());
        assert!(summary.is_none());

        // Recovery: a summarizer that rejects inputs over 200 tokens still catches up, in rounds.
        let limited = RejectLarge {
            limit: 200,
            largest: AtomicUsize::new(0),
        };
        let calls = block_on(refresh_summary(&ctx, &policy, &est, &limited, &mut summary)).unwrap();
        assert!(calls > 1, "the catch-up takes several rounds");
        assert!(limited.largest.load(Ordering::Relaxed) <= 200);
        let summary = summary.unwrap();
        assert_eq!(needs_summary(&ctx, &policy, &est, Some(&summary)), Ok(None));
        let window = build_window(&ctx, &policy, &est, Some(&summary)).unwrap();
        assert!(window.uncovered.is_empty());
        assert!(!window.over_budget);
    }

    #[test]
    fn a_summary_over_its_limit_is_rejected_and_the_previous_one_kept() {
        let est = CharsPerToken;
        let ctx = chat(10, 30);
        let policy = unclipped(400, 3);
        assert_eq!(policy.max_summary_tokens, 100);
        // From scratch: the first round's summary is too large and nothing is stored.
        let mut summary = None;
        let result = block_on(refresh_summary(&ctx, &policy, &est, &Growing, &mut summary));
        assert!(matches!(
            result,
            Err(WindowError::SummaryTooLarge { limit: 100, .. })
        ));
        assert!(summary.is_none());
        // With a previous summary: it stays as it was.
        let previous = Summary {
            text: Arc::from("short"),
            covers_until: 1,
        };
        let mut summary = Some(previous.clone());
        let result = block_on(refresh_summary(&ctx, &policy, &est, &Growing, &mut summary));
        assert!(matches!(result, Err(WindowError::SummaryTooLarge { .. })));
        assert_eq!(summary, Some(previous));
    }

    #[test]
    fn a_caller_supplied_oversized_summary_is_clipped_in_the_window() {
        let est = CharsPerToken;
        let ctx = chat(3, 10);
        let policy = WindowPolicy::new(4000, 8);
        let summary = Summary {
            text: Arc::from("s".repeat(40_000)),
            covers_until: 1,
        };
        let window = build_window(&ctx, &policy, &est, Some(&summary)).unwrap();
        assert!(window.estimated_tokens <= policy.max_tokens);
        assert!(!window.over_budget);
        let summary_piece = &window.context.messages()[1];
        assert!(est.estimate(summary_piece) <= policy.max_summary_tokens);
        assert!(summary_piece.text().ends_with(CLIP_MARKER));
        // The full summary is untouched in the caller's hands.
        assert_eq!(summary.text.len(), 40_000);
    }

    #[test]
    fn a_large_tool_result_is_clipped_so_the_window_stays_within_budget() {
        let est = CharsPerToken;
        let mut ctx = Context::with_system(PROMPT);
        ctx.push(Message::user("look up my booking"));
        let id = ctx.next_tool_call_id(TurnEpoch::ZERO);
        ctx.push(Message::assistant_with_calls(
            "",
            vec![ToolCall {
                id: id.clone(),
                provider_id: None,
                name: "lookup".into(),
                arguments: json!({ "booking": 7 }),
                provider_data: None,
            }],
        ));
        // 20000 two-byte characters: about 10000 estimated tokens, and multi-byte on purpose.
        ctx.push(Message::tool_result(id.clone(), "ğ".repeat(20_000)));
        ctx.push(Message::assistant("done"));
        let policy = WindowPolicy::new(4000, 8);

        let window = build_window(&ctx, &policy, &est, None).unwrap();
        window.context.validate().unwrap();
        assert!(window.estimated_tokens <= policy.max_tokens);
        assert!(!window.over_budget, "a clipped latest turn fits the budget");
        let Message::Tool {
            call_id, content, ..
        } = &window.context.messages()[3]
        else {
            panic!("the tool result is at index 3 of the window");
        };
        assert_eq!(call_id, &id, "the result keeps its call id");
        assert!(content.ends_with(CLIP_MARKER));
        assert!(est.estimate(&window.context.messages()[3]) <= policy.max_message_tokens);
        // The transcript itself is not modified.
        assert_eq!(ctx.messages()[3].text().len(), 20_000 * 2);
    }

    #[test]
    fn a_limited_refresh_never_covers_past_the_last_turn_start_at_or_below_the_limit() {
        let est = CharsPerToken;
        let policy = unclipped(300, 2);
        let ctx = chat(10, 20);
        // The system prompt is message 0; turn k starts at message 1 + 2k.
        let mut unlimited = None;
        block_on(refresh_summary(&ctx, &policy, &est, &Echo, &mut unlimited)).unwrap();
        let unlimited_end = unlimited.as_ref().unwrap().covers_until;
        assert_eq!(unlimited_end, 1 + 2 * 8, "turns 8 and 9 are kept");

        // A limit at a turn start, and one inside a turn (rounded down to the turn start).
        for (limit, expected) in [(1 + 2 * 6, 1 + 2 * 6), (1 + 2 * 6 + 1, 1 + 2 * 6)] {
            let mut summary = None;
            let calls = block_on(refresh_summary_until(
                &ctx,
                &policy,
                &est,
                &Echo,
                &mut summary,
                limit,
            ))
            .unwrap();
            assert!(calls >= 1);
            assert_eq!(summary.unwrap().covers_until, expected, "limit {limit}");
        }
        // A limit that leaves nothing final to summarize: no summarizer call at all.
        let mut summary = None;
        let calls = block_on(refresh_summary_until(
            &ctx,
            &policy,
            &est,
            &Broken,
            &mut summary,
            1,
        ));
        assert_eq!(calls, Ok(0), "{calls:?}");
        assert!(summary.is_none());
        // A limit past what the window drops changes nothing.
        let mut summary = None;
        block_on(refresh_summary_until(
            &ctx,
            &policy,
            &est,
            &Echo,
            &mut summary,
            usize::MAX,
        ))
        .unwrap();
        assert_eq!(summary.unwrap().covers_until, unlimited_end);
    }

    /// The longest prefix (at a character boundary) whose clipped message fits, found the slow
    /// way.
    fn longest_fit(message: &Message, limit: usize, est: &dyn TokenEstimator) -> usize {
        let text = message.text();
        (0..=text.len())
            .filter(|&len| text.is_char_boundary(len))
            .filter(|&len| {
                est.estimate(&with_text(
                    message,
                    &format!("{}{CLIP_MARKER}", &text[..len]),
                )) <= limit
            })
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn the_clip_is_the_longest_fitting_prefix_whatever_the_size_of_the_text() {
        let est = CharsPerToken;
        // Below, around and far above the first probe of the search, ASCII and multibyte.
        for (unit, repeats) in [
            ("a", 50),
            ("a", 300),
            ("a", 1_000),
            ("ğüşıöç ", 150),
            ("x", 4_000),
        ] {
            let message =
                Message::tool_result(ToolCallId::new(TurnEpoch::ZERO, 0), unit.repeat(repeats));
            for limit in [6, 9, 40, 100, 257, 700] {
                if est.estimate(&message) <= limit {
                    continue;
                }
                let clip = clipped(&message, limit, &est);
                let expected = longest_fit(&message, limit, &est);
                let kept = clip.text().strip_suffix(CLIP_MARKER).map_or(0, str::len);
                assert_eq!(
                    kept, expected,
                    "{unit:?} x{repeats}, limit {limit}: not the longest fitting prefix"
                );
            }
        }
    }

    /// An estimator that adds up the bytes of text it is asked about.
    struct Counting(AtomicUsize);

    impl TokenEstimator for Counting {
        fn estimate(&self, message: &Message) -> usize {
            self.0.fetch_add(message.text().len(), Ordering::Relaxed);
            CharsPerToken.estimate(message)
        }
    }

    #[test]
    fn clipping_a_huge_message_costs_in_proportion_to_the_limit_not_to_the_text() {
        // 4 MiB of text clipped to 250 tokens (about 1 KB): the search handles candidates of a
        // few KB, not megabytes. Bisection from the full length would look at about 8 MiB
        // (4 MiB, 2 MiB, 1 MiB ... before the first candidate that fits).
        let message =
            Message::tool_result(ToolCallId::new(TurnEpoch::ZERO, 0), "x".repeat(1 << 22));
        let est = Counting(AtomicUsize::new(0));
        let clip = clipped(&message, 250, &est);
        assert!(CharsPerToken.estimate(&clip) <= 250);
        let looked_at = est.0.load(Ordering::Relaxed);
        // The first call looks at the whole message (to see that it is over the limit); the
        // search itself must stay small.
        let searched = looked_at - (1 << 22);
        assert!(
            searched < 64 * 1024,
            "the search looked at {searched} bytes"
        );
    }

    #[test]
    fn clipping_keeps_a_prefix_on_a_character_boundary_and_respects_the_limit() {
        let est = CharsPerToken;
        let text = "ğüşıöç ".repeat(300);
        let id = ToolCallId::new(TurnEpoch::ZERO, 0);
        let messages = [
            Message::user(text.clone()),
            Message::assistant(text.clone()),
            Message::system(text.clone()),
            Message::tool_result(id, text.clone()),
        ];
        for message in &messages {
            for limit in [0, 1, 4, 5, 20, 100, 400] {
                let clip = clipped(message, limit, &est);
                assert_eq!(
                    std::mem::discriminant(&*clip),
                    std::mem::discriminant(message),
                    "the role is kept"
                );
                if est.estimate(message) <= limit {
                    assert_eq!(&*clip, message, "a message that fits is not touched");
                    continue;
                }
                match clip.text().strip_suffix(CLIP_MARKER) {
                    Some(prefix) => {
                        assert!(text.starts_with(prefix), "limit {limit}: not a prefix");
                        assert!(
                            est.estimate(&clip) <= limit,
                            "limit {limit}: over the limit"
                        );
                    }
                    None => assert!(
                        clip.text().is_empty(),
                        "limit {limit}: only an empty text may lack the marker"
                    ),
                }
                if limit >= 100 {
                    assert!(
                        clip.text().len() > CLIP_MARKER.len(),
                        "limit {limit}: a clip keeps some text"
                    );
                }
            }
        }
    }

    #[test]
    fn a_failing_summarizer_leaves_the_summary_unchanged() {
        let est = CharsPerToken;
        let ctx = chat(10, 30);
        let policy = unclipped(400, 3);
        let mut summary = None;
        let result = block_on(refresh_summary(&ctx, &policy, &est, &Broken, &mut summary));
        assert_eq!(
            result,
            Err(WindowError::Summarizer {
                reason: "timeout".into()
            })
        );
        assert!(summary.is_none());
        let window = build_window(&ctx, &policy, &est, None).unwrap();
        assert!(!window.uncovered.is_empty(), "the gap is reported");
    }
}
