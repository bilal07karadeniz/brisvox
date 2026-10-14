//! The round task of one user turn (`docs/PIPELINE.md` section 4). The design is in the
//! [`crate::llm`] module docs; this file is its implementation.

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use brisvox_agent::assemble::{Assembled, Delta, Response, ResponseAssembler};
use brisvox_agent::context::{Context, Message, ToolCall, ToolCallId};
use brisvox_agent::flows::{Entered, FlowManager};
use brisvox_agent::tools::{ClipKey, Execution, FillerPlan, Speculation, ToolEngine};
use brisvox_core::clock::deadline_after;
use brisvox_core::frame::{ControlKind, DataFrame, Frame, TextFrame};
use brisvox_core::{FrameSender, TurnEpoch};
use brisvox_providers::{LlmEvent, LlmProvider, LlmRequest, LlmStream, ToolSpec};
use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::convert::{delta_of, provider_data_delta};
use crate::guard::{Delivery, GuardedSender};
use crate::llm::{LiveRound, MAX_PROVIDER_DATA_BYTES, Shared, SpokenFiller, bump, control, lock};
use crate::llm_spec::{LeadTrim, SpecDone, SpecHandoff, SpecItem, SpecShared};
use crate::playout::RoundKey;

/// How long the filler timer sleeps when no tool of the round asks for a filler (never fires: the
/// select branch is disabled).
const NO_FILLER: Duration = Duration::from_secs(3600);

/// When a line the runner speaks itself goes into the context.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Placement {
    /// At once (the fallback line: no call is open).
    Now,
    /// Once no tool call is open (a filler, spoken while the tools run): before the next
    /// assistant message, or when the turn is settled. With the key of the filler's
    /// pre-rendered clip when the policy's cache has it: the round is cued in the
    /// [`PlayoutBook`](crate::PlayoutBook) and the TTS stage plays the clip.
    AfterTools(Option<ClipKey>),
}

/// What the turn does after a round.
enum Next {
    /// Another LLM round (the tool results are in the context).
    Another,
    /// The turn is over: send `LlmResponseEnded`.
    Done,
    /// Stop without a word: the turn was cancelled or the pipeline is gone.
    Stop,
}

/// What a tool phase leads to.
enum After {
    Continue,
    Fallback,
    End,
    Stop,
}

/// One provider attempt of a round.
enum Attempt {
    Complete(ResponseAssembler),
    /// Failed; `emitted` says whether text of this response went downstream already.
    Failed {
        emitted: bool,
    },
    Cancelled,
}

enum Streamed {
    Complete(ResponseAssembler),
    Failed,
    Cancelled,
}

enum Ev {
    Event(LlmEvent),
    Closed,
    Timeout,
    Cancelled,
}

/// Which timeline marks of a speculative round are written already.
#[derive(Debug, Default)]
struct SpecMarked {
    first: bool,
    done: bool,
}

/// How following a committed speculation ended.
enum Followed {
    Complete(Box<SpecDone>),
    /// The helper received the whole response but could not finish it.
    FinishFailed,
    /// The helper's stream failed; `emitted` says whether text went downstream already.
    Failed {
        emitted: bool,
    },
    Cancelled,
}

/// The state of one turn's round task.
pub(crate) struct Turn<P: LlmProvider> {
    s: Arc<Shared<P>>,
    out: GuardedSender,
    upstream: Option<FrameSender>,
    epoch: TurnEpoch,
    token: CancellationToken,
    next_ordinal: u32,
    tool_rounds: u32,
    /// Some text of the turn went downstream (a filler or fallback counts).
    spoke: bool,
    /// Text was sent since the last `Flush`.
    unflushed: bool,
    /// The committed speculative first round, consumed by the first round.
    spec: Option<SpecHandoff>,
    /// A bot line: the turn speaks it instead of asking the LLM.
    line: Option<Arc<str>>,
}

/// A context that holds `used` tool calls of `epoch`, so the assembler mints the next ids after
/// them without being given the real (large, shared) context.
fn proxy_context(epoch: TurnEpoch, used: u32) -> Context {
    let mut context = Context::new();
    if used > 0 {
        let calls = (0..used)
            .map(|seq| ToolCall {
                id: ToolCallId::new(epoch, seq),
                provider_id: None,
                name: Arc::from("-"),
                arguments: Value::Null,
                provider_data: None,
            })
            .collect();
        context.push(Message::assistant_with_calls("", calls));
    }
    context
}

/// The sequence number of a tool call id (`t<epoch>.<seq>`).
fn seq_of(id: &ToolCallId) -> u32 {
    id.as_str()
        .rsplit_once('.')
        .and_then(|(_, seq)| seq.parse().ok())
        .unwrap_or(0)
}

fn tool_name(call: &Assembled) -> Value {
    match call {
        Assembled::Complete(call) => Value::String(call.name.to_string()),
        Assembled::Invalid(call) => call
            .name
            .as_ref()
            .map_or(Value::Null, |name| Value::String(name.to_string())),
    }
}

/// An error result in the shape the tool engine uses: `{"error": kind, "tool": name, "detail": text}`.
fn error_result(call: &Assembled, kind: &str, detail: &str) -> Message {
    Message::tool_error(
        call.id().clone(),
        json!({"error": kind, "tool": tool_name(call), "detail": detail}).to_string(),
    )
}

/// The assembler delta of a `Done.provider_data`: dropped (counted) when it is larger than
/// the cap or not JSON; parsed on the offload when it is as large as an offloaded response
/// (`inline_finish_bytes`), because parsing is linear in its size.
///
/// The cap is [`MAX_PROVIDER_DATA_BYTES`], and at most half of a message's allowance in the
/// window (`window.max_message_tokens`, 2 bytes per token): the blob is echoed in every later
/// request and counts against the budget, and a message that is over its allowance even
/// without text cannot be clipped to a valid message.
pub(crate) async fn provider_data_of<P: LlmProvider>(
    s: &Shared<P>,
    text: Option<&str>,
) -> Option<Delta> {
    let text = text?;
    let cap = MAX_PROVIDER_DATA_BYTES.min(s.llm.window.max_message_tokens.saturating_mul(2));
    if text.len() > cap {
        tracing::warn!(target: "brisvox::engine", bytes = text.len(), "dropping oversized provider data");
        bump(&s.count().provider_data_oversized);
        return None;
    }
    let parsed = if text.len() < s.llm.inline_finish_bytes {
        provider_data_delta(Some(text))
    } else {
        bump(&s.count().provider_data_offloaded);
        let owned = text.to_owned();
        match s
            .offload
            .run(s.timeouts.analysis_job, move || {
                provider_data_delta(Some(&owned))
            })
            .await
        {
            Ok(done) => done.value,
            Err(error) => {
                tracing::warn!(target: "brisvox::engine", %error, "dropping provider data (offload failed)");
                bump(&s.count().provider_data_invalid);
                return None;
            }
        }
    };
    match parsed {
        Ok(delta) => delta,
        Err(error) => {
            tracing::warn!(target: "brisvox::engine", %error, "dropping provider data");
            bump(&s.count().provider_data_invalid);
            None
        }
    }
}

/// Parses the tool calls of the response: inline when it is small, on the offload when large.
pub(crate) async fn finish_response<P: LlmProvider>(
    s: &Shared<P>,
    epoch: TurnEpoch,
    assembler: ResponseAssembler,
) -> Option<Response> {
    let used = used_calls_of(s, epoch).await;
    let proxy = proxy_context(epoch, used);
    if assembler.buffered_bytes() < s.llm.inline_finish_bytes {
        bump(&s.count().finishes_inline);
        return Some(assembler.finish(epoch, &proxy));
    }
    bump(&s.count().finishes_offloaded);
    match s
        .offload
        .run(s.timeouts.analysis_job, move || {
            assembler.finish(epoch, &proxy)
        })
        .await
    {
        Ok(done) => Some(done.value),
        Err(error) => {
            tracing::warn!(target: "brisvox::engine", %error, "response finish failed");
            bump(&s.count().finish_failures);
            None
        }
    }
}

/// Tool calls of this epoch the next minted id must come after. With a flow the manager knows
/// about calls a reset removed from the context (its ids must not repeat, or the engine's
/// idempotency key would mistake a new call for an old one).
async fn used_calls_of<P: LlmProvider>(s: &Shared<P>, epoch: TurnEpoch) -> u32 {
    match &s.flow {
        Some(flow) => {
            let flow = flow.lock().await;
            let context = lock(&s.context);
            flow.next_tool_call_ids(&context, epoch, 1)
                .first()
                .map_or(0, seq_of)
        }
        None => seq_of(&lock(&s.context).next_tool_call_id(epoch)),
    }
}

/// A copy of the context, made on the offload when the context is heavy (the copy is deep:
/// tool call arguments and provider data are JSON values).
pub(crate) async fn copy_context_of<P: LlmProvider>(s: &Shared<P>) -> Context {
    if crate::request::is_heavy(&lock(&s.context)) {
        let context = Arc::clone(&s.context);
        match s
            .offload
            .run(s.timeouts.analysis_job, move || lock(&context).clone())
            .await
        {
            Ok(done) => return done.value,
            Err(error) => {
                // Not on the worker: the copy is heavy, and a timed-out pool job may still hold
                // the lock. A blocking thread can wait for it.
                tracing::warn!(target: "brisvox::engine", %error, "offloaded context copy failed; copying on a blocking thread");
                let context = Arc::clone(&s.context);
                if let Ok(copy) = tokio::task::spawn_blocking(move || lock(&context).clone()).await
                {
                    return copy;
                }
            }
        }
    }
    lock(&s.context).clone()
}

impl<P: LlmProvider> Turn<P> {
    pub(crate) fn new(
        s: Arc<Shared<P>>,
        out: GuardedSender,
        upstream: Option<FrameSender>,
        epoch: TurnEpoch,
        token: CancellationToken,
    ) -> Self {
        Self {
            s,
            out,
            upstream,
            epoch,
            token,
            next_ordinal: 0,
            tool_rounds: 0,
            spoke: false,
            unflushed: false,
            spec: None,
            line: None,
        }
    }

    /// The turn speaks `text` verbatim as its response, without an LLM request (a bot line).
    #[must_use]
    pub(crate) fn with_line(mut self, text: Arc<str>) -> Self {
        self.line = Some(text);
        self
    }

    /// The first round of this turn is the committed speculation `handoff` instead of a new LLM
    /// request.
    #[must_use]
    pub(crate) fn with_speculation(mut self, handoff: SpecHandoff) -> Self {
        self.spec = Some(handoff);
        self
    }

    /// The task body of a turn: [`Self::run`], with a panic contained. A panic anywhere in the
    /// rounds (a provider adapter, a converter) must not leave the pipeline waiting for a
    /// response end that never comes: the turn is closed like a failed one and the session goes
    /// on (counted as `round_panics`).
    pub(crate) async fn run_contained(mut self) {
        let outcome = CatchUnwind::new(self.run()).await;
        if let Err(payload) = outcome {
            // A payload whose own drop panics must not take the task down either.
            let _ = catch_unwind(AssertUnwindSafe(move || drop(payload)));
            self.recover_from_panic().await;
        }
    }

    /// Closes a turn whose task panicked: the partial text and the open calls are settled (this
    /// task is the only mutator of the context while it lives), and the turn ends.
    async fn recover_from_panic(&mut self) {
        let s = Arc::clone(&self.s);
        bump(&s.count().round_panics);
        tracing::error!(target: "brisvox::engine", epoch = ?self.epoch, "round task panicked; closing the turn");
        s.settle_live();
        s.close_open_calls();
        s.place_filler();
        s.validate(&lock(&s.context), "panic recovery");
        self.send_end().await;
    }

    /// The task body: rounds until the model stops asking for tools, then the end of the turn.
    /// The summary is **not** awaited here: a request never waits for it (see
    /// [`crate::request`]).
    async fn run(&mut self) {
        if let Some(text) = self.line.take() {
            if self.say(text, Placement::Now, true).await {
                self.end_turn().await;
            }
            return;
        }
        self.enter_pending_transition().await;
        loop {
            match self.round().await {
                Next::Another => {}
                Next::Done => break,
                Next::Stop => return,
            }
        }
        self.end_turn().await;
    }

    /// Sends `frame` downstream. `false` when the epoch is over or the pipeline is gone.
    async fn emit(&self, frame: Frame) -> bool {
        matches!(self.out.send(frame).await, Ok(Delivery::Sent))
    }

    fn next_key(&mut self) -> RoundKey {
        let key = RoundKey::new(self.epoch, self.next_ordinal);
        self.next_ordinal = self.next_ordinal.saturating_add(1);
        key
    }

    // -- rounds -------------------------------------------------------------------------------

    async fn round(&mut self) -> Next {
        let s = Arc::clone(&self.s);
        let key = self.next_key();
        // Registered before the frame goes out: the TTS stage may look the round up as soon as it
        // sees `LlmResponseStarted`.
        if let Some(book) = &s.book {
            book.register_round(key);
        }
        if !self
            .emit(control(self.epoch, ControlKind::LlmResponseStarted))
            .await
        {
            return Next::Stop;
        }
        if let Some(handoff) = self.spec.take() {
            return self.round_speculated(key, handoff).await;
        }
        let tools = self.round_tools().await;
        let built = s.builder.build_offloaded(&s.context, &tools).await;
        let built = match built {
            Ok(built) => built,
            Err(error) => {
                bump(&s.count().request_build_errors);
                tracing::error!(target: "brisvox::engine", %error, "cannot build the LLM request");
                return Next::Done;
            }
        };
        bump(&s.count().rounds);
        lock(&s.inner).live = Some(LiveRound {
            key,
            text: String::new(),
        });
        let timeline_round = s.timeline.llm_request(self.epoch, s.clock.now());
        let streamed = self.stream(timeline_round, built.request).await;
        self.after_stream(key, timeline_round, streamed).await
    }

    /// What follows a response stream: the end of the round, or its completion.
    async fn after_stream(
        &mut self,
        key: RoundKey,
        timeline_round: Option<usize>,
        streamed: Streamed,
    ) -> Next {
        match streamed {
            Streamed::Cancelled => Next::Stop,
            Streamed::Failed => {
                // Keep what was said; the next turn recovers.
                self.s.settle_live();
                Next::Done
            }
            Streamed::Complete(assembler) => self.complete(key, timeline_round, assembler).await,
        }
    }

    /// The tools of the next request: the flow's current node, or all registered tools.
    async fn round_tools(&self) -> Vec<ToolSpec> {
        let Some(tools) = &self.s.tools else {
            return Vec::new();
        };
        let offered = lock(&self.s.offered).clone();
        let shown = |name: &Arc<str>| offered.as_ref().is_none_or(|set| set.contains(name));
        match &self.s.flow {
            Some(flow) => {
                let flow = flow.lock().await;
                flow.tool_specs()
                    .into_iter()
                    .filter(|spec| shown(&spec.name))
                    .filter_map(|spec| tools.specs.get(&spec.name).cloned())
                    .collect()
            }
            None => tools
                .specs
                .iter()
                .filter(|(name, _)| shown(name))
                .map(|(_, spec)| spec.clone())
                .collect(),
        }
    }

    /// One response, with at most `retries` retries while nothing of it was spoken.
    async fn stream(&mut self, timeline_round: Option<usize>, request: LlmRequest) -> Streamed {
        self.stream_after(timeline_round, request, 0).await
    }

    /// [`Self::stream`] when `retries` of the budget are spent already (a failed speculative
    /// attempt counts as the first attempt).
    async fn stream_after(
        &mut self,
        timeline_round: Option<usize>,
        request: LlmRequest,
        mut retries: u32,
    ) -> Streamed {
        let s = Arc::clone(&self.s);
        loop {
            match self.attempt(timeline_round, &request).await {
                Attempt::Complete(assembler) => return Streamed::Complete(assembler),
                Attempt::Cancelled => return Streamed::Cancelled,
                Attempt::Failed { emitted } => {
                    if !emitted && retries < s.llm.retries {
                        retries += 1;
                        bump(&s.count().retries);
                        tokio::select! {
                            biased;
                            () = self.token.cancelled() => return Streamed::Cancelled,
                            () = s.clock.sleep(s.llm.retry_delay) => {}
                        }
                        continue;
                    }
                    bump(&s.count().failed_rounds);
                    return Streamed::Failed;
                }
            }
        }
    }

    async fn next_event(&self, stream: &mut LlmStream, deadline: Instant) -> Ev {
        let sleep = self.s.clock.sleep_until(deadline);
        tokio::pin!(sleep);
        tokio::select! {
            biased;
            () = self.token.cancelled() => Ev::Cancelled,
            () = &mut sleep => Ev::Timeout,
            event = stream.next() => match event {
                Some(event) => Ev::Event(event),
                None => Ev::Closed,
            },
        }
    }

    /// One request and its stream, see the module docs of [`crate::llm`].
    async fn attempt(&mut self, timeline_round: Option<usize>, request: &LlmRequest) -> Attempt {
        let s = Arc::clone(&self.s);
        let first_deadline = deadline_after(s.clock.now(), s.timeouts.llm_first_token);
        bump(&s.count().requests);
        let sleep = s.clock.sleep_until(first_deadline);
        tokio::pin!(sleep);
        let opened = tokio::select! {
            biased;
            () = self.token.cancelled() => return Attempt::Cancelled,
            () = &mut sleep => {
                bump(&s.count().first_token_timeouts);
                return Attempt::Failed { emitted: false };
            }
            opened = s.provider.stream(request.clone()) => opened,
        };
        let mut stream = match opened {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(target: "brisvox::engine", %error, "llm request failed");
                bump(&s.count().request_errors);
                return Attempt::Failed { emitted: false };
            }
        };
        let mut assembler = ResponseAssembler::new();
        let mut emitted = false;
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
                Ev::Event(event) => event,
                Ev::Cancelled => return Attempt::Cancelled,
                Ev::Timeout => {
                    let counter = if got_event {
                        &s.count().idle_timeouts
                    } else {
                        &s.count().first_token_timeouts
                    };
                    bump(counter);
                    return Attempt::Failed { emitted };
                }
                Ev::Closed => {
                    bump(&s.count().streams_cut_off);
                    return Attempt::Failed { emitted };
                }
            };
            got_event = true;
            // The first text or tool-call fragment is the first token (an empty response, an
            // error or a bare `Done` is not).
            if !first_marked
                && matches!(
                    event,
                    LlmEvent::TextDelta(_) | LlmEvent::ToolCallDelta { .. }
                )
            {
                first_marked = true;
                if let Some(round) = timeline_round {
                    s.timeline.llm_first_token(self.epoch, round, s.clock.now());
                }
            }
            match event {
                LlmEvent::TextDelta(text) => {
                    // The leading whitespace of the response is not spoken.
                    let Some(piece) = trim.apply(text) else {
                        continue;
                    };
                    if assembler.push(Delta::Text(Arc::clone(&piece))).is_err() {
                        bump(&s.count().assemble_errors);
                        return Attempt::Failed { emitted };
                    }
                    // The raw text so far is a superset of what was sent: record it first.
                    if let Some(live) = lock(&s.inner).live.as_mut() {
                        live.text.push_str(&piece);
                    }
                    let frame = DataFrame::LlmText(TextFrame {
                        epoch: self.epoch,
                        text: piece,
                    });
                    if !self.emit(frame.into()).await {
                        return Attempt::Cancelled;
                    }
                    emitted = true;
                    self.spoke = true;
                    self.unflushed = true;
                }
                LlmEvent::ToolCallDelta { .. } => {
                    if let Some(delta) = delta_of(&event)
                        && assembler.push(delta).is_err()
                    {
                        bump(&s.count().assemble_errors);
                        return Attempt::Failed { emitted };
                    }
                }
                LlmEvent::Done { provider_data, .. } => {
                    if let Some(delta) = provider_data_of(&self.s, provider_data.as_deref()).await
                        && assembler.push(delta).is_err()
                    {
                        bump(&s.count().assemble_errors);
                    }
                    return Attempt::Complete(assembler);
                }
                LlmEvent::Error(error) => {
                    tracing::warn!(target: "brisvox::engine", %error, "llm stream failed");
                    bump(&s.count().stream_errors);
                    return Attempt::Failed { emitted };
                }
            }
        }
    }

    /// The response is complete: finish it, push the assistant message once, then end the turn
    /// or run the tools.
    async fn complete(
        &mut self,
        key: RoundKey,
        timeline_round: Option<usize>,
        assembler: ResponseAssembler,
    ) -> Next {
        let s = Arc::clone(&self.s);
        if let Some(round) = timeline_round {
            s.timeline.llm_done(self.epoch, round, s.clock.now());
        }
        let Some(response) = finish_response(&s, self.epoch, assembler).await else {
            s.settle_live();
            return Next::Done;
        };
        if self.token.is_cancelled() {
            // Halted while finishing: the runner owns the context from here on.
            return Next::Stop;
        }
        self.push_response(key, &response);
        let (_, calls, _) = response.into_parts();
        if calls.is_empty() {
            return Next::Done;
        }
        self.tools(calls).await
    }

    /// Pushes the assistant message of the round to the context, once, and links it to the round.
    fn push_response(&self, key: RoundKey, response: &Response) {
        self.push_message(key, response.assistant_message());
    }

    /// [`Self::push_response`] for an assistant message made already (`None`: empty response).
    fn push_message(&self, key: RoundKey, message: Option<Message>) {
        let s = &self.s;
        let mut context = lock(&s.context);
        let mut inner = lock(&s.inner);
        inner.live = None;
        if let Some(message) = message {
            // A filler spoken during the tool round before this response precedes it.
            s.place_filler_in(&mut context, &mut inner);
            let index = context.len();
            context.push(message);
            s.record_round(&mut inner, key, index);
        }
    }

    // -- speculation -------------------------------------------------------------------

    /// The first round of a committed speculation: the helper has been (and may still be)
    /// streaming the response; its buffered pieces go downstream now, then the live ones, and
    /// the round completes like any other. The request and the stream belong to the helper.
    async fn round_speculated(&mut self, key: RoundKey, mut handoff: SpecHandoff) -> Next {
        let s = Arc::clone(&self.s);
        bump(&s.count().rounds);
        lock(&s.inner).live = Some(LiveRound {
            key,
            text: String::new(),
        });
        // The timeline gets the instants things really happened at: the request was sent when
        // the speculation started, so the turn's post-endpoint latency shows the gain.
        let requested_at = handoff.shared.marks().requested_at;
        let timeline_round = s
            .timeline
            .llm_request(self.epoch, requested_at.unwrap_or_else(|| s.clock.now()));
        let followed = self.follow_speculation(&mut handoff, timeline_round).await;
        match followed {
            Followed::Cancelled => Next::Stop,
            Followed::Complete(done) => self.complete_speculated(key, *done).await,
            Followed::FinishFailed => {
                s.settle_live();
                Next::Done
            }
            Followed::Failed { emitted } => {
                // The speculative stream was the first attempt of the round.
                if emitted || s.llm.retries == 0 {
                    bump(&s.count().failed_rounds);
                    s.settle_live();
                    return Next::Done;
                }
                bump(&s.count().retries);
                let tools = self.round_tools().await;
                let built = match s.builder.build_offloaded(&s.context, &tools).await {
                    Ok(built) => built,
                    Err(error) => {
                        bump(&s.count().request_build_errors);
                        tracing::error!(target: "brisvox::engine", %error, "cannot build the LLM request");
                        return Next::Done;
                    }
                };
                tokio::select! {
                    biased;
                    () = self.token.cancelled() => return Next::Stop,
                    () = s.clock.sleep(s.llm.retry_delay) => {}
                }
                let streamed = self.stream_after(timeline_round, built.request, 1).await;
                self.after_stream(key, timeline_round, streamed).await
            }
        }
    }

    /// Releases the pieces the helper has buffered and follows the rest of its response, with
    /// the first-token and idle timeouts of a round (the clock starts at the commit; the helper
    /// has its own limits too). While the helper runs the ReadOnly tools of the response no idle
    /// timeout applies: the tools have theirs.
    async fn follow_speculation(
        &mut self,
        handoff: &mut SpecHandoff,
        timeline_round: Option<usize>,
    ) -> Followed {
        let s = Arc::clone(&self.s);
        // The timeline marks are written as the events arrive (not after the whole response), at
        // the instants the helper saw them, so a barge-in mid-response leaves them in place.
        let mut marked = SpecMarked::default();
        let first_deadline = deadline_after(s.clock.now(), s.timeouts.llm_first_token);
        let mut got_item = false;
        let mut emitted = false;
        let mut tools_phase = false;
        loop {
            let limit = if tools_phase {
                None
            } else if got_item {
                Some(deadline_after(s.clock.now(), s.timeouts.llm_idle))
            } else {
                Some(first_deadline)
            };
            let sleep = s
                .clock
                .sleep_until(limit.unwrap_or_else(|| deadline_after(s.clock.now(), NO_FILLER)));
            tokio::pin!(sleep);
            let item = tokio::select! {
                biased;
                () = self.token.cancelled() => return Followed::Cancelled,
                () = &mut sleep, if limit.is_some() => {
                    bump(if got_item {
                        &s.count().idle_timeouts
                    } else {
                        &s.count().first_token_timeouts
                    });
                    return Followed::Failed { emitted };
                }
                item = handoff.rx.recv() => item,
            };
            got_item = true;
            match item {
                Some(SpecItem::Text(piece)) => {
                    // The raw text so far is a superset of what was sent: record it first.
                    if let Some(live) = lock(&s.inner).live.as_mut() {
                        live.text.push_str(&piece);
                    }
                    self.mark_spec(&handoff.shared, timeline_round, &mut marked, false);
                    let frame = DataFrame::LlmText(TextFrame {
                        epoch: self.epoch,
                        text: piece,
                    });
                    if !self.emit(frame.into()).await {
                        return Followed::Cancelled;
                    }
                    emitted = true;
                    self.spoke = true;
                    self.unflushed = true;
                }
                Some(SpecItem::ToolsStarted) => {
                    self.mark_spec(&handoff.shared, timeline_round, &mut marked, true);
                    tools_phase = true;
                }
                Some(SpecItem::Done(done)) => {
                    self.mark_spec(&handoff.shared, timeline_round, &mut marked, true);
                    return Followed::Complete(done);
                }
                Some(SpecItem::FinishFailed) => {
                    self.mark_spec(&handoff.shared, timeline_round, &mut marked, true);
                    return Followed::FinishFailed;
                }
                Some(SpecItem::Failed) => return Followed::Failed { emitted },
                None => {
                    bump(&s.count().streams_cut_off);
                    return Followed::Failed { emitted };
                }
            }
        }
    }

    /// Writes the timeline marks of the speculative round as far as the helper has reached them:
    /// the first token once, the completion (`done`) once.
    fn mark_spec(
        &self,
        shared: &SpecShared,
        round: Option<usize>,
        marked: &mut SpecMarked,
        done: bool,
    ) {
        let Some(round) = round else {
            return;
        };
        let marks = shared.marks();
        if !marked.first
            && let Some(at) = marks.first_token_at
        {
            marked.first = true;
            self.s.timeline.llm_first_token(self.epoch, round, at);
        }
        if done
            && !marked.done
            && let Some(at) = marks.done_at
        {
            marked.done = true;
            self.s.timeline.llm_done(self.epoch, round, at);
        }
    }

    /// The speculative response is complete: push the assistant message once, then end the turn
    /// or commit the speculative tool phase.
    async fn complete_speculated(&mut self, key: RoundKey, done: SpecDone) -> Next {
        if self.token.is_cancelled() {
            // Halted meanwhile: the runner owns the context from here on.
            return Next::Stop;
        }
        let SpecDone {
            message,
            had_calls,
            calls,
            speculation,
            filler,
        } = done;
        self.push_message(key, message);
        if !had_calls {
            return Next::Done;
        }
        match speculation {
            Some(held) => match held.into_committed() {
                Some(speculation) => self.tools_speculated(speculation, filler).await,
                None => Next::Done,
            },
            None => self.tools(calls).await,
        }
    }

    /// The tool phase of a committed speculation: the ReadOnly calls have run, `commit` runs the
    /// held SideEffect calls (once) and returns every result in issue order.
    async fn tools_speculated(
        &mut self,
        speculation: Speculation,
        filler: Option<FillerPlan>,
    ) -> Next {
        let s = Arc::clone(&self.s);
        self.tool_rounds = self.tool_rounds.saturating_add(1);
        bump(&s.count().tool_rounds);
        let calls = speculation.held_calls().count() + speculation.answered().count();
        s.count()
            .tool_calls
            .fetch_add(u64::try_from(calls).unwrap_or(u64::MAX), Ordering::Relaxed);
        // The text before the tool is spoken while the held calls run.
        if self.unflushed {
            if !self.emit(control(self.epoch, ControlKind::Flush)).await {
                return Next::Stop;
            }
            self.unflushed = false;
        }
        let Some(tools) = &s.tools else {
            // A speculation exists only with a tool engine; keep the context valid regardless.
            s.close_open_calls();
            return Next::Done;
        };
        let token = self.token.clone();
        let committed = tools.engine.commit(speculation, &token);
        let Some(mut execution) = self.race_filler(committed, filler).await else {
            return Next::Stop;
        };
        if self.token.is_cancelled() {
            return Next::Stop;
        }
        {
            let mut context = lock(&s.context);
            execution.append_to(&mut context);
            s.validate(&context, "tool results");
        }
        if execution.budget_exhausted {
            bump(&s.count().tool_budget_exhausted);
            self.fallback_and_done().await
        } else {
            self.drain_late_notes();
            Next::Another
        }
    }

    // -- tools --------------------------------------------------------------------------------

    async fn tools(&mut self, calls: Vec<Assembled>) -> Next {
        let s = Arc::clone(&self.s);
        if self.tool_rounds >= s.llm.max_tool_rounds {
            bump(&s.count().tool_round_limit_hits);
            self.answer_without_running(
                &calls,
                "tool_round_limit",
                "the limit of tool rounds for this turn is reached, so the tools did not run; answer the user without them",
            );
            return self.fallback_and_done().await;
        }
        self.tool_rounds = self.tool_rounds.saturating_add(1);
        bump(&s.count().tool_rounds);
        s.count().tool_calls.fetch_add(
            u64::try_from(calls.len()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        // The text before the tool is spoken while the tool runs.
        if self.unflushed {
            if !self.emit(control(self.epoch, ControlKind::Flush)).await {
                return Next::Stop;
            }
            self.unflushed = false;
        }
        match self.execute(calls).await {
            After::Stop => Next::Stop,
            After::Fallback => self.fallback_and_done().await,
            After::End => Next::Done,
            After::Continue => {
                self.drain_late_notes();
                Next::Another
            }
        }
    }

    async fn fallback_and_done(&mut self) -> Next {
        bump(&self.s.count().fallbacks);
        let text = Arc::clone(&self.s.fallback);
        if self.say(text, Placement::Now, false).await {
            Next::Done
        } else {
            Next::Stop
        }
    }

    /// Answers the calls of tools an application stopped offering
    /// ([`ConversationEdit::SetTools`](crate::agent_input::ConversationEdit::SetTools)) with an
    /// `unknown_tool` error result, without running them; returns the others.
    fn screen_hidden(&self, calls: Vec<Assembled>) -> Vec<Assembled> {
        let Some(offered) = lock(&self.s.offered).clone() else {
            return calls;
        };
        let (shown, hidden): (Vec<_>, Vec<_>) = calls.into_iter().partition(|call| match call {
            Assembled::Complete(call) => offered.contains(&call.name),
            // A malformed call is the engine's to answer (it counts toward the budget).
            Assembled::Invalid(_) => true,
        });
        if !hidden.is_empty() {
            self.s.count().hidden_tool_calls.fetch_add(
                u64::try_from(hidden.len()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
            self.answer_without_running(
                &hidden,
                "unknown_tool",
                "this tool is not available now; answer without it",
            );
        }
        shown
    }

    /// Answers every call with an error result without running it, so the context stays valid.
    fn answer_without_running(&self, calls: &[Assembled], kind: &str, detail: &str) {
        if self.token.is_cancelled() {
            return;
        }
        let mut context = lock(&self.s.context);
        for call in calls {
            context.push(error_result(call, kind, detail));
        }
        self.s.validate(&context, "unanswered tool calls");
    }

    /// Runs one response's calls. See the module docs of [`crate::llm`].
    async fn execute(&mut self, calls: Vec<Assembled>) -> After {
        let s = Arc::clone(&self.s);
        let Some(tools) = &s.tools else {
            bump(&s.count().tools_unavailable);
            self.answer_without_running(
                &calls,
                "unknown_tool",
                "no tools are available in this conversation",
            );
            return After::Continue;
        };
        let calls = self.screen_hidden(calls);
        let filler = s.filler.plan(&tools.registry, &calls);
        let mut flow_guard = match &s.flow {
            Some(flow) => Some(flow.lock().await),
            None => None,
        };
        let (admitted, screened) = match flow_guard.as_deref() {
            Some(flow) => {
                let mut screened = flow.screen(calls);
                (screened.take_admitted(), Some(screened))
            }
            None => (calls, None),
        };
        let Some(mut execution) = self.run_engine(&tools.engine, admitted, filler).await else {
            return After::Stop;
        };
        if self.token.is_cancelled() {
            return After::Stop;
        }
        let mut exhausted = execution.budget_exhausted;
        let mut respond = true;
        match (flow_guard.as_deref_mut(), screened) {
            (Some(flow), Some(screened)) => {
                let applied = {
                    let mut context = lock(&s.context);
                    let applied = flow.apply_results(&mut context, screened, execution);
                    s.validate(&context, "flow tool results");
                    applied
                };
                match applied {
                    Ok(applied) => {
                        respond = applied.respond;
                        exhausted |= applied.budget_exhausted;
                        if applied.pending.is_some()
                            && let Some(entered) = self.enter_pending(flow).await
                        {
                            respond = entered.respond_immediately;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(target: "brisvox::engine", %error, "flow could not apply the tool results");
                        bump(&s.count().flow_errors);
                        s.close_open_calls();
                        respond = false;
                    }
                }
            }
            _ => {
                let mut context = lock(&s.context);
                execution.append_to(&mut context);
                s.validate(&context, "tool results");
            }
        }
        if exhausted {
            bump(&s.count().tool_budget_exhausted);
            After::Fallback
        } else if respond {
            After::Continue
        } else {
            After::End
        }
    }

    /// `ToolEngine::execute` raced against the filler timer. The filler is spoken as its own
    /// round while the tools keep running.
    async fn run_engine(
        &mut self,
        engine: &Arc<ToolEngine>,
        calls: Vec<Assembled>,
        filler: Option<FillerPlan>,
    ) -> Option<Execution> {
        let token = self.token.clone();
        let execution = engine.execute(calls, self.epoch, &token);
        self.race_filler(execution, filler).await
    }

    /// Drives `execution` (a tool phase) and, if it takes longer than the plan's delay (on the
    /// injected clock), speaks the policy's filler meanwhile. `None` when the epoch ended.
    async fn race_filler<F>(
        &mut self,
        execution: F,
        filler: Option<FillerPlan>,
    ) -> Option<Execution>
    where
        F: Future<Output = Execution> + Send,
    {
        tokio::pin!(execution);
        let mut filler_pending = filler.is_some();
        let sleep = self
            .s
            .clock
            .sleep(filler.as_ref().map_or(NO_FILLER, |plan| plan.after));
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                biased;
                execution = &mut execution => return Some(execution),
                () = &mut sleep, if filler_pending => {
                    filler_pending = false;
                    let Some(choice) = filler.as_ref().and_then(|plan| self.s.filler.choose(plan)) else {
                        continue;
                    };
                    bump(&self.s.count().fillers);
                    if choice.clip.is_some() {
                        bump(&self.s.count().filler_clips);
                    }
                    let (text, clip) = (choice.text, choice.clip);
                    // Sending the filler can wait for space downstream. The tools keep being
                    // polled meanwhile, so a back-pressured pipeline does not stall them (or
                    // their timeout accounting). A clip key cues the TTS stage to play the
                    // pre-rendered clip instead of rendering the text.
                    let say = self.say(text, Placement::AfterTools(clip), true);
                    tokio::pin!(say);
                    let mut finished = None;
                    let spoke = loop {
                        tokio::select! {
                            biased;
                            spoke = &mut say => break spoke,
                            done = &mut execution, if finished.is_none() => finished = Some(done),
                        }
                    };
                    if !spoke {
                        // A finished execution dropped here (or with this task at an abort) never
                        // reached the context: its `Delivery` reports its SideEffect results as late
                        // notes and orphans the calls, so a regeneration does not run them again.
                        return None;
                    }
                    if let Some(done) = finished {
                        return Some(done);
                    }
                }
            }
        }
    }

    /// Late SideEffect results of earlier turns, appended only while no tool call is open (a note
    /// between a call and its result would make the context invalid).
    fn drain_late_notes(&self) {
        let Some(tools) = &self.s.tools else {
            return;
        };
        if self.token.is_cancelled() {
            return;
        }
        let mut context = lock(&self.s.context);
        if !context.pending_tool_calls().is_empty() {
            return;
        }
        for note in tools.engine.take_late_notes() {
            context.push(note);
            bump(&self.s.count().late_notes);
        }
    }

    // -- flow ---------------------------------------------------------------------------------

    /// Finishes a transition the previous turn decided but did not enter (it was cancelled while
    /// the summary was made).
    async fn enter_pending_transition(&self) {
        let Some(flow) = &self.s.flow else {
            return;
        };
        let mut flow = flow.lock().await;
        if flow.pending_transition().is_some() {
            self.enter_pending(&mut flow).await;
        }
    }

    /// Enters the node the last round decided on. The flow works on a copy of the context (its
    /// summarizer awaits, and no lock is held across an await); the copy replaces the context
    /// when the transition succeeded. Only this task mutates the context while it is alive.
    async fn enter_pending(&self, flow: &mut FlowManager) -> Option<Entered> {
        let s = &self.s;
        if self.token.is_cancelled() {
            return None;
        }
        let mut snapshot = copy_context_of(s).await;
        let summarizer = s.builder.summarizer();
        match flow
            .resume_pending(&mut snapshot, summarizer.as_deref())
            .await
        {
            Ok(Some(entered)) => {
                s.validate(&snapshot, "flow transition");
                *lock(&s.context) = snapshot;
                bump(&s.count().flow_transitions);
                if entered.context_replaced {
                    // Message indices of the old history mean nothing any more.
                    let mut inner = lock(&s.inner);
                    for record in inner.records.drain(..) {
                        if let Some(book) = &s.book {
                            book.mark_applied(record.key);
                        }
                    }
                    s.builder.reset_summary();
                }
                Some(entered)
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(target: "brisvox::engine", %error, "flow transition failed");
                bump(&s.count().flow_errors);
                None
            }
        }
    }

    // -- speech that is not the model's response ----------------------------------------------

    /// Speaks `text` as a round of its own (`LlmResponseStarted`, the text, optionally `Flush`).
    /// Both kinds are part of the conversation and cut to what was heard like any other assistant
    /// text: a fallback line goes into the context at once, a filler (spoken while tool calls are
    /// open) once no call is open ([`Placement::AfterTools`]). `false` when the epoch is over.
    async fn say(&mut self, text: Arc<str>, placement: Placement, flush: bool) -> bool {
        let s = Arc::clone(&self.s);
        // Text still buffered downstream (the unrun round's text before the fallback) is its own
        // utterance: without this `Flush` it would merge with `text` and be registered under the
        // later round's ordinal.
        if self.unflushed {
            if !self.emit(control(self.epoch, ControlKind::Flush)).await {
                return false;
            }
            self.unflushed = false;
        }
        let key = self.next_key();
        if let Some(book) = &s.book {
            book.register_round(key);
            // Before `LlmResponseStarted`: the TTS stage reads the cue at the round's first text.
            if let Placement::AfterTools(Some(clip)) = &placement {
                book.cue_clip(key, clip.clone());
            }
        }
        if !self
            .emit(control(self.epoch, ControlKind::LlmResponseStarted))
            .await
        {
            return false;
        }
        if self.token.is_cancelled() {
            return false;
        }
        {
            let mut context = lock(&s.context);
            let mut inner = lock(&s.inner);
            // An earlier filler that is still waiting comes first.
            s.place_filler_in(&mut context, &mut inner);
            match placement {
                Placement::Now => {
                    let index = context.len();
                    context.push(Message::assistant(Arc::clone(&text)));
                    s.record_round(&mut inner, key, index);
                }
                Placement::AfterTools(_) => {
                    inner.filler = Some(SpokenFiller {
                        key,
                        text: Arc::clone(&text),
                    });
                }
            }
        }
        let frame = DataFrame::LlmText(TextFrame {
            epoch: self.epoch,
            text,
        });
        if !self.emit(frame.into()).await {
            return false;
        }
        self.spoke = true;
        self.unflushed = true;
        if flush {
            if !self.emit(control(self.epoch, ControlKind::Flush)).await {
                return false;
            }
            self.unflushed = false;
        }
        true
    }

    // -- the end ------------------------------------------------------------------------------

    /// `LlmResponseEnded`; when nothing was spoken in the whole turn, also a non-blocking
    /// upstream `TtsEnded` (no TTS will report the end of a response that has no text); then the
    /// summary of the dropped turns is brought up to date by a background task, off the
    /// response's critical path and not tied to this task (a quick next turn aborts this task,
    /// not the summary).
    async fn end_turn(&mut self) {
        self.send_end().await;
        self.s.spawn_summary();
    }

    /// The frames that end a turn (see [`Self::end_turn`]), and the end of the `responding` state.
    async fn send_end(&mut self) {
        let s = Arc::clone(&self.s);
        let sent = self
            .emit(control(self.epoch, ControlKind::LlmResponseEnded))
            .await;
        if sent && !self.spoke {
            bump(&s.count().silent_turns);
            if let Some(upstream) = &self.upstream {
                if upstream
                    .try_send(control(self.epoch, ControlKind::TtsEnded))
                    .is_ok()
                {
                    bump(&s.count().upstream_tts_ended);
                } else {
                    bump(&s.count().upstream_dropped);
                }
            }
        }
        lock(&s.inner).responding = false;
    }
}

/// A future that contains a panic of the future it wraps (`catch_unwind` around each poll).
struct CatchUnwind<F>(Pin<Box<F>>);

impl<F: Future<Output = ()>> CatchUnwind<F> {
    fn new(future: F) -> Self {
        Self(Box::pin(future))
    }
}

impl<F: Future<Output = ()>> Future for CatchUnwind<F> {
    type Output = std::thread::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match catch_unwind(AssertUnwindSafe(|| this.0.as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(())) => Poll::Ready(Ok(())),
            Err(payload) => Poll::Ready(Err(payload)),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
mod tests {
    use brisvox_core::{Clock, Direction, Inbox, QueueConfig, SharedClock, SimClock};
    use brisvox_core::{TimelineRecorder, TurnEpoch};
    use brisvox_sim::mock_llm::MockLlm;
    use tokio::sync::mpsc;

    use super::*;
    use crate::config::EngineConfig;
    use crate::guard::TaskGuard;
    use crate::llm::LlmStats;
    use crate::llm_runner::LlmRunner;

    const MS: Duration = Duration::from_millis(1);

    struct Fixture {
        turn: Turn<MockLlm>,
        tx: mpsc::Sender<SpecItem>,
        handoff: SpecHandoff,
        clock: SimClock,
        stats: LlmStats,
        // Kept alive: the receiving end of the turn's downstream sender, and the runner.
        _inbox: Inbox,
        _runner: LlmRunner<MockLlm>,
    }

    /// A turn that follows a committed speculation whose helper says nothing (the test holds the
    /// sending end), with the first-token and idle limits at 300 ms.
    fn fixture() -> Fixture {
        let clock = SimClock::new();
        let shared: SharedClock = Arc::new(clock);
        let mut config = EngineConfig::DEFAULT;
        config.timeouts.llm_first_token = 300 * MS;
        config.timeouts.llm_idle = 300 * MS;
        let runner = LlmRunner::builder(
            Arc::new(MockLlm::new(shared.clone())),
            &config,
            shared.clone(),
            TimelineRecorder::new(),
        )
        .build();
        let stats = runner.stats();
        let epoch = TurnEpoch::new(1);
        let inbox = Inbox::new(QueueConfig::DEFAULT);
        let out = GuardedSender::new(
            inbox.sender(Direction::Downstream),
            inbox.epoch_watch(),
            epoch,
        );
        let turn = Turn::new(
            runner.shared_for_test(),
            out,
            None,
            epoch,
            CancellationToken::new(),
        );
        let (tx, rx) = mpsc::channel(8);
        let handoff = SpecHandoff {
            rx,
            guard: TaskGuard::spawn("test_helper", std::future::pending()),
            shared: Arc::new(SpecShared::default()),
        };
        Fixture {
            turn,
            tx,
            handoff,
            clock,
            stats,
            _inbox: inbox,
            _runner: runner,
        }
    }

    /// The committed round's own first-token limit: a helper that never says anything must not
    /// hang the turn (a backstop for a helper stuck outside its own timeouts).
    #[tokio::test(start_paused = true)]
    async fn following_a_silent_helper_times_out_at_the_first_token_limit() {
        let mut f = fixture();
        let started = f.clock.now();
        let followed = tokio::time::timeout(
            Duration::from_secs(10),
            f.turn.follow_speculation(&mut f.handoff, None),
        )
        .await
        .expect("the committed round must give up on a silent helper");
        assert!(matches!(followed, Followed::Failed { emitted: false }));
        assert_eq!(f.clock.now() - started, 300 * MS);
        let stats = f.stats.snapshot();
        assert_eq!((stats.first_token_timeouts, stats.idle_timeouts), (1, 0));
        drop(f.tx);
    }

    /// After the first piece the idle limit applies, and the text already sent stays sent.
    #[tokio::test(start_paused = true)]
    async fn following_a_helper_that_goes_quiet_times_out_at_the_idle_limit() {
        let mut f = fixture();
        f.tx.send(SpecItem::Text(Arc::from("Half of")))
            .await
            .unwrap();
        let started = f.clock.now();
        let followed = tokio::time::timeout(
            Duration::from_secs(10),
            f.turn.follow_speculation(&mut f.handoff, None),
        )
        .await
        .expect("the committed round must give up on a quiet helper");
        assert!(matches!(followed, Followed::Failed { emitted: true }));
        assert_eq!(f.clock.now() - started, 300 * MS);
        let stats = f.stats.snapshot();
        assert_eq!((stats.first_token_timeouts, stats.idle_timeouts), (0, 1));
        drop(f.tx);
    }

    /// While the helper runs the ReadOnly tools no idle limit applies to the committed round.
    #[tokio::test(start_paused = true)]
    async fn following_a_helper_in_its_tool_phase_has_no_idle_limit() {
        let mut f = fixture();
        f.tx.send(SpecItem::ToolsStarted).await.unwrap();
        let tx = f.tx.clone();
        let clock = f.clock;
        let sender = tokio::spawn(async move {
            clock.sleep(5_000 * MS).await;
            tx.send(SpecItem::FinishFailed).await.unwrap();
        });
        let followed = f.turn.follow_speculation(&mut f.handoff, None).await;
        assert!(matches!(followed, Followed::FinishFailed));
        sender.await.unwrap();
        let stats = f.stats.snapshot();
        assert_eq!((stats.first_token_timeouts, stats.idle_timeouts), (0, 0));
    }
}
