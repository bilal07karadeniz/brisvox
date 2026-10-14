//! Reference session stages over the mock providers: a deliberately small stand-in for the
//! M2/M3 engine so whole calls can be simulated now. Real stages replace them later.
//!
//! ```text
//! input -> EnergyVad -> SttStage -> TurnManager -> LlmStage -> TtsStage -> OutputPacer -> transport
//!          (KeepAll: user side)            (DropStale: bot side)
//! ```
//! - `SttStage` streams user audio to the STT provider (`try_send`: a full channel drops the
//!   newest audio and counts it), forwards the VAD stop and then requests `Finalize`, and turns
//!   the provider's `speech_final` into a `UtteranceFinal` control frame. A failed session also
//!   ends with `UtteranceFinal` (so no turn waits forever) and is reconnected on the next audio,
//!   with a backoff.
//! - `TurnManager` owns epochs. A user turn ends when the VAD has stopped and every
//!   requested finalize has been answered; speech while the bot is active is a barge-in (new
//!   epoch + `Interrupt`); it records the VAD/STT/turn timeline marks. An utterance with no text
//!   ends the turn without a response.
//! - `LlmStage` keeps the conversation (an agent `Context`) and streams a response per turn. An
//!   interrupted response is cut at the playback position of the barge-in instant (word log x
//!   playout log, via `brisvox_agent::played`), before the next request or at stop, so the
//!   context holds only what the caller heard. A response that ended without an interrupt is
//!   cut at its end position the same way (synthesis that failed half-way, for instance). A
//!   response the TTS flushed cleanly whose audio played out in full (`Playout::is_complete`)
//!   is left as sent: the rewrite rule and the rate estimate only apply to a cut, never to text
//!   that was heard to the end.
//! - `TtsStage` synthesizes per turn (context id = epoch), cancels on interrupt (priority
//!   channel, `try_send`), logs word timestamps, and reports `TtsEnded` even when synthesis fails.
//! - `OutputPacer` releases audio at real time (waits abandoned at once when an interrupt is
//!   queued), logs each chunk with its playout start per turn (`Playout`), and reports `TtsEnded`
//!   upstream once a turn's audio has played out, so the turn manager knows the bot went quiet.
//!   The played position is read from the log at an instant: a chunk in flight at a barge-in
//!   counts only the part that had played by then.
//!
//! Every provider await is bounded by a timeout (connect 5 s, send 2 s). Helper tasks (provider
//! readers) push through `ProcessorContext::sender` and are aborted when their stage is dropped
//! or the turn is interrupted (abort is asynchronous: a late frame is stale and gated away).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_agent::context::{Context, Message as AgentMessage};
use brisvox_agent::played::{
    PlayedConfig, PlayedText, Truncation, TruncationStats, apply_truncation,
};
use brisvox_core::frame::{
    AudioFrame, ControlFrame, ControlKind, DataFrame, Frame, SampleRate, SystemFrame, TextFrame,
    TranscriptFrame,
};
use brisvox_core::{
    Direction, EpochPolicy, Processor, ProcessorContext, SharedClock, TimelineRecorder, TurnEpoch,
    TurnEvent,
};
use brisvox_providers::{
    ContextId, LlmEvent, LlmProvider, LlmRequest, Message, Role, SttConfig, SttEvent, SttInput,
    SttProvider, ToolCall as ProviderToolCall, TtsConfig, TtsEvent, TtsInput, TtsProvider,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

type CoreResult = brisvox_core::Result<()>;

/// Bound on provider connects.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on sends into provider sessions.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(2);
/// Wait before reconnecting after a failed connect.
pub const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);

fn control(epoch: TurnEpoch, kind: ControlKind) -> Frame {
    ControlFrame { epoch, kind }.into()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Aborts a helper task when dropped.
#[derive(Default)]
struct TaskGuard(Option<JoinHandle<()>>);

impl TaskGuard {
    fn set(&mut self, handle: JoinHandle<()>) {
        if let Some(old) = self.0.replace(handle) {
            old.abort();
        }
    }

    fn abort(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }

    /// Whether there is a task and it is still running.
    fn running(&self) -> bool {
        self.0.as_ref().is_some_and(|h| !h.is_finished())
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.abort();
    }
}

/// A word the TTS reported: (epoch, word, offset into that turn's audio).
pub type WordLog = Arc<Mutex<Vec<(TurnEpoch, Arc<str>, Duration)>>>;

/// The playout of one turn's audio: each chunk released to the transport, with the instant it
/// started to play (its release time: the pacer releases a chunk when its playout slot starts).
#[derive(Debug, Default, Clone)]
pub struct Playout {
    chunks: Vec<(Instant, Duration)>,
    /// The TTS flushed the turn.
    flushed: bool,
    /// Some text of the turn was not synthesized (the session died or refused it), so a flush
    /// no longer means that everything sent was spoken.
    lost: bool,
    /// The pacer saw the turn's end after the last chunk had played out.
    ended: bool,
}

impl Playout {
    /// Records a chunk of `duration` that started to play at `start`.
    pub fn push(&mut self, start: Instant, duration: Duration) {
        self.chunks.push((start, duration));
    }

    /// All audio released so far (the played position once the turn has played out).
    pub fn total(&self) -> Duration {
        self.chunks.iter().map(|(_, duration)| *duration).sum()
    }

    /// Records that the TTS flushed the turn.
    pub fn mark_flushed(&mut self) {
        self.flushed = true;
    }

    /// Records that text of the turn may not have been synthesized (a lost session, a refused
    /// send): the turn is never complete, whatever the provider reports afterwards.
    pub fn mark_lost(&mut self) {
        self.lost = true;
    }

    /// Records that the turn's audio has played out to its end.
    pub fn mark_ended(&mut self) {
        self.ended = true;
    }

    /// Whether the whole utterance was synthesized (flushed, nothing lost on the way) and has
    /// played out. Only then is everything that was sent known to have been heard: a failed
    /// synthesis or a cut-off turn is never complete.
    pub fn is_complete(&self) -> bool {
        self.flushed && self.ended && !self.lost
    }

    /// The playback position at `at`: the audio that had played by then. A chunk in flight at
    /// `at` counts only the part that has played, so an interruption inside a chunk cuts there.
    pub fn position(&self, at: Instant) -> Duration {
        self.chunks
            .iter()
            .map(|(start, duration)| at.saturating_duration_since(*start).min(*duration))
            .sum()
    }
}

/// Playout per turn, logged by the pacer as the transport receives audio.
pub type PlayLog = Arc<Mutex<HashMap<TurnEpoch, Playout>>>;

// ---------------------------------------------------------------------------------------------
// STT

/// Streams user audio to an STT provider; `Finalize` on VAD stop; transcripts out (epoch 0).
pub struct SttStage<P: SttProvider> {
    provider: Arc<P>,
    sample_rate: SampleRate,
    clock: SharedClock,
    input: Option<mpsc::Sender<SttInput>>,
    reader: TaskGuard,
    retry_at: Option<Instant>,
    dropped_audio: u64,
}

impl<P: SttProvider> SttStage<P> {
    /// A stage using `provider` for audio at `sample_rate`.
    pub fn new(provider: Arc<P>, sample_rate: SampleRate, clock: SharedClock) -> Self {
        Self {
            provider,
            sample_rate,
            clock,
            input: None,
            reader: TaskGuard::default(),
            retry_at: None,
            dropped_audio: 0,
        }
    }

    /// The live session's input, connecting (or reconnecting after a failure) when needed.
    async fn session(&mut self, ctx: &ProcessorContext) -> Option<mpsc::Sender<SttInput>> {
        if self.reader.running()
            && let Some(input) = &self.input
        {
            return Some(input.clone());
        }
        self.input = None;
        let now = self.clock.now();
        if self.retry_at.is_some_and(|at| now < at) {
            return None;
        }
        let config = SttConfig {
            sample_rate: self.sample_rate,
            language: None,
        };
        let connected = tokio::time::timeout(CONNECT_TIMEOUT, self.provider.connect(config)).await;
        let Ok(Ok(session)) = connected else {
            tracing::warn!("stt connect failed; retrying after a backoff");
            self.retry_at = Some(self.clock.now() + RECONNECT_BACKOFF);
            return None;
        };
        self.retry_at = None;
        let downstream = ctx.sender(Direction::Downstream)?;
        let clock = self.clock.clone();
        let mut events = session.events;
        self.reader.set(tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let SttEvent::Transcript {
                    text,
                    is_final,
                    speech_final,
                    ..
                } = event
                else {
                    tracing::warn!("stt session failed");
                    break;
                };
                let frame = TranscriptFrame {
                    epoch: TurnEpoch::ZERO,
                    text,
                    is_final,
                    received_at: clock.now(),
                };
                if downstream
                    .send(DataFrame::Transcript(frame).into())
                    .await
                    .is_err()
                {
                    return;
                }
                if speech_final
                    && downstream
                        .send(control(TurnEpoch::ZERO, ControlKind::UtteranceFinal))
                        .await
                        .is_err()
                {
                    return;
                }
            }
            // The session ended or failed: release any turn waiting for a final.
            let _ = downstream
                .send(control(TurnEpoch::ZERO, ControlKind::UtteranceFinal))
                .await;
        }));
        self.input = Some(session.input.clone());
        Some(session.input)
    }
}

impl<P: SttProvider> Processor for SttStage<P> {
    fn name(&self) -> &'static str {
        "stt"
    }

    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> CoreResult {
        match (&frame, dir) {
            (Frame::Data(DataFrame::Audio(audio)), Direction::Downstream) => {
                if let Some(input) = self.session(ctx).await
                    && input
                        .try_send(SttInput::Audio(audio.data().clone()))
                        .is_err()
                {
                    // Never block the user side on the provider: drop and count.
                    self.dropped_audio += 1;
                }
                Ok(()) // user audio ends here
            }
            (Frame::System(SystemFrame::UserStoppedSpeaking { .. }), Direction::Downstream) => {
                // The stop goes first, so the turn manager counts the finalize it causes.
                ctx.forward(frame, dir).await?;
                let requested = self
                    .input
                    .as_ref()
                    .is_some_and(|input| input.try_send(SttInput::Finalize).is_ok());
                if !requested {
                    // No session to finalize: answer for it so the turn can end.
                    ctx.push(
                        control(TurnEpoch::ZERO, ControlKind::UtteranceFinal),
                        Direction::Downstream,
                    )
                    .await?;
                }
                Ok(())
            }
            _ => ctx.forward(frame, dir).await,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Turn manager

/// Owns turn epochs; ends user turns; detects barge-in; records turn timeline marks.
pub struct TurnManager {
    clock: SharedClock,
    timeline: TimelineRecorder,
    epoch: TurnEpoch,
    started_any: bool,
    listening: bool,
    vad_stopped: bool,
    /// Finalizes requested (VAD stops) not yet answered with `UtteranceFinal`.
    pending_finals: u32,
    bot_active: bool,
    finals: String,
}

impl TurnManager {
    /// A turn manager recording on `timeline`.
    pub fn new(clock: SharedClock, timeline: TimelineRecorder) -> Self {
        Self {
            clock,
            timeline,
            epoch: TurnEpoch::ZERO,
            started_any: false,
            listening: false,
            vad_stopped: false,
            pending_finals: 0,
            bot_active: false,
            finals: String::new(),
        }
    }

    async fn speech_started(&mut self, at: Instant, ctx: &mut ProcessorContext) -> CoreResult {
        self.vad_stopped = false;
        if self.listening {
            return Ok(()); // pause and resume within one turn
        }
        self.listening = true;
        self.finals.clear();
        if !self.started_any {
            self.started_any = true;
            self.timeline
                .mark(self.epoch, TurnEvent::UserSpeechStart, at);
            return Ok(());
        }
        let old = self.epoch;
        self.epoch = old.next();
        if self.bot_active {
            self.bot_active = false;
            self.timeline
                .interrupt(old, self.epoch, self.clock.now(), Some(at));
            ctx.push(
                SystemFrame::Interrupt {
                    new_epoch: self.epoch,
                }
                .into(),
                Direction::Downstream,
            )
            .await
        } else {
            self.timeline
                .mark(self.epoch, TurnEvent::UserSpeechStart, at);
            Ok(())
        }
    }

    async fn maybe_end_turn(&mut self, ctx: &mut ProcessorContext) -> CoreResult {
        if !self.listening || !self.vad_stopped || self.pending_finals > 0 {
            return Ok(());
        }
        let now = self.clock.now();
        self.timeline.mark(self.epoch, TurnEvent::SttFinal, now);
        self.timeline.mark(self.epoch, TurnEvent::TurnEnd, now);
        self.listening = false;
        let text = std::mem::take(&mut self.finals);
        let text = text.trim();
        if text.is_empty() {
            return Ok(()); // nothing recognisable: no response
        }
        self.bot_active = true;
        let transcript = TranscriptFrame {
            epoch: self.epoch,
            text: Arc::from(text),
            is_final: true,
            received_at: now,
        };
        ctx.push(
            DataFrame::Transcript(transcript).into(),
            Direction::Downstream,
        )
        .await?;
        ctx.push(
            control(self.epoch, ControlKind::UserTurnEnded),
            Direction::Downstream,
        )
        .await
    }
}

impl Processor for TurnManager {
    fn name(&self) -> &'static str {
        "turn_manager"
    }

    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> CoreResult {
        match (frame, dir) {
            (Frame::System(SystemFrame::UserStartedSpeaking { at }), Direction::Downstream) => {
                self.speech_started(at, ctx).await
            }
            (Frame::System(SystemFrame::UserStoppedSpeaking { at }), Direction::Downstream) => {
                if self.listening {
                    self.vad_stopped = true;
                    self.pending_finals += 1;
                    self.timeline.mark(self.epoch, TurnEvent::UserSpeechEnd, at);
                }
                Ok(())
            }
            (Frame::Data(DataFrame::Transcript(t)), Direction::Downstream) => {
                if t.is_final && self.listening && !t.text.is_empty() {
                    if !self.finals.is_empty() {
                        self.finals.push(' ');
                    }
                    self.finals.push_str(&t.text);
                }
                Ok(())
            }
            (
                Frame::Control(ControlFrame {
                    kind: ControlKind::UtteranceFinal,
                    ..
                }),
                Direction::Downstream,
            ) => {
                self.pending_finals = self.pending_finals.saturating_sub(1);
                self.maybe_end_turn(ctx).await
            }
            (
                Frame::Control(ControlFrame {
                    epoch,
                    kind: ControlKind::TtsEnded,
                }),
                Direction::Upstream,
            ) => {
                if epoch == self.epoch {
                    self.bot_active = false;
                }
                Ok(())
            }
            // Lifecycle frames go on; nothing else from the user side does.
            (frame @ Frame::System(_), dir) => ctx.forward(frame, dir).await,
            (frame, Direction::Upstream) => ctx.forward(frame, Direction::Upstream).await,
            _ => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// LLM

/// A response whose message must be cut to the words played: the message's index in the context,
/// its turn, and the playback position the cut is made at.
#[derive(Debug, Clone, Copy)]
struct Cut {
    index: usize,
    epoch: TurnEpoch,
    played: Duration,
}

/// Keeps the conversation and streams one response per user turn.
pub struct LlmStage<P: LlmProvider> {
    provider: Arc<P>,
    clock: SharedClock,
    timeline: TimelineRecorder,
    system_prompt: Arc<str>,
    context: Arc<Mutex<Context>>,
    words: WordLog,
    played: PlayLog,
    truncations: Arc<Mutex<TruncationStats>>,
    pending_user: Option<Arc<str>>,
    /// The assistant message of the response in progress: (context index, epoch).
    responding: Option<(usize, TurnEpoch)>,
    /// An interrupted response, cut at the playback position of the interrupt.
    cut: Option<Cut>,
    reader: TaskGuard,
}

/// The provider-side view of a context: what the LLM request carries.
fn provider_messages(context: &Context) -> Vec<Message> {
    context
        .messages()
        .iter()
        .map(|message| match message {
            AgentMessage::System { text } => Message::new(Role::System, Arc::clone(text)),
            AgentMessage::User { text } => Message::new(Role::User, Arc::clone(text)),
            // The calls keep the internal id, the one the tool results answer with (the
            // reference never makes calls; a production context does, see `SessionProbes`).
            AgentMessage::Assistant {
                text, tool_calls, ..
            } => Message::assistant(
                Arc::clone(text),
                tool_calls
                    .iter()
                    .map(|call| ProviderToolCall {
                        id: Arc::from(call.id.as_str()),
                        name: Arc::clone(&call.name),
                        arguments: Arc::from(call.arguments.to_string()),
                    })
                    .collect(),
                None,
            ),
            AgentMessage::Tool {
                call_id, content, ..
            } => Message::tool_result(call_id.as_str(), Arc::clone(content)),
        })
        .collect()
}

impl<P: LlmProvider> LlmStage<P> {
    /// A stage using `provider`; `words` and `played` come from the TTS stage and the pacer.
    pub fn new(
        provider: Arc<P>,
        clock: SharedClock,
        timeline: TimelineRecorder,
        words: WordLog,
        played: PlayLog,
    ) -> Self {
        Self {
            provider,
            clock,
            timeline,
            system_prompt: Arc::from("You are a helpful voice agent."),
            context: Arc::new(Mutex::new(Context::new())),
            words,
            played,
            truncations: Arc::new(Mutex::new(TruncationStats::default())),
            pending_user: None,
            responding: None,
            cut: None,
            reader: TaskGuard::default(),
        }
    }

    /// The conversation so far, shared for asserts (the stage keeps it as an agent [`Context`]).
    pub fn history(&self) -> Arc<Mutex<Context>> {
        Arc::clone(&self.context)
    }

    /// The truncation counters, shared for asserts and metrics.
    pub fn truncations(&self) -> Arc<Mutex<TruncationStats>> {
        Arc::clone(&self.truncations)
    }

    /// The playback position of `epoch` now (the audio that has played by the clock).
    fn played_now(&self, epoch: TurnEpoch) -> Duration {
        let now = self.clock.now();
        lock(&self.played)
            .get(&epoch)
            .map_or(Duration::ZERO, |playout| playout.position(now))
    }

    /// Cuts the response's message to what was played, via [`PlayedText`] and
    /// [`apply_truncation`]: the sent text up to the last word that started playing,
    /// punctuation and spacing kept. The position is the one at the interrupt (or now, when the
    /// response ended without one). Without word timestamps the position is estimated from the
    /// speaking rate instead. One-shot: a cut is applied once, at the next response or at stop.
    ///
    /// A response whose audio was synthesized cleanly and played out in full was heard in full:
    /// its message is left as sent, since neither the rewrite rule (numbers and dates the TTS
    /// speaks as other words) nor the speaking-rate estimate may drop text the caller heard.
    fn apply_truncation(&mut self) {
        let cut = match self.cut.take() {
            Some(cut) => cut,
            None => match self.responding.take() {
                Some((index, epoch)) => Cut {
                    index,
                    epoch,
                    played: self.played_now(epoch),
                },
                None => return,
            },
        };
        let heard_in_full = lock(&self.played)
            .get(&cut.epoch)
            .is_some_and(|playout| playout.is_complete() && cut.played >= playout.total());
        let sent = match lock(&self.context).messages().get(cut.index) {
            Some(AgentMessage::Assistant { text, .. }) => Arc::clone(text),
            _ => return,
        };
        let truncation = if heard_in_full {
            // Everything was heard: keep the text as sent (trailing whitespace aside, which is
            // not a word), without a word mapping that could drop part of it. Not a truncation:
            // not recorded.
            let text = sent.trim_end();
            Truncation {
                text: Arc::from(text),
                estimated: false,
                words_heard: text.split_whitespace().count(),
                unmatched_words: 0,
                gaps: 0,
                played: cut.played,
            }
        } else {
            let mut tracker = PlayedText::new();
            tracker.push_sent(&sent);
            for (_, word, start) in lock(&self.words).iter().filter(|(e, _, _)| *e == cut.epoch) {
                tracker.push_word(Arc::clone(word), *start);
            }
            let truncation = tracker.heard(cut.played, &PlayedConfig::default());
            lock(&self.truncations).record(&truncation);
            truncation
        };
        if let Err(error) = apply_truncation(&mut lock(&self.context), cut.index, &truncation) {
            tracing::warn!(%error, "played-text truncation not applied");
        }
    }

    async fn respond(&mut self, epoch: TurnEpoch, ctx: &ProcessorContext) -> CoreResult {
        self.apply_truncation();
        let Some(user) = self.pending_user.take() else {
            return Ok(());
        };
        let (request, index) = {
            let mut conversation = lock(&self.context);
            conversation.push(AgentMessage::user(user));
            let mut messages = vec![Message::new(Role::System, Arc::clone(&self.system_prompt))];
            messages.extend(provider_messages(&conversation));
            // The assistant message is filled in as the response streams.
            conversation.push(AgentMessage::assistant(""));
            (
                LlmRequest {
                    messages,
                    tools: Vec::new(),
                },
                conversation.len() - 1,
            )
        };
        self.responding = Some((index, epoch));
        let Some(downstream) = ctx.sender(Direction::Downstream) else {
            return Ok(());
        };
        let (clock, timeline, provider, conversation) = (
            self.clock.clone(),
            self.timeline.clone(),
            Arc::clone(&self.provider),
            Arc::clone(&self.context),
        );
        self.reader.set(tokio::spawn(async move {
            timeline.mark(epoch, TurnEvent::LlmRequest, clock.now());
            let _ = downstream
                .send(control(epoch, ControlKind::LlmResponseStarted))
                .await;
            let mut answer = String::new();
            match tokio::time::timeout(CONNECT_TIMEOUT, provider.stream(request)).await {
                Ok(Ok(mut stream)) => {
                    while let Some(event) = stream.next().await {
                        match event {
                            LlmEvent::TextDelta(text) => {
                                timeline.mark(epoch, TurnEvent::LlmFirstToken, clock.now());
                                answer.push_str(&text);
                                // Reference-stage cost: copies the answer so far per delta
                                // (O(n^2) per response) so an interrupt without word timestamps
                                // keeps what was generated; real stages must not.
                                if let Some(AgentMessage::Assistant { text, .. }) =
                                    lock(&conversation).messages_mut().get_mut(index)
                                {
                                    *text = Arc::from(answer.as_str());
                                }
                                let frame = DataFrame::LlmText(TextFrame { epoch, text });
                                if downstream.send(frame.into()).await.is_err() {
                                    return;
                                }
                            }
                            LlmEvent::ToolCallDelta { .. } => {
                                timeline.mark(epoch, TurnEvent::LlmFirstToken, clock.now());
                            }
                            LlmEvent::Done { .. } => break,
                            LlmEvent::Error(error) => {
                                tracing::warn!(%error, "llm stream failed");
                                break;
                            }
                        }
                    }
                }
                Ok(Err(error)) => tracing::warn!(%error, "llm request failed"),
                Err(_) => tracing::warn!("llm request timed out"),
            }
            if answer.is_empty() {
                // Nothing was said: no assistant message.
                let mut c = lock(&conversation);
                if c.len() == index + 1 {
                    c.messages_mut().pop();
                }
            }
            let _ = downstream
                .send(control(epoch, ControlKind::LlmResponseEnded))
                .await;
        }));
        Ok(())
    }
}

impl<P: LlmProvider> Processor for LlmStage<P> {
    fn name(&self) -> &'static str {
        "llm"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> CoreResult {
        match (&frame, dir) {
            (Frame::Data(DataFrame::Transcript(t)), Direction::Downstream) => {
                self.pending_user = Some(Arc::clone(&t.text));
                Ok(())
            }
            (
                Frame::Control(ControlFrame {
                    epoch,
                    kind: ControlKind::UserTurnEnded,
                }),
                Direction::Downstream,
            ) => self.respond(*epoch, ctx).await,
            (Frame::System(SystemFrame::Interrupt { .. }), _) => {
                self.reader.abort();
                // The caller barged in now: what has played by now is what they heard.
                if let Some((index, epoch)) = self.responding.take() {
                    self.cut = Some(Cut {
                        index,
                        epoch,
                        played: self.played_now(epoch),
                    });
                }
                ctx.forward(frame, dir).await
            }
            (Frame::System(SystemFrame::Stop), Direction::Downstream) => {
                self.apply_truncation();
                ctx.forward(frame, dir).await
            }
            _ => ctx.forward(frame, dir).await,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// TTS

/// Synthesizes each turn's text (context = epoch), cancels on interrupt, logs words.
pub struct TtsStage<P: TtsProvider> {
    provider: Arc<P>,
    sample_rate: SampleRate,
    clock: SharedClock,
    timeline: TimelineRecorder,
    words: WordLog,
    played: Option<PlayLog>,
    input: Option<mpsc::Sender<TtsInput>>,
    cancel: Option<mpsc::Sender<ContextId>>,
    speaking: Option<TurnEpoch>,
    /// The turn whose `Flush` was sent and whose `Flushed` is still due (shared with the
    /// reader, which reports `TtsEnded` for it if the session dies first).
    awaiting_flush: Arc<Mutex<Option<TurnEpoch>>>,
    reader: TaskGuard,
    retry_at: Option<Instant>,
}

impl<P: TtsProvider> TtsStage<P> {
    /// A stage producing `sample_rate` audio with `provider`, logging words into `words`.
    pub fn new(
        provider: Arc<P>,
        sample_rate: SampleRate,
        clock: SharedClock,
        timeline: TimelineRecorder,
        words: WordLog,
    ) -> Self {
        Self {
            provider,
            sample_rate,
            clock,
            timeline,
            words,
            played: None,
            input: None,
            cancel: None,
            speaking: None,
            awaiting_flush: Arc::default(),
            reader: TaskGuard::default(),
            retry_at: None,
        }
    }

    /// Also records into `played` which turns the provider flushed and which lost text, so the
    /// LLM stage can tell a response that was synthesized and played in full from one that was
    /// cut short.
    #[must_use]
    pub fn with_play_log(mut self, played: PlayLog) -> Self {
        self.played = Some(played);
        self
    }

    /// Notes that text of `epoch` may not be synthesized (see [`Playout::mark_lost`]).
    fn mark_lost(&self, epoch: TurnEpoch) {
        if let Some(played) = &self.played {
            lock(played).entry(epoch).or_default().mark_lost();
        }
    }

    async fn session(&mut self, ctx: &ProcessorContext) -> Option<mpsc::Sender<TtsInput>> {
        if self.reader.running()
            && let Some(input) = &self.input
        {
            return Some(input.clone());
        }
        if self.reader.0.is_some()
            && let Some(epoch) = self.speaking
        {
            // The session died with this turn's text in flight: a new session cannot speak it.
            self.mark_lost(epoch);
        }
        self.input = None;
        self.cancel = None;
        let now = self.clock.now();
        if self.retry_at.is_some_and(|at| now < at) {
            return None;
        }
        let config = TtsConfig {
            sample_rate: self.sample_rate,
            voice: Arc::from("reference"),
        };
        let connected = tokio::time::timeout(CONNECT_TIMEOUT, self.provider.connect(config)).await;
        let Ok(Ok(session)) = connected else {
            tracing::warn!("tts connect failed; retrying after a backoff");
            self.retry_at = Some(self.clock.now() + RECONNECT_BACKOFF);
            return None;
        };
        self.retry_at = None;
        let downstream = ctx.sender(Direction::Downstream)?;
        let (clock, timeline, words, rate, awaiting, played) = (
            self.clock.clone(),
            self.timeline.clone(),
            Arc::clone(&self.words),
            self.sample_rate,
            Arc::clone(&self.awaiting_flush),
            self.played.clone(),
        );
        let mut events = session.events;
        self.reader.set(tokio::spawn(async move {
            let mut seq = 0u64;
            // The first-byte mark takes the timeline lock: once per turn, not per chunk.
            let mut marked: Option<TurnEpoch> = None;
            while let Some(event) = events.recv().await {
                let frame = match event {
                    TtsEvent::Audio { context, audio } => {
                        let epoch = epoch_of(context);
                        let now = clock.now();
                        if marked != Some(epoch) {
                            timeline.mark(epoch, TurnEvent::TtsFirstByte, now);
                            marked = Some(epoch);
                        }
                        seq += 1;
                        match AudioFrame::new(audio, rate, now, seq, epoch) {
                            Ok(a) => Frame::from(a),
                            Err(_) => continue,
                        }
                    }
                    TtsEvent::Word {
                        context,
                        word,
                        start,
                    } => {
                        lock(&words).push((epoch_of(context), word, start));
                        continue;
                    }
                    TtsEvent::Flushed { context } => {
                        let epoch = epoch_of(context);
                        let mut awaiting = lock(&awaiting);
                        if *awaiting == Some(epoch) {
                            *awaiting = None;
                        }
                        drop(awaiting);
                        if let Some(played) = &played {
                            lock(played).entry(epoch).or_default().mark_flushed();
                        }
                        control(epoch, ControlKind::TtsEnded)
                    }
                    TtsEvent::Cancelled { .. } => continue,
                    TtsEvent::Error(error) => {
                        tracing::warn!(%error, "tts session failed");
                        break;
                    }
                };
                if downstream.send(frame).await.is_err() {
                    return;
                }
            }
            // The session failed or closed: a flushed turn would otherwise never end (a turn
            // not flushed yet is ended by the stage when its LLM response ends).
            let pending = lock(&awaiting).take();
            if let Some(epoch) = pending {
                let _ = downstream.send(control(epoch, ControlKind::TtsEnded)).await;
            }
        }));
        self.cancel = Some(session.cancel);
        self.input = Some(session.input.clone());
        Some(session.input)
    }
}

fn context_of(epoch: TurnEpoch) -> ContextId {
    ContextId(u64::from(epoch.get()))
}

fn epoch_of(context: ContextId) -> TurnEpoch {
    TurnEpoch::new(u32::try_from(context.0).unwrap_or(u32::MAX))
}

impl<P: TtsProvider> Processor for TtsStage<P> {
    fn name(&self) -> &'static str {
        "tts"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> CoreResult {
        match (&frame, dir) {
            (Frame::Data(DataFrame::LlmText(t)), Direction::Downstream) => {
                let now = self.clock.now();
                self.timeline
                    .mark(t.epoch, TurnEvent::LlmFirstSentence, now);
                self.timeline.mark(t.epoch, TurnEvent::TtsRequest, now);
                let mut sent = false;
                if let Some(input) = self.session(ctx).await {
                    let text = TtsInput::Text {
                        context: context_of(t.epoch),
                        text: Arc::clone(&t.text),
                    };
                    if let Ok(Ok(())) = tokio::time::timeout(SEND_TIMEOUT, input.send(text)).await {
                        self.speaking = Some(t.epoch);
                        sent = true;
                    }
                }
                if !sent {
                    // This text will never be spoken, whatever is flushed for the turn later.
                    self.mark_lost(t.epoch);
                }
                Ok(())
            }
            (
                Frame::Control(ControlFrame {
                    epoch,
                    kind: ControlKind::LlmResponseEnded,
                }),
                Direction::Downstream,
            ) => {
                let flushed = match &self.input {
                    Some(input) if self.speaking == Some(*epoch) && self.reader.running() => {
                        let flush = TtsInput::Flush {
                            context: context_of(*epoch),
                        };
                        *lock(&self.awaiting_flush) = Some(*epoch);
                        let sent = matches!(
                            tokio::time::timeout(SEND_TIMEOUT, input.send(flush)).await,
                            Ok(Ok(()))
                        );
                        // If the reader died meanwhile it may have missed the marker: then
                        // nobody else will end the turn, so this stage does.
                        if !sent || !self.reader.running() {
                            *lock(&self.awaiting_flush) = None;
                            false
                        } else {
                            true
                        }
                    }
                    _ => false,
                };
                if !flushed {
                    // Nothing (more) will be synthesized for this turn: it is over at once.
                    ctx.push(
                        control(*epoch, ControlKind::TtsEnded),
                        Direction::Downstream,
                    )
                    .await?;
                }
                Ok(())
            }
            (Frame::System(SystemFrame::Interrupt { .. }), _) => {
                if let (Some(cancel), Some(epoch)) = (&self.cancel, self.speaking.take()) {
                    // Priority channel; if it is full the provider is already cancelling.
                    let _ = cancel.try_send(context_of(epoch));
                    let mut awaiting = lock(&self.awaiting_flush);
                    if *awaiting == Some(epoch) {
                        *awaiting = None;
                    }
                }
                ctx.forward(frame, dir).await
            }
            _ => ctx.forward(frame, dir).await,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Output pacing

/// Releases audio at real time on the clock (one frame's duration after the previous), so the
/// transport sees playout timing. Logs played audio per turn; reports `TtsEnded` upstream once
/// a turn's audio has played out.
pub struct OutputPacer {
    clock: SharedClock,
    next_play: Option<Instant>,
    played: PlayLog,
}

impl OutputPacer {
    /// A pacer on `clock` logging into `played`.
    pub fn new(clock: SharedClock, played: PlayLog) -> Self {
        Self {
            clock,
            next_play: None,
            played,
        }
    }

    /// Sleeps until `at`, or returns `false` at once if an interrupt making `epoch` stale is
    /// queued for this processor.
    async fn wait(&self, at: Instant, epoch: TurnEpoch, ctx: &ProcessorContext) -> bool {
        // A due deadline must not touch the timer: tokio rounds every sleep up to the next
        // timer tick (1 ms on Linux, ~15.6 ms on Windows), which would delay each turn's first
        // frame by a tick (found by the load simulation).
        if at <= self.clock.now() {
            // Same verdict the select below gives at once: an interrupt already queued for this
            // processor makes the frame stale (one atomic load, no timer).
            return !ctx
                .epoch_watch()
                .is_some_and(|watch| epoch.is_stale(watch.latest()));
        }
        if let Some(watch) = ctx.epoch_watch().cloned() {
            tokio::select! {
                biased;
                () = watch.wait_beyond(epoch) => false,
                () = self.clock.sleep_until(at) => true,
            }
        } else {
            self.clock.sleep_until(at).await;
            true
        }
    }
}

impl Processor for OutputPacer {
    fn name(&self) -> &'static str {
        "pacer"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> CoreResult {
        match (&frame, dir) {
            (Frame::Data(DataFrame::Audio(audio)), Direction::Downstream) => {
                let now = self.clock.now();
                let at = self.next_play.map_or(now, |next| next.max(now));
                if !self.wait(at, audio.epoch(), ctx).await {
                    return Ok(()); // stale now; the interrupt is handled next
                }
                self.next_play = Some(at + audio.duration());
                lock(&self.played)
                    .entry(audio.epoch())
                    .or_default()
                    .push(at, audio.duration());
                ctx.forward(frame, dir).await
            }
            (
                Frame::Control(ControlFrame {
                    epoch,
                    kind: ControlKind::TtsEnded,
                }),
                Direction::Downstream,
            ) => {
                // Wait until the last frame has played out, then tell the turn manager.
                if let Some(next) = self.next_play
                    && !self.wait(next, *epoch, ctx).await
                {
                    return Ok(());
                }
                if let Some(playout) = lock(&self.played).get_mut(epoch) {
                    playout.mark_ended();
                }
                ctx.push(control(*epoch, ControlKind::TtsEnded), Direction::Upstream)
                    .await?;
                ctx.forward(frame, dir).await
            }
            (Frame::System(SystemFrame::Interrupt { .. }), _) => {
                self.next_play = None; // flush: the next turn starts playing at once
                ctx.forward(frame, dir).await
            }
            _ => ctx.forward(frame, dir).await,
        }
    }
}

/// Shared handles for asserting on a finished simulated session.
#[derive(Clone)]
pub struct SessionProbes {
    /// The conversation as the LLM stage keeps it (interrupted responses cut to played words).
    pub context: Arc<Mutex<Context>>,
    /// Word timestamps per turn.
    pub words: WordLog,
    /// Playout per turn.
    pub played: PlayLog,
    /// Counters of the played-text truncations the LLM stage made.
    pub truncations: Arc<Mutex<TruncationStats>>,
}

impl SessionProbes {
    /// Probes over the given shared state; any pipeline that keeps its conversation, word log,
    /// playout log and truncation counters in these shapes can hand them to the suite.
    pub fn new(
        context: Arc<Mutex<Context>>,
        words: WordLog,
        played: PlayLog,
        truncations: Arc<Mutex<TruncationStats>>,
    ) -> Self {
        Self {
            context,
            words,
            played,
            truncations,
        }
    }

    /// A copy of the conversation, in the provider's message form (system prompt excluded).
    pub fn history(&self) -> Vec<Message> {
        provider_messages(&lock(&self.context))
    }

    /// A copy of the word log.
    pub fn words(&self) -> Vec<(TurnEpoch, Arc<str>, Duration)> {
        lock(&self.words).clone()
    }

    /// All audio released in turn `epoch`.
    pub fn played(&self, epoch: TurnEpoch) -> Duration {
        lock(&self.played)
            .get(&epoch)
            .map_or(Duration::ZERO, Playout::total)
    }

    /// The playback position of turn `epoch` at `at`: what had played by then.
    pub fn played_at(&self, epoch: TurnEpoch, at: Instant) -> Duration {
        lock(&self.played)
            .get(&epoch)
            .map_or(Duration::ZERO, |playout| playout.position(at))
    }

    /// A copy of the truncation counters.
    pub fn truncation_stats(&self) -> TruncationStats {
        *lock(&self.truncations)
    }
}

impl std::fmt::Debug for SessionProbes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionProbes").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use brisvox_core::SimClock;

    use super::*;
    use crate::mock_llm::MockLlm;

    const MS: Duration = Duration::from_millis(1);

    fn clock() -> SharedClock {
        Arc::new(SimClock::new())
    }

    // ---- Playout (hand-computed positions; the sim oracle shares this code, so it is pinned
    // here independently) ----

    #[tokio::test(start_paused = true)]
    async fn playout_position_is_the_part_of_each_chunk_that_has_played() {
        let clock = clock();
        let t0 = clock.now() + 100 * MS;
        let mut playout = Playout::default();
        // Three contiguous 40 ms chunks, then an underrun gap of 80 ms, then one more chunk.
        for i in 0..3u32 {
            playout.push(t0 + 40 * MS * i, 40 * MS);
        }
        playout.push(t0 + 200 * MS, 40 * MS);

        assert_eq!(playout.total(), 160 * MS);
        // Before the first chunk starts: nothing has played (saturating, no underflow).
        assert_eq!(playout.position(clock.now()), Duration::ZERO);
        assert_eq!(playout.position(t0), Duration::ZERO);
        // Inside the first chunk.
        assert_eq!(playout.position(t0 + 10 * MS), 10 * MS);
        // Exactly at a chunk boundary: the chunk before it has fully played, the next has not
        // started.
        assert_eq!(playout.position(t0 + 40 * MS), 40 * MS);
        assert_eq!(playout.position(t0 + 80 * MS), 80 * MS);
        // Mid-chunk of the third chunk.
        assert_eq!(playout.position(t0 + 95 * MS), 95 * MS);
        // The end of the contiguous run, and across the gap: the position does not advance.
        assert_eq!(playout.position(t0 + 120 * MS), 120 * MS);
        assert_eq!(playout.position(t0 + 150 * MS), 120 * MS);
        assert_eq!(playout.position(t0 + 200 * MS), 120 * MS);
        // Inside the chunk after the gap, and after the end.
        assert_eq!(playout.position(t0 + 215 * MS), 135 * MS);
        assert_eq!(playout.position(t0 + 240 * MS), 160 * MS);
        assert_eq!(playout.position(t0 + 10_000 * MS), 160 * MS);
    }

    #[test]
    fn playout_is_complete_only_after_a_lossless_flush_and_a_played_out_end() {
        let mut playout = Playout::default();
        assert!(!playout.is_complete());
        playout.mark_ended();
        assert!(!playout.is_complete(), "ended without a clean flush");
        let mut playout = Playout::default();
        playout.mark_flushed();
        assert!(!playout.is_complete(), "flushed but not played out");
        playout.mark_ended();
        assert!(playout.is_complete());
        playout.mark_lost();
        assert!(!playout.is_complete(), "text was lost on the way");
    }

    // ---- LlmStage: a fully played reply is never cut ----

    /// A stage whose first message is an assistant reply `text` of turn 0, with `words`
    /// (word, start ms) logged and `chunks` 40 ms chunks played from now on.
    fn stage_with_reply(
        clock: &SharedClock,
        text: &str,
        words: &[(&str, u64)],
        chunks: u32,
        flushed: bool,
        ended: bool,
    ) -> LlmStage<MockLlm> {
        let word_log = WordLog::default();
        let play_log = PlayLog::default();
        let mut stage = LlmStage::new(
            Arc::new(MockLlm::new(clock.clone())),
            clock.clone(),
            TimelineRecorder::new(),
            Arc::clone(&word_log),
            Arc::clone(&play_log),
        );
        lock(&stage.context).push(AgentMessage::assistant(text));
        stage.responding = Some((0, TurnEpoch::ZERO));
        for (word, start) in words {
            lock(&word_log).push((
                TurnEpoch::ZERO,
                Arc::from(*word),
                Duration::from_millis(*start),
            ));
        }
        let mut playout = Playout::default();
        let t0 = clock.now();
        for i in 0..chunks {
            playout.push(t0 + 40 * MS * i, 40 * MS);
        }
        if flushed {
            playout.mark_flushed();
        }
        if ended {
            playout.mark_ended();
        }
        lock(&play_log).insert(TurnEpoch::ZERO, playout);
        stage
    }

    fn reply_of(stage: &LlmStage<MockLlm>) -> Option<String> {
        lock(&stage.context)
            .messages()
            .first()
            .and_then(|m| match m {
                AgentMessage::Assistant { text, .. } => Some(text.to_string()),
                _ => None,
            })
    }

    /// "3.5 kg" is spoken as four words the tracker cannot map back to one token.
    const REWRITTEN: &[(&str, u64)] = &[
        ("Oran", 0),
        ("üç", 300),
        ("nokta", 500),
        ("beş", 700),
        ("kilogram", 900),
        ("ağır", 1_200),
        ("geldi", 1_400),
    ];
    const REWRITTEN_TEXT: &str = "Oran 3.5 kg ağır geldi";

    #[tokio::test(start_paused = true)]
    async fn a_fully_played_reply_with_rewritten_words_keeps_its_whole_text() {
        let clock = clock();
        // 40 chunks of 40 ms = 1.6 s of audio, all played out by the time the cut is applied.
        let mut stage = stage_with_reply(&clock, REWRITTEN_TEXT, REWRITTEN, 40, true, true);
        clock.sleep(5_000 * MS).await;
        stage.apply_truncation();
        assert_eq!(reply_of(&stage).as_deref(), Some(REWRITTEN_TEXT));
        assert_eq!(lock(&stage.truncations).truncations, 0, "nothing was cut");
    }

    #[tokio::test(start_paused = true)]
    async fn a_reply_that_did_not_end_cleanly_is_still_cut_at_the_rewrite() {
        let clock = clock();
        // The same audio and position, but the playout did not reach a clean end: only the
        // provable prefix is kept (the documented behaviour for a cut).
        let mut stage = stage_with_reply(&clock, REWRITTEN_TEXT, REWRITTEN, 40, true, false);
        clock.sleep(5_000 * MS).await;
        stage.apply_truncation();
        assert_eq!(reply_of(&stage).as_deref(), Some("Oran"));
        assert_eq!(lock(&stage.truncations).truncations, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_reply_with_lost_text_is_cut_even_when_flushed_and_played_out() {
        let clock = clock();
        // The TTS session died and a new one flushed the turn: the text the first session lost
        // was never spoken, so the flush says nothing about it.
        let mut stage = stage_with_reply(&clock, REWRITTEN_TEXT, REWRITTEN, 40, true, true);
        lock(&stage.played)
            .get_mut(&TurnEpoch::ZERO)
            .unwrap()
            .mark_lost();
        clock.sleep(5_000 * MS).await;
        stage.apply_truncation();
        assert_eq!(reply_of(&stage).as_deref(), Some("Oran"));
        assert_eq!(lock(&stage.truncations).truncations, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_fully_played_reply_loses_only_its_trailing_whitespace() {
        let clock = clock();
        let mut stage = stage_with_reply(&clock, "Sure, ", &[("Sure,", 0)], 20, true, true);
        clock.sleep(5_000 * MS).await;
        stage.apply_truncation();
        assert_eq!(reply_of(&stage).as_deref(), Some("Sure,"));
        assert_eq!(lock(&stage.truncations).truncations, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_reply_flushed_and_ended_but_cut_before_its_end_is_still_cut() {
        let clock = clock();
        let mut stage = stage_with_reply(&clock, REWRITTEN_TEXT, REWRITTEN, 40, true, true);
        // Only 0.5 s of the 1.6 s had played: the caller heard up to (not including) "nokta",
        // which starts at 500 ms, and the run starting at "üç" cannot be mapped back.
        clock.sleep(500 * MS).await;
        stage.apply_truncation();
        assert_eq!(reply_of(&stage).as_deref(), Some("Oran"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_fully_played_reply_without_word_timings_is_not_estimated() {
        let clock = clock();
        // 64 words = 319 chars spoken in 20 s (about 16 chars/s, faster than the 14 chars/s
        // estimate): the estimate would cut it at about 280 chars, mid-sentence.
        let text = vec!["word"; 64].join(" ");
        let mut stage = stage_with_reply(&clock, &text, &[], 500, true, true);
        clock.sleep(30_000 * MS).await;
        stage.apply_truncation();
        assert_eq!(reply_of(&stage).as_deref(), Some(text.as_str()));
        assert_eq!(lock(&stage.truncations).estimated, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_same_reply_cut_short_without_word_timings_is_estimated() {
        let clock = clock();
        let text = vec!["word"; 64].join(" ");
        // Not flushed (synthesis died half-way): the estimate applies.
        let mut stage = stage_with_reply(&clock, &text, &[], 500, false, true);
        clock.sleep(30_000 * MS).await;
        stage.apply_truncation();
        let kept = reply_of(&stage).unwrap_or_default();
        assert!(
            kept.len() < text.len() && text.starts_with(&kept),
            "{kept:?}"
        );
        assert_eq!(lock(&stage.truncations).estimated, 1);
    }
}
