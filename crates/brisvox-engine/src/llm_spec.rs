//! The speculative run of the [`LlmRunner`](crate::LlmRunner). The protocol and the
//! rules for a hit are in [`crate::speculation`]; this file is the helper task and its hand-over.
//!
//! # Shape
//! A candidate end of turn starts ONE helper task ([`SpecRun`], a [`TaskGuard`]). The runner
//! hands it a **copy** of the settled context plus the candidate as the user message, taken
//! before the task is spawned (the real context is not touched: the user message is pushed only
//! at the commit, so a discarded speculation leaves no trace, and the turn end cannot change what
//! the helper builds its request from). The helper builds the request, streams the response of the FIRST round exactly as the round
//! task would, and buffers what the round would have sent downstream in a bounded channel
//! ([`SpecItem`]). Nothing reaches the pipeline, the [`PlayoutBook`](crate::PlayoutBook) or the
//! timeline from here.
//!
//! - **Text** is kept as the pieces `LlmText` would carry (the leading whitespace of the
//!   response already stripped, see [`LeadTrim`]).
//! - **Tool calls.** At `Done` the assembler is finished like the round task does. With a tool
//!   engine (and no flow) the helper runs [`ToolEngine::speculate`](brisvox_agent::tools::ToolEngine::speculate):
//!   ReadOnly calls run now, SideEffect calls are HELD inside the returned
//!   [`Speculation`] and run only if the turn commits
//!   ([`ToolEngine::commit`](brisvox_agent::tools::ToolEngine::commit), exactly once, by the
//!   engine's idempotency key). A discarded speculation drops them unrun.
//! - **Commit.** The runner hands the receiver to the round task of the turn
//!   ([`SpecHandoff`]), which replays the buffered pieces at once and then follows the live
//!   stream; the first round is `complete_speculated` instead of an LLM request. Later rounds
//!   (after a tool phase) are ordinary rounds.
//! - **Discard.** Dropping the [`SpecRun`] aborts the helper (a ReadOnly tool in flight is
//!   cancelled with it), nothing else needs undoing.
//!
//! # Bounds and cancellation
//! The channel is bounded ([`crate::SpeculationConfig::max_buffered_events`]): when it is full the
//! helper waits (back-pressure, nothing is dropped). The helper ends by itself at the
//! speculation's `deadline` (`max_age` after its start) unless the turn committed meanwhile, so
//! a turn that never confirms cannot hold a provider stream open. It also ends when its token is
//! cancelled (the token becomes the round task's token at a commit, so one cancel stops both).
//! Every provider wait has the explicit first-token / idle timeout of the round task.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_agent::assemble::{Assembled, Delta, ResponseAssembler};
use brisvox_agent::context::{Context, Message};
use brisvox_agent::tools::{FillerPlan, Speculation, ToolEngine};
use brisvox_core::TurnEpoch;
use brisvox_core::clock::deadline_after;
use brisvox_providers::{LlmEvent, LlmProvider, LlmStream, ToolSpec};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::SpeculationConfig;
use crate::convert::delta_of;
use crate::guard::TaskGuard;
use crate::llm::{Shared, bump as bump_llm, lock};
use crate::llm_turn::{finish_response, provider_data_of};
use crate::speculation::{Discard, SpecCounters, add, bump};

/// Strips the leading whitespace of a response, once: whitespace-only deltas before the first
/// text are skipped, the first delta with text loses its leading whitespace.
#[derive(Debug, Default)]
pub(crate) struct LeadTrim {
    started: bool,
}

impl LeadTrim {
    /// The piece to emit for `text`, or `None` when it is only whitespace before the start.
    pub(crate) fn apply(&mut self, text: Arc<str>) -> Option<Arc<str>> {
        if self.started {
            return Some(text);
        }
        let trimmed = text.trim_start();
        if trimmed.is_empty() {
            return None;
        }
        self.started = true;
        Some(if trimmed.len() == text.len() {
            text
        } else {
            Arc::from(trimmed)
        })
    }
}

/// What the speculative helper hands to the round task of the committed turn.
pub(crate) enum SpecItem {
    /// A piece of text, as `LlmText` would carry it.
    Text(Arc<str>),
    /// The ReadOnly calls of the response are running (the wait for `Done` may be long: no idle
    /// timeout applies, the tools have their own).
    ToolsStarted,
    /// The response is complete.
    Done(Box<SpecDone>),
    /// The response was received but could not be finished (a large response whose offloaded
    /// finish failed): the text stands, there are no calls.
    FinishFailed,
    /// The stream failed (counted by the helper).
    Failed,
}

/// A complete speculative first round.
pub(crate) struct SpecDone {
    /// The assistant message to push (`None` for an empty response).
    pub(crate) message: Option<Message>,
    /// The response asked for tools.
    pub(crate) had_calls: bool,
    /// The calls, when they were NOT speculated (no tool engine, or the engine is not used).
    pub(crate) calls: Vec<Assembled>,
    /// The speculative tool phase: ReadOnly calls done, SideEffect calls held.
    pub(crate) speculation: Option<HeldSpeculation>,
    /// When the tool phase needs a filler (the tool layer's [`FillerPolicy`]).
    pub(crate) filler: Option<FillerPlan>,
}

/// A speculative tool phase that gives back the retry-budget round it spent unless it is
/// committed: a discarded speculation (the helper aborted, the buffer dropped, the turn halted)
/// must not leave the turn with less budget than a run without speculation would have.
pub(crate) struct HeldSpeculation {
    engine: Arc<ToolEngine>,
    speculation: Option<Speculation>,
}

impl HeldSpeculation {
    fn new(engine: Arc<ToolEngine>, speculation: Speculation) -> Self {
        Self {
            engine,
            speculation: Some(speculation),
        }
    }

    /// Takes the speculation out to commit it; nothing is refunded then.
    pub(crate) fn into_committed(mut self) -> Option<Speculation> {
        self.speculation.take()
    }
}

impl Drop for HeldSpeculation {
    fn drop(&mut self) {
        if let Some(speculation) = self.speculation.take() {
            self.engine.refund_speculation(&speculation);
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Marks {
    pub(crate) requested_at: Option<Instant>,
    pub(crate) first_token_at: Option<Instant>,
    pub(crate) done_at: Option<Instant>,
}

/// State the helper and the runner share.
#[derive(Debug, Default)]
pub(crate) struct SpecShared {
    /// `RUNNING` until the runner claims the speculation (the turn commits: the helper then
    /// ignores its age limit) or the helper reaches its deadline (it then ends). One
    /// compare-and-swap decides, so exactly one side wins and a claimed speculation can never be
    /// cut short by its own expiry.
    state: AtomicU8,
    /// Bytes of text and tool arguments streamed so far (for the token estimate).
    output_bytes: AtomicU64,
    /// Tokens the provider reported at `Done` (0 when none).
    input_tokens: AtomicU64,
    output_tokens: AtomicU64,
    marks: Mutex<Marks>,
}

/// The speculation is running and still unclaimed.
const RUNNING: u8 = 0;
/// The turn claimed the speculation: it will not expire.
const CLAIMED: u8 = 1;
/// The helper reached its deadline unclaimed and ended.
const EXPIRED: u8 = 2;

impl SpecShared {
    /// Claims the speculation for the turn. `false` when the helper expired first.
    fn claim(&self) -> bool {
        match self
            .state
            .compare_exchange(RUNNING, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => true,
            Err(previous) => previous == CLAIMED,
        }
    }

    /// The helper's deadline passed: `true` when it may end (nobody claimed it), `false` when
    /// the turn claimed it first and the helper must carry on.
    fn expire(&self) -> bool {
        self.state
            .compare_exchange(RUNNING, EXPIRED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn marks(&self) -> Marks {
        *lock(&self.marks)
    }

    fn mark(&self, f: impl FnOnce(&mut Marks)) {
        f(&mut lock(&self.marks));
    }

    /// Output tokens spent so far: what the provider reported, else 4 bytes per token.
    fn output_tokens_spent(&self) -> u64 {
        match self.output_tokens.load(Ordering::Relaxed) {
            0 => self.output_bytes.load(Ordering::Relaxed).div_ceil(4),
            reported => reported,
        }
    }
}

/// A running speculation, held by the runner until the turn ends.
pub(crate) struct SpecRun {
    pub(crate) epoch: TurnEpoch,
    /// [`crate::speculation::normalize`] of the candidate text.
    pub(crate) norm: String,
    /// [`fingerprint`] of the settled context the request was built from (before the user
    /// message): its length and, per message, the role, the byte length and the tail of the text.
    pub(crate) basis_fp: u64,
    /// The helper ends by itself at this instant unless committed.
    pub(crate) deadline: Instant,
    token: CancellationToken,
    shared: Arc<SpecShared>,
    rx: mpsc::Receiver<SpecItem>,
    guard: TaskGuard,
}

/// What the round task of a committed turn gets.
pub(crate) struct SpecHandoff {
    pub(crate) rx: mpsc::Receiver<SpecItem>,
    /// Keeps the helper alive; the helper is aborted when the round task drops it.
    #[allow(dead_code)] // held for its `Drop`
    pub(crate) guard: TaskGuard,
    pub(crate) shared: Arc<SpecShared>,
}

impl SpecRun {
    /// Counts and drops the speculation: the helper is cancelled and aborted, the tokens it
    /// spent are wasted.
    pub(crate) fn discard(self, counters: &SpecCounters, why: Discard) {
        counters.discarded(why);
        add(
            &counters.wasted_output_tokens,
            self.shared.output_tokens_spent(),
        );
        add(
            &counters.wasted_input_tokens,
            self.shared.input_tokens.load(Ordering::Relaxed),
        );
        self.token.cancel();
        // `guard` drops here and aborts the helper.
    }

    /// Lifts the helper's age limit: the turn has decided to use this speculation. Called as
    /// soon as the decision is made (before the awaits that remain until [`Self::commit`]), so
    /// the helper cannot expire in between and hand an empty stream to the turn. Atomic against
    /// the helper's own expiry: returns `false` when the helper had expired first (the caller
    /// then discards the speculation as expired and runs the turn plainly), so a claimed
    /// speculation always keeps a live helper.
    #[must_use]
    pub(crate) fn claim(&self) -> bool {
        self.shared.claim()
    }

    /// Commits the speculation (already claimed with [`Self::claim`]) at `now`: the round task
    /// of the turn takes over its receiver and the helper itself. Returns the token the round
    /// task must use (the helper's, so one cancel stops both).
    pub(crate) fn commit(
        self,
        counters: &SpecCounters,
        now: Instant,
    ) -> (CancellationToken, SpecHandoff) {
        debug_assert_eq!(
            self.shared.state.load(Ordering::Acquire),
            CLAIMED,
            "committed without a claim"
        );
        bump(&counters.hits);
        if let Some(requested) = self.shared.marks().requested_at {
            let ahead = now.saturating_duration_since(requested);
            add(
                &counters.head_start_ms,
                u64::try_from(ahead.as_millis()).unwrap_or(u64::MAX),
            );
        }
        (
            self.token,
            SpecHandoff {
                rx: self.rx,
                guard: self.guard,
                shared: self.shared,
            },
        )
    }
}

/// A fingerprint of a context: its length and, per message, the role, the byte length and the
/// last bytes of the text, and the ids of the tool calls. Two contexts with the same fingerprint
/// are the same conversation for the purpose of "is the speculative request still the request of
/// a plain run": a truncation that shortens an assistant message in place changes its length, a
/// late note adds a message. The only mutations of a settled context are appends and shortenings
/// (a cut keeps a prefix), so the lengths and the tail identify it without reading the text.
///
/// O(messages), not O(bytes): the fingerprint runs on a tokio worker twice per speculated turn,
/// once under the context lock, and a context can hold hundreds of KB of tool results (a pass
/// over the bytes would break the 50 us rule). Hashed with the fixed-key default hasher; a
/// collision is as unlikely as a chance equal 64-bit hash.
pub(crate) fn fingerprint(context: &Context) -> u64 {
    use std::hash::{Hash, Hasher};
    /// How many trailing bytes of a text are hashed besides its length.
    const TAIL: usize = 16;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    context.len().hash(&mut hasher);
    for message in context.messages() {
        std::mem::discriminant(message).hash(&mut hasher);
        let text = message.text().as_bytes();
        text.len().hash(&mut hasher);
        text.get(text.len().saturating_sub(TAIL)..)
            .unwrap_or_default()
            .hash(&mut hasher);
        if let Message::Assistant { tool_calls, .. } = message {
            tool_calls.len().hash(&mut hasher);
            for call in tool_calls {
                call.id.as_str().hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

/// Starts the helper for the candidate of user turn `epoch`. `context` is the settled context
/// with the candidate as its last (user) message, `basis_fp` the [`fingerprint`] of the settled context
/// before it. `token` is a child of the runner's root token. Must run inside a tokio runtime.
#[allow(clippy::too_many_arguments)] // one call site, every argument is distinct state
pub(crate) fn start<P: LlmProvider>(
    s: &Arc<Shared<P>>,
    counters: &Arc<SpecCounters>,
    token: CancellationToken,
    epoch: TurnEpoch,
    context: Context,
    norm: String,
    basis_fp: u64,
    max_age: Duration,
    buffered: usize,
) -> SpecRun {
    let deadline = deadline_after(s.clock.now(), max_age);
    let shared = Arc::new(SpecShared::default());
    // At least room for a terminal item behind a full buffer of text; never more than the
    // documented limit (a config that skipped `validate` must not panic tokio or go unbounded).
    let capacity = buffered.clamp(2, SpeculationConfig::MAX_BUFFERED_EVENTS_LIMIT);
    let (tx, rx) = mpsc::channel(capacity);
    bump(&counters.started);
    let task = Task {
        s: Arc::clone(s),
        counters: Arc::clone(counters),
        epoch,
        context: Mutex::new(Some(Arc::new(Mutex::new(context)))),
        token: token.clone(),
        shared: Arc::clone(&shared),
        tx,
        deadline,
    };
    let guard = TaskGuard::spawn("llm_speculation", task.run());
    SpecRun {
        epoch,
        norm,
        basis_fp,
        deadline,
        token,
        shared,
        rx,
        guard,
    }
}

struct Task<P: LlmProvider> {
    s: Arc<Shared<P>>,
    counters: Arc<SpecCounters>,
    epoch: TurnEpoch,
    /// The context the request is built from; taken (and freed) when the request is built.
    context: Mutex<Option<Arc<Mutex<Context>>>>,
    token: CancellationToken,
    shared: Arc<SpecShared>,
    tx: mpsc::Sender<SpecItem>,
    deadline: Instant,
}

/// Why waiting for the next stream event ended.
enum Next {
    Event(LlmEvent),
    Closed,
    Timeout,
}

impl<P: LlmProvider> Task<P> {
    async fn run(self) {
        // The age limit applies until the turn commits; after that the round task's own
        // timeouts take over.
        let expiry = async {
            self.s.clock.sleep_until(self.deadline).await;
            if !self.shared.expire() {
                // Claimed by the turn: the age limit no longer applies.
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            () = self.token.cancelled() => {}
            () = expiry => {
                tracing::debug!(target: "brisvox::engine", epoch = self.epoch.get(), "speculation expired before the turn ended");
            }
            () = self.body() => {}
        }
    }

    /// Hands `item` to the committed round task (or waits for room). `false` when nobody listens.
    async fn send(&self, item: SpecItem) -> bool {
        self.tx.send(item).await.is_ok()
    }

    async fn fail(&self) {
        bump(&self.counters.failures);
        self.send(SpecItem::Failed).await;
    }

    async fn next_event(&self, stream: &mut LlmStream, deadline: Instant) -> Next {
        let sleep = self.s.clock.sleep_until(deadline);
        tokio::pin!(sleep);
        tokio::select! {
            biased;
            () = &mut sleep => Next::Timeout,
            event = stream.next() => match event {
                Some(event) => Next::Event(event),
                None => Next::Closed,
            },
        }
    }

    /// The first round of the turn, as the round task would run it, into the channel.
    async fn body(&self) {
        let s = &self.s;
        let tools = tool_specs(s);
        let Some(scratch) = lock(&self.context).take() else {
            return;
        };
        let request = match s.builder.build_offloaded(&scratch, &tools).await {
            Ok(built) => built.request,
            Err(error) => {
                bump_llm(&s.count().request_build_errors);
                tracing::error!(target: "brisvox::engine", %error, "cannot build the speculative LLM request");
                self.fail().await;
                return;
            }
        };
        drop(scratch);
        let first_deadline = deadline_after(s.clock.now(), s.timeouts.llm_first_token);
        bump_llm(&s.count().requests);
        self.shared
            .mark(|marks| marks.requested_at = Some(s.clock.now()));
        let sleep = s.clock.sleep_until(first_deadline);
        tokio::pin!(sleep);
        let opened = tokio::select! {
            biased;
            () = &mut sleep => {
                bump_llm(&s.count().first_token_timeouts);
                self.fail().await;
                return;
            }
            opened = s.provider.stream(request) => opened,
        };
        let mut stream = match opened {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(target: "brisvox::engine", %error, "speculative llm request failed");
                bump_llm(&s.count().request_errors);
                self.fail().await;
                return;
            }
        };
        let mut assembler = ResponseAssembler::new();
        let mut trim = LeadTrim::default();
        let mut got_event = false;
        let mut first_marked = false;
        loop {
            let deadline = if got_event {
                deadline_after(s.clock.now(), s.timeouts.llm_idle)
            } else {
                first_deadline
            };
            let event = match self.next_event(&mut stream, deadline).await {
                Next::Event(event) => event,
                Next::Timeout => {
                    bump_llm(if got_event {
                        &s.count().idle_timeouts
                    } else {
                        &s.count().first_token_timeouts
                    });
                    self.fail().await;
                    return;
                }
                Next::Closed => {
                    bump_llm(&s.count().streams_cut_off);
                    self.fail().await;
                    return;
                }
            };
            got_event = true;
            if !first_marked
                && matches!(
                    event,
                    LlmEvent::TextDelta(_) | LlmEvent::ToolCallDelta { .. }
                )
            {
                first_marked = true;
                self.shared
                    .mark(|marks| marks.first_token_at = Some(s.clock.now()));
            }
            match event {
                LlmEvent::TextDelta(text) => {
                    let Some(piece) = trim.apply(text) else {
                        continue;
                    };
                    if assembler.push(Delta::Text(Arc::clone(&piece))).is_err() {
                        bump_llm(&s.count().assemble_errors);
                        self.fail().await;
                        return;
                    }
                    self.count_bytes(piece.len());
                    if !self.send(SpecItem::Text(piece)).await {
                        return;
                    }
                }
                LlmEvent::ToolCallDelta { ref arguments, .. } => {
                    self.count_bytes(arguments.len());
                    if let Some(delta) = delta_of(&event)
                        && assembler.push(delta).is_err()
                    {
                        bump_llm(&s.count().assemble_errors);
                        self.fail().await;
                        return;
                    }
                }
                LlmEvent::Done {
                    usage,
                    provider_data,
                    ..
                } => {
                    if let Some(usage) = usage {
                        self.shared
                            .input_tokens
                            .store(u64::from(usage.input_tokens), Ordering::Relaxed);
                        self.shared
                            .output_tokens
                            .store(u64::from(usage.output_tokens), Ordering::Relaxed);
                    }
                    if let Some(delta) = provider_data_of(s, provider_data.as_deref()).await
                        && assembler.push(delta).is_err()
                    {
                        bump_llm(&s.count().assemble_errors);
                    }
                    self.complete(assembler).await;
                    return;
                }
                LlmEvent::Error(error) => {
                    tracing::warn!(target: "brisvox::engine", %error, "speculative llm stream failed");
                    bump_llm(&s.count().stream_errors);
                    self.fail().await;
                    return;
                }
            }
        }
    }

    fn count_bytes(&self, n: usize) {
        self.shared
            .output_bytes
            .fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// `Done`: finish the response, run the speculative tool phase, hand everything over.
    async fn complete(&self, assembler: ResponseAssembler) {
        let s = &self.s;
        self.shared
            .mark(|marks| marks.done_at = Some(s.clock.now()));
        let Some(response) = finish_response(s, self.epoch, assembler).await else {
            self.send(SpecItem::FinishFailed).await;
            return;
        };
        let message = response.assistant_message();
        let (_, calls, _) = response.into_parts();
        let had_calls = !calls.is_empty();
        let mut done = SpecDone {
            message,
            had_calls,
            calls: Vec::new(),
            speculation: None,
            filler: None,
        };
        if !had_calls {
            self.send(SpecItem::Done(Box::new(done))).await;
            return;
        }
        match &s.tools {
            // A flow screens the calls against the node and may enter another one; the runner
            // never starts a speculation with a flow, this arm is only a guard.
            Some(tools) if s.flow.is_none() => {
                done.filler = s.filler.plan(&tools.registry, &calls);
                if !self.send(SpecItem::ToolsStarted).await {
                    return;
                }
                let speculation = tools.engine.speculate(calls, self.epoch, &self.token).await;
                bump(&self.counters.tool_phases);
                add(
                    &self.counters.held_side_effects,
                    u64::try_from(speculation.held_calls().count()).unwrap_or(u64::MAX),
                );
                done.speculation =
                    Some(HeldSpeculation::new(Arc::clone(&tools.engine), speculation));
            }
            _ => done.calls = calls,
        }
        self.send(SpecItem::Done(Box::new(done))).await;
    }
}

/// The tools of a speculative request: all registered ones (a runner with a flow never
/// speculates).
fn tool_specs<P: LlmProvider>(s: &Shared<P>) -> Vec<ToolSpec> {
    s.tools
        .as_ref()
        .map(|tools| tools.specs.values().cloned().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_age_limit_and_the_claim_are_one_atomic_decision() {
        // Claimed first: the helper's expiry no longer applies, the claim is idempotent.
        let shared = SpecShared::default();
        assert!(shared.claim());
        assert!(!shared.expire());
        assert!(shared.claim());
        // Expired first: the claim fails, so the turn never adopts a helper that has ended.
        let shared = SpecShared::default();
        assert!(shared.expire());
        assert!(!shared.claim());
        assert!(!shared.claim());
    }

    #[test]
    fn the_fingerprint_sees_a_shortened_message_and_an_appended_note() {
        let mut a = Context::new();
        a.push(Message::user("hello"));
        a.push(Message::assistant("Part and the rest"));
        let mut b = Context::new();
        b.push(Message::user("hello"));
        b.push(Message::assistant("Part and the rest"));
        assert_eq!(fingerprint(&a), fingerprint(&b));
        // Same number of messages, one shortened in place (what a truncation does).
        let mut c = Context::new();
        c.push(Message::user("hello"));
        c.push(Message::assistant("Part"));
        assert_eq!(a.len(), c.len());
        assert_ne!(fingerprint(&a), fingerprint(&c));
        // A note appended changes it too.
        b.push(Message::system("late note"));
        assert_ne!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn the_fingerprint_costs_the_messages_not_the_bytes() {
        // A big tool result: the fingerprint must not read its bytes (that would take far longer
        // than the message count suggests). Checked on the relation, not on a stopwatch: two
        // texts of equal length and equal tail hash equal whatever their middle is.
        let mut a = Context::new();
        a.push(Message::user("hello"));
        a.push(Message::assistant(format!(
            "{}0123456789abcdef",
            "x".repeat(1 << 20)
        )));
        let mut b = Context::new();
        b.push(Message::user("hello"));
        b.push(Message::assistant(format!(
            "{}0123456789abcdef",
            "y".repeat(1 << 20)
        )));
        assert_eq!(
            fingerprint(&a),
            fingerprint(&b),
            "the middle of a text is not read"
        );
        // The length and the tail are.
        let mut c = Context::new();
        c.push(Message::user("hello"));
        c.push(Message::assistant(format!(
            "{}0123456789abcdef",
            "x".repeat((1 << 20) - 1)
        )));
        assert_ne!(fingerprint(&a), fingerprint(&c));
        let mut d = Context::new();
        d.push(Message::user("hello"));
        d.push(Message::assistant(format!(
            "{}0123456789abcdeg",
            "x".repeat(1 << 20)
        )));
        assert_ne!(fingerprint(&a), fingerprint(&d));
    }

    #[test]
    fn the_leading_whitespace_of_a_response_is_stripped_once() {
        let mut trim = LeadTrim::default();
        assert_eq!(trim.apply(Arc::from("  ")), None);
        assert_eq!(trim.apply(Arc::from("\n ")), None);
        assert_eq!(trim.apply(Arc::from("  Hi")).as_deref(), Some("Hi"));
        // After the start nothing is touched, whitespace-only deltas included.
        assert_eq!(trim.apply(Arc::from(" there")).as_deref(), Some(" there"));
        assert_eq!(trim.apply(Arc::from(" ")).as_deref(), Some(" "));
        // A first delta without leading whitespace is passed on as it is.
        let mut trim = LeadTrim::default();
        let text: Arc<str> = Arc::from("Hello");
        let piece = trim.apply(Arc::clone(&text)).unwrap();
        assert!(
            Arc::ptr_eq(&piece, &text),
            "no copy when nothing is stripped"
        );
    }
}
