//! The [`LlmRunner`] processor and its builder. The design is in the [`crate::llm`] module docs.

use std::sync::{Arc, Mutex};

use brisvox_agent::context::Context;
use brisvox_agent::context::Message;
use brisvox_agent::flows::FlowManager;
use brisvox_agent::played::{TruncationOutcome, TruncationStats, apply_truncation};
use brisvox_agent::tools::{FillerClips, FillerPolicy, ToolEngine, ToolRegistry};
use brisvox_agent::window::Summarizer;
use brisvox_core::clock::deadline_after;
use brisvox_core::frame::{ControlFrame, ControlKind, DataFrame, Frame, SystemFrame};
use brisvox_core::{
    Direction, EpochPolicy, Processor, ProcessorContext, SharedClock, TimelineRecorder, TurnEpoch,
};
use brisvox_providers::LlmProvider;
use tokio_util::sync::CancellationToken;

use crate::agent_input::{ConversationEdit, EditReceiver, MAX_EDITS_PER_SETTLE};
use crate::config::{EngineConfig, SpeculationConfig};
use crate::convert::to_provider_tool;
use crate::guard::{GuardedSender, JoinOutcome, TaskGuard};
use crate::llm::{
    DEFAULT_FALLBACK, DEFAULT_FILLER, Inner, LlmStats, Shared, ToolSetup, bump, control, lock,
};
use crate::llm_spec::{self, SpecRun};
use crate::llm_turn::{Turn, copy_context_of};
use crate::offload::Offload;
use crate::playout::PlayoutBook;
use crate::request::{RequestBuilder, RequestStats};
use crate::speculation::{Discard, Hint, SpeculationStats, bump as spec_bump, normalize};
use crate::truncation::{
    Cut, CutRequest, KeepAll, PlayoutTruncation, PlayoutTruncationConfig, TruncationSource,
};

/// Builds an [`LlmRunner`]. Everything optional has a default: no system prompt, no tools, no
/// flow, no summarizer, no playout book, inline offload, the [`KeepAll`] truncation source.
pub struct LlmRunnerBuilder<P: LlmProvider> {
    provider: Arc<P>,
    config: EngineConfig,
    clock: SharedClock,
    timeline: TimelineRecorder,
    context: Arc<Mutex<Context>>,
    tools: Option<ToolSetup>,
    flow: Option<FlowManager>,
    summarizer: Option<Arc<dyn Summarizer>>,
    book: Option<Arc<PlayoutBook>>,
    offload: Option<Offload>,
    truncation: Arc<dyn TruncationSource>,
    filler: FillerPolicy,
    fallback: Arc<str>,
    edits: Option<EditReceiver>,
}

impl<P: LlmProvider> LlmRunnerBuilder<P> {
    /// A builder over `provider` with the LLM settings of `config`.
    pub fn new(
        provider: Arc<P>,
        config: &EngineConfig,
        clock: SharedClock,
        timeline: TimelineRecorder,
    ) -> Self {
        Self {
            provider,
            config: *config,
            clock,
            timeline,
            context: Arc::new(Mutex::new(Context::new())),
            tools: None,
            flow: None,
            summarizer: None,
            book: None,
            offload: None,
            truncation: Arc::new(KeepAll),
            filler: FillerPolicy::from_config(config.llm.filler).with_text(DEFAULT_FILLER),
            fallback: Arc::from(DEFAULT_FALLBACK),
            edits: None,
        }
    }

    /// Applies the conversation edits of `edits` (see [`crate::agent_input`]) at the
    /// runner's safe points: before each response starts, never while a round task runs.
    #[must_use]
    pub fn with_conversation_edits(mut self, edits: EditReceiver) -> Self {
        self.edits = Some(edits);
        self
    }

    /// Starts the conversation with a system prompt as the leading message (the window always
    /// keeps leading system messages). Without it the context starts empty.
    #[must_use]
    pub fn with_system_prompt(self, prompt: impl Into<Arc<str>>) -> Self {
        {
            let mut context = lock(&self.context);
            let message = Message::system(prompt);
            match context.messages_mut().first_mut() {
                Some(first @ Message::System { .. }) => *first = message,
                _ => context.messages_mut().insert(0, message),
            }
        }
        self
    }

    /// Gives the LLM tools: `registry` lists them (name-ordered in every request) and `engine`
    /// runs the calls. The engine must have been built over the same registry.
    #[must_use]
    pub fn with_tools(mut self, registry: Arc<ToolRegistry>, engine: Arc<ToolEngine>) -> Self {
        let specs = registry
            .specs()
            .map(|spec| (Arc::clone(&spec.name), to_provider_tool(spec)))
            .collect();
        self.tools = Some(ToolSetup {
            registry,
            engine,
            specs,
        });
        self
    }

    /// Drives the conversation with a flow: the current node's prompt becomes message 0 of the
    /// context, its tools are the only ones offered, and the tool results of every round are
    /// applied to the flow, which may enter another node. The flow's registry must be the one
    /// given to [`Self::with_tools`].
    ///
    /// The nodes' `Say` actions are not played by this runner (a follow-up); they are ignored.
    ///
    /// # Errors
    /// [`brisvox_agent::FlowError`] when the flow cannot start (it was started already).
    pub fn with_flow(mut self, mut flow: FlowManager) -> Result<Self, brisvox_agent::FlowError> {
        flow.start(&mut lock(&self.context))?;
        self.flow = Some(flow);
        Ok(self)
    }

    /// Keeps the summary of dropped turns up to date with `summarizer` (an LLM call, usually).
    #[must_use]
    pub fn with_summarizer(mut self, summarizer: Arc<dyn Summarizer>) -> Self {
        self.summarizer = Some(summarizer);
        self
    }

    /// Records every round in `book`: registered when it starts, with the context index of its
    /// assistant message.
    #[must_use]
    pub fn with_book(mut self, book: Arc<PlayoutBook>) -> Self {
        self.book = Some(book);
        self
    }

    /// Records the rounds in `book` and cuts interrupted answers to what the book says was
    /// played ([`PlayoutTruncation`], with the `lead_margin` and `settle_wait` of the engine
    /// config this builder was made with). Shorthand for [`Self::with_book`] plus
    /// [`Self::with_truncation`]; the stages that write the book must be given the same one.
    #[must_use]
    pub fn with_playout(self, book: Arc<PlayoutBook>) -> Self {
        let source = PlayoutTruncation::new(
            Arc::clone(&book),
            self.clock.clone(),
            PlayoutTruncationConfig::from_engine(&self.config),
        );
        self.with_book(book).with_truncation(Arc::new(source))
    }

    /// Where the runner learns what the caller heard (default: [`KeepAll`]).
    #[must_use]
    pub fn with_truncation(mut self, source: Arc<dyn TruncationSource>) -> Self {
        self.truncation = source;
        self
    }

    /// Where large responses are finished (default: inline on the calling task).
    #[must_use]
    pub fn with_offload(mut self, offload: Offload) -> Self {
        self.offload = Some(offload);
        self
    }

    /// The canned filler spoken while a slow tool runs and the line spoken when the tool budget
    /// is spent. The filler replaces the generic phrase of the current [`FillerPolicy`] (its
    /// delay, per-tool phrases and clip cache stay).
    #[must_use]
    pub fn with_phrases(
        mut self,
        filler: impl Into<Arc<str>>,
        fallback: impl Into<Arc<str>>,
    ) -> Self {
        self.filler = self.filler.with_text(filler);
        self.fallback = fallback.into();
        self
    }

    /// The filler policy of the tool layer: the delay after which a slow tool round is
    /// announced, the phrases (generic and per tool) and the cache of pre-rendered clips. The
    /// default is built from [`LlmConfig::filler`](crate::LlmConfig::filler) (by default a fixed
    /// 1.2 s for every tool that has a filler) with [`DEFAULT_FILLER`]; this replaces it whole.
    #[must_use]
    pub fn with_filler_policy(mut self, policy: FillerPolicy) -> Self {
        self.filler = policy;
        self
    }

    /// Gives the current [`FillerPolicy`] the cache of pre-rendered clips (its phrases name
    /// clips by `ClipKey`; a key reaches the filler round only when the cache has the clip).
    /// Give the TTS stage the same store
    /// ([`TtsStage::with_clips`](crate::TtsStage::with_clips)) so the clip is played.
    #[must_use]
    pub fn with_filler_clips(mut self, clips: Arc<dyn FillerClips>) -> Self {
        self.filler = self.filler.with_clips(clips);
        self
    }

    /// The runner.
    pub fn build(self) -> LlmRunner<P> {
        let config = self.config;
        let mut builder = RequestBuilder::new(
            config.llm.window,
            config.llm.summary_timeout,
            config.llm.tool_result_floor_tokens,
        );
        if let Some(summarizer) = self.summarizer {
            builder = builder.with_summarizer(summarizer);
        }
        let offload = self
            .offload
            .unwrap_or_else(|| Offload::Inline(self.clock.clone()));
        let builder = Arc::new(builder.with_offload(offload.clone(), config.timeouts.analysis_job));
        let root = CancellationToken::new();
        LlmRunner {
            shared: Arc::new(Shared {
                provider: self.provider,
                clock: self.clock,
                timeline: self.timeline,
                timeouts: config.timeouts,
                llm: config.llm,
                context: self.context,
                builder,
                tools: self.tools,
                flow: self.flow.map(tokio::sync::Mutex::new),
                book: self.book,
                offload,
                truncation: self.truncation,
                truncation_stats: Arc::new(Mutex::new(TruncationStats::default())),
                filler: self.filler,
                fallback: self.fallback,
                stats: LlmStats::default(),
                inner: Mutex::new(Inner::default()),
                cancel: root.clone(),
                summary_task: Mutex::new(None),
                offered: Mutex::new(None),
            }),
            edits: self.edits,
            task: None,
            zombie: None,
            root,
            pending_user: None,
            warm: None,
            spec: None,
            spec_config: config.speculation,
            spec_stats: SpeculationStats::default(),
        }
    }
}

/// Reads the state of the runner's flow from outside the pipeline (tests, diagnostics).
pub struct FlowProbe<P: LlmProvider> {
    shared: Arc<Shared<P>>,
}

impl<P: LlmProvider> FlowProbe<P> {
    /// The flow's current node; `None` when the runner has no flow. Waits for a round that is
    /// using the flow right now, so call it between turns.
    pub async fn node(&self) -> Option<brisvox_agent::NodeId> {
        let flow = self.shared.flow.as_ref()?;
        Some(flow.lock().await.current().clone())
    }

    /// Flow transitions entered so far.
    pub async fn transitions(&self) -> u64 {
        match self.shared.flow.as_ref() {
            Some(flow) => flow.lock().await.transitions(),
            None => 0,
        }
    }
}

struct RoundTask {
    guard: TaskGuard,
    token: CancellationToken,
}

/// The LLM stage. See the [`crate::llm`] module docs.
pub struct LlmRunner<P: LlmProvider> {
    shared: Arc<Shared<P>>,
    task: Option<RoundTask>,
    /// A round task that did not stop within `Timeouts::join` at a halt. It was aborted and ends
    /// at its next await point; until it is gone the context must not be mutated, so every later
    /// halt joins it again first.
    zombie: Option<TaskGuard>,
    /// Parent of every round's token: cancelled by `Cancel` and when the runner is dropped, so
    /// tool calls of a dying session are told to stop.
    root: CancellationToken,
    /// The final transcript of the turn that is about to end.
    pending_user: Option<Arc<str>>,
    warm: Option<TaskGuard>,
    /// The speculative run of the turn in progress, if any.
    spec: Option<SpecRun>,
    spec_config: SpeculationConfig,
    spec_stats: SpeculationStats,
    /// Conversation edits of the application, applied at the safe points.
    edits: Option<EditReceiver>,
}

impl<P: LlmProvider> Drop for LlmRunner<P> {
    fn drop(&mut self) {
        self.root.cancel();
        // Count what the dying session still had in flight (the metric outlives the runner).
        self.drop_speculation(Discard::Abandoned);
    }
}

impl<P: LlmProvider> LlmRunner<P> {
    /// A builder, see [`LlmRunnerBuilder::new`].
    pub fn builder(
        provider: Arc<P>,
        config: &EngineConfig,
        clock: SharedClock,
        timeline: TimelineRecorder,
    ) -> LlmRunnerBuilder<P> {
        LlmRunnerBuilder::new(provider, config, clock, timeline)
    }

    /// The conversation, shared for probes (the simulator's `SessionProbes`).
    pub fn context(&self) -> Arc<Mutex<Context>> {
        Arc::clone(&self.shared.context)
    }

    /// The played-text truncation counters, shared for probes.
    pub fn truncations(&self) -> Arc<Mutex<TruncationStats>> {
        Arc::clone(&self.shared.truncation_stats)
    }

    /// The runner's counters.
    pub fn stats(&self) -> LlmStats {
        self.shared.stats.clone()
    }

    /// The metrics of speculative generation (hit rate, wasted tokens, ...). All zero unless
    /// [`EngineConfig::speculation`] is enabled.
    pub fn speculation_stats(&self) -> SpeculationStats {
        self.spec_stats.clone()
    }

    /// The request builder's counters (windows, clipping, summaries).
    pub fn request_stats(&self) -> RequestStats {
        self.shared.builder.stats()
    }

    /// The request builder, for tests and tooling.
    pub fn request_builder(&self) -> &RequestBuilder {
        &self.shared.builder
    }

    /// A handle to read the flow's state while the runner is inside a pipeline.
    pub fn flow_probe(&self) -> FlowProbe<P> {
        FlowProbe {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Stops the live round task, if any, and settles the context. Returns whether the task is
    /// gone, so the context may be mutated.
    ///
    /// The task is cancelled (its token, so a tool engine call ends) and aborted, then joined with
    /// a bounded wait. Only after the join does the runner touch the context: the partial
    /// assistant message for a response that was still streaming, and error results for tool
    /// calls that were still open. With `interrupted`, the rounds of the context are stamped with
    /// the instant of the barge-in for the truncation; that instant is read **first**, before the
    /// bounded join, so a slow join cannot make the caller's position look later than it was.
    ///
    /// A task that does not stop within the bound is kept (`zombie`) and joined again by the next
    /// halt, so the "no mutation while a task is alive" rule also holds across halts.
    async fn halt(&mut self, interrupted: bool) -> bool {
        let barged_in_at = interrupted.then(|| self.shared.clock.now());
        let shared = Arc::clone(&self.shared);
        let mut safe = true;
        if let Some(mut zombie) = self.zombie.take() {
            match zombie
                .cancel_and_join_ref(&*shared.clock, shared.timeouts.join)
                .await
            {
                JoinOutcome::TimedOut => {
                    bump(&shared.count().join_timeouts);
                    self.zombie = Some(zombie);
                    safe = false;
                }
                JoinOutcome::Panicked => bump(&shared.count().halts),
                _ => {}
            }
        }
        if let Some(RoundTask { mut guard, token }) = self.task.take() {
            token.cancel();
            let still_responding = lock(&shared.inner).responding;
            match guard
                .cancel_and_join_ref(&*shared.clock, shared.timeouts.join)
                .await
            {
                JoinOutcome::TimedOut => {
                    bump(&shared.count().join_timeouts);
                    self.zombie = Some(guard);
                    safe = false;
                }
                JoinOutcome::Aborted if still_responding => bump(&shared.count().halts),
                JoinOutcome::Panicked => bump(&shared.count().halts),
                JoinOutcome::Aborted | JoinOutcome::Completed => {}
            }
        }
        lock(&shared.inner).responding = false;
        if safe {
            shared.settle_live();
            shared.close_open_calls();
            // A filler spoken while the cut-off tools ran goes after their closed results.
            shared.place_filler();
            shared.validate(&lock(&shared.context), "halt");
        } else {
            bump(&shared.count().mutations_skipped);
        }
        if let Some(now) = barged_in_at {
            for record in &mut lock(&shared.inner).records {
                record.interrupted_at.get_or_insert(now);
            }
            // A held confirmation does not survive a barge-in: the user may not have
            // heard what they would be confirming, and the context after an interruption holds
            // only what was played.
            if let Some(tools) = &shared.tools {
                tools.engine.confirmations().clear();
            }
        }
        safe
    }

    /// Cuts the assistant messages of the finished or interrupted rounds to what the caller
    /// heard, last round first (see [`crate::truncation`]).
    ///
    /// The whole pass is bounded by `Timeouts::analysis_job` on the injected clock: a source that
    /// does not answer (a playback position that never arrives) must not stall the processor and
    /// every frame queued behind it. A question that is still open at the deadline, and every
    /// later one, is answered with [`Cut::Keep`] and counted (`cut_timeouts`).
    async fn apply_truncation(&self) {
        let shared = &self.shared;
        let records = std::mem::take(&mut lock(&shared.inner).records);
        let deadline = deadline_after(shared.clock.now(), shared.timeouts.analysis_job);
        for record in records.iter().rev() {
            let Some(index) = record.msg_index else {
                continue;
            };
            let sent = match lock(&shared.context).messages().get(index) {
                Some(Message::Assistant { text, .. }) => Arc::clone(text),
                _ => {
                    bump(&shared.count().cuts_skipped);
                    continue;
                }
            };
            bump(&shared.count().cut_requests);
            let request = CutRequest {
                key: record.key,
                sent,
                interrupted_at: record.interrupted_at,
            };
            let decision = tokio::select! {
                biased;
                decision = shared.truncation.cut(&request) => decision,
                () = shared.clock.sleep_until(deadline) => {
                    bump(&shared.count().cut_timeouts);
                    tracing::warn!(target: "brisvox::engine", "truncation source did not answer in time; keeping the message");
                    Cut::Keep
                }
            };
            self.apply_cut(index, decision);
            if let Some(book) = &shared.book {
                book.mark_applied(record.key);
            }
        }
    }

    fn apply_cut(&self, index: usize, decision: Cut) {
        let shared = &self.shared;
        let (truncation, counted) = match decision {
            Cut::Keep => {
                bump(&shared.count().cuts_kept);
                return;
            }
            Cut::Heard(truncation) => (truncation, true),
            Cut::HeardInFull(truncation) => (truncation, false),
        };
        let mut context = lock(&shared.context);
        match apply_truncation(&mut context, index, &truncation) {
            Ok(TruncationOutcome::Unchanged) => {}
            Ok(TruncationOutcome::Shortened) => bump(&shared.count().cuts_shortened),
            Ok(TruncationOutcome::Removed) => bump(&shared.count().cuts_removed),
            Err(error) => {
                bump(&shared.count().cut_errors);
                tracing::warn!(target: "brisvox::engine", %error, "played-text truncation not applied");
                return;
            }
        }
        if counted {
            bump(&shared.count().cuts_heard);
            lock(&shared.truncation_stats).record(&truncation);
        } else {
            bump(&shared.count().cuts_whole);
        }
        shared.validate(&context, "truncation");
    }

    /// `UserTurnEnded`: settle the previous response, push the user message, start the round task
    /// (or, when the speculation of this turn matches, hand it the speculation).
    async fn user_turn_ended(
        &mut self,
        epoch: TurnEpoch,
        ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        ctx.forward(
            control(epoch, ControlKind::UserTurnEnded),
            Direction::Downstream,
        )
        .await?;
        let safe = self.halt(false).await;
        let shared = Arc::clone(&self.shared);
        let Some(text) = self.pending_user.take() else {
            self.drop_speculation(Discard::Abandoned);
            bump(&shared.count().empty_turns);
            self.tts_ended_upstream(epoch, ctx);
            return Ok(());
        };
        if !safe {
            // The previous task did not stop: leave the context alone and drop this turn.
            self.drop_speculation(Discard::Abandoned);
            self.tts_ended_upstream(epoch, ctx);
            return Ok(());
        }
        let speculation = self.judge_speculation(epoch, &text);
        self.apply_truncation().await;
        let speculation = {
            let mut context = lock(&shared.context);
            // A call still open here would make every later request invalid.
            shared.close_open_calls_in(&mut context);
            if let Some(tools) = &shared.tools {
                for note in tools.engine.take_late_notes() {
                    context.push(note);
                    bump(&shared.count().late_notes);
                }
            }
            let edited = apply_edits(&shared, self.edits.as_mut(), &mut context);
            // The speculative request was built from the context as it was then: if anything
            // was added or cut since (a late tool note, a truncation), the response may differ.
            // An application edit discards it too, whatever the fingerprint says: the
            // fingerprint does not see a system prompt replaced in place by one of the same
            // length and ending, nor the offered tools.
            let speculation = match speculation {
                Some(spec) if edited == 0 && llm_spec::fingerprint(&context) == spec.basis_fp => {
                    Some(spec)
                }
                Some(spec) => {
                    spec.discard(self.spec_stats.counters(), Discard::ContextChanged);
                    None
                }
                None => None,
            };
            context.push(Message::user(text));
            shared.validate(&context, "user message");
            // The user's turn is in the context: a confirmation requested before it can now be
            // given; one requested in the response to this turn cannot.
            if let Some(tools) = &shared.tools {
                tools.engine.confirmations().note_user_turn();
            }
            speculation
        };
        let (Some(downstream), Some(watch)) = (
            ctx.sender(Direction::Downstream),
            ctx.epoch_watch().cloned(),
        ) else {
            if let Some(spec) = speculation {
                spec.discard(self.spec_stats.counters(), Discard::Abandoned);
            }
            bump(&shared.count().spawn_failures);
            return Ok(());
        };
        let (token, handoff) = match speculation {
            Some(spec) => {
                let (token, handoff) = spec.commit(self.spec_stats.counters(), shared.clock.now());
                (token, Some(handoff))
            }
            None => (self.root.child_token(), None),
        };
        let mut turn = Turn::new(
            Arc::clone(&shared),
            GuardedSender::new(downstream, watch, epoch),
            ctx.sender(Direction::Upstream),
            epoch,
            token.clone(),
        );
        if let Some(handoff) = handoff {
            turn = turn.with_speculation(handoff);
        }
        lock(&shared.inner).responding = true;
        bump(&shared.count().turns);
        self.task = Some(RoundTask {
            guard: TaskGuard::spawn("llm_round", turn.run_contained()),
            token,
        });
        Ok(())
    }

    /// A bot line: the turn driver started a bot turn in `epoch` and sent its text.
    /// The context is settled as at a user turn's end (minus the user message) and a round task
    /// speaks the line as the epoch's response, recorded in the context and cut to what was
    /// heard like any response.
    async fn bot_line(
        &mut self,
        epoch: TurnEpoch,
        text: Arc<str>,
        ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        self.drop_speculation(Discard::Abandoned);
        let shared = Arc::clone(&self.shared);
        if !self.halt(false).await {
            // The previous task did not stop: leave the context alone and drop the line.
            bump(&shared.count().bot_lines_dropped);
            self.tts_ended_upstream(epoch, ctx);
            return Ok(());
        }
        self.apply_truncation().await;
        {
            let mut context = lock(&shared.context);
            shared.close_open_calls_in(&mut context);
            if let Some(tools) = &shared.tools {
                for note in tools.engine.take_late_notes() {
                    context.push(note);
                    bump(&shared.count().late_notes);
                }
            }
            apply_edits(&shared, self.edits.as_mut(), &mut context);
            shared.validate(&context, "bot line");
        }
        let (Some(downstream), Some(watch)) = (
            ctx.sender(Direction::Downstream),
            ctx.epoch_watch().cloned(),
        ) else {
            bump(&shared.count().spawn_failures);
            return Ok(());
        };
        let token = self.root.child_token();
        let turn = Turn::new(
            Arc::clone(&shared),
            GuardedSender::new(downstream, watch, epoch),
            ctx.sender(Direction::Upstream),
            epoch,
            token.clone(),
        )
        .with_line(text);
        lock(&shared.inner).responding = true;
        bump(&shared.count().bot_lines);
        self.task = Some(RoundTask {
            guard: TaskGuard::spawn("llm_line", turn.run_contained()),
            token,
        });
        Ok(())
    }

    // -- speculation -------------------------------------------------------------------

    /// Drops the speculation, if any, counted under `why` (its helper is aborted).
    fn drop_speculation(&mut self, why: Discard) {
        if let Some(spec) = self.spec.take() {
            spec.discard(self.spec_stats.counters(), why);
        }
    }

    /// Takes the speculation out of its slot and decides whether it answers the turn that ended
    /// with `text` as `epoch`: the same turn, the same normalised text, still young enough.
    /// Anything else is discarded (counted). The context check happens after the settle.
    fn judge_speculation(&mut self, epoch: TurnEpoch, text: &str) -> Option<SpecRun> {
        let spec = self.spec.take()?;
        let counters = self.spec_stats.counters();
        if spec.epoch != epoch {
            spec.discard(counters, Discard::Abandoned);
            return None;
        }
        if normalize(text) != spec.norm {
            spec.discard(counters, Discard::Changed);
            return None;
        }
        if self.shared.clock.now() >= spec.deadline {
            spec.discard(counters, Discard::Expired);
            return None;
        }
        // From here on the helper does not expire: the settle below awaits. The claim and the
        // helper's own expiry are one atomic decision; losing it means the helper has ended.
        if !spec.claim() {
            spec.discard(counters, Discard::Expired);
            return None;
        }
        Some(spec)
    }

    /// A candidate end of turn: run the LLM on `text` with the output buffered. Does nothing
    /// when speculation is off, when a flow is driven (a pending transition would change the
    /// context behind the request), while the previous turn is still being answered, or when the
    /// context cannot be settled. A candidate equal (normalised) to the running speculation
    /// keeps it; another one replaces it.
    async fn speculate(&mut self, epoch: TurnEpoch, text: &Arc<str>) {
        let config = self.spec_config;
        if !config.enabled {
            return;
        }
        let shared = Arc::clone(&self.shared);
        let stats = self.spec_stats.clone();
        let counters = stats.counters();
        if shared.flow.is_some() {
            spec_bump(&counters.skipped_flow);
            return;
        }
        let norm = normalize(text);
        if norm.is_empty() {
            return;
        }
        if let Some(current) = &self.spec
            && current.epoch == epoch
            && current.norm == norm
        {
            spec_bump(&counters.kept);
            return;
        }
        self.drop_speculation(Discard::Superseded);
        if lock(&shared.inner).responding
            || shared
                .book
                .as_ref()
                .is_some_and(|book| !book.is_settled_through(epoch))
        {
            // The previous answer is still being generated or played. The cut to what was heard
            // is taken here (the records are consumed), so it must not be taken while the
            // caller's playback position still moves: a plain turn would cut later.
            spec_bump(&counters.skipped_busy);
            return;
        }
        // Settle the context exactly as the turn end would, minus the user message: the previous
        // response is cut to what was heard, open calls are closed, late notes are in.
        if !self.halt(false).await {
            spec_bump(&counters.skipped_unsafe);
            return;
        }
        self.apply_truncation().await;
        {
            let mut context = lock(&shared.context);
            shared.close_open_calls_in(&mut context);
            if let Some(tools) = &shared.tools {
                for note in tools.engine.take_late_notes() {
                    context.push(note);
                    bump(&shared.count().late_notes);
                }
            }
            apply_edits(&shared, self.edits.as_mut(), &mut context);
            shared.validate(&context, "speculation settle");
        }
        // The helper works on a copy taken now, so nothing the turn end does to the context
        // (the user message) can reach its request.
        let mut copy = copy_context_of(&shared).await;
        let basis_fp = llm_spec::fingerprint(&copy);
        copy.push(Message::user(Arc::clone(text)));
        self.spec = Some(llm_spec::start(
            &shared,
            counters,
            self.root.child_token(),
            epoch,
            copy,
            norm,
            basis_fp,
            config.max_age,
            config.max_buffered_events,
        ));
    }

    /// A non-blocking upstream `TtsEnded` (never awaited from `process`).
    fn tts_ended_upstream(&self, epoch: TurnEpoch, ctx: &ProcessorContext) {
        let shared = &self.shared;
        if let Some(upstream) = ctx.sender(Direction::Upstream) {
            if upstream
                .try_send(control(epoch, ControlKind::TtsEnded))
                .is_ok()
            {
                bump(&shared.count().upstream_tts_ended);
            } else {
                bump(&shared.count().upstream_dropped);
            }
        }
    }

    fn start_warm(&mut self) {
        if self.warm.is_some() {
            return;
        }
        let shared = Arc::clone(&self.shared);
        self.warm = Some(TaskGuard::spawn("llm_warm", async move {
            let timeout = shared.clock.sleep(shared.timeouts.connect);
            tokio::select! {
                biased;
                warmed = shared.provider.warm() => match warmed {
                    Ok(()) => bump(&shared.count().warms),
                    Err(error) => {
                        tracing::debug!(target: "brisvox::engine", %error, "llm warm-up failed");
                        bump(&shared.count().warm_failures);
                    }
                },
                () = timeout => {
                    tracing::debug!(target: "brisvox::engine", "llm warm-up timed out");
                    bump(&shared.count().warm_failures);
                }
            }
        }));
    }
}

impl<P: LlmProvider> Processor for LlmRunner<P> {
    fn name(&self) -> &'static str {
        "llm"
    }

    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::DropStale
    }

    async fn process(
        &mut self,
        frame: Frame,
        direction: Direction,
        ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        if direction == Direction::Upstream {
            // Never awaited from `process` (processor.rs, "Pushing upstream without deadlock").
            if let Some(upstream) = ctx.sender(Direction::Upstream)
                && upstream.try_send(frame).is_err()
            {
                bump(&self.shared.count().upstream_dropped);
            }
            return Ok(());
        }
        match &frame {
            Frame::Data(DataFrame::Transcript(transcript)) => {
                match Hint::of(transcript) {
                    None => self.pending_user = Some(Arc::clone(&transcript.text)),
                    Some(Hint::Candidate(text)) => self.speculate(transcript.epoch, &text).await,
                    Some(Hint::Withdraw) => {
                        if self
                            .spec
                            .as_ref()
                            .is_some_and(|spec| spec.epoch == transcript.epoch)
                        {
                            self.drop_speculation(Discard::Withdrawn);
                        }
                    }
                }
                Ok(())
            }
            Frame::Control(ControlFrame {
                kind: ControlKind::UserTurnStarted,
                ..
            }) => {
                // A new user turn: whatever was speculated belongs to a turn that is over.
                self.drop_speculation(Discard::Abandoned);
                ctx.forward(frame, direction).await
            }
            Frame::Control(ControlFrame {
                epoch,
                kind: ControlKind::UserTurnEnded,
            }) => self.user_turn_ended(*epoch, ctx).await,
            // Nothing upstream of the runner produces LLM text: an `LlmText` here is a bot line
            // from the turn driver, the response of its epoch.
            Frame::Data(DataFrame::LlmText(line)) => {
                let (epoch, text) = (line.epoch, Arc::clone(&line.text));
                self.bot_line(epoch, text, ctx).await
            }
            Frame::System(SystemFrame::Interrupt { .. }) => {
                self.drop_speculation(Discard::Abandoned);
                self.halt(true).await;
                ctx.forward(frame, direction).await
            }
            Frame::System(SystemFrame::Start) => {
                self.start_warm();
                ctx.forward(frame, direction).await
            }
            Frame::System(SystemFrame::Stop) => {
                self.drop_speculation(Discard::Abandoned);
                if self.halt(false).await {
                    self.apply_truncation().await;
                }
                ctx.forward(frame, direction).await
            }
            Frame::System(SystemFrame::Cancel) => {
                self.root.cancel();
                self.drop_speculation(Discard::Abandoned);
                self.task = None;
                self.warm = None;
                ctx.forward(frame, direction).await
            }
            _ => ctx.forward(frame, direction).await,
        }
    }
}

/// Applies the waiting conversation edits (at most [`MAX_EDITS_PER_SETTLE`]) to `context` and
/// returns how many it applied. Called only at a safe point: no round task is alive, so nothing
/// else touches the context or the offered tools.
fn apply_edits<P: LlmProvider>(
    shared: &Shared<P>,
    edits: Option<&mut EditReceiver>,
    context: &mut Context,
) -> usize {
    let Some(edits) = edits else {
        return 0;
    };
    let mut applied = 0;
    while applied < MAX_EDITS_PER_SETTLE {
        let Ok(edit) = edits.rx.try_recv() else {
            break;
        };
        match edit {
            ConversationEdit::SetSystemPrompt(prompt) => {
                let message = Message::system(prompt);
                match context.messages_mut().first_mut() {
                    Some(first @ Message::System { .. }) => *first = message,
                    _ => context.messages_mut().insert(0, message),
                }
            }
            ConversationEdit::Append(message) => context.push(message),
            ConversationEdit::SetTools(names) => {
                let offered = names.map(|names| Arc::new(names.iter().cloned().collect()));
                *lock(&shared.offered) = offered;
            }
        }
        bump(&shared.count().edits_applied);
        applied += 1;
    }
    applied
}

#[cfg(test)]
impl<P: LlmProvider> LlmRunner<P> {
    /// The shared state, for the unit tests of the round task.
    pub(crate) fn shared_for_test(&self) -> Arc<Shared<P>> {
        Arc::clone(&self.shared)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
mod tests {
    use std::time::Duration;

    use brisvox_core::{Clock, SimClock};
    use brisvox_sim::mock_llm::{LlmFault, LlmScript, MockLlm};

    use super::*;
    use crate::llm_spec::SpecItem;

    const SECOND: Duration = Duration::from_secs(1);
    const MS: Duration = Duration::from_millis(1);

    fn runner(max_age: Duration, script: LlmScript) -> (LlmRunner<MockLlm>, SimClock, SharedClock) {
        let clock = SimClock::new();
        let shared: SharedClock = Arc::new(clock);
        let llm = MockLlm::new(shared.clone());
        llm.push(script);
        let mut config = EngineConfig::DEFAULT;
        config.speculation = SpeculationConfig::ENABLED;
        config.speculation.max_age = max_age;
        let runner = LlmRunner::builder(
            Arc::new(llm),
            &config,
            shared.clone(),
            TimelineRecorder::new(),
        )
        .build();
        (runner, clock, shared)
    }

    /// The claim at the turn end is what keeps the helper alive through the awaits that remain
    /// before the commit (the settle). Here the settle is simulated by letting the clock run past
    /// `max_age` between the decision and the commit: the helper must not end at its age limit,
    /// the committed turn must still get the answer.
    #[tokio::test(start_paused = true)]
    async fn a_claimed_speculation_keeps_its_helper_past_max_age_while_the_turn_settles() {
        // The answer comes 1.5 s after the request: after the age limit (1 s).
        let script = LlmScript::text(["Hi."]).with_fault(LlmFault::Stall {
            before_event: 0,
            duration: 1_500 * MS,
        });
        let (mut runner, clock, shared) = runner(SECOND, script);
        let epoch = TurnEpoch::new(1);
        runner.speculate(epoch, &Arc::from("hello")).await;
        assert!(runner.spec.is_some(), "the candidate started a speculation");

        // Just before the deadline: the turn ends with the same words and claims it.
        clock.sleep(900 * MS).await;
        let spec = runner
            .judge_speculation(epoch, "Hello!")
            .expect("young enough: claimed");
        // The settle (truncation source, offloaded work) takes longer than what was left.
        clock.sleep(300 * MS).await;
        let (_token, mut handoff) = spec.commit(runner.spec_stats.counters(), shared.now());
        let first = handoff.rx.recv().await;
        assert!(
            matches!(&first, Some(SpecItem::Text(text)) if &**text == "Hi."),
            "the helper must have outlived max_age, but its stream ended or failed"
        );
        let second = handoff.rx.recv().await;
        assert!(matches!(second, Some(SpecItem::Done(_))));
        assert_eq!(runner.spec_stats.snapshot().hits, 1);
    }

    /// The other side of the same atomic decision: a helper that reached its deadline first has
    /// ended, so the turn is not handed its dead receiver.
    #[tokio::test(start_paused = true)]
    async fn an_expired_speculation_cannot_be_claimed() {
        let script = LlmScript::text(["Hi."]).with_fault(LlmFault::Stall {
            before_event: 0,
            duration: 5 * SECOND,
        });
        let (mut runner, clock, _) = runner(SECOND, script);
        let epoch = TurnEpoch::new(1);
        runner.speculate(epoch, &Arc::from("hello")).await;
        // The helper's own timer fires at 1 s; the runner's clock is read at 1.05 s.
        clock.sleep(1_050 * MS).await;
        assert!(runner.judge_speculation(epoch, "hello").is_none());
        assert_eq!(runner.spec_stats.snapshot().expired, 1);
    }
}
