//! [`LlmRunner`](crate::LlmRunner): the LLM stage of the production pipeline (`docs/PIPELINE.md` sections 1 to 4
//! and 7).
//!
//! ```text
//! TurnDriver --Transcript, UserTurnEnded--> LlmRunner --LlmResponseStarted, LlmText, Flush,
//!                                              |            LlmResponseEnded--> SegmenterStage
//!                                              +--round task--> LlmProvider (stream)
//!                                              |                ToolEngine (execute)
//!                                              +--upstream TtsEnded (nothing was spoken)
//! ```
//!
//! # One task per turn
//! `UserTurnEnded` pushes the user message and spawns **one round task** per turn (a
//! [`TaskGuard`] and a child [`CancellationToken`]). The task owns the turn's rounds: it builds
//! the request through the [`RequestBuilder`] (context window), streams the response,
//! emits `LlmText` for every delta the moment it arrives, and runs the tool loop. Per round:
//!
//! 1. `LlmResponseStarted{epoch}`; the round `(epoch, ordinal)` is registered in the
//!    [`PlayoutBook`]; `timeline.llm_request` opens the round (visible in
//!    `TurnRecord::llm_rounds`, with `llm_first_token` and `llm_done`).
//! 2. The provider's stream runs under an explicit first-token timeout
//!    (`Timeouts::llm_first_token`, covering the request and the first event) and an idle
//!    timeout between events (`Timeouts::llm_idle`), both on the injected clock.
//! 3. `TextDelta`s go to the [`ResponseAssembler`](brisvox_agent::ResponseAssembler) and, immediately, downstream as `LlmText`
//!    (leading whitespace of the first delta of the round is stripped once, before both).
//!    `ToolCallDelta`s go to the assembler.
//! 4. At `Done` the assembler is finished (inline when small, on the [`Offload`] from
//!    [`LlmConfig::inline_finish_bytes`](crate::LlmConfig)) and the assistant message is pushed
//!    to the context **once**, before anything executes.
//! 5. No tool calls: `LlmResponseEnded`, the turn is over. With calls: `Flush` (the text before
//!    the tool is spoken while it runs), then `ToolEngine::execute` raced against the filler
//!    timer (the tool layer's [`FillerPolicy`] decides the
//!    delay, by default `LlmConfig::filler`: 1.2 s for every called tool that has a filler,
//!    and the phrase). If the
//!    timer wins, the filler is spoken as its own round (own `LlmResponseStarted`, own ordinal).
//!    When the policy names a pre-rendered clip of the phrase, the round is cued with it in the
//!    [`PlayoutBook`] first, and the TTS stage plays the clip instead of
//!    rendering the text.
//!    It is put into the context as an assistant message of its own once no call is open: before
//!    the next assistant message of the turn, or when the turn is settled (barge-in, end of the
//!    turn), never between a call and its result and never as the last message of a request.
//!    Its round is cut to what was heard like any other, so a filler that was played is in the
//!    context and one that was not is removed. The results are appended with
//!    `Execution::append_to`, late notes of earlier SideEffect calls are appended only when no
//!    call is open, and the next round starts.
//! 6. At most [`LlmConfig::max_tool_rounds`](crate::LlmConfig) rounds execute tools; beyond that
//!    (or when the engine's retry budget is spent) the calls are answered with error results
//!    without running and a fixed fallback line is spoken (it is in the context, so a later cut
//!    treats it like any other assistant text). Text of the unrun round that is still buffered
//!    downstream is flushed first (every round of speech that is not the model's response starts
//!    with a `Flush` when text is pending), so the two are separate utterances.
//!
//! # Round ordinals
//! The frames carry no round number, so downstream stages count: the ordinal of a round is the
//! number of `LlmResponseStarted` frames of its epoch before it (0, 1, 2, ...), filler and
//! fallback rounds included. The runner registers the same `(epoch, ordinal)` in the
//! [`PlayoutBook`] **before** it sends the frame, so the TTS stage can register the round's
//! utterance under that key the moment it sees `LlmResponseStarted`.
//!
//! Without speculation `SideEffect` and `ReadOnly` calls are both **non-speculative**: nothing is
//! dispatched before `Done`, and a round task is never re-run after dispatch. Exactly-once
//! execution comes from the tool engine's idempotency key `(session, epoch, call id)`.
//!
//! # Speculation (opt-in)
//! With [`EngineConfig::speculation`](crate::EngineConfig::speculation) enabled, the first round
//! of a turn may be *started before the turn is confirmed*: the driver offers a candidate end of
//! turn ([`crate::speculation`]), a helper task (`llm_spec`) runs the request on a copy
//! of the context with the candidate as the user message and **buffers** the output. The real
//! context, the `PlayoutBook`, the timeline marks of the response and the downstream stages see
//! nothing until the turn ends with the same (normalised) text and the context is unchanged;
//! then the round task of the turn takes the helper over and releases the buffer at once.
//! Otherwise the helper is dropped and the turn runs as without speculation. A speculative tool
//! phase runs the ReadOnly calls and *holds* the SideEffect calls (`ToolEngine::speculate`);
//! they run, once, at `ToolEngine::commit`. See `docs/PIPELINE.md` section 12.
//!
//! # Barge-in
//! `Interrupt` cancels the turn's token, aborts the task and joins it (bounded by
//! `Timeouts::join`); only then does the runner touch the context, so **the context is never
//! mutated while the task is alive**. A task that does not stop within the bound is kept as a
//! *zombie* (counted: `join_timeouts`, `mutations_skipped`), the context is left alone, and every
//! later halt joins it again first; a task that was halted never writes to the context itself
//! (its token is checked before each mutation). The instant of the barge-in is read **before**
//! the join, so a slow join cannot make the caller's position look later than it was. It then
//! pushes a partial assistant message with the raw text streamed so far (a superset of what was spoken; the truncation cuts it later), closes
//! open tool calls with `Context::interrupt_pending_tool_calls`, remembers when the barge-in
//! happened for the truncation, and forwards the `Interrupt`. A SideEffect tool keeps running on
//! its own task; its real result comes back as a late note on a later turn, exactly once.
//!
//! # Played text
//! Before the user message is pushed (and at `Stop`), the runner asks its [`TruncationSource`]
//! how much of each round's assistant message the caller heard, last round first, and applies the
//! answer with `apply_truncation` ([`crate::truncation`]). The default source keeps every message
//! as written; the production pipeline plugs in the [`PlayoutBook`]. The whole pass is bounded by
//! `Timeouts::analysis_job` on the injected clock: a source that does not answer must not stall
//! the processor (and with it every frame queued behind), so a question still open at the
//! deadline, and every later one, is answered with `Cut::Keep` and counted (`cut_timeouts`).
//!
//! # The summary is off the request path
//! A request is built with the window summary at hand and never waits for the summarizer. After
//! each response a background task (owned by the runner, one at a time, cancelled with it)
//! refreshes the summary *ahead*: for the window as it will look once the next user message is
//! in ([`crate::request`]). A request that still finds dropped turns uncovered goes out anyway and
//! is counted (`uncovered_requests`).
//!
//! # A panic does not hang the call
//! The round task is wrapped so that a panic in it (a provider adapter, a converter) is contained:
//! the partial text and the open calls are settled, `LlmResponseEnded` (and the upstream
//! `TtsEnded` when nothing was spoken) are sent, and the next turn goes on (`round_panics`).
//!
//! # Errors
//! A failed request, a stalled first token, an error event, a stream that ended without `Done`
//! or an idle stall is retried at most [`LlmConfig::retries`](crate::LlmConfig) times, and only
//! while nothing of the response has been spoken. After output nothing is retried: the partial
//! text is kept in the context, `LlmResponseEnded` is sent, and the next turn recovers. When the
//! turn produced no speech at all, the runner also sends a non-blocking upstream `TtsEnded`, so
//! the turn machine hears that the bot is done.
//!
//! # Concurrency
//! The context is an `Arc<Mutex<Context>>` (std mutex, never held across an await, the probe
//! surface of the simulator). The runner task and the round task never mutate it at the same time
//! (see Barge-in). Frames the round task sends go through a [`GuardedSender`](crate::GuardedSender), which drops frames
//! of an epoch that an interrupt already ended.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use brisvox_agent::context::{Context, Message, ToolCallId};
use brisvox_agent::flows::FlowManager;
use brisvox_agent::played::TruncationStats;
use brisvox_agent::tools::{FillerPolicy, ToolEngine, ToolRegistry};
use brisvox_core::frame::{ControlFrame, ControlKind, Frame};
use brisvox_core::{SharedClock, TimelineRecorder, TurnEpoch};
use brisvox_providers::{LlmProvider, ToolSpec, Usage};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::{LlmConfig, Timeouts};
use crate::guard::TaskGuard;
use crate::offload::Offload;
use crate::playout::{PlayoutBook, RoundKey};
use crate::request::RequestBuilder;
use crate::truncation::TruncationSource;

/// What the filler round says by default (English). Replace it with
/// [`LlmRunnerBuilder::with_phrases`](crate::LlmRunnerBuilder::with_phrases) for other languages.
pub const DEFAULT_FILLER: &str = brisvox_agent::tools::DEFAULT_FILLER_TEXT;

/// What the runner says by default when the tool budget is spent (English).
pub const DEFAULT_FALLBACK: &str = "Sorry, I could not complete that. Could you say it again?";

/// [`DEFAULT_FALLBACK`] in Turkish: said instead of it in a Turkish response of a call with
/// several languages (`crate::language`) when the runner's fallback was left at its default.
pub const DEFAULT_FALLBACK_TR: &str = "Üzgünüm, bunu tamamlayamadım. Tekrar söyler misiniz?";

/// Rounds remembered for truncation; older ones are dropped (counted). A turn has a handful.
pub(crate) const MAX_RECORDS: usize = 128;

/// Largest `Done.provider_data` the runner keeps, in bytes (and at most half a message's
/// allowance in the window: `window.max_message_tokens * 2` bytes). The text is opaque to the
/// engine (a thinking signature, reasoning state) and echoed in every later request, so a larger
/// blob is dropped (counted as `provider_data_oversized`) instead of growing the window
/// unboundedly. Raise `window.max_tokens` for a provider whose reasoning state is large.
pub const MAX_PROVIDER_DATA_BYTES: usize = 32 * 1024;

macro_rules! counters {
    ($($(#[$doc:meta])* $name:ident),* $(,)?) => {
        #[derive(Debug, Default)]
        pub(crate) struct Counters {
            $(pub(crate) $name: AtomicU64,)*
        }

        /// A copy of the [`LlmStats`] counters.
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct LlmStatsSnapshot {
            $($(#[$doc])* pub $name: u64,)*
        }

        impl Counters {
            fn snapshot(&self) -> LlmStatsSnapshot {
                LlmStatsSnapshot {
                    $($name: self.$name.load(Ordering::Relaxed),)*
                }
            }
        }
    };
}

counters! {
    /// User turns answered (round tasks spawned).
    turns,
    /// LLM rounds started (each sent `LlmResponseStarted`), canned fillers and fallbacks excluded.
    rounds,
    /// Provider requests made, retries included.
    requests,
    /// Rounds retried after a failure before any output.
    retries,
    /// Rounds that failed for good (after the retries, or after output).
    failed_rounds,
    /// Provider requests refused or failed before streaming.
    request_errors,
    /// Streams that produced `LlmEvent::Error`.
    stream_errors,
    /// Streams that ended without `Done` or `Error` (cut off).
    streams_cut_off,
    /// Rounds that got no first event within `llm_first_token`.
    first_token_timeouts,
    /// Streams that went silent for `llm_idle` after they started.
    idle_timeouts,
    /// Responses the assembler refused (size or call cap, conflicting fragments).
    assemble_errors,
    /// Prompt tokens the provider reported, summed over every response of the call that reported
    /// usage: normal rounds, tool-loop rounds and speculative rounds, committed or discarded
    /// (a discarded one is billed too). Counted once per response, at its `Done`.
    input_tokens,
    /// Of `input_tokens`, the tokens served from the provider's prompt cache.
    cached_input_tokens,
    /// Generated tokens the provider reported, summed like `input_tokens`.
    output_tokens,
    /// Responses that finished (`Done`) without reporting usage: the three token counters above
    /// are lower bounds by what these responses used.
    responses_without_usage,
    /// `Done.provider_data` that was not JSON (dropped).
    provider_data_invalid,
    /// `Done.provider_data` larger than [`MAX_PROVIDER_DATA_BYTES`] (dropped, not parsed).
    provider_data_oversized,
    /// `Done.provider_data` parsed on the offload pool (it is large) instead of inline.
    provider_data_offloaded,
    /// Responses finished on the async task (small).
    finishes_inline,
    /// Responses finished on the offload pool (large).
    finishes_offloaded,
    /// Offloaded finishes that failed (saturated, timed out, panicked): the round ends with its
    /// text only.
    finish_failures,
    /// Requests that could not be built (invalid context).
    request_build_errors,
    /// Rounds whose tool calls were executed.
    tool_rounds,
    /// Tool calls handed to the tool engine (or answered without it).
    tool_calls,
    /// Responses that asked for more tool rounds than `max_tool_rounds`.
    tool_round_limit_hits,
    /// Tool calls answered with an error because the runner has no tool engine.
    tools_unavailable,
    /// Tool phases the engine ended with its retry budget spent.
    tool_budget_exhausted,
    /// Filler rounds spoken.
    fillers,
    /// Spoken fillers put into the context (before a cut to what was heard removes or shortens
    /// them).
    fillers_in_context,
    /// Filler rounds whose phrase had a pre-rendered clip ready in the policy's cache: the round
    /// is cued in the `PlayoutBook` and the TTS stage plays the clip instead of rendering the
    /// text (the stage's `TtsStats::clips` / `clip_fallbacks` tell which happened).
    filler_clips,
    /// Fallback lines spoken.
    fallbacks,
    /// Late SideEffect notes appended to the context.
    late_notes,
    /// Bot lines spoken ([`crate::agent_input`]): turns whose response is a given line.
    bot_lines,
    /// Bot lines dropped because the previous round task did not stop in time.
    bot_lines_dropped,
    /// Conversation edits applied.
    edits_applied,
    /// Tool calls answered `unknown_tool` because an application stopped offering the tool.
    hidden_tool_calls,
    /// Turns that ended with an upstream `TtsEnded` from the runner (nothing was spoken).
    upstream_tts_ended,
    /// Upstream frames the runner could not queue (full lane).
    upstream_dropped,
    /// Turns that ended without any spoken text.
    silent_turns,
    /// Round tasks cancelled by an interrupt, a new turn or a stop.
    halts,
    /// Halts whose task did not stop within `Timeouts::join` (the context was left alone).
    join_timeouts,
    /// Partial assistant messages pushed at a halt.
    partial_messages,
    /// Open tool calls closed with an error result at a halt.
    interrupted_tool_calls,
    /// Round tasks that panicked (contained: the turn was closed and the session went on).
    round_panics,
    /// Background summary tasks started.
    summary_tasks,
    /// Summary requests skipped because the previous background task was still running.
    summary_in_flight,
    /// Rounds the truncation source was asked about.
    cut_requests,
    /// Cuts applied (`Cut::Heard`).
    cuts_heard,
    /// Messages applied whole (`Cut::HeardInFull`).
    cuts_whole,
    /// Messages kept as written (`Cut::Keep`).
    cuts_kept,
    /// Applied cuts that shortened the message.
    cuts_shortened,
    /// Applied cuts that removed the message (nothing was heard).
    cuts_removed,
    /// Truncation questions the source did not answer within the bound (the message was kept as
    /// written).
    cut_timeouts,
    /// Cuts `apply_truncation` refused (the heard text was not a prefix of the message).
    cut_errors,
    /// Rounds whose message was no longer where the record said.
    cuts_skipped,
    /// Rounds forgotten before they were cut (the record list is bounded).
    records_dropped,
    /// Times the context failed `Context::validate` after a runner mutation.
    validate_failures,
    /// Context mutations skipped because a halted task did not stop in time.
    mutations_skipped,
    /// Flow transitions entered.
    flow_transitions,
    /// Flow calls that failed.
    flow_errors,
    /// Provider warm-ups completed.
    warms,
    /// Provider warm-ups that failed or timed out.
    warm_failures,
    /// `UserTurnEnded` frames with no transcript (answered with an upstream `TtsEnded`).
    empty_turns,
    /// Round tasks that could not be spawned (the pipeline had no neighbour or epoch watch).
    spawn_failures,
}

pub(crate) fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Counters of an [`LlmRunner`](crate::LlmRunner), shared and readable while the session runs.
#[derive(Debug, Clone, Default)]
pub struct LlmStats {
    counters: Arc<Counters>,
    turn_usage: Arc<TurnUsage>,
}

/// The usage of the provider responses of the turn that is running, summed (what the turn's
/// `bot.llm_stopped` event carries). One short lock per response and one per turn end; no
/// allocation.
#[derive(Debug, Default)]
struct TurnUsage {
    slot: Mutex<TurnUsageSlot>,
}

#[derive(Debug, Default, Clone, Copy)]
struct TurnUsageSlot {
    epoch: u32,
    reported: bool,
    input: u64,
    cached: u64,
    output: u64,
}

impl LlmStats {
    /// A copy of the counters.
    pub fn snapshot(&self) -> LlmStatsSnapshot {
        self.counters.snapshot()
    }

    pub(crate) fn counters(&self) -> &Counters {
        &self.counters
    }

    /// Counts the usage of a response that finished (once per response): into the call's
    /// totals. `None` counts a response without usage.
    pub(crate) fn count_usage(&self, usage: Option<Usage>) {
        let counters = &self.counters;
        match usage {
            Some(usage) => {
                counters
                    .input_tokens
                    .fetch_add(u64::from(usage.input_tokens), Ordering::Relaxed);
                counters
                    .cached_input_tokens
                    .fetch_add(u64::from(usage.cached_input_tokens), Ordering::Relaxed);
                counters
                    .output_tokens
                    .fetch_add(u64::from(usage.output_tokens), Ordering::Relaxed);
            }
            None => bump(&counters.responses_without_usage),
        }
    }

    /// Adds the usage of a response of the turn of `epoch` to the turn's sum (the totals are
    /// [`Self::count_usage`]'s). A response of a newer epoch starts a new sum; one of an older
    /// epoch than the sum's (a stale turn's `Done` landing after the next turn began) is not
    /// part of any turn still to be reported and leaves the sum alone.
    pub(crate) fn add_turn_usage(&self, epoch: TurnEpoch, usage: Usage) {
        let mut slot = lock(&self.turn_usage.slot);
        if epoch.get() < slot.epoch {
            return;
        }
        if slot.epoch != epoch.get() {
            *slot = TurnUsageSlot {
                epoch: epoch.get(),
                ..TurnUsageSlot::default()
            };
        }
        slot.reported = true;
        slot.input = slot.input.saturating_add(u64::from(usage.input_tokens));
        slot.cached = slot
            .cached
            .saturating_add(u64::from(usage.cached_input_tokens));
        slot.output = slot.output.saturating_add(u64::from(usage.output_tokens));
    }

    /// The summed usage of the turn of `epoch`, once (the sum is cleared); `None` when no
    /// response of the turn reported usage.
    pub(crate) fn take_turn_usage(&self, epoch: TurnEpoch) -> Option<Usage> {
        let mut slot = lock(&self.turn_usage.slot);
        if slot.epoch != epoch.get() || !slot.reported {
            return None;
        }
        let taken = *slot;
        *slot = TurnUsageSlot::default();
        let clamp = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
        Some(Usage {
            input_tokens: clamp(taken.input),
            cached_input_tokens: clamp(taken.cached),
            output_tokens: clamp(taken.output),
        })
    }
}

/// An assistant message of the context that may have to be cut to what was heard.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RoundRecord {
    pub(crate) key: RoundKey,
    pub(crate) msg_index: Option<usize>,
    pub(crate) interrupted_at: Option<Instant>,
}

/// The response being streamed: its raw text so far.
#[derive(Debug)]
pub(crate) struct LiveRound {
    pub(crate) key: RoundKey,
    pub(crate) text: String,
}

/// A filler spoken while a tool round ran, waiting for its place in the context: it
/// cannot go between a tool call and its result, nor end the next request.
#[derive(Debug)]
pub(crate) struct SpokenFiller {
    pub(crate) key: RoundKey,
    pub(crate) text: Arc<str>,
}

#[derive(Debug, Default)]
pub(crate) struct Inner {
    pub(crate) records: Vec<RoundRecord>,
    pub(crate) live: Option<LiveRound>,
    /// The filler spoken last, not yet in the context.
    pub(crate) filler: Option<SpokenFiller>,
    /// A round task is working on the turn (set at spawn, cleared when the response ended).
    pub(crate) responding: bool,
}

/// The tools of the session: what the LLM may call and the engine that runs the calls.
pub(crate) struct ToolSetup {
    pub(crate) registry: Arc<ToolRegistry>,
    pub(crate) engine: Arc<ToolEngine>,
    /// Provider forms of every registered tool, by name.
    pub(crate) specs: BTreeMap<Arc<str>, ToolSpec>,
}

/// Everything the runner and its round tasks share.
pub(crate) struct Shared<P: LlmProvider> {
    pub(crate) provider: Arc<P>,
    pub(crate) clock: SharedClock,
    pub(crate) timeline: TimelineRecorder,
    pub(crate) timeouts: Timeouts,
    pub(crate) llm: LlmConfig,
    pub(crate) context: Arc<Mutex<Context>>,
    pub(crate) builder: Arc<RequestBuilder>,
    pub(crate) tools: Option<ToolSetup>,
    pub(crate) flow: Option<tokio::sync::Mutex<FlowManager>>,
    pub(crate) book: Option<Arc<PlayoutBook>>,
    pub(crate) offload: Offload,
    pub(crate) truncation: Arc<dyn TruncationSource>,
    pub(crate) truncation_stats: Arc<Mutex<TruncationStats>>,
    pub(crate) filler: FillerPolicy,
    pub(crate) fallback: Arc<str>,
    pub(crate) stats: LlmStats,
    pub(crate) inner: Mutex<Inner>,
    /// Cancelled when the runner is cancelled or dropped; parent of every round's token and of
    /// the background summary task.
    pub(crate) cancel: CancellationToken,
    /// The background summary task, if one was started (at most one runs at a time).
    pub(crate) summary_task: Mutex<Option<TaskGuard>>,
    /// The tools offered to the LLM when an application narrowed them
    /// ([`ConversationEdit::SetTools`](crate::agent_input::ConversationEdit::SetTools));
    /// `None`: every registered tool. Changed only at a safe point (no round task alive).
    pub(crate) offered: Mutex<Option<Arc<BTreeSet<Arc<str>>>>>,
    /// The call languages (`crate::language`): each turn pins the reply language in its request.
    pub(crate) languages: Option<Arc<crate::language::LanguageTracker>>,
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn control(epoch: TurnEpoch, kind: ControlKind) -> Frame {
    ControlFrame { epoch, kind }.into()
}

impl<P: LlmProvider> Shared<P> {
    /// Starts the background refresh of the window summary (off the request path), unless there
    /// is no summarizer or the previous refresh is still running. The task ends when it is done,
    /// when the runner is cancelled or dropped (`cancel`), and is bounded by the summary timeout
    /// on the injected clock. It is owned by `self`, so no detached task is left behind.
    pub(crate) fn spawn_summary(self: &Arc<Self>) {
        if self.builder.summarizer().is_none() {
            return;
        }
        let mut slot = lock(&self.summary_task);
        if slot.as_ref().is_some_and(|task| !task.is_finished()) {
            bump(&self.count().summary_in_flight);
            return;
        }
        bump(&self.count().summary_tasks);
        let shared = Arc::clone(self);
        *slot = Some(TaskGuard::spawn("llm_summary", async move {
            tokio::select! {
                biased;
                () = shared.cancel.cancelled() => {}
                () = shared.builder.refresh_ahead(&shared.context, &*shared.clock) => {}
            }
        }));
    }

    pub(crate) fn count(&self) -> &Counters {
        self.stats.counters()
    }

    /// Remembers that the assistant message at `msg_index` belongs to round `key` (and tells the
    /// playout book). Bounded: the oldest record is dropped (counted) beyond [`MAX_RECORDS`].
    pub(crate) fn record_round(&self, inner: &mut Inner, key: RoundKey, msg_index: usize) {
        if inner.records.len() >= MAX_RECORDS {
            inner.records.remove(0);
            bump(&self.count().records_dropped);
        }
        inner.records.push(RoundRecord {
            key,
            msg_index: Some(msg_index),
            interrupted_at: None,
        });
        if let Some(book) = &self.book {
            book.set_msg_index(key, msg_index);
        }
    }

    /// Turns the response being streamed into a partial assistant message: the raw text so far,
    /// a superset of what was spoken. Returns whether a message was pushed. The caller must be
    /// the only mutator of the context (the round task itself, or the runner after a join).
    pub(crate) fn settle_live(&self) -> bool {
        let mut ctx = lock(&self.context);
        let mut inner = lock(&self.inner);
        // A filler spoken before this response comes first (the response was streamed after
        // it, and no call is open while a response streams).
        self.place_filler_in(&mut ctx, &mut inner);
        let Some(live) = inner.live.take() else {
            return false;
        };
        if live.text.is_empty() {
            return false;
        }
        let index = ctx.len();
        ctx.push(Message::assistant(live.text));
        self.record_round(&mut inner, live.key, index);
        bump(&self.count().partial_messages);
        true
    }

    /// Puts the spoken filler into the context as an assistant message of its own, cut later to
    /// what was heard like any round, when no tool call is open. Returns whether it was placed.
    /// The caller must be the only mutator of the context (the round task, or the runner after a
    /// join).
    pub(crate) fn place_filler_in(&self, context: &mut Context, inner: &mut Inner) -> bool {
        if inner.filler.is_none() || !context.pending_tool_calls().is_empty() {
            return false;
        }
        let Some(filler) = inner.filler.take() else {
            return false;
        };
        let index = context.len();
        context.push(Message::assistant(filler.text));
        self.record_round(inner, filler.key, index);
        bump(&self.count().fillers_in_context);
        true
    }

    /// [`Self::place_filler_in`] with the locks taken here.
    pub(crate) fn place_filler(&self) -> bool {
        let mut context = lock(&self.context);
        let mut inner = lock(&self.inner);
        self.place_filler_in(&mut context, &mut inner)
    }

    /// Closes the tool calls still open in the context with error results.
    pub(crate) fn close_open_calls(&self) -> Vec<ToolCallId> {
        self.close_open_calls_in(&mut lock(&self.context))
    }

    /// [`Self::close_open_calls`] on a context the caller already holds.
    pub(crate) fn close_open_calls_in(&self, context: &mut Context) -> Vec<ToolCallId> {
        let closed = context.interrupt_pending_tool_calls();
        let n = u64::try_from(closed.len()).unwrap_or(u64::MAX);
        self.count()
            .interrupted_tool_calls
            .fetch_add(n, Ordering::Relaxed);
        closed
    }

    /// Checks the context after a mutation and counts a failure.
    pub(crate) fn validate(&self, context: &Context, what: &'static str) {
        if let Err(error) = context.validate() {
            bump(&self.count().validate_failures);
            tracing::error!(target: "brisvox::engine", %error, what, "context invalid after mutation");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn usage(input: u32, cached: u32, output: u32) -> Usage {
        Usage {
            input_tokens: input,
            cached_input_tokens: cached,
            output_tokens: output,
        }
    }

    #[test]
    fn the_totals_count_each_response_once_and_those_without_usage_apart() {
        let stats = LlmStats::default();
        stats.count_usage(Some(usage(10, 4, 3)));
        stats.count_usage(Some(usage(5, 0, 2)));
        stats.count_usage(None);
        let s = stats.snapshot();
        assert_eq!(
            (
                s.input_tokens,
                s.cached_input_tokens,
                s.output_tokens,
                s.responses_without_usage
            ),
            (15, 4, 5, 1)
        );
    }

    #[test]
    fn the_turn_sum_is_taken_once_per_epoch_and_a_newer_epoch_starts_afresh() {
        let stats = LlmStats::default();
        let (one, two) = (TurnEpoch::new(1), TurnEpoch::new(2));
        assert_eq!(stats.take_turn_usage(one), None, "nothing reported");
        stats.add_turn_usage(one, usage(10, 4, 3));
        stats.add_turn_usage(one, usage(5, 0, 2));
        assert_eq!(stats.take_turn_usage(two), None, "another epoch's sum");
        assert_eq!(stats.take_turn_usage(one), Some(usage(15, 4, 5)));
        assert_eq!(stats.take_turn_usage(one), None, "taken");
        // A response of a newer epoch drops the older, untaken sum.
        stats.add_turn_usage(one, usage(1, 0, 1));
        stats.add_turn_usage(two, usage(2, 0, 2));
        assert_eq!(stats.take_turn_usage(one), None);
        assert_eq!(stats.take_turn_usage(two), Some(usage(2, 0, 2)));
        // A stale turn's response landing after the next turn began does not reset its sum.
        stats.add_turn_usage(two, usage(3, 1, 1));
        stats.add_turn_usage(one, usage(100, 0, 100));
        assert_eq!(stats.take_turn_usage(one), None);
        assert_eq!(stats.take_turn_usage(two), Some(usage(3, 1, 1)));
        stats.add_turn_usage(two, usage(0, 0, 0));
        assert_eq!(stats.take_turn_usage(two), Some(usage(0, 0, 0)));
        // The sum saturates instead of wrapping.
        stats.add_turn_usage(two, usage(u32::MAX, 0, 0));
        stats.add_turn_usage(two, usage(u32::MAX, 0, 0));
        assert_eq!(stats.take_turn_usage(two), Some(usage(u32::MAX, 0, 0)));
    }
}
