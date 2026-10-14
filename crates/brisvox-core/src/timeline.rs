//! Per-turn latency timeline (definitions in `docs/LATENCY.md`).
//!
//! Every stage marks [`TurnEvent`]s on a shared [`TimelineRecorder`] with the turn epoch it is
//! working for and a timestamp from the session `Clock`. Turn epochs are turn numbers: the turn
//! state machine starts a new epoch for every user turn. Each event follows a
//! [`MarkRule`]: most keep their first occurrence; `UserSpeechEnd`, `VadStop`, `SttFinalize` and
//! `SttFinal` keep the latest one until the turn-end decision / LLM request (a user may pause and
//! resume). Stages that do not know epochs (the VAD worker, the STT stage) mark the open user
//! turn with [`TimelineRecorder::mark_open_turn`].
//!
//! When an event for a newer epoch arrives, the current turn is finalised: a summary tracing
//! event (target `brisvox::timeline`) is emitted and its [`TurnRecord`] joins a bounded history.
//! Events for older epochs are late and only counted. A barge-in is recorded atomically with
//! [`TimelineRecorder::interrupt`], so no other stage can rotate the turn first.
//!
//! Marking takes one uncontended mutex and never allocates once the history has grown.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

use crate::epoch::TurnEpoch;

/// Finished turns kept in the recorder's history (grown lazily up to this bound).
pub const HISTORY_CAPACITY: usize = 32;

/// LLM requests timed per turn (one per tool-loop round). Further requests of the same
/// turn get no round record; they are counted in [`TimelineStats::llm_rounds_dropped`]. Raised
/// from 8 to 32 after a review: a fixed array of instants is cheap, and a tool loop longer
/// than this is a runaway the turn loop should stop anyway.
pub const MAX_LLM_ROUNDS: usize = 32;

/// A point in a turn's life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TurnEvent {
    /// VAD: the user started speaking.
    UserSpeechStart,
    /// VAD: the user stopped speaking. Latest stop before `TurnEnd` wins; VADs should backdate it
    /// to the last speech frame (not the hangover end).
    UserSpeechEnd,
    /// STT: first interim transcript of the turn received.
    SttInterim,
    /// STT: final transcript received. Latest final before `LlmRequest` wins (the one the
    /// response is based on).
    SttFinal,
    /// Turn state machine decided the user's turn is over.
    TurnEnd,
    /// LLM request sent (bytes on the wire). First request of the turn; the later requests of a
    /// tool loop are timed per round by [`TimelineRecorder::llm_request`].
    LlmRequest,
    /// LLM: first token (text or tool-call delta) received.
    LlmFirstToken,
    /// Segmenter: first speakable chunk (sentence/clause) ready for TTS.
    LlmFirstSentence,
    /// TTS request sent.
    TtsRequest,
    /// TTS: first audio byte received.
    TtsFirstByte,
    /// Transport: first bot audio frame of the turn handed to the output transport.
    FirstAudioOut,
    /// The turn was interrupted (barge-in accepted). Recorded via
    /// [`TimelineRecorder::interrupt`].
    Interrupted,
    /// Transport: bot audio output stopped after an interruption (playout flushed). Marked with
    /// the interrupted turn's epoch after the turn has rotated; patched into its record.
    AudioOutStopped,
    /// VAD: the stop was confirmed, at the moment of the decision (not backdated like
    /// `UserSpeechEnd`: this is `UserSpeechEnd` plus the VAD's minimum silence plus the time
    /// the VAD took). Latest stop before `TurnEnd` wins. Recorded with
    /// [`TimelineRecorder::mark_open_turn`] (the VAD does not know turn epochs).
    VadStop,
    /// STT: the `Finalize` for the VAD stop was handed to the provider session. Latest before
    /// `TurnEnd` wins. Recorded with [`TimelineRecorder::mark_open_turn`].
    SttFinalize,
    /// Segmenter: the first chunk of the response was handed to the TTS stage (once per
    /// response, so the segmenter pays one mark, not one per chunk). Together with
    /// [`Self::TtsFirstText`] it times the hop between the two stages.
    FirstChunk,
    /// TTS: the first non-blank text of the response reached the TTS stage (once per response).
    TtsFirstText,
}

/// How repeated marks of one event within a turn are resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkRule {
    /// The first occurrence wins.
    First,
    /// Later occurrences replace earlier ones until the given event has been recorded.
    LatestUntil(TurnEvent),
}

impl TurnEvent {
    /// Number of event kinds.
    pub const COUNT: usize = 17;

    /// Every event kind, in declaration (and slot) order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::UserSpeechStart,
        Self::UserSpeechEnd,
        Self::SttInterim,
        Self::SttFinal,
        Self::TurnEnd,
        Self::LlmRequest,
        Self::LlmFirstToken,
        Self::LlmFirstSentence,
        Self::TtsRequest,
        Self::TtsFirstByte,
        Self::FirstAudioOut,
        Self::Interrupted,
        Self::AudioOutStopped,
        Self::VadStop,
        Self::SttFinalize,
        Self::FirstChunk,
        Self::TtsFirstText,
    ];

    /// Stable name, for logs and metrics.
    pub const fn name(self) -> &'static str {
        match self {
            Self::UserSpeechStart => "user_speech_start",
            Self::UserSpeechEnd => "user_speech_end",
            Self::SttInterim => "stt_interim",
            Self::SttFinal => "stt_final",
            Self::TurnEnd => "turn_end",
            Self::LlmRequest => "llm_request",
            Self::LlmFirstToken => "llm_first_token",
            Self::LlmFirstSentence => "llm_first_sentence",
            Self::TtsRequest => "tts_request",
            Self::TtsFirstByte => "tts_first_byte",
            Self::FirstAudioOut => "first_audio_out",
            Self::Interrupted => "interrupted",
            Self::AudioOutStopped => "audio_out_stopped",
            Self::VadStop => "vad_stop",
            Self::SttFinalize => "stt_finalize",
            Self::FirstChunk => "first_chunk",
            Self::TtsFirstText => "tts_first_text",
        }
    }

    /// How repeated marks are resolved.
    pub const fn rule(self) -> MarkRule {
        match self {
            Self::UserSpeechEnd => MarkRule::LatestUntil(Self::TurnEnd),
            Self::SttFinal => MarkRule::LatestUntil(Self::LlmRequest),
            Self::VadStop | Self::SttFinalize => MarkRule::LatestUntil(Self::TurnEnd),
            _ => MarkRule::First,
        }
    }

    /// Whether the event naturally happens after its turn was finished (it then patches the
    /// finished record instead of counting as late).
    pub const fn after_turn(self) -> bool {
        matches!(self, Self::AudioOutStopped)
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// Timestamps of one LLM request of a turn (one tool-loop round).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RoundMarks {
    request: Option<Instant>,
    first_token: Option<Instant>,
    done: Option<Instant>,
}

/// Metrics of one LLM request of a turn, see [`TurnRecord::llm_rounds`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LlmRoundMetrics {
    /// Zero-based index of the request within its turn.
    pub round: usize,
    /// `first token − request`: time to the first text or tool-call delta of this request.
    pub time_to_first_token: Option<Duration>,
    /// `done − request`: time until this request's response was complete.
    pub duration: Option<Duration>,
}

/// The timestamps of one turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnRecord {
    epoch: TurnEpoch,
    marks: [Option<Instant>; TurnEvent::COUNT],
    rounds: [RoundMarks; MAX_LLM_ROUNDS],
    round_count: usize,
}

impl TurnRecord {
    fn new(epoch: TurnEpoch) -> Self {
        Self {
            epoch,
            marks: [None; TurnEvent::COUNT],
            rounds: [RoundMarks::default(); MAX_LLM_ROUNDS],
            round_count: 0,
        }
    }

    /// Metrics of every LLM request of the turn, in order (the first is the request behind
    /// [`TurnEvent::LlmRequest`]). Empty when the turn made no LLM request.
    pub fn llm_rounds(&self) -> impl Iterator<Item = LlmRoundMetrics> + '_ {
        self.rounds
            .iter()
            .take(self.round_count)
            .enumerate()
            .map(|(round, marks)| round_metrics(round, marks))
    }

    /// Metrics of LLM request `round` of the turn, if it was timed.
    pub fn llm_round(&self, round: usize) -> Option<LlmRoundMetrics> {
        if round >= self.round_count {
            return None;
        }
        self.rounds
            .get(round)
            .map(|marks| round_metrics(round, marks))
    }

    /// When LLM request `round` of the turn was sent, if it was timed (for exporters that need
    /// the round's absolute span; [`Self::llm_round`] gives its durations).
    pub fn llm_round_request_at(&self, round: usize) -> Option<Instant> {
        if round >= self.round_count {
            return None;
        }
        self.rounds.get(round).and_then(|marks| marks.request)
    }

    /// Number of LLM requests timed in this turn.
    pub const fn llm_round_count(&self) -> usize {
        self.round_count
    }

    /// Opens the next round at `at`; `None` when the turn is full ([`MAX_LLM_ROUNDS`]).
    fn open_round(&mut self, at: Instant) -> Option<usize> {
        let round = self.round_count;
        let slot = self.rounds.get_mut(round)?;
        *slot = RoundMarks {
            request: Some(at),
            ..RoundMarks::default()
        };
        self.round_count = round.saturating_add(1);
        Some(round)
    }

    /// First-wins mark of a round's first token or completion. Returns whether it changed.
    fn mark_round(&mut self, round: usize, event: RoundEvent, at: Instant) -> bool {
        if round >= self.round_count {
            return false;
        }
        let Some(slot) = self.rounds.get_mut(round) else {
            return false;
        };
        let field = match event {
            RoundEvent::FirstToken => &mut slot.first_token,
            RoundEvent::Done => &mut slot.done,
        };
        if field.is_some() {
            return false;
        }
        *field = Some(at);
        true
    }

    /// The turn's epoch.
    pub const fn epoch(&self) -> TurnEpoch {
        self.epoch
    }

    /// When `event` happened in this turn, if it did.
    pub fn at(&self, event: TurnEvent) -> Option<Instant> {
        self.marks.get(event.index()).copied().flatten()
    }

    /// Recorded events in slot order with their offset from the earliest mark of the turn.
    pub fn events(&self) -> impl Iterator<Item = (TurnEvent, Duration)> + '_ {
        let origin = self.marks.iter().flatten().min().copied();
        TurnEvent::ALL.into_iter().filter_map(move |event| {
            let at = self.at(event)?;
            Some((event, at.saturating_duration_since(origin?)))
        })
    }

    /// Time from `from` to `to`, if both happened and in that order.
    pub fn between(&self, from: TurnEvent, to: TurnEvent) -> Option<Duration> {
        let (from, to) = (self.at(from)?, self.at(to)?);
        to.checked_duration_since(from)
    }

    /// The derived stage latencies defined in `docs/LATENCY.md`.
    pub fn metrics(&self) -> TurnMetrics {
        use TurnEvent as E;
        TurnMetrics {
            end_of_turn_delay: self.between(E::UserSpeechEnd, E::TurnEnd),
            stt_final_delay: self.between(E::UserSpeechEnd, E::SttFinal),
            llm_time_to_first_token: self.between(E::LlmRequest, E::LlmFirstToken),
            llm_time_to_first_sentence: self.between(E::LlmRequest, E::LlmFirstSentence),
            tts_time_to_first_byte: self.between(E::TtsRequest, E::TtsFirstByte),
            post_endpoint_latency: self.between(E::TurnEnd, E::FirstAudioOut),
            response_latency: self.between(E::UserSpeechEnd, E::FirstAudioOut),
            time_to_silence: self.between(E::Interrupted, E::AudioOutStopped),
        }
    }

    /// Applies `event`'s [`MarkRule`]. Returns whether the record changed.
    fn mark(&mut self, event: TurnEvent, at: Instant) -> bool {
        let replace = match event.rule() {
            MarkRule::First => self.at(event).is_none(),
            MarkRule::LatestUntil(until) => self.at(until).is_none(),
        };
        match self.marks.get_mut(event.index()) {
            Some(slot) if replace || slot.is_none() => {
                *slot = Some(at);
                true
            }
            _ => false,
        }
    }
}

/// The per-round events of [`TimelineRecorder::llm_first_token`] and [`TimelineRecorder::llm_done`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoundEvent {
    FirstToken,
    Done,
}

/// Derives a round's metrics from its marks (`None` where an endpoint is missing or reversed).
fn round_metrics(round: usize, marks: &RoundMarks) -> LlmRoundMetrics {
    LlmRoundMetrics {
        round,
        time_to_first_token: span(marks.request, marks.first_token),
        duration: span(marks.request, marks.done),
    }
}

/// `to − from` when both happened and `to` is not before `from`.
fn span(from: Option<Instant>, to: Option<Instant>) -> Option<Duration> {
    to?.checked_duration_since(from?)
}

/// Stage latencies of one turn (`None` where an endpoint is missing or reversed).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnMetrics {
    /// `TurnEnd − UserSpeechEnd`: time the end-of-turn policy waited.
    pub end_of_turn_delay: Option<Duration>,
    /// `SttFinal − UserSpeechEnd`.
    pub stt_final_delay: Option<Duration>,
    /// `LlmFirstToken − LlmRequest`.
    pub llm_time_to_first_token: Option<Duration>,
    /// `LlmFirstSentence − LlmRequest`.
    pub llm_time_to_first_sentence: Option<Duration>,
    /// `TtsFirstByte − TtsRequest`.
    pub tts_time_to_first_byte: Option<Duration>,
    /// `FirstAudioOut − TurnEnd`: everything after the end-of-turn decision (providers +
    /// internal overhead).
    pub post_endpoint_latency: Option<Duration>,
    /// `FirstAudioOut − UserSpeechEnd`: server-side voice-to-voice (excludes transport delay).
    pub response_latency: Option<Duration>,
    /// `AudioOutStopped − Interrupted`: how fast the bot went silent after a barge-in.
    pub time_to_silence: Option<Duration>,
}

/// Recorder statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TimelineStats {
    /// Marks that changed a record. Each recorder call counts once: an `llm_*` call that changes
    /// the turn mark or its round counts as one recorded mark, not two.
    pub recorded: u64,
    /// Marks ignored by the event's [`MarkRule`]. Each `llm_*` call that changes neither counts
    /// once.
    pub repeated: u64,
    /// Marks for an epoch older than the current turn (or already finished).
    pub late: u64,
    /// Finished turns evicted from the bounded history before being drained.
    pub evicted: u64,
    /// LLM requests of a turn beyond [`MAX_LLM_ROUNDS`], which get no round record.
    pub llm_rounds_dropped: u64,
}

struct State {
    current: Option<TurnRecord>,
    /// Epoch of the most recently finished turn.
    last_finished: Option<TurnEpoch>,
    history: VecDeque<TurnRecord>,
    stats: TimelineStats,
}

impl State {
    /// Moves `current` to the history; returns it for the summary event.
    fn finish_current(&mut self) -> Option<TurnRecord> {
        let record = self.current.take()?;
        if self.history.len() >= HISTORY_CAPACITY {
            self.history.pop_front();
            self.stats.evicted += 1;
        }
        self.history.push_back(record);
        self.last_finished = Some(record.epoch);
        Some(record)
    }

    /// Makes `epoch` the current turn, finishing an older one. Returns the finished record, or
    /// `None` (outer) when `epoch` is late.
    fn enter(&mut self, epoch: TurnEpoch) -> Option<Option<TurnRecord>> {
        match self.current.map(|c| c.epoch) {
            Some(current) if epoch == current => Some(None),
            Some(current) if epoch < current => None,
            _ if self.last_finished.is_some_and(|last| epoch <= last) => None,
            _ => {
                let finished = self.finish_current();
                self.current = Some(TurnRecord::new(epoch));
                Some(finished)
            }
        }
    }

    fn record(&mut self, event: TurnEvent, at: Instant) -> bool {
        let changed = self.current.as_mut().is_some_and(|c| c.mark(event, at));
        self.stats.count(changed);
        changed
    }
}

impl TimelineStats {
    /// Counts one mark: recorded when it changed the record, repeated otherwise.
    fn count(&mut self, changed: bool) {
        if changed {
            self.recorded += 1;
        } else {
            self.repeated += 1;
        }
    }
}

/// Told by a [`TimelineRecorder`] about turns that finished, for exporters (metrics, traces).
///
/// Called once per turn, outside the recorder's lock, on whichever task marked the event that
/// finished the turn (a stage of the call's pipeline): implementations must be quick and must
/// not block (bump atomics, push to a bounded queue).
pub trait TimelineSink: Send + Sync {
    /// A turn finished: a newer turn began, or the recorder was [`finish`](TimelineRecorder::finish)ed.
    /// The record is final except for events that happen after the turn
    /// ([`TurnEvent::after_turn`], and a barge-in recorded late), which arrive through
    /// [`Self::turn_patched`].
    fn turn_finished(&self, record: &TurnRecord);

    /// A finished turn gained an event after the fact (`record` is the patched copy). The default
    /// ignores it.
    fn turn_patched(&self, _record: &TurnRecord) {}
}

/// Shared per-session timeline. Cheap to clone; every stage holds one.
#[derive(Clone)]
pub struct TimelineRecorder {
    state: Arc<Mutex<State>>,
    /// Told of every finished turn; `None` for a recorder nobody observes.
    sink: Option<Arc<dyn TimelineSink>>,
    /// The span current when the recorder was made (the session's): the turn summaries are
    /// emitted inside it, whichever task finishes the turn, so a log line names its session.
    span: tracing::Span,
}

impl Default for TimelineRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl TimelineRecorder {
    /// An empty recorder.
    pub fn new() -> Self {
        Self {
            span: tracing::Span::current(),
            state: Arc::new(Mutex::new(State {
                current: None,
                last_finished: None,
                history: VecDeque::new(),
                stats: TimelineStats::default(),
            })),
            sink: None,
        }
    }

    /// An empty recorder that tells `sink` about every turn it finishes (and about the late
    /// patches of finished turns), see [`TimelineSink`].
    pub fn with_sink(sink: Arc<dyn TimelineSink>) -> Self {
        Self {
            sink: Some(sink),
            ..Self::new()
        }
    }

    /// Emits the summary event of a finished turn and hands it to the sink.
    fn publish(&self, record: &TurnRecord) {
        self.span.in_scope(|| emit_summary(record));
        if let Some(sink) = &self.sink {
            sink.turn_finished(record);
        }
    }

    /// Hands a finished turn that was patched afterwards to the sink.
    fn publish_patched(&self, record: &TurnRecord) {
        if let Some(sink) = &self.sink {
            sink.turn_patched(record);
        }
    }

    /// Critical sections cannot panic, so a poisoned lock still guards consistent data.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records `event` for the turn `epoch` at `at` (from the session `Clock`).
    ///
    /// Stages must pass the epoch of the frame or turn they are working on, never a guessed
    /// future one. A newer epoch finishes the current turn; an older (or finished) one is late
    /// and ignored.
    pub fn mark(&self, epoch: TurnEpoch, event: TurnEvent, at: Instant) {
        let mut state = self.lock();
        let Some(finished) = state.enter(epoch) else {
            let patched = event
                .after_turn()
                .then(|| {
                    state
                        .history
                        .iter_mut()
                        .rev()
                        .find(|r| r.epoch == epoch)
                        .and_then(|r| r.mark(event, at).then_some(*r))
                })
                .flatten();
            if let Some(record) = patched {
                state.stats.recorded += 1;
                drop(state);
                self.publish_patched(&record);
                tracing::debug!(
                    target: "brisvox::timeline",
                    epoch = epoch.get(),
                    event = event.name(),
                    "finished turn patched"
                );
            } else {
                state.stats.late += 1;
            }
            return;
        };
        let recorded = state.record(event, at);
        drop(state);
        if let Some(record) = finished {
            self.publish(&record);
        }
        if recorded {
            tracing::trace!(
                target: "brisvox::timeline",
                epoch = epoch.get(),
                event = event.name(),
                "turn event"
            );
        }
    }

    /// Records `event` at `at` on the turn in progress while it is still open (its `TurnEnd` not
    /// yet recorded), for stages that do not know turn epochs (the VAD worker, the STT stage).
    /// Anything else is late and only counted: no turn yet, the current turn already ended (a
    /// pause the user resumed after the bot's turn began belongs to no open user turn), or the
    /// recorder finished.
    ///
    /// The epoch of the open user turn is the driver's: it entered the turn when the user began
    /// to speak, long before the stop or the `Finalize` the caller marks.
    pub fn mark_open_turn(&self, event: TurnEvent, at: Instant) {
        let mut state = self.lock();
        let open = state
            .current
            .is_some_and(|turn| turn.at(TurnEvent::TurnEnd).is_none());
        if !open {
            state.stats.late += 1;
            return;
        }
        let epoch = state.current.map(|turn| turn.epoch);
        let recorded = state.record(event, at);
        drop(state);
        if recorded && let Some(epoch) = epoch {
            tracing::trace!(
                target: "brisvox::timeline",
                epoch = epoch.get(),
                event = event.name(),
                "turn event"
            );
        }
    }

    /// Records a barge-in atomically: marks `Interrupted` at `at` on turn `old` and starts turn
    /// `new`, seeding its `UserSpeechStart` with `speech_start` (the barge-in speech began while
    /// `old` was current). If another stage already moved to `new`, `Interrupted` is still
    /// attached to the finished `old` record.
    pub fn interrupt(
        &self,
        old: TurnEpoch,
        new: TurnEpoch,
        at: Instant,
        speech_start: Option<Instant>,
    ) {
        let mut state = self.lock();
        let mut summaries: [Option<TurnRecord>; 2] = [None, None];
        let mut patched = None;
        if let Some(finished) = state.enter(old) {
            summaries[0] = finished;
            state.record(TurnEvent::Interrupted, at);
        } else if let Some(record) = state.history.iter_mut().rev().find(|r| r.epoch == old) {
            // `old` was already finished by an early mark of `new`: patch it.
            if record.mark(TurnEvent::Interrupted, at) {
                patched = Some(*record);
                state.stats.recorded += 1;
                tracing::info!(
                    target: "brisvox::timeline",
                    epoch = old.get(),
                    "turn interrupted (recorded after its summary)"
                );
            }
        } else {
            state.stats.late += 1;
        }
        if let Some(finished) = state.enter(new) {
            summaries[1] = finished;
            if let Some(start) = speech_start {
                state.record(TurnEvent::UserSpeechStart, start);
            }
        }
        drop(state);
        for record in summaries.iter().flatten() {
            self.publish(record);
        }
        if let Some(record) = &patched {
            self.publish_patched(record);
        }
    }

    /// Whether `event` is already recorded for the turn `epoch` (the turn in progress or a
    /// finished one still in the history). Does not mark, does not count anything and never
    /// starts a turn: a stage uses it to skip a mark that another stage already made.
    pub fn is_marked(&self, epoch: TurnEpoch, event: TurnEvent) -> bool {
        let state = self.lock();
        state
            .current
            .iter()
            .chain(state.history.iter().rev())
            .find(|record| record.epoch() == epoch)
            .is_some_and(|record| record.at(event).is_some())
    }

    /// Runs `f` on the turn `epoch` (finishing the previous turn if needed, like [`Self::mark`]).
    /// `None` when `epoch` is late; the late event is counted.
    fn with_turn<R>(
        &self,
        epoch: TurnEpoch,
        f: impl FnOnce(&mut TurnRecord, &mut TimelineStats) -> R,
    ) -> Option<R> {
        let mut state = self.lock();
        let Some(finished) = state.enter(epoch) else {
            state.stats.late += 1;
            return None;
        };
        let State { current, stats, .. } = &mut *state;
        let out = current.as_mut().map(|turn| f(turn, stats));
        drop(state);
        if let Some(record) = finished {
            self.publish(&record);
        }
        out
    }

    /// Records the start of an LLM request of turn `epoch` at `at` and opens its round: each
    /// request of a tool loop gets its own [`LlmRoundMetrics`]. Also marks
    /// [`TurnEvent::LlmRequest`] (first wins, as before). Returns the round index to pass to
    /// [`Self::llm_first_token`] and [`Self::llm_done`], or `None` when the turn is late or full
    /// ([`MAX_LLM_ROUNDS`], counted in [`TimelineStats::llm_rounds_dropped`]).
    pub fn llm_request(&self, epoch: TurnEpoch, at: Instant) -> Option<usize> {
        self.with_turn(epoch, |turn, stats| {
            let mark = turn.mark(TurnEvent::LlmRequest, at);
            let round = turn.open_round(at);
            if round.is_none() {
                stats.llm_rounds_dropped += 1;
            }
            stats.count(mark || round.is_some());
            round
        })
        .flatten()
    }

    /// First token (text or tool-call delta) of LLM request `round` of turn `epoch`. Also marks
    /// [`TurnEvent::LlmFirstToken`] (first wins).
    pub fn llm_first_token(&self, epoch: TurnEpoch, round: usize, at: Instant) {
        self.with_turn(epoch, |turn, stats| {
            let mark = turn.mark(TurnEvent::LlmFirstToken, at);
            let round_changed = turn.mark_round(round, RoundEvent::FirstToken, at);
            stats.count(mark || round_changed);
        });
    }

    /// Completion of LLM request `round` of turn `epoch` (its response is fully received).
    pub fn llm_done(&self, epoch: TurnEpoch, round: usize, at: Instant) {
        self.with_turn(epoch, |turn, stats| {
            stats.count(turn.mark_round(round, RoundEvent::Done, at));
        });
    }

    /// Finishes the current turn. Call at session end: summaries are otherwise emitted only when
    /// a newer turn starts. Later marks for that epoch or older are late.
    pub fn finish(&self) {
        let finished = self.lock().finish_current();
        if let Some(record) = finished {
            self.publish(&record);
        }
    }

    /// The turn in progress, if any.
    pub fn current(&self) -> Option<TurnRecord> {
        self.lock().current
    }

    /// Finished turns, oldest first (at most [`HISTORY_CAPACITY`]).
    pub fn history(&self) -> Vec<TurnRecord> {
        self.lock().history.iter().copied().collect()
    }

    /// Removes and returns finished turns, oldest first (for exporters; avoids evictions).
    pub fn drain_finished(&self) -> Vec<TurnRecord> {
        self.lock().history.drain(..).collect()
    }

    /// Counters.
    pub fn stats(&self) -> TimelineStats {
        self.lock().stats
    }
}

/// Emits the per-turn summary event (outside the recorder's lock; skipped when INFO is off).
fn emit_summary(record: &TurnRecord) {
    if !tracing::enabled!(target: "brisvox::timeline", tracing::Level::INFO) {
        return;
    }
    let m = record.metrics();
    let us = |d: Option<Duration>| d.map(|d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX));
    for round in record.llm_rounds() {
        let us = |d: Option<Duration>| d.map(|d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX));
        tracing::info!(
            target: "brisvox::timeline",
            epoch = record.epoch.get(),
            llm_round = round.round,
            llm_round_ttft_us = us(round.time_to_first_token),
            llm_round_duration_us = us(round.duration),
            "llm request finished"
        );
    }
    tracing::info!(
        target: "brisvox::timeline",
        epoch = record.epoch.get(),
        interrupted = record.at(TurnEvent::Interrupted).is_some(),
        llm_rounds = record.llm_round_count(),
        end_of_turn_delay_us = us(m.end_of_turn_delay),
        stt_final_delay_us = us(m.stt_final_delay),
        llm_ttft_us = us(m.llm_time_to_first_token),
        llm_first_sentence_us = us(m.llm_time_to_first_sentence),
        tts_ttfb_us = us(m.tts_time_to_first_byte),
        post_endpoint_latency_us = us(m.post_endpoint_latency),
        response_latency_us = us(m.response_latency),
        time_to_silence_us = us(m.time_to_silence),
        "turn finished"
    );
}

impl std::fmt::Debug for TimelineRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimelineRecorder")
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
