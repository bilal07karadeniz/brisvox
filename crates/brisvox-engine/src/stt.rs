//! [`SttStage`]: streams the caller's audio to an STT provider and answers every VAD stop with
//! exactly one `UtteranceFinal` (`docs/PIPELINE.md` sections 1, 2 and 7).
//!
//! ```text
//! AudioIn --audio, VAD events--> SttStage --audio (forwarded), VAD events, Transcript,
//!                                  |                 UtteranceFinal--> TurnDriver
//!                                  +--commands--> session task --SttInput--> provider
//!                                                      ^--------SttEvent-----------+
//! ```
//!
//! # What the stage does
//! - **Audio** is forwarded downstream at once (the turn driver feeds its Smart Turn ring from
//!   it) and copied to the session task through a bounded command queue. The copy is a
//!   refcounted `Bytes` clone; a full queue drops the new frame and counts it, so the user side
//!   never waits for the provider.
//! - **`UserStoppedSpeaking`** is forwarded first, then a `Finalize` is requested for it. A VAD
//!   stop overtakes queued audio (system lane) and is dated before the last speech samples have
//!   necessarily reached this stage when its inbox is slow, so the `Finalize` is held until the
//!   stage has processed an audio frame stamped at or after the stop: it can never precede the
//!   speech it finalizes. (In normal operation that frame has long been processed and the
//!   `Finalize` goes out at once.) A held `Finalize` is released by a hang-up, by the queue
//!   overflow rule below, and by [`SttStageConfig::max_finalize_defer`]: the stage arms a clock
//!   deadline (one [`DeadlineTimer`] task, the same protocol as the turn driver) for the oldest
//!   held `Finalize`, so the cap holds even when no further audio arrives.
//! - **Transcripts** (interim and final segments) become `Transcript` frames in epoch zero.
//!   Empty segments are not forwarded.
//! - **Exactly one `UtteranceFinal` per `Finalize`**, in order, whatever happens:
//!   - the provider answers (`from_finalize` and `speech_final`): the real answer;
//!   - there is no session (still connecting, down, degraded), the `Finalize` cannot be queued,
//!     or the provider's input is full: a synthetic answer at once;
//!   - the session dies (error, closed channel) with `Finalize`s outstanding: one synthetic
//!     answer each. A synthetic answer does not lose the caller's words: when the provider had
//!     sent an interim hypothesis since the last final segment, it is forwarded as a final
//!     segment just before the synthetic `UtteranceFinal` (counted as `interims_promoted`), the
//!     same fallback the turn driver applies when its finals gate expires. A REAL answer that
//!     carries no final text does the same when no final segment was forwarded since the last
//!     `UtteranceFinal` (a session that died mid-utterance leaves its interim behind, and its
//!     successor, which heard almost none of the audio, answers the `Finalize` empty);
//!   - the oldest outstanding `Finalize` is not answered within
//!     [`SttStageConfig::finalize_timeout`]: the session is declared dead (restarted), so every
//!     outstanding `Finalize` gets its synthetic answer and the provider's late answer can never
//!     be seen, let alone counted twice.
//!
//!   A `speech_final` that does not answer a `Finalize` (the provider's own endpointing) closes
//!   nothing here: its segments are forwarded and the next `UtteranceFinal` closes them (the
//!   turn driver joins the segments between two `UtteranceFinal`s into one final). A
//!   `from_finalize` result with no `Finalize` outstanding is late or duplicate: dropped and
//!   counted.
//! - **Connect and reconnect.** The session task warms the provider (`SttProvider::warm`: DNS,
//!   TLS, a parked socket) and connects when the pipeline starts (prewarm), each with an
//!   explicit timeout (the connect: [`SttStageConfig::connect_timeout`]; the warm-up: at most
//!   a few hundred milliseconds (`WARM_BUDGET`), so a stalled warm-up delays the connect by a fraction of a second, not
//!   by the connect timeout; a failed or timed-out warm-up is counted in `warm_failures` and
//!   never stops the connect), and reconnects after a dead session or a failed connect with a
//!   fixed backoff, at most [`SttStageConfig::max_reconnects`] consecutive attempts. A
//!   successful connect does NOT reset the budget (a provider that accepts the connection and
//!   then drops it would otherwise be retried for ever): the budget is restored when the session
//!   proves healthy, by delivering a final transcript segment or by living at least ten
//!   seconds.
//!   Then the stage is degraded: it stops trying until the next `UserStartedSpeaking` (the next
//!   turn) grants a fresh budget. While no session exists audio is dropped (counted) and `Finalize`s
//!   are answered synthetically, so a turn never waits for a dead STT.
//!
//! # Upstream
//! Upstream frames are forwarded with a non-blocking `try_send` (a full lane drops the frame and
//! counts it as `upstream_dropped`), never awaited from `process`: the neighbour upstream may be
//! blocked pushing into this stage's inbox (`core/processor.rs`, "Pushing upstream without
//! deadlock").
//!
//! # Concurrency
//! The provider is spoken to only by the session task, which owns the session, the ledger of
//! outstanding `Finalize`s and every frame the stage emits besides the forwarded ones; all its
//! frames and the answer ledger are therefore strictly ordered. `process` never awaits the
//! provider (the commands queue is a mutex-guarded ring that never blocks), and the task is
//! aborted when the stage is dropped, which closes the provider session.
//!
//! # Bounds
//! The command queue holds at most [`SttStageConfig::audio_capacity`] audio frames (drop-newest,
//! counted) and [`SttStageConfig::finalize_capacity`] `Finalize`s (overflow: answered
//! synthetically). Network awaits (`connect`) have an explicit timeout; sends to the provider are
//! `try_send` (a full provider input means a stuck provider and ends the session).

use std::collections::VecDeque;
use std::future::{Future, pending};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_core::frame::{
    ControlFrame, ControlKind, DataFrame, Frame, SampleRate, SystemFrame, TranscriptFrame,
};
use brisvox_core::{
    Clock, Direction, EpochPolicy, FrameSender, Processor, ProcessorContext, Result as CoreResult,
    SharedClock, TimelineRecorder, TurnEpoch, TurnEvent,
};
use brisvox_providers::{SttConfig, SttEvent, SttInput, SttProvider, SttSession};
use bytes::Bytes;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;

use crate::config::EngineConfig;
use crate::event_slot::{EventSlot, SlotOverflow};
use crate::guard::TaskGuard;
use crate::timer::DeadlineTimer;

/// The `Wake` token of the stage's deadline timer.
const WAKE_TOKEN: u32 = 2;

/// A session that lived at least this long before dying was healthy: its death does not count
/// against the reconnect budget.
const HEALTHY_SESSION: Duration = Duration::from_secs(10);

/// Settings of the [`SttStage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SttStageConfig {
    /// Sample rate of the audio the stage receives and sends to the provider (the engine's model
    /// rate: [`AudioIn`](crate::AudioIn) converts to it).
    pub sample_rate: SampleRate,
    /// BCP-47 language for the provider, or `None` for its default.
    pub language: Option<Arc<str>>,
    /// Establishing a provider session (the provider warm-up before the first one gets
    /// `min(this, 400 ms)`). Default: [`Timeouts::connect`](crate::Timeouts::connect).
    pub connect_timeout: Duration,
    /// Wait before a reconnect attempt. Default:
    /// [`Timeouts::reconnect_backoff`](crate::Timeouts::reconnect_backoff).
    pub reconnect_backoff: Duration,
    /// Consecutive failed attempts (connect failures and sessions that die) before the stage is
    /// degraded until the next turn. Default: [`Timeouts::max_reconnects`](crate::Timeouts).
    pub max_reconnects: u32,
    /// How long a `Finalize` may stay unanswered before the session is restarted and the turn
    /// gets a synthetic final. Default: [`Timeouts::finalize`](crate::Timeouts::finalize).
    pub finalize_timeout: Duration,
    /// Audio frames queued for the session task (drop-newest beyond). Default 64 (1.3 s of
    /// 20 ms frames, the provider channel capacity).
    pub audio_capacity: usize,
    /// `Finalize`s queued for the session task and outstanding at the provider (more are
    /// answered synthetically). Default 32.
    pub finalize_capacity: usize,
    /// Longest a `Finalize` is held back for the audio it finalizes (see the module docs): a
    /// clock deadline releases it even if no audio arrives. Default 1 s.
    pub max_finalize_defer: Duration,
}

impl SttStageConfig {
    /// Settings derived from the engine configuration.
    pub fn from_engine(config: &EngineConfig) -> Self {
        Self {
            sample_rate: config.rates.model,
            language: None,
            connect_timeout: config.timeouts.connect,
            reconnect_backoff: config.timeouts.reconnect_backoff,
            max_reconnects: config.timeouts.max_reconnects,
            finalize_timeout: config.timeouts.finalize,
            audio_capacity: 64,
            finalize_capacity: 32,
            max_finalize_defer: Duration::from_secs(1),
        }
    }
}

impl Default for SttStageConfig {
    fn default() -> Self {
        Self::from_engine(&EngineConfig::DEFAULT)
    }
}

/// Counters of one [`SttStage`], shared with its session task.
#[derive(Debug, Default)]
struct Counters {
    audio_in: AtomicU64,
    audio_sent: AtomicU64,
    audio_dropped_queue: AtomicU64,
    audio_dropped_down: AtomicU64,
    audio_dropped_provider: AtomicU64,
    finalizes_requested: AtomicU64,
    finalizes_deferred: AtomicU64,
    finalizes_sent: AtomicU64,
    finals_real: AtomicU64,
    finals_synthetic: AtomicU64,
    endpoint_closes: AtomicU64,
    late_answers: AtomicU64,
    transcripts: AtomicU64,
    connects: AtomicU64,
    connect_failures: AtomicU64,
    warms: AtomicU64,
    warm_failures: AtomicU64,
    session_deaths: AtomicU64,
    finalize_timeouts: AtomicU64,
    degraded: AtomicU64,
    interims_promoted: AtomicU64,
    upstream_dropped: AtomicU64,
}

/// A point-in-time copy of the [`SttStage`] counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SttStatsSnapshot {
    /// Audio frames received.
    pub audio_in: u64,
    /// Audio frames handed to the provider session.
    pub audio_sent: u64,
    /// Audio frames dropped because the command queue was full.
    pub audio_dropped_queue: u64,
    /// Audio frames dropped because there was no session (connecting, down, degraded).
    pub audio_dropped_down: u64,
    /// Audio frames dropped because the provider's input channel was full.
    pub audio_dropped_provider: u64,
    /// `Finalize`s asked for (one per forwarded VAD stop).
    pub finalizes_requested: u64,
    /// `Finalize`s that had to wait for the audio they finalize (slow inbox).
    pub finalizes_deferred: u64,
    /// `Finalize`s sent to the provider.
    pub finalizes_sent: u64,
    /// `UtteranceFinal`s that were the provider's answer.
    pub finals_real: u64,
    /// `UtteranceFinal`s the stage made up (no session, dead session, timeout, overflow).
    pub finals_synthetic: u64,
    /// `speech_final` results that did not answer a `Finalize` (provider endpointing).
    pub endpoint_closes: u64,
    /// `from_finalize` results with no `Finalize` outstanding (late or duplicate), dropped.
    pub late_answers: u64,
    /// `Transcript` frames forwarded.
    pub transcripts: u64,
    /// Sessions established.
    pub connects: u64,
    /// Connects that failed or timed out.
    pub connect_failures: u64,
    /// Provider warm-ups (`SttProvider::warm`) completed before the first connect.
    pub warms: u64,
    /// Warm-ups that failed or timed out (never fatal: the connect follows regardless).
    pub warm_failures: u64,
    /// Sessions that ended (error, close, stuck input, `Finalize` timeout).
    pub session_deaths: u64,
    /// `Finalize`s that timed out (each also a death).
    pub finalize_timeouts: u64,
    /// Times the stage gave up reconnecting until the next turn.
    pub degraded: u64,
    /// Synthetic finals that carried the latest interim hypothesis as a final segment.
    pub interims_promoted: u64,
    /// Upstream frames dropped because the upstream lane was full.
    pub upstream_dropped: u64,
}

/// Shared handle on the counters of one [`SttStage`].
#[derive(Debug, Clone)]
pub struct SttStats {
    counters: Arc<Counters>,
}

impl SttStats {
    /// The counters now.
    pub fn snapshot(&self) -> SttStatsSnapshot {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        SttStatsSnapshot {
            audio_in: get(&c.audio_in),
            audio_sent: get(&c.audio_sent),
            audio_dropped_queue: get(&c.audio_dropped_queue),
            audio_dropped_down: get(&c.audio_dropped_down),
            audio_dropped_provider: get(&c.audio_dropped_provider),
            finalizes_requested: get(&c.finalizes_requested),
            finalizes_deferred: get(&c.finalizes_deferred),
            finalizes_sent: get(&c.finalizes_sent),
            finals_real: get(&c.finals_real),
            finals_synthetic: get(&c.finals_synthetic),
            endpoint_closes: get(&c.endpoint_closes),
            late_answers: get(&c.late_answers),
            transcripts: get(&c.transcripts),
            connects: get(&c.connects),
            connect_failures: get(&c.connect_failures),
            warms: get(&c.warms),
            warm_failures: get(&c.warm_failures),
            session_deaths: get(&c.session_deaths),
            finalize_timeouts: get(&c.finalize_timeouts),
            degraded: get(&c.degraded),
            interims_promoted: get(&c.interims_promoted),
            upstream_dropped: get(&c.upstream_dropped),
        }
    }
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// What the stage tells its session task, in order.
#[derive(Debug)]
enum Cmd {
    /// Caller audio for the provider.
    Audio(Bytes),
    /// Finalize what the provider has heard so far.
    Finalize,
    /// A new turn began: a degraded stage may try to connect again.
    Kick,
}

#[derive(Debug)]
struct QueueInner {
    cmds: VecDeque<Cmd>,
    audio: usize,
    finalizes: usize,
    /// `Finalize`s that did not fit; the task answers each synthetically.
    overflowed: u32,
}

/// The bounded command queue between the stage and its session task. A mutex-guarded ring
/// (never held across an await) plus a `Notify` permit; see the module docs for the overflow
/// policies.
#[derive(Debug)]
struct CmdQueue {
    inner: Mutex<QueueInner>,
    notify: Notify,
    audio_capacity: usize,
    finalize_capacity: usize,
}

impl CmdQueue {
    fn new(audio_capacity: usize, finalize_capacity: usize) -> Self {
        let audio_capacity = audio_capacity.max(1);
        let finalize_capacity = finalize_capacity.max(1);
        Self {
            inner: Mutex::new(QueueInner {
                // Room for every command class at its cap, so a push never allocates.
                cmds: VecDeque::with_capacity(
                    audio_capacity
                        .saturating_add(finalize_capacity)
                        .saturating_add(2),
                ),
                audio: 0,
                finalizes: 0,
                overflowed: 0,
            }),
            notify: Notify::new(),
            audio_capacity,
            finalize_capacity,
        }
    }

    fn lock(&self) -> MutexGuard<'_, QueueInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues audio; `false` when the queue is full (the frame is dropped).
    fn push_audio(&self, audio: Bytes) -> bool {
        let mut inner = self.lock();
        if inner.audio >= self.audio_capacity {
            return false;
        }
        inner.audio += 1;
        inner.cmds.push_back(Cmd::Audio(audio));
        drop(inner);
        self.notify.notify_one();
        true
    }

    /// Queues a `Finalize`; `false` when it did not fit (it is then answered synthetically).
    fn push_finalize(&self) -> bool {
        let mut inner = self.lock();
        let fits = inner.finalizes < self.finalize_capacity;
        if fits {
            inner.finalizes += 1;
            inner.cmds.push_back(Cmd::Finalize);
        } else {
            inner.overflowed = inner.overflowed.saturating_add(1);
        }
        drop(inner);
        self.notify.notify_one();
        fits
    }

    fn push_kick(&self) {
        let mut inner = self.lock();
        // A kick is idempotent: one queued at the back is enough.
        if !matches!(inner.cmds.back(), Some(Cmd::Kick)) {
            inner.cmds.push_back(Cmd::Kick);
        }
        drop(inner);
        self.notify.notify_one();
    }

    /// Moves every queued command into `out` (appending), returning the number of overflowed
    /// `Finalize`s.
    fn take(&self, out: &mut VecDeque<Cmd>) -> u32 {
        let mut inner = self.lock();
        out.extend(inner.cmds.drain(..));
        inner.audio = 0;
        inner.finalizes = 0;
        std::mem::take(&mut inner.overflowed)
    }
}

/// Why a connect attempt failed (never carries a credential: provider errors are redacted).
#[derive(Debug)]
enum ConnectFailure {
    Provider(brisvox_providers::Error),
    Timeout,
}

type ConnectFuture = Pin<Box<dyn Future<Output = Result<SttSession, ConnectFailure>> + Send>>;

/// The longest a provider warm-up may delay the connect that follows it. Warming (DNS, TLS, a
/// parked socket) only pays when it is quick: a warm-up that stalls says nothing about the real
/// connect, so it gets this short budget (or the connect timeout when that is shorter), never
/// the whole connect timeout, and the connect starts right after it.
pub(crate) const WARM_BUDGET: Duration = Duration::from_millis(400);

/// The deadline of a provider warm-up for a stage with this connect timeout.
pub(crate) fn warm_budget(connect_timeout: Duration) -> Duration {
    connect_timeout.min(WARM_BUDGET)
}

/// `provider.warm()` with a deadline on the injected clock: DNS, TLS or a parked socket, so the
/// connect that follows is short. Success and failure are counted; a failure never stops the
/// connect (`warm` is an optimisation, `connect` does the real work and has its own timeout).
async fn warm_provider<P: SttProvider>(
    provider: &P,
    clock: &dyn Clock,
    timeout: Duration,
    counters: &Counters,
) {
    tokio::select! {
        biased;
        warmed = provider.warm() => match warmed {
            Ok(()) => bump(&counters.warms),
            Err(error) => {
                tracing::debug!(target: "brisvox::engine", %error, "stt warm-up failed");
                bump(&counters.warm_failures);
            }
        },
        () = brisvox_core::clock::wait_for(clock, timeout) => {
            tracing::debug!(target: "brisvox::engine", "stt warm-up timed out");
            bump(&counters.warm_failures);
        }
    }
}

/// The session task: owns the provider session and answers every `Finalize`.
struct Session<P: SttProvider> {
    provider: Arc<P>,
    config: SttStageConfig,
    clock: SharedClock,
    timeline: Option<TimelineRecorder>,
    queue: Arc<CmdQueue>,
    out: FrameSender,
    counters: Arc<Counters>,
    session: Option<SttSession>,
    connecting: Option<ConnectFuture>,
    /// The provider has not been warmed yet: the first connect attempt warms it first.
    warm_pending: bool,
    /// Request times of the `Finalize`s the provider has not answered, oldest first.
    ledger: VecDeque<Instant>,
    retry_at: Option<Instant>,
    failures: u32,
    degraded: bool,
    /// When the live session was established (for the health rule).
    connected_at: Option<Instant>,
    /// The latest interim hypothesis since the last final segment.
    interim: Option<Arc<str>>,
    /// A non-empty final segment was forwarded since the last `UtteranceFinal`. While it is
    /// `false`, a pending interim is the only record of what the caller said, so it is promoted
    /// by a real final too (a session that died mid-utterance answers its successor's
    /// `Finalize` with an empty result).
    final_forwarded: bool,
}

impl<P: SttProvider> Session<P> {
    fn begin_connect(&mut self) {
        if self.session.is_some() || self.connecting.is_some() {
            return;
        }
        let provider = Arc::clone(&self.provider);
        let clock = Arc::clone(&self.clock);
        let timeout = self.config.connect_timeout;
        let config = SttConfig {
            sample_rate: self.config.sample_rate,
            language: self.config.language.clone(),
        };
        self.retry_at = None;
        // The first attempt (the prewarm at Start) warms the provider first; reconnects do not
        // (the endpoint was just in use).
        let warm = std::mem::take(&mut self.warm_pending).then(|| Arc::clone(&self.counters));
        self.connecting = Some(Box::pin(async move {
            if let Some(counters) = warm {
                warm_provider(&*provider, &*clock, warm_budget(timeout), &counters).await;
            }
            tokio::select! {
                connected = provider.connect(config) => connected.map_err(ConnectFailure::Provider),
                () = brisvox_core::clock::wait_for(&*clock, timeout) => Err(ConnectFailure::Timeout),
            }
        }));
    }

    /// A failed attempt or a dead session: schedule the next attempt or give up until the next
    /// turn.
    fn schedule_reconnect(&mut self) {
        self.failures = self.failures.saturating_add(1);
        if self.failures > self.config.max_reconnects {
            if !self.degraded {
                self.degraded = true;
                bump(&self.counters.degraded);
                tracing::warn!(
                    target: "brisvox::engine",
                    failures = self.failures,
                    "stt gave up reconnecting until the next turn"
                );
            }
            self.retry_at = None;
        } else {
            self.retry_at = Some(self.clock.now() + self.config.reconnect_backoff);
        }
    }

    async fn emit(&mut self, frame: Frame) -> bool {
        self.out.send(frame).await.is_ok()
    }

    async fn emit_final(&mut self, real: bool) -> bool {
        bump(if real {
            &self.counters.finals_real
        } else {
            &self.counters.finals_synthetic
        });
        let interim = self.interim.take();
        let final_forwarded = std::mem::take(&mut self.final_forwarded);
        if let Some(text) = interim
            && (!real || !final_forwarded)
            && !text.trim().is_empty()
        {
            // The provider will not finish this utterance (synthetic answer), or its answer
            // carried no final text although the words were heard (a session that died
            // mid-utterance): keep the words it had heard.
            bump(&self.counters.interims_promoted);
            let frame = TranscriptFrame {
                epoch: TurnEpoch::ZERO,
                text,
                is_final: true,
                received_at: self.clock.now(),
            };
            if !self.emit(DataFrame::Transcript(frame).into()).await {
                return false;
            }
        }
        self.emit(
            ControlFrame {
                epoch: TurnEpoch::ZERO,
                kind: ControlKind::UtteranceFinal,
            }
            .into(),
        )
        .await
    }

    /// Ends the session: every outstanding `Finalize` gets its synthetic answer. Returns `false`
    /// when the pipeline is gone.
    async fn die(&mut self, why: &'static str) -> bool {
        self.session = None;
        let lived = self
            .connected_at
            .take()
            .map(|at| self.clock.now().saturating_duration_since(at));
        if lived.is_some_and(|lived| lived >= HEALTHY_SESSION) {
            self.failures = 0;
        }
        bump(&self.counters.session_deaths);
        tracing::warn!(
            target: "brisvox::engine",
            reason = why,
            outstanding = self.ledger.len(),
            "stt session ended"
        );
        self.schedule_reconnect();
        while self.ledger.pop_front().is_some() {
            if !self.emit_final(false).await {
                return false;
            }
        }
        true
    }

    async fn on_event(&mut self, event: Option<SttEvent>) -> bool {
        let Some(SttEvent::Transcript {
            text,
            is_final,
            speech_final,
            from_finalize,
        }) = event
        else {
            return self.die("provider session ended").await;
        };
        if is_final {
            // A final segment is a real result: the session is healthy.
            self.failures = 0;
        }
        if from_finalize && self.ledger.is_empty() {
            // Answers a `Finalize` that was already answered synthetically, or an unrequested
            // one: never forward it, never count it as a final.
            bump(&self.counters.late_answers);
            return true;
        }
        if !text.trim().is_empty() {
            bump(&self.counters.transcripts);
            if is_final {
                self.interim = None;
                self.final_forwarded = true;
            } else {
                self.interim = Some(Arc::clone(&text));
            }
            let frame = TranscriptFrame {
                epoch: TurnEpoch::ZERO,
                text,
                is_final,
                received_at: self.clock.now(),
            };
            if !self.emit(DataFrame::Transcript(frame).into()).await {
                return false;
            }
        }
        if speech_final {
            if from_finalize {
                self.ledger.pop_front();
                return self.emit_final(true).await;
            }
            bump(&self.counters.endpoint_closes);
        }
        true
    }

    async fn on_finalize(&mut self) -> bool {
        let now = self.clock.now();
        let Some(session) = &self.session else {
            // No session: answer at once, the turn must not wait for a dead STT.
            return self.emit_final(false).await;
        };
        if self.ledger.len() >= self.config.finalize_capacity {
            return self.emit_final(false).await;
        }
        if session.input.try_send(SttInput::Finalize).is_ok() {
            bump(&self.counters.finalizes_sent);
            if let Some(timeline) = &self.timeline {
                timeline.mark_open_turn(TurnEvent::SttFinalize, now);
            }
            self.ledger.push_back(now);
            return true;
        }
        // The provider's input is full or closed: a stuck provider. The outstanding `Finalize`s
        // and this one are answered synthetically and the session restarts.
        if !self.die("provider input stuck").await {
            return false;
        }
        self.emit_final(false).await
    }

    async fn on_audio(&mut self, audio: Bytes) -> bool {
        match &self.session {
            None => bump(&self.counters.audio_dropped_down),
            Some(session) => match session.input.try_send(SttInput::Audio(audio)) {
                Ok(()) => bump(&self.counters.audio_sent),
                Err(mpsc::error::TrySendError::Full(_)) => {
                    bump(&self.counters.audio_dropped_provider);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    bump(&self.counters.audio_dropped_provider);
                    return self.die("provider input closed").await;
                }
            },
        }
        true
    }

    fn on_kick(&mut self) {
        if self.degraded {
            self.degraded = false;
            self.failures = 0;
            self.begin_connect();
        }
    }

    async fn on_connected(&mut self, result: Result<SttSession, ConnectFailure>) {
        self.connecting = None;
        match result {
            Ok(session) => {
                bump(&self.counters.connects);
                // `failures` is deliberately kept: see the module docs ("Connect and reconnect").
                self.degraded = false;
                self.connected_at = Some(self.clock.now());
                self.retry_at = None;
                self.session = Some(session);
            }
            Err(failure) => {
                bump(&self.counters.connect_failures);
                match &failure {
                    ConnectFailure::Timeout => {
                        tracing::warn!(target: "brisvox::engine", "stt connect timed out");
                    }
                    ConnectFailure::Provider(error) => {
                        tracing::warn!(target: "brisvox::engine", %error, "stt connect failed");
                    }
                }
                self.schedule_reconnect();
            }
        }
    }

    /// The earliest time something here needs attention without any input.
    fn next_deadline(&self) -> Option<Instant> {
        let finalize = self
            .ledger
            .front()
            .map(|at| brisvox_core::clock::deadline_after(*at, self.config.finalize_timeout));
        let reconnect = if self.session.is_none() && self.connecting.is_none() {
            self.retry_at
        } else {
            None
        };
        match (finalize, reconnect) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    async fn on_deadline(&mut self) -> bool {
        let now = self.clock.now();
        if let Some(front) = self.ledger.front()
            && now >= brisvox_core::clock::deadline_after(*front, self.config.finalize_timeout)
        {
            bump(&self.counters.finalize_timeouts);
            // The late answer must not be seen: restart the session.
            return self.die("finalize timed out").await;
        }
        if self.session.is_none()
            && self.connecting.is_none()
            && self.retry_at.is_some_and(|at| now >= at)
        {
            self.begin_connect();
        }
        true
    }

    async fn run(mut self) {
        self.begin_connect();
        let mut batch: VecDeque<Cmd> = VecDeque::with_capacity(
            self.config
                .audio_capacity
                .saturating_add(self.config.finalize_capacity)
                .saturating_add(2),
        );
        loop {
            let deadline = self.next_deadline();
            tokio::select! {
                biased;
                event = next_event(&mut self.session) => {
                    if !self.on_event(event).await {
                        return;
                    }
                }
                connected = next_connect(&mut self.connecting) => {
                    self.on_connected(connected).await;
                }
                () = self.queue.notify.notified() => {
                    let overflowed = self.queue.take(&mut batch);
                    while let Some(cmd) = batch.pop_front() {
                        let alive = match cmd {
                            Cmd::Audio(audio) => self.on_audio(audio).await,
                            Cmd::Finalize => self.on_finalize().await,
                            Cmd::Kick => {
                                self.on_kick();
                                true
                            }
                        };
                        if !alive {
                            return;
                        }
                    }
                    for _ in 0..overflowed {
                        if !self.emit_final(false).await {
                            return;
                        }
                    }
                }
                () = wait_for_deadline(&self.clock, deadline) => {
                    if !self.on_deadline().await {
                        return;
                    }
                }
            }
        }
    }
}

/// The next event of the live session; pending without one. `None`: the session ended.
async fn next_event(session: &mut Option<SttSession>) -> Option<SttEvent> {
    match session {
        Some(session) => session.events.recv().await,
        None => pending().await,
    }
}

async fn next_connect(
    connecting: &mut Option<ConnectFuture>,
) -> Result<SttSession, ConnectFailure> {
    match connecting {
        Some(future) => future.as_mut().await,
        None => pending().await,
    }
}

async fn wait_for_deadline(clock: &SharedClock, deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => brisvox_core::clock::wait_until(&**clock, deadline).await,
        None => pending().await,
    }
}

/// The STT stage; see the module docs.
pub struct SttStage<P: SttProvider> {
    provider: Arc<P>,
    config: SttStageConfig,
    clock: SharedClock,
    /// Where the session task marks [`TurnEvent::SttFinalize`] ([`Self::with_timeline`]).
    timeline: Option<TimelineRecorder>,
    queue: Arc<CmdQueue>,
    counters: Arc<Counters>,
    task: Option<TaskGuard>,
    /// Timestamp of the latest audio frame handed to the session task.
    last_audio: Option<Instant>,
    /// VAD stops whose `Finalize` waits for the audio up to them: (stop time, when it arrived).
    held: VecDeque<(Instant, Instant)>,
    /// Wakes the stage when the oldest held `Finalize` has waited `max_finalize_defer`.
    slot: EventSlot<()>,
    timer: Option<DeadlineTimer>,
}

impl<P: SttProvider> SttStage<P> {
    /// A stage using `provider`. The session task starts when the pipeline starts.
    pub fn new(provider: Arc<P>, config: SttStageConfig, clock: SharedClock) -> Self {
        let queue = Arc::new(CmdQueue::new(
            config.audio_capacity,
            config.finalize_capacity,
        ));
        Self {
            provider,
            config,
            clock,
            timeline: None,
            queue,
            counters: Arc::new(Counters::default()),
            task: None,
            last_audio: None,
            held: VecDeque::new(),
            slot: EventSlot::new(4, SlotOverflow::DropOldest),
            timer: None,
        }
    }

    /// Marks [`TurnEvent::SttFinalize`] on `timeline` (the open user turn,
    /// [`TimelineRecorder::mark_open_turn`]) whenever a `Finalize` is handed to the provider
    /// session. The session assembly applies the call's.
    #[must_use]
    pub fn with_timeline(mut self, timeline: TimelineRecorder) -> Self {
        self.timeline = Some(timeline);
        self
    }

    /// Counters of this stage (usable after the stage moved into a pipeline).
    pub fn stats(&self) -> SttStats {
        SttStats {
            counters: Arc::clone(&self.counters),
        }
    }

    fn start_task(&mut self, ctx: &ProcessorContext) {
        if self.task.is_some() {
            return;
        }
        let Some(out) = ctx.sender(Direction::Downstream) else {
            tracing::warn!(
                target: "brisvox::engine",
                "stt has no downstream neighbour: no session task started"
            );
            return;
        };
        let session = Session {
            provider: Arc::clone(&self.provider),
            config: self.config.clone(),
            clock: Arc::clone(&self.clock),
            timeline: self.timeline.clone(),
            queue: Arc::clone(&self.queue),
            out,
            counters: Arc::clone(&self.counters),
            session: None,
            connecting: None,
            warm_pending: true,
            ledger: VecDeque::new(),
            retry_at: None,
            failures: 0,
            degraded: false,
            connected_at: None,
            interim: None,
            final_forwarded: false,
        };
        self.task = Some(TaskGuard::spawn("stt.session", session.run()));
    }

    fn request_finalize(&mut self) {
        bump(&self.counters.finalizes_requested);
        // A `Finalize` that does not fit is answered synthetically by the session task.
        let _ = self.queue.push_finalize();
    }

    /// Releases the held `Finalize`s whose audio has been handed on, or that waited too long.
    fn release_held(&mut self, up_to: Option<Instant>) {
        if self.held.is_empty() {
            return;
        }
        let now = self.clock.now();
        while let Some(&(at, arrived)) = self.held.front() {
            let audio_passed = up_to.is_some_and(|ts| ts >= at);
            let waited_enough =
                now.saturating_duration_since(arrived) >= self.config.max_finalize_defer;
            if !(audio_passed || waited_enough) {
                break;
            }
            self.held.pop_front();
            self.request_finalize();
        }
    }

    /// Arms the deadline timer for the oldest held `Finalize` (none: disarmed). The timer task
    /// is created the first time something is held.
    fn sync_timer(&mut self, ctx: &ProcessorContext) {
        let next = self.held.front().map(|&(_, arrived)| {
            brisvox_core::clock::deadline_after(arrived, self.config.max_finalize_defer)
        });
        if self.timer.is_none() {
            if next.is_none() {
                return;
            }
            let Some(me) = ctx.self_sender() else {
                return;
            };
            self.timer = Some(DeadlineTimer::spawn(
                Arc::clone(&self.clock),
                self.slot.clone(),
                || (),
                me,
                WAKE_TOKEN,
            ));
        }
        if let Some(timer) = self.timer.as_mut() {
            timer.set(next);
        }
    }

    fn flush_held(&mut self) {
        while self.held.pop_front().is_some() {
            self.request_finalize();
        }
    }
}

impl<P: SttProvider> std::fmt::Debug for SttStage<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SttStage")
            .field("provider", &self.provider.name())
            .field("held_finalizes", &self.held.len())
            .finish_non_exhaustive()
    }
}

impl<P: SttProvider> Processor for SttStage<P> {
    fn name(&self) -> &'static str {
        "stt"
    }

    /// The user side stays in epoch zero and survives interrupts.
    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        frame: Frame,
        direction: Direction,
        ctx: &mut ProcessorContext,
    ) -> CoreResult<()> {
        if direction == Direction::Upstream {
            // Never awaited from `process` (see the module docs, "Upstream").
            if let Some(upstream) = ctx.sender(Direction::Upstream)
                && upstream.try_send(frame).is_err()
            {
                bump(&self.counters.upstream_dropped);
            }
            return Ok(());
        }
        let result = self.downstream(frame, ctx).await;
        self.sync_timer(ctx);
        result
    }
}

impl<P: SttProvider> SttStage<P> {
    async fn downstream(&mut self, frame: Frame, ctx: &mut ProcessorContext) -> CoreResult<()> {
        match &frame {
            Frame::Data(DataFrame::Audio(audio)) => {
                self.start_task(ctx);
                bump(&self.counters.audio_in);
                let timestamp = audio.timestamp();
                if !self.queue.push_audio(audio.data().clone()) {
                    bump(&self.counters.audio_dropped_queue);
                }
                self.last_audio = Some(timestamp);
                self.release_held(Some(timestamp));
            }
            Frame::System(SystemFrame::Start) => self.start_task(ctx),
            Frame::System(SystemFrame::Wake { token }) if *token == WAKE_TOKEN => {
                let _ = self.slot.drain();
                self.release_held(None);
                return Ok(());
            }
            Frame::System(SystemFrame::UserStartedSpeaking { .. }) => {
                self.start_task(ctx);
                self.queue.push_kick();
            }
            Frame::System(SystemFrame::UserStoppedSpeaking { at }) => {
                self.start_task(ctx);
                let at = *at;
                // The stop goes first; the `Finalize` it causes follows (and so does its final).
                ctx.forward(frame, Direction::Downstream).await?;
                if self.last_audio.is_some_and(|ts| ts >= at) {
                    self.request_finalize();
                } else {
                    bump(&self.counters.finalizes_deferred);
                    if self.held.len() >= self.config.finalize_capacity {
                        self.held.pop_front();
                        self.request_finalize();
                    }
                    self.held.push_back((at, self.clock.now()));
                }
                return Ok(());
            }
            Frame::System(SystemFrame::Stop | SystemFrame::Cancel) => self.flush_held(),
            _ => {}
        }
        ctx.forward(frame, Direction::Downstream).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_queue_drops_audio_beyond_its_cap_and_overflows_finalizes() {
        let queue = CmdQueue::new(2, 1);
        assert!(queue.push_audio(Bytes::from_static(b"a")));
        assert!(queue.push_audio(Bytes::from_static(b"b")));
        assert!(!queue.push_audio(Bytes::from_static(b"c")), "audio full");
        assert!(queue.push_finalize());
        assert!(!queue.push_finalize(), "finalize queue full");
        queue.push_kick();
        queue.push_kick();
        let mut out = VecDeque::new();
        assert_eq!(queue.take(&mut out), 1, "one finalize overflowed");
        let kinds: Vec<&str> = out
            .iter()
            .map(|cmd| match cmd {
                Cmd::Audio(_) => "audio",
                Cmd::Finalize => "finalize",
                Cmd::Kick => "kick",
            })
            .collect();
        assert_eq!(
            kinds,
            ["audio", "audio", "finalize", "kick"],
            "kick coalesces"
        );
        // Taking resets the caps.
        assert!(queue.push_audio(Bytes::from_static(b"d")));
        assert!(queue.push_finalize());
    }

    #[test]
    fn config_follows_the_engine_config() {
        let config = SttStageConfig::from_engine(&EngineConfig::DEFAULT);
        assert_eq!(config.sample_rate, SampleRate::Hz16000);
        assert_eq!(config.connect_timeout, Duration::from_secs(5));
        assert_eq!(config.finalize_timeout, Duration::from_secs(3));
        assert!(config.finalize_timeout > EngineConfig::DEFAULT.final_wait);
    }
}
