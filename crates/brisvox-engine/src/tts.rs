//! [`TtsStage`]: speaks the segmenter's chunks through a streaming TTS provider and delivers
//! call-rate audio, word timestamps and the end of the response (`docs/PIPELINE.md` sections 1,
//! 5, 6 and 7).
//!
//! ```text
//! SegmenterStage --LlmText chunks, Flush, LlmResponse*--> TtsStage
//!                                                           |  process(): contexts, text, flush, cancel
//!                                                           v
//!                                              provider session (TtsInput / cancel / TtsEvent)
//!                                                           |
//!                                              reader task: OutputFramer (resample + 20 ms frames)
//!                                                           v
//!   UtteranceStarted, Audio (call output rate), Word, TtsEnded --> TransportOut
//! ```
//!
//! # Contexts and utterances
//! One LLM round that has text is one **utterance**: a provider context with a session-monotonic
//! id (`ContextId` and the transport's `UtteranceId` carry the same number, and so do the
//! `utterance` of `ControlKind::UtteranceStarted` and of `WordTimestamp`). The stage counts the
//! `LlmResponseStarted` frames of the epoch to know the round (the ordinal the LLM runner
//! registered in the [`PlayoutBook`]) and registers `(epoch, round) -> utterance` there. Every
//! chunk is recorded in the book **verbatim** (`push_sent`, the raw slice the segmenter stage
//! produced) before the text, normalized for speech (`Normalizer`; chunks longer than
//! [`INLINE_CHUNK_CHARS`] are normalized on the [`Offload`], not on the worker), goes to the
//! provider. A round without text opens no context.
//!
//! # Flush and the end of the response
//! A `Flush` control frame and `LlmResponseEnded` send the provider's `Flush` for the open
//! context (first, before anything is forwarded: the last words wait for it). **Exactly one
//! `TtsEnded` per response** follows `LlmResponseEnded`: once every context of the epoch has been
//! flushed by the provider (the reader task emits it right after the last audio of the last
//! context), or at once when the epoch has no open context (an empty response, a provider that
//! is down, a response whose contexts were lost). The decision is taken under one mutex shared
//! by the stage and the reader, so it is made exactly once whoever gets there last.
//!
//! # The reader task
//! The reader owns the [`OutputFramer`] (24 kHz to the call's output rate, see
//! [`CallRatesCell`]; flushed at `Flushed`, reset when its context was cancelled) and emits, in
//! provider order: `UtteranceStarted` before the first audio of an utterance (and again when the
//! audio switches between utterances), the audio as exact frames of
//! [`EngineRates::frame`](crate::EngineRates::frame) (the last one of an utterance may be
//! shorter), `Word` frames, and `TtsEnded`. Per provider chunk it allocates once (documented in
//! the framer); per frame nothing. A very long chunk is converted in pieces of
//! [`TtsStageConfig::max_piece`] with a yield in between, so one chunk never holds a worker for
//! more than about 50 us. Sends into the pipeline are abandoned as soon as an interrupt for a
//! newer epoch is queued for the stage.
//!
//! # Interrupts
//! `Interrupt` removes the ledger entries of older epochs and cancels their contexts through the
//! provider's priority `cancel` channel (`try_send`; a full channel means the provider is already
//! cancelling), then is forwarded. The reader drops events of unknown (cancelled) contexts and
//! resets its framer when it sees that the context it was framing is gone. A new epoch that
//! starts without an interrupt (the previous response finished) does the same for contexts of
//! older epochs that never finished (a dropped turn).
//!
//! # Errors and reconnects
//! - The session connects when the pipeline starts (prewarm: `TtsProvider::warm` first under a
//!   short budget of at most 400 ms, counted in `warms` / `warm_failures`, never fatal)
//!   and again when a round with text finds it dead, or earlier, when `LlmResponseStarted` of a
//!   new round finds a session that died while idle (provider idle-close; the reconnect then
//!   overlaps the LLM's time to first token). Every connect has an explicit timeout on the
//!   injected clock and is abandoned when an interrupt arrives. A failed connect starts a backoff
//!   ([`TtsStageConfig::reconnect_backoff`]): text arriving meanwhile is dropped (counted), as
//!   is the rest of the round. After more than [`TtsStageConfig::max_reconnects`] consecutive
//!   failures the stage is degraded until the next turn grants a fresh budget. A session that
//!   delivered audio, or lived 10 s, was healthy: its death does not count.
//! - A session that dies (error event, closed channel, or silence of
//!   [`TtsStageConfig::idle_timeout`] while the provider owes events: text it has not reacted
//!   to or a flush it has not confirmed, counted from the first unanswered input) loses its
//!   contexts: the reader marks each utterance **lost** in the book (the heard prefix then stops
//!   before the first untimed word and the whole message is never kept as fully heard), emits
//!   `TtsEnded` for the epochs that had asked for it, and the stage reconnects at the next round. The rest of a
//!   round whose context died is not resurrected (dropped, counted).
//! - A send to the provider that does not fit waits at most [`TtsStageConfig::send_timeout`]; a
//!   failed send marks the utterance lost. A send still waiting when an interrupt for a newer
//!   epoch is queued is abandoned at once (no `send_timeout` per frame on a barge-in); the
//!   utterance is marked lost in the book (its text did not all reach the provider) but its
//!   context stays in the ledger, so the interrupt cancels it at the provider.
//!
//! # Whitespace
//! A chunk that is only whitespace (the segmenter's overflow cut hands over the whitespace run
//! it would otherwise drop, so that the chunks still concatenate to the message) is recorded in
//! the book as sent text but never sent to the provider. With no context open yet it is kept
//! (up to [`WHITESPACE_LEAD_CAP`] bytes per round) and recorded when the round's first spoken
//! chunk opens its context.
//!
//! # Filler clips
//! A round the LLM runner cued with a clip key in the book ([`PlayoutBook::cue_clip`], a filler
//! with a pre-rendered clip) is played from the stage's
//! [`FillerClipStore`] ([`TtsStage::with_clips`]) when the clip is there at the provider's rate;
//! otherwise the provider renders the text (counted, `clip_fallbacks`). The round gets a **clip
//! context**: an id of the current session that the provider never hears of. Its text is
//! recorded in the book verbatim; at its flush the clip goes to the reader on a bounded queue
//! ([`CLIP_QUEUE`]; never awaited, a full queue drops the clip and marks it lost). The reader
//! plays it once no earlier context is in the ledger, through the same framer path as provider
//! audio (`UtteranceStarted`, frames, then the clip's words and the end of its context), so the
//! played-text cut and the one `TtsEnded` per response work unchanged. A provider context opened
//! while a clip is pending waits for it first (abandoned at a barge-in, bounded by
//! [`CLIP_WAIT_LIMIT`]): a provider streams contexts independently and would overtake the clip.
//! A stale clip is dropped like stale audio, and its id is never sent to the provider's cancel
//! channel.
//!
//! # Upstream
//! Upstream frames (`TtsStarted`, `TtsEnded` from the transport stage) are forwarded with a
//! non-blocking `try_send`, never awaited from `process`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_agent::{INLINE_CHUNK_CHARS, Language, Normalizer, normalize};
use brisvox_core::frame::{
    AudioFrame, ControlFrame, ControlKind, DataFrame, Frame, SampleRate, SystemFrame, WordTimestamp,
};
use brisvox_core::queue::TrySendError;
use brisvox_core::{
    Direction, EpochPolicy, EpochWatch, FrameSender, Processor, ProcessorContext,
    Result as CoreResult, SharedClock, TimelineRecorder, TurnEpoch, TurnEvent,
};
use brisvox_providers::{ContextId, TtsConfig, TtsEvent, TtsInput, TtsProvider, TtsSession};
use bytes::Bytes;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;

use crate::config::EngineConfig;
use crate::filler_clip::{FillerClip, FillerClipStore};
use crate::guard::TaskGuard;
use crate::language::LanguageTracker;
use crate::offload::Offload;
use crate::playout::{PlayoutBook, RoundKey};
use crate::rates::CallRatesCell;
use crate::tts_audio::OutputFramer;

/// A session that lived at least this long before dying was healthy: its death does not count
/// against the reconnect budget.
const HEALTHY_SESSION: Duration = Duration::from_secs(10);

/// Most bytes of whitespace-only text the stage keeps for a round that has no context yet (see
/// "Whitespace" in the [module docs](self)). Beyond it the whitespace is dropped (counted) and the
/// played-text truncation of that round reports `NotAPrefix` (the safe failure).
pub const WHITESPACE_LEAD_CAP: usize = 64 * 1024;

/// Clips queued for one session's reader: a filler is one round per tool round, so a
/// few suffice. Overflow policy: a clip that does not fit is dropped and its utterance marked
/// lost (`TtsStatsSnapshot::clips_dropped`); the stage never waits for room.
pub const CLIP_QUEUE: usize = 4;

/// Longest the stage holds a provider context back for a clip cued before it (see
/// `TtsStatsSnapshot::clip_waits`). The wait normally ends when the clip's audio is handed on;
/// the bound covers a reader stuck behind a full pipeline. A provider that goes silent ends
/// the wait sooner (its session dies after [`TtsStageConfig::idle_timeout`]).
pub const CLIP_WAIT_LIMIT: Duration = Duration::from_secs(30);

/// A clip context the reader is to play: the context id (also the utterance) and the audio.
#[derive(Debug)]
struct ClipPlay {
    context: u64,
    clip: Arc<FillerClip>,
}

/// Settings of the [`TtsStage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtsStageConfig {
    /// The rate the provider delivers (`TtsConfig::sample_rate`). Default: the engine's
    /// [`EngineRates::tts`](crate::EngineRates::tts), 24 kHz.
    pub tts_rate: SampleRate,
    /// Length of one output frame. Default: [`EngineRates::frame`](crate::EngineRates::frame),
    /// 20 ms.
    pub frame: Duration,
    /// Provider voice id. Default `"default"`.
    pub voice: Arc<str>,
    /// Language of the speech normalizer (numbers, dates, abbreviations). Default English.
    pub language: Language,
    /// Establishing a provider session. Default: [`Timeouts::connect`](crate::Timeouts::connect).
    pub connect_timeout: Duration,
    /// One send into a full provider channel. Default:
    /// [`Timeouts::send`](crate::Timeouts::send).
    pub send_timeout: Duration,
    /// Wait before a reconnect attempt after a failure. Default:
    /// [`Timeouts::reconnect_backoff`](crate::Timeouts::reconnect_backoff).
    pub reconnect_backoff: Duration,
    /// Consecutive failures (connects, unhealthy sessions) tolerated before the stage gives up
    /// until the next turn. Default: [`Timeouts::max_reconnects`](crate::Timeouts).
    pub max_reconnects: u32,
    /// Silence from the provider while it owes events (text it has not reacted to, or a flush it
    /// has not confirmed; an open context whose input was all answered owes nothing) after which
    /// the session is declared dead. Default: [`Timeouts::llm_idle`](crate::Timeouts::llm_idle).
    pub idle_timeout: Duration,
    /// Bound on a job the stage puts on the [`Offload`] (normalizing a long chunk, building a
    /// resampler). Default: [`Timeouts::analysis_job`](crate::Timeouts::analysis_job).
    pub offload_timeout: Duration,
    /// Longest stretch of provider audio converted without yielding to the runtime. Default
    /// 100 ms.
    pub max_piece: Duration,
}

impl TtsStageConfig {
    /// Settings derived from the engine configuration.
    pub fn from_engine(config: &EngineConfig) -> Self {
        Self {
            tts_rate: config.rates.tts,
            frame: config.rates.frame,
            voice: Arc::from("default"),
            language: Language::En,
            connect_timeout: config.timeouts.connect,
            send_timeout: config.timeouts.send,
            reconnect_backoff: config.timeouts.reconnect_backoff,
            max_reconnects: config.timeouts.max_reconnects,
            idle_timeout: config.timeouts.llm_idle,
            offload_timeout: config.timeouts.analysis_job,
            max_piece: Duration::from_millis(100),
        }
    }

    /// The same with the speech normalizer's language set.
    #[must_use]
    pub fn with_language(mut self, language: Language) -> Self {
        self.language = language;
        self
    }

    /// The same with the provider voice set.
    #[must_use]
    pub fn with_voice(mut self, voice: impl Into<Arc<str>>) -> Self {
        self.voice = voice.into();
        self
    }

    /// Samples in one frame at `rate` (at least 1).
    fn frame_samples(&self, rate: SampleRate) -> usize {
        let samples = self.frame.as_nanos().saturating_mul(u128::from(rate.hz())) / 1_000_000_000;
        usize::try_from(samples).unwrap_or(usize::MAX).max(1)
    }

    /// Bytes of provider audio in one piece (at least one sample).
    fn piece_bytes(&self) -> usize {
        let samples = self
            .max_piece
            .as_nanos()
            .saturating_mul(u128::from(self.tts_rate.hz()))
            / 1_000_000_000;
        usize::try_from(samples)
            .unwrap_or(usize::MAX)
            .max(1)
            .saturating_mul(2)
    }
}

impl Default for TtsStageConfig {
    fn default() -> Self {
        Self::from_engine(&EngineConfig::DEFAULT)
    }
}

// ---------------------------------------------------------------------------------------------
// Counters

#[derive(Debug, Default)]
struct Counters {
    chunks: AtomicU64,
    contexts: AtomicU64,
    text_sent: AtomicU64,
    chars_sent: AtomicU64,
    text_dropped: AtomicU64,
    normalized: AtomicU64,
    normalize_offloaded: AtomicU64,
    normalize_failures: AtomicU64,
    flushes: AtomicU64,
    cancels: AtomicU64,
    cancel_overflow: AtomicU64,
    connects: AtomicU64,
    connect_failures: AtomicU64,
    warms: AtomicU64,
    warm_failures: AtomicU64,
    session_deaths: AtomicU64,
    stalls: AtomicU64,
    degraded: AtomicU64,
    send_failures: AtomicU64,
    contexts_lost: AtomicU64,
    audio_events: AtomicU64,
    audio_frames: AtomicU64,
    audio_ms: AtomicU64,
    words: AtomicU64,
    unknown_events: AtomicU64,
    stale_dropped: AtomicU64,
    resampler_builds: AtomicU64,
    resampler_failures: AtomicU64,
    framing_errors: AtomicU64,
    utterance_starts: AtomicU64,
    tts_ended: AtomicU64,
    duplicate_ends: AtomicU64,
    upstream_dropped: AtomicU64,
    whitespace_chunks: AtomicU64,
    whitespace_dropped: AtomicU64,
    clips: AtomicU64,
    clips_played: AtomicU64,
    clip_fallbacks: AtomicU64,
    clips_dropped: AtomicU64,
    clip_waits: AtomicU64,
    clip_wait_timeouts: AtomicU64,
    language_reconnects: AtomicU64,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// A point-in-time copy of the [`TtsStage`] counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TtsStatsSnapshot {
    /// Text chunks received.
    pub chunks: u64,
    /// Contexts (utterances) opened.
    pub contexts: u64,
    /// Chunks sent to the provider.
    pub text_sent: u64,
    /// Unicode scalar values (chars) of the text sent to the provider, after normalization,
    /// summed over the call. Text that was not sent (dropped chunks, whitespace kept back, clip
    /// phrases the stage plays from a pre-rendered clip) is not counted. This is what a
    /// provider that bills by character bills for, up to its own counting rules.
    pub chars_sent: u64,
    /// Chunks dropped: no session, a dead context, a failed send.
    pub text_dropped: u64,
    /// Chunks whose spoken form differs from the written one.
    pub normalized: u64,
    /// Chunks normalized on the offload (longer than `INLINE_CHUNK_CHARS`).
    pub normalize_offloaded: u64,
    /// Offloaded normalizations that failed or timed out (the written text was spoken).
    pub normalize_failures: u64,
    /// `Flush` sent to the provider.
    pub flushes: u64,
    /// Contexts cancelled on the provider's priority channel.
    pub cancels: u64,
    /// Cancels the provider's full cancel channel refused.
    pub cancel_overflow: u64,
    /// Sessions established.
    pub connects: u64,
    /// Connects that failed or timed out.
    pub connect_failures: u64,
    /// Provider warm-ups (`TtsProvider::warm`) completed at Start.
    pub warms: u64,
    /// Warm-ups that failed or timed out (never fatal: the connect follows regardless).
    pub warm_failures: u64,
    /// Sessions that ended (error event, closed channel, silence).
    pub session_deaths: u64,
    /// Of those, sessions ended because the provider went silent while owing events.
    pub stalls: u64,
    /// Times the stage gave up reconnecting until the next turn.
    pub degraded: u64,
    /// Sends to the provider that failed or timed out.
    pub send_failures: u64,
    /// Contexts lost with their session (marked lost in the book).
    pub contexts_lost: u64,
    /// Audio events received from the provider.
    pub audio_events: u64,
    /// Audio frames emitted.
    pub audio_frames: u64,
    /// Milliseconds of audio emitted (truncated per frame).
    pub audio_ms: u64,
    /// Word frames emitted.
    pub words: u64,
    /// Provider events for a context the ledger no longer knows (cancelled), dropped.
    pub unknown_events: u64,
    /// Frames the reader dropped because their epoch had ended.
    pub stale_dropped: u64,
    /// Resamplers built.
    pub resampler_builds: u64,
    /// Resamplers that could not be built (the audio of that chunk was lost).
    pub resampler_failures: u64,
    /// Chunks the framer failed on.
    pub framing_errors: u64,
    /// `UtteranceStarted` frames emitted.
    pub utterance_starts: u64,
    /// `TtsEnded` frames emitted.
    pub tts_ended: u64,
    /// `LlmResponseEnded` for an epoch that already ended (no second `TtsEnded`).
    pub duplicate_ends: u64,
    /// Upstream frames dropped because the upstream lane was full.
    pub upstream_dropped: u64,
    /// Whitespace-only chunks (the segmenter's overflow cut): recorded in the book as sent text,
    /// never sent to the provider.
    pub whitespace_chunks: u64,
    /// Whitespace-only chunks that could not be recorded (no context yet and the lead already at
    /// its cap [`WHITESPACE_LEAD_CAP`]).
    pub whitespace_dropped: u64,
    /// Rounds played from a pre-rendered filler clip instead of the provider: queued
    /// for the reader at the round's flush.
    pub clips: u64,
    /// Of those, clips the reader played to the end (the rest were cut by a barge-in or lost).
    pub clips_played: u64,
    /// Rounds cued with a clip that were rendered by the provider instead: no clip store, the
    /// clip is not in it, or its rate is not the provider's.
    pub clip_fallbacks: u64,
    /// Clips dropped before they played: the epoch ended first, their session died, or the
    /// reader's clip queue ([`CLIP_QUEUE`]) was full (their utterance is then marked lost).
    pub clips_dropped: u64,
    /// Provider contexts held back until a clip cued before them was out (their audio would
    /// otherwise overtake it).
    pub clip_waits: u64,
    /// Of those, waits that reached [`CLIP_WAIT_LIMIT`] (the context was opened anyway).
    pub clip_wait_timeouts: u64,
    /// Provider sessions replaced because the response is in another language than the
    /// session (a different voice or model).
    pub language_reconnects: u64,
}

/// Shared handle on the counters of one [`TtsStage`].
#[derive(Debug, Clone)]
pub struct TtsStats {
    shared: Arc<Shared>,
}

impl TtsStats {
    /// The counters now.
    pub fn snapshot(&self) -> TtsStatsSnapshot {
        let c = &self.shared.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        TtsStatsSnapshot {
            chunks: get(&c.chunks),
            contexts: get(&c.contexts),
            text_sent: get(&c.text_sent),
            chars_sent: get(&c.chars_sent),
            text_dropped: get(&c.text_dropped),
            normalized: get(&c.normalized),
            normalize_offloaded: get(&c.normalize_offloaded),
            normalize_failures: get(&c.normalize_failures),
            flushes: get(&c.flushes),
            cancels: get(&c.cancels),
            cancel_overflow: get(&c.cancel_overflow),
            connects: get(&c.connects),
            connect_failures: get(&c.connect_failures),
            warms: get(&c.warms),
            warm_failures: get(&c.warm_failures),
            session_deaths: get(&c.session_deaths),
            stalls: get(&c.stalls),
            degraded: get(&c.degraded),
            send_failures: get(&c.send_failures),
            contexts_lost: get(&c.contexts_lost),
            audio_events: get(&c.audio_events),
            audio_frames: get(&c.audio_frames),
            audio_ms: get(&c.audio_ms),
            words: get(&c.words),
            unknown_events: get(&c.unknown_events),
            stale_dropped: get(&c.stale_dropped),
            resampler_builds: get(&c.resampler_builds),
            resampler_failures: get(&c.resampler_failures),
            framing_errors: get(&c.framing_errors),
            utterance_starts: get(&c.utterance_starts),
            tts_ended: get(&c.tts_ended),
            duplicate_ends: get(&c.duplicate_ends),
            upstream_dropped: get(&c.upstream_dropped),
            whitespace_chunks: get(&c.whitespace_chunks),
            whitespace_dropped: get(&c.whitespace_dropped),
            clips: get(&c.clips),
            clips_played: get(&c.clips_played),
            clip_fallbacks: get(&c.clip_fallbacks),
            clips_dropped: get(&c.clips_dropped),
            clip_waits: get(&c.clip_waits),
            clip_wait_timeouts: get(&c.clip_wait_timeouts),
            language_reconnects: get(&c.language_reconnects),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The ledger shared by the stage and the reader

/// A context the provider is working on.
#[derive(Debug, Clone, Copy)]
struct Live {
    epoch: TurnEpoch,
    generation: u64,
    /// Text was sent on this context and the provider has not produced an event for it since.
    text_pending: bool,
    /// The `Flush` of this context was sent and its `Flushed` has not arrived yet.
    flush_pending: bool,
    /// A pre-rendered clip the reader plays: the provider knows nothing of it.
    clip: bool,
}

/// What the ledger knows about the response of one epoch.
#[derive(Debug, Default)]
struct TurnLedger {
    /// Contexts opened and not yet flushed, cancelled or lost.
    pending: u32,
    /// `LlmResponseEnded` arrived: `TtsEnded` is owed once `pending` is zero.
    end_requested: bool,
}

/// The result of taking a context out of the ledger.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Removal {
    /// The context was in the ledger.
    known: bool,
    /// Removing it completed a response that was waiting only for it: emit `TtsEnded` for it.
    ended: Option<TurnEpoch>,
}

#[derive(Debug, Default)]
struct Ledger {
    contexts: HashMap<u64, Live>,
    turns: HashMap<TurnEpoch, TurnLedger>,
    /// The newest epoch whose `TtsEnded` was decided (a second request for it is a duplicate).
    last_ended: Option<TurnEpoch>,
}

impl Ledger {
    fn open(&mut self, id: u64, epoch: TurnEpoch, generation: u64, clip: bool) {
        self.contexts.insert(
            id,
            Live {
                epoch,
                generation,
                text_pending: false,
                flush_pending: false,
                clip,
            },
        );
        self.turns.entry(epoch).or_default().pending += 1;
    }

    /// Whether a context opened before `id` is still in the ledger (a clip waits for it: its
    /// audio must come first).
    fn earlier_open(&self, id: u64) -> bool {
        self.contexts.keys().any(|other| *other < id)
    }

    /// Whether a clip context is still in the ledger (queued, waiting or playing).
    fn clip_pending(&self) -> bool {
        self.contexts.values().any(|live| live.clip)
    }

    fn remove(&mut self, id: u64) -> Removal {
        let Some(live) = self.contexts.remove(&id) else {
            return Removal::default();
        };
        Removal {
            known: true,
            ended: self.settle(live.epoch),
        }
    }

    /// One context of `epoch` is gone; returns `Some(epoch)` if the response is now complete.
    fn settle(&mut self, epoch: TurnEpoch) -> Option<TurnEpoch> {
        let turn = self.turns.get_mut(&epoch)?;
        turn.pending = turn.pending.saturating_sub(1);
        if turn.pending == 0 && turn.end_requested {
            self.turns.remove(&epoch);
            self.note_ended(epoch);
            return Some(epoch);
        }
        None
    }

    fn note_ended(&mut self, epoch: TurnEpoch) {
        self.last_ended = Some(self.last_ended.map_or(epoch, |last| last.max(epoch)));
    }

    /// `LlmResponseEnded` of `epoch`. Returns what the caller must do.
    fn request_end(&mut self, epoch: TurnEpoch) -> EndRequest {
        if self.last_ended.is_some_and(|last| last >= epoch) {
            return EndRequest::Duplicate;
        }
        let turn = self.turns.entry(epoch).or_default();
        turn.end_requested = true;
        if turn.pending == 0 {
            self.turns.remove(&epoch);
            self.note_ended(epoch);
            EndRequest::EmitNow
        } else {
            EndRequest::Wait
        }
    }

    /// Removes every context of an epoch before `epoch`; returns the ids of those the provider
    /// knows (clip contexts are only forgotten: the reader drops a clip whose context is gone).
    fn cancel_before(&mut self, epoch: TurnEpoch) -> Vec<u64> {
        let mut provider = Vec::new();
        self.contexts.retain(|id, live| {
            if live.epoch >= epoch {
                return true;
            }
            if !live.clip {
                provider.push(*id);
            }
            false
        });
        provider.sort_unstable();
        self.turns.retain(|turn_epoch, _| *turn_epoch >= epoch);
        provider
    }

    /// The session of `generation` ended: removes its contexts. Returns their ids and the epochs
    /// whose response is now complete.
    fn fail_generation(&mut self, generation: u64) -> (Vec<u64>, Vec<TurnEpoch>) {
        let lost: Vec<(u64, TurnEpoch)> = self
            .contexts
            .iter()
            .filter(|(_, live)| live.generation == generation)
            .map(|(id, live)| (*id, live.epoch))
            .collect();
        let mut ended = Vec::new();
        for (id, epoch) in &lost {
            self.contexts.remove(id);
            if let Some(epoch) = self.settle(*epoch) {
                ended.push(epoch);
            }
        }
        ended.sort_unstable();
        (lost.into_iter().map(|(id, _)| id).collect(), ended)
    }

    /// Text is about to be sent on `id`: the provider owes a reply (any event for the context).
    fn expect_reply(&mut self, id: u64) {
        if let Some(live) = self.contexts.get_mut(&id) {
            live.text_pending = true;
        }
    }

    /// The `Flush` of `id` is about to be sent: the provider owes `Flushed`.
    fn expect_flushed(&mut self, id: u64) {
        if let Some(live) = self.contexts.get_mut(&id) {
            live.flush_pending = true;
        }
    }

    /// The provider produced audio or a word for `id`: the text sent so far was answered.
    /// Returns the context.
    fn answered(&mut self, id: u64) -> Option<Live> {
        let live = self.contexts.get_mut(&id)?;
        live.text_pending = false;
        Some(*live)
    }

    /// Whether the provider of `generation` owes an event: text it has not reacted to, or a
    /// flush it has not confirmed. An open context alone (everything sent was answered, the
    /// round is still streaming in) is **not** an obligation: silence is then the LLM's.
    fn awaiting(&self, generation: u64) -> bool {
        self.contexts
            .values()
            .any(|live| live.generation == generation && (live.text_pending || live.flush_pending))
    }
}

/// What `LlmResponseEnded` has to do after the ledger heard of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndRequest {
    /// Nothing is pending: send `TtsEnded` now.
    EmitNow,
    /// Contexts are pending: the reader sends `TtsEnded` when the last one is flushed.
    Wait,
    /// The epoch already ended.
    Duplicate,
}

#[derive(Debug, Default)]
struct Shared {
    ledger: Mutex<Ledger>,
    counters: Counters,
    /// Poked by the stage after it queued text or a flush: the reader re-evaluates whether the
    /// provider owes it events (silence detection, [`Ledger::awaiting`]).
    owed: Notify,
    /// Poked by the reader when a context left the ledger (its audio is out, or its session
    /// died): a stage waiting for a clip to play before it opens the next context looks again.
    drained: Notify,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

// ---------------------------------------------------------------------------------------------
// The stage

/// Where the current response of the stage stands.
#[derive(Debug)]
struct TurnState {
    epoch: TurnEpoch,
    /// `LlmResponseStarted` frames of this epoch seen so far (the round ordinal is this minus 1).
    rounds_started: u32,
    open: Option<OpenContext>,
    /// The first chunk of the epoch was marked on the timeline.
    marked: bool,
    /// The round whose text is being dropped (no session): it is not retried chunk by chunk.
    skipped_round: Option<u32>,
    /// Whitespace-only text that arrived before the context of `lead_round` was opened.
    lead: String,
    lead_round: u32,
    /// The clip of the open context when it is a clip context, until its flush queues it.
    clip: Option<Arc<FillerClip>>,
    /// The language of this response (an index of the call languages), locked when the
    /// epoch began.
    language: Option<usize>,
}

impl TurnState {
    fn new(epoch: TurnEpoch, language: Option<usize>) -> Self {
        Self {
            language,
            epoch,
            rounds_started: 0,
            open: None,
            marked: false,
            skipped_round: None,
            lead: String::new(),
            lead_round: 0,
            clip: None,
        }
    }

    fn round(&self) -> u32 {
        self.rounds_started.saturating_sub(1)
    }
}

/// The context the current round speaks on.
#[derive(Debug, Clone, Copy)]
struct OpenContext {
    id: u64,
    round: u32,
    generation: u64,
    /// `Flush` was sent.
    flushed: bool,
    /// The context was lost (failed send, dead session): further text of the round is dropped.
    dead: bool,
    /// A clip context: its text is recorded in the book, never sent to the provider.
    clip: bool,
}

/// The provider session the stage holds.
struct Session {
    input: mpsc::Sender<TtsInput>,
    cancel: mpsc::Sender<ContextId>,
    /// Clips for the reader to play in order with the provider's audio.
    clips: mpsc::Sender<ClipPlay>,
    reader: TaskGuard,
    generation: u64,
    since: Instant,
    /// The reader delivered audio on this session.
    healthy: Arc<AtomicBool>,
    /// When the reader found the session dead (set once, by the reader).
    died: Arc<Mutex<Option<Instant>>>,
    /// The index of the language the session speaks (its voice and model), when the call has
    /// several languages.
    language: Option<usize>,
}

/// The TTS stage. See the [module docs](self).
pub struct TtsStage<P: TtsProvider> {
    provider: Arc<P>,
    config: TtsStageConfig,
    clock: SharedClock,
    timeline: TimelineRecorder,
    book: Arc<PlayoutBook>,
    rates: CallRatesCell,
    offload: Offload,
    normalizer: Normalizer,
    shared: Arc<Shared>,
    session: Option<Session>,
    turn: Option<TurnState>,
    /// The next context id (1, 2, ...; never reused within the stage).
    next_context: u64,
    /// The next session generation.
    generation: u64,
    /// Consecutive failures counted against the reconnect budget.
    failures: u32,
    retry_at: Option<Instant>,
    degraded: bool,
    /// Responses that completed while the stage handled a frame and still owe their `TtsEnded`.
    owed_ends: Vec<TurnEpoch>,
    /// Pre-rendered filler clips.
    clips: Option<Arc<FillerClipStore>>,
    /// The call languages: the voice, model and normalizer per response.
    languages: Option<Arc<LanguageTracker>>,
}

impl<P: TtsProvider> std::fmt::Debug for TtsStage<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TtsStage")
            .field("provider", &self.provider.name())
            .field("session", &self.session.is_some())
            .field("failures", &self.failures)
            .finish_non_exhaustive()
    }
}

impl<P: TtsProvider> TtsStage<P> {
    /// A stage speaking through `provider`.
    ///
    /// Defaults for the optional parts, which a production assembly overrides: a private
    /// [`PlayoutBook`] ([`Self::with_book`]), output rate equal to the provider rate, so no
    /// resampling ([`Self::with_rates`]), and an [`Offload::Inline`] on `clock`
    /// ([`Self::with_offload`]; production passes the inference pool).
    pub fn new(
        provider: Arc<P>,
        config: TtsStageConfig,
        clock: SharedClock,
        timeline: TimelineRecorder,
    ) -> Self {
        let rates = CallRatesCell::new(brisvox_transport::CallRates::symmetric(config.tts_rate));
        Self {
            normalizer: Normalizer::new(config.language),
            offload: Offload::Inline(clock.clone()),
            provider,
            config,
            clock,
            timeline,
            book: PlayoutBook::shared(crate::playout::DEFAULT_BOOK_CAPACITY),
            rates,
            shared: Arc::new(Shared::default()),
            session: None,
            turn: None,
            next_context: 1,
            generation: 0,
            failures: 0,
            retry_at: None,
            degraded: false,
            owed_ends: Vec::new(),
            clips: None,
            languages: None,
        }
    }

    /// Speaks each response in the language the call tracker locked for its epoch: that
    /// language voice and TTS model (a session speaking another one is replaced before the
    /// response first text) and its speech normalizer. The stage starts in the tracker current
    /// language. See [`crate::language`].
    #[must_use]
    pub fn with_languages(mut self, tracker: Arc<LanguageTracker>) -> Self {
        if let Some(profile) = tracker.current_profile() {
            self.config.voice = Arc::clone(&profile.voice);
            self.config.language = profile.language;
            self.normalizer = Normalizer::new(profile.language);
        }
        self.languages = Some(tracker);
        self
    }

    /// Registers utterances and sent text in `book` (shared with the LLM runner, the transport
    /// stage and the pump).
    #[must_use]
    pub fn with_book(mut self, book: Arc<PlayoutBook>) -> Self {
        self.book = book;
        self
    }

    /// Resamples to the output rate in `rates` (published by the pump).
    #[must_use]
    pub fn with_rates(mut self, rates: CallRatesCell) -> Self {
        self.rates = rates;
        self
    }

    /// Plays the pre-rendered filler clips of `store` for the rounds the LLM runner cued with a
    /// clip key (see "Filler clips" in the [module docs](self)). The same store is the
    /// filler policy's clip cache.
    #[must_use]
    pub fn with_clips(mut self, store: Arc<FillerClipStore>) -> Self {
        self.clips = Some(store);
        self
    }

    /// Runs heavy work (long chunks to normalize, resampler construction) on `offload`.
    #[must_use]
    pub fn with_offload(mut self, offload: Offload) -> Self {
        self.offload = offload;
        self
    }

    /// A handle on this stage's counters.
    pub fn stats(&self) -> TtsStats {
        TtsStats {
            shared: Arc::clone(&self.shared),
        }
    }

    fn counters(&self) -> &Counters {
        &self.shared.counters
    }

    // -- turns --------------------------------------------------------------------------------

    /// Makes `epoch` the current response. A newer epoch than the one in progress means the
    /// previous response is over, so contexts it left behind are cancelled, and the reconnect
    /// budget is renewed.
    fn sync_turn(&mut self, epoch: TurnEpoch) {
        if self.turn.as_ref().is_some_and(|turn| turn.epoch == epoch) {
            return;
        }
        let stale = self.shared.lock().cancel_before(epoch);
        self.cancel_contexts(&stale);
        // A clip of the older epoch waiting in the reader is gone too.
        self.shared.owed.notify_one();
        let language = self.languages.as_ref().map(|t| t.language_for(epoch));
        self.turn = Some(TurnState::new(epoch, language));
        self.failures = 0;
        self.degraded = false;
        self.apply_language(language);
    }

    /// The speech normalizer follows the language of the response.
    fn apply_language(&mut self, language: Option<usize>) {
        let Some(profile) = language.and_then(|i| self.languages.as_ref()?.profile(i)) else {
            return;
        };
        if profile.language != self.config.language {
            self.config.language = profile.language;
            self.normalizer = Normalizer::new(profile.language);
        }
    }

    /// The language the next session should speak: the response, else the call current one.
    fn wanted_language(&self) -> Option<usize> {
        let tracker = self.languages.as_ref()?;
        Some(
            self.turn
                .as_ref()
                .and_then(|turn| turn.language)
                .unwrap_or_else(|| tracker.current()),
        )
    }

    /// Replaces a live session that speaks another language than the response with a new one
    /// (a voice and a model are fixed when a provider session opens). Only between responses:
    /// the language is locked per epoch, so a session never changes under an open context of
    /// the same epoch, and contexts of older epochs were cancelled by [`Self::sync_turn`].
    async fn align_language(&mut self, ctx: &ProcessorContext, epoch: TurnEpoch) {
        let Some(want) = self.wanted_language() else {
            return;
        };
        let differs = self
            .session
            .as_ref()
            .is_some_and(|session| session.language != Some(want));
        if !differs {
            return;
        }
        if self.turn.as_ref().is_some_and(|turn| turn.open.is_some()) {
            return;
        }
        tracing::info!(
            target: "brisvox::engine",
            language = self
                .languages
                .as_ref()
                .and_then(|t| t.profile(want))
                .map_or("", |p| &*p.code),
            "tts session replaced for the language of the response"
        );
        bump(&self.counters().language_reconnects);
        // Dropping the session closes it at the provider; its cancelled contexts are gone from
        // the ledger already. Replacing a session on purpose is not a death: it never counts
        // against the reconnect budget, and a session that proved healthy renews it.
        if let Some(old) = self.session.take() {
            let lived = self.clock.now().saturating_duration_since(old.since);
            if old.healthy.load(Ordering::Relaxed) || lived >= HEALTHY_SESSION {
                self.failures = 0;
            }
        }
        // The new session then connects like any other: a failure here starts the ordinary
        // backoff, counts against the budget and drops this response's text.
        self.ensure_session(ctx, Some(epoch)).await;
    }

    fn cancel_contexts(&self, ids: &[u64]) {
        let Some(session) = &self.session else {
            return;
        };
        for id in ids {
            bump(&self.counters().cancels);
            if session.cancel.try_send(ContextId(*id)).is_err() {
                bump(&self.counters().cancel_overflow);
            }
        }
    }

    // -- session ------------------------------------------------------------------------------

    fn session_alive(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| !session.reader.is_finished())
    }

    /// A live session, connecting if needed (see the module docs for the policy). `epoch` is the
    /// epoch the caller works for: the connect is abandoned when it ends.
    async fn ensure_session(&mut self, ctx: &ProcessorContext, epoch: Option<TurnEpoch>) -> bool {
        if self.session_alive() {
            return true;
        }
        if let Some(dead) = self.session.take() {
            self.account_death(&dead);
        }
        if self.degraded {
            return false;
        }
        if self.failures > self.config.max_reconnects {
            self.degraded = true;
            bump(&self.counters().degraded);
            tracing::warn!(target: "brisvox::engine", "tts degraded until the next turn");
            return false;
        }
        let now = self.clock.now();
        if self.retry_at.is_some_and(|at| now < at) {
            return false;
        }
        let language = self.wanted_language();
        let profile = language.and_then(|i| self.languages.as_ref()?.profile(i));
        let tts_config = TtsConfig {
            sample_rate: self.config.tts_rate,
            voice: profile.map_or_else(|| Arc::clone(&self.config.voice), |p| Arc::clone(&p.voice)),
            language: profile.map(|p| Arc::clone(&p.code)),
            model: profile.and_then(|p| p.tts_model.clone()),
        };
        let connect = self.provider.connect(tts_config);
        let timeout = self.clock.sleep(self.config.connect_timeout);
        let abandoned = async {
            match epoch {
                Some(epoch) => stale(ctx.epoch_watch(), epoch).await,
                None => std::future::pending::<()>().await,
            }
        };
        let connected = tokio::select! {
            biased;
            () = abandoned => return false,
            () = timeout => None,
            connected = connect => Some(connected),
        };
        match connected {
            Some(Ok(session)) => {
                // A successful connect does not renew the reconnect budget (a provider that
                // accepts the connection and drops it would be retried for ever); a healthy
                // session does (`account_death`).
                self.retry_at = None;
                self.start_session(session, ctx, language);
                true
            }
            Some(Err(error)) => {
                self.connect_failed();
                tracing::warn!(target: "brisvox::engine", %error, "tts connect failed");
                false
            }
            None => {
                self.connect_failed();
                tracing::warn!(target: "brisvox::engine", "tts connect timed out");
                false
            }
        }
    }

    /// `provider.warm()` with a deadline of `min(connect_timeout, 400 ms)` on the injected
    /// clock (a stalled warm-up must not hold the connect, nor this stage's other frames, for a
    /// whole connect timeout): DNS, TLS or a parked socket, so the connect that follows is
    /// short. Success and failure are counted; a failure never stops the connect (`warm` is an
    /// optimisation).
    async fn warm_provider(&mut self) {
        let timeout = self
            .clock
            .sleep(crate::stt::warm_budget(self.config.connect_timeout));
        tokio::select! {
            biased;
            warmed = self.provider.warm() => match warmed {
                Ok(()) => bump(&self.counters().warms),
                Err(error) => {
                    tracing::debug!(target: "brisvox::engine", %error, "tts warm-up failed");
                    bump(&self.counters().warm_failures);
                }
            },
            () = timeout => {
                tracing::debug!(target: "brisvox::engine", "tts warm-up timed out");
                bump(&self.counters().warm_failures);
            }
        }
    }

    fn connect_failed(&mut self) {
        bump(&self.counters().connect_failures);
        self.failures = self.failures.saturating_add(1);
        self.retry_at = Some(self.clock.now() + self.config.reconnect_backoff);
    }

    /// Counts the death of a session against the budget unless it was healthy. (The death
    /// itself is counted by the reader task, which sees it first.)
    fn account_death(&mut self, dead: &Session) {
        // The backoff and the lifetime count from the moment the reader saw the session die,
        // not from the moment the stage noticed (possibly turns later).
        let died = dead
            .died
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .unwrap_or_else(|| self.clock.now());
        let lived = died.saturating_duration_since(dead.since);
        if dead.healthy.load(Ordering::Relaxed) || lived >= HEALTHY_SESSION {
            self.failures = 0;
        } else {
            self.failures = self.failures.saturating_add(1);
            self.retry_at = Some(died + self.config.reconnect_backoff);
        }
    }

    fn start_session(
        &mut self,
        session: TtsSession,
        ctx: &ProcessorContext,
        language: Option<usize>,
    ) {
        bump(&self.counters().connects);
        self.generation = self.generation.saturating_add(1);
        let healthy = Arc::new(AtomicBool::new(false));
        let died = Arc::new(Mutex::new(None));
        let reader = Reader {
            generation: self.generation,
            shared: Arc::clone(&self.shared),
            downstream: ctx.sender(Direction::Downstream),
            watch: ctx.epoch_watch().cloned(),
            clock: self.clock.clone(),
            timeline: self.timeline.clone(),
            book: Arc::clone(&self.book),
            rates: self.rates.clone(),
            offload: self.offload.clone(),
            config: self.config.clone(),
            healthy: Arc::clone(&healthy),
            died: Arc::clone(&died),
            framer: None,
            last_context: None,
            seq: 0,
            marked: None,
            frames: Vec::new(),
            waiting: None,
            playing_clip: false,
        };
        let TtsSession {
            input,
            cancel,
            events,
        } = session;
        let (clips, clip_queue) = mpsc::channel(CLIP_QUEUE);
        self.session = Some(Session {
            input,
            cancel,
            clips,
            reader: TaskGuard::spawn("tts_reader", reader.run(events, clip_queue)),
            generation: self.generation,
            since: self.clock.now(),
            healthy,
            died,
            language,
        });
    }

    // -- chunks -------------------------------------------------------------------------------

    /// A whitespace-only chunk (the segmenter's overflow cut keeps the text it would drop as a
    /// chunk of its own): it is recorded as sent text so that the chunks keep concatenating to
    /// the message, but it is not spoken and opens no context.
    fn whitespace(&mut self, round: u32, raw: &Arc<str>) {
        bump(&self.counters().whitespace_chunks);
        let Some(turn) = &mut self.turn else {
            return;
        };
        if let Some(open) = turn.open
            && open.round == round
        {
            if !open.flushed {
                self.book.push_sent(open.id, raw);
            }
            return;
        }
        if turn.lead_round != round {
            turn.lead.clear();
            turn.lead_round = round;
        }
        if turn.lead.len().saturating_add(raw.len()) > WHITESPACE_LEAD_CAP {
            bump(&self.shared.counters.whitespace_dropped);
            return;
        }
        turn.lead.push_str(raw);
    }

    /// The whitespace kept for `round` before its context existed.
    fn take_lead(&mut self, round: u32) -> Option<String> {
        let turn = self.turn.as_mut()?;
        if turn.lead_round != round || turn.lead.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut turn.lead))
    }

    /// Text of the current round: opens its context if needed, records the chunk and sends it.
    async fn text(&mut self, epoch: TurnEpoch, raw: Arc<str>, ctx: &mut ProcessorContext) {
        bump(&self.counters().chunks);
        self.sync_turn(epoch);
        let round = self.turn.as_ref().map_or(0, TurnState::round);
        if raw.chars().all(char::is_whitespace) {
            self.whitespace(round, &raw);
            return;
        }
        if let Some(turn) = &mut self.turn
            && !turn.marked
        {
            turn.marked = true;
            let now = self.clock.now();
            self.timeline.mark(epoch, TurnEvent::TtsFirstText, now);
            self.timeline.mark(epoch, TurnEvent::LlmFirstSentence, now);
            self.timeline.mark(epoch, TurnEvent::TtsRequest, now);
        }
        if self
            .turn
            .as_ref()
            .is_some_and(|t| t.skipped_round == Some(round))
        {
            bump(&self.counters().text_dropped);
            return;
        }
        let needs_context = self
            .turn
            .as_ref()
            .and_then(|turn| turn.open)
            .is_none_or(|open| open.round != round);
        let opened = if needs_context {
            match self.cued_clip(epoch, round) {
                Some(clip) => self.open_clip_context(epoch, round, clip, ctx).await,
                None => self.open_context(epoch, round, ctx).await,
            }
        } else {
            true
        };
        if !opened {
            if let Some(turn) = &mut self.turn {
                turn.skipped_round = Some(round);
            }
            bump(&self.counters().text_dropped);
            return;
        }
        let Some(open) = self.turn.as_ref().and_then(|turn| turn.open) else {
            bump(&self.counters().text_dropped);
            return;
        };
        if open.flushed {
            // The runner flushes at the end of a round and starts the next one with
            // `LlmResponseStarted`; text after the flush has no context to be spoken on (the
            // provider may have closed it, and the book allows one utterance per round).
            tracing::warn!(target: "brisvox::engine", "tts text after the flush of its round; dropped");
            bump(&self.counters().text_dropped);
            return;
        }
        if open.dead || open.generation != self.current_generation() || !self.session_alive() {
            self.mark_dead(open.id);
            bump(&self.counters().text_dropped);
            return;
        }
        if open.clip {
            // The clip speaks this text: it is only recorded (verbatim, like provider text), so
            // the played-text cut works on it; the reader plays the audio after the flush.
            if let Some(lead) = self.take_lead(round) {
                self.book.push_sent(open.id, &lead);
            }
            self.book.push_sent(open.id, &raw);
            return;
        }
        let Some(spoken) = self.spoken(&raw, epoch, ctx).await else {
            bump(&self.counters().text_dropped);
            return;
        };
        if let Some(lead) = self.take_lead(round) {
            self.book.push_sent(open.id, &lead);
        }
        self.book.push_sent(open.id, &raw);
        // Before the send: the provider may answer before the send call returns.
        self.shared.lock().expect_reply(open.id);
        let chars = u64::try_from(spoken.chars().count()).unwrap_or(u64::MAX);
        let sent = self
            .send_input(
                TtsInput::Text {
                    context: ContextId(open.id),
                    text: spoken,
                },
                epoch,
                ctx,
            )
            .await;
        match sent {
            SendOutcome::Sent => {
                bump(&self.counters().text_sent);
                self.counters()
                    .chars_sent
                    .fetch_add(chars, Ordering::Relaxed);
                self.shared.owed.notify_one();
            }
            SendOutcome::Stale => {
                // The epoch ended while the send waited: the text never reached the provider, so
                // the book must not count it as sent text, but the context stays in the ledger
                // so that the interrupt on its way cancels it at the provider.
                bump(&self.counters().text_dropped);
                self.book.mark_lost(open.id);
            }
            SendOutcome::Failed => {
                bump(&self.counters().text_dropped);
                self.lose_context(open.id);
            }
        }
    }

    fn current_generation(&self) -> u64 {
        self.session
            .as_ref()
            .map_or(0, |session| session.generation)
    }

    /// Opens the context of `(epoch, round)`. `false` when there is no session to speak on.
    async fn open_context(
        &mut self,
        epoch: TurnEpoch,
        round: u32,
        ctx: &mut ProcessorContext,
    ) -> bool {
        // A previous round's context that was never flushed is flushed now.
        self.flush_open(epoch, ctx).await;
        // A provider streams contexts independently: the audio of this one could overtake a
        // clip the reader has not played yet. (Before the session check: the session may die
        // while the stage waits.)
        if !self.wait_for_clips(epoch, ctx).await {
            return false;
        }
        self.align_language(ctx, epoch).await;
        if !self.ensure_session(ctx, Some(epoch)).await {
            return false;
        }
        let id = self.next_context;
        self.next_context = self.next_context.saturating_add(1);
        let generation = self.current_generation();
        self.shared.lock().open(id, epoch, generation, false);
        self.book
            .register_utterance(RoundKey::new(epoch, round), id);
        bump(&self.counters().contexts);
        if let Some(turn) = &mut self.turn {
            turn.open = Some(OpenContext {
                id,
                round,
                generation,
                flushed: false,
                dead: false,
                clip: false,
            });
            turn.clip = None;
        }
        true
    }

    /// The clip round `(epoch, round)` is to be played with: the runner cued it in the book and
    /// the store holds it at the provider's rate. A cue the stage cannot play is counted, and
    /// the text is rendered by the provider instead.
    fn cued_clip(&self, epoch: TurnEpoch, round: u32) -> Option<Arc<FillerClip>> {
        let key = self.book.clip_cue(RoundKey::new(epoch, round))?;
        let clip = self
            .clips
            .as_ref()
            .and_then(|store| store.get(&key))
            .filter(|clip| clip.rate() == self.config.tts_rate);
        if clip.is_none() {
            bump(&self.counters().clip_fallbacks);
            tracing::debug!(target: "brisvox::engine", %key, "filler clip not playable; the text is rendered");
        }
        clip
    }

    /// Holds the opening of a provider context until every clip context is out of the ledger
    /// (played, dropped, or lost with its session), so the provider's audio for it cannot come
    /// before a clip that was cued earlier. Abandoned when an interrupt for a newer epoch is
    /// queued (`false`); bounded by [`CLIP_WAIT_LIMIT`] on the injected clock, after which the
    /// context is opened anyway (counted).
    async fn wait_for_clips(&mut self, epoch: TurnEpoch, ctx: &ProcessorContext) -> bool {
        if !self.shared.lock().clip_pending() {
            return true;
        }
        bump(&self.counters().clip_waits);
        let limit = self.clock.sleep(CLIP_WAIT_LIMIT);
        tokio::pin!(limit);
        loop {
            // Registered before the check: a context that leaves in between is not missed (the
            // reader's `notify_one` also leaves a permit when nobody waits).
            let drained = self.shared.drained.notified();
            tokio::pin!(drained);
            drained.as_mut().enable();
            if !self.shared.lock().clip_pending() {
                return true;
            }
            tokio::select! {
                biased;
                () = stale(ctx.epoch_watch(), epoch) => return false,
                () = &mut limit => {
                    bump(&self.counters().clip_wait_timeouts);
                    tracing::warn!(target: "brisvox::engine", "a filler clip did not play in time; the next utterance goes ahead");
                    return true;
                }
                () = &mut drained => {}
            }
        }
    }

    /// Opens the clip context of `(epoch, round)`: a context of the current session (its reader
    /// plays the clip, after the audio of every earlier context) that the provider never hears
    /// of. `false` when there is no session.
    async fn open_clip_context(
        &mut self,
        epoch: TurnEpoch,
        round: u32,
        clip: Arc<FillerClip>,
        ctx: &mut ProcessorContext,
    ) -> bool {
        self.flush_open(epoch, ctx).await;
        self.align_language(ctx, epoch).await;
        if !self.ensure_session(ctx, Some(epoch)).await {
            return false;
        }
        let id = self.next_context;
        self.next_context = self.next_context.saturating_add(1);
        let generation = self.current_generation();
        self.shared.lock().open(id, epoch, generation, true);
        self.book
            .register_utterance(RoundKey::new(epoch, round), id);
        bump(&self.counters().contexts);
        if let Some(turn) = &mut self.turn {
            turn.open = Some(OpenContext {
                id,
                round,
                generation,
                flushed: false,
                dead: false,
                clip: true,
            });
            turn.clip = Some(clip);
        }
        true
    }

    /// The text to speak for `raw`: its normalized form. `None` if the epoch ended meanwhile.
    async fn spoken(
        &self,
        raw: &Arc<str>,
        epoch: TurnEpoch,
        ctx: &ProcessorContext,
    ) -> Option<Arc<str>> {
        let small = raw.chars().take(INLINE_CHUNK_CHARS + 1).count() <= INLINE_CHUNK_CHARS;
        if small {
            return Some(match self.normalizer.normalize(raw) {
                std::borrow::Cow::Borrowed(_) => Arc::clone(raw),
                std::borrow::Cow::Owned(text) => {
                    bump(&self.counters().normalized);
                    Arc::from(text)
                }
            });
        }
        bump(&self.counters().normalize_offloaded);
        let (text, language) = (raw.to_string(), self.config.language);
        let job = self.offload.run(self.config.offload_timeout, move || {
            normalize(&text, language).into_owned()
        });
        let outcome = tokio::select! {
            biased;
            () = stale(ctx.epoch_watch(), epoch) => return None,
            outcome = job => outcome,
        };
        match outcome {
            Ok(done) => {
                if *done.value != **raw {
                    bump(&self.counters().normalized);
                }
                Some(Arc::from(done.value))
            }
            Err(error) => {
                // The written text is spoken instead: unnormalized, but not lost.
                bump(&self.counters().normalize_failures);
                tracing::debug!(target: "brisvox::engine", %error, "offloaded normalization failed");
                Some(Arc::clone(raw))
            }
        }
    }

    /// Sends `input` to the provider: at once if there is room, else waiting at most
    /// [`TtsStageConfig::send_timeout`] (and not past the end of `epoch`).
    async fn send_input(
        &self,
        input: TtsInput,
        epoch: TurnEpoch,
        ctx: &ProcessorContext,
    ) -> SendOutcome {
        let Some(session) = &self.session else {
            return SendOutcome::Failed;
        };
        let input = match session.input.try_send(input) {
            Ok(()) => return SendOutcome::Sent,
            Err(mpsc::error::TrySendError::Closed(_)) => return SendOutcome::Failed,
            Err(mpsc::error::TrySendError::Full(input)) => input,
        };
        let timeout = self.clock.sleep(self.config.send_timeout);
        tokio::select! {
            biased;
            () = stale(ctx.epoch_watch(), epoch) => SendOutcome::Stale,
            () = timeout => {
                bump(&self.counters().send_failures);
                tracing::warn!(target: "brisvox::engine", "tts send timed out");
                SendOutcome::Failed
            }
            sent = session.input.send(input) => {
                if sent.is_err() {
                    bump(&self.counters().send_failures);
                    SendOutcome::Failed
                } else {
                    SendOutcome::Sent
                }
            }
        }
    }

    // -- flush, end ---------------------------------------------------------------------------

    /// Sends the provider's `Flush` for the open context, once.
    async fn flush_open(&mut self, epoch: TurnEpoch, ctx: &ProcessorContext) {
        let Some(open) = self.turn.as_ref().and_then(|turn| turn.open) else {
            return;
        };
        if open.flushed {
            return;
        }
        if let Some(turn) = &mut self.turn
            && let Some(open) = &mut turn.open
        {
            open.flushed = true;
        }
        if open.dead || open.generation != self.current_generation() || !self.session_alive() {
            self.mark_dead(open.id);
            return;
        }
        if open.clip {
            self.queue_clip(open.id);
            return;
        }
        bump(&self.counters().flushes);
        self.shared.lock().expect_flushed(open.id);
        let flush = TtsInput::Flush {
            context: ContextId(open.id),
        };
        match self.send_input(flush, epoch, ctx).await {
            SendOutcome::Sent => self.shared.owed.notify_one(),
            // Left in the ledger: the interrupt for the newer epoch cancels it.
            SendOutcome::Stale => {}
            SendOutcome::Failed => self.lose_context(open.id),
        }
    }

    /// Hands the open clip context's clip to the reader, which plays it once the audio of every
    /// earlier context is out. Never waits: a full queue drops the clip ([`CLIP_QUEUE`]).
    fn queue_clip(&mut self, id: u64) {
        let clip = self.turn.as_mut().and_then(|turn| turn.clip.take());
        let queued = match (clip, &self.session) {
            (Some(clip), Some(session)) => session
                .clips
                .try_send(ClipPlay { context: id, clip })
                .is_ok(),
            _ => false,
        };
        if queued {
            bump(&self.counters().clips);
            // The reader may be parked with nothing owed: wake it to look at the clip.
            self.shared.owed.notify_one();
        } else {
            bump(&self.counters().clips_dropped);
            self.lose_context(id);
        }
    }

    /// The context cannot be spoken on any more (its session died): its text will not be
    /// completed.
    fn mark_dead(&mut self, id: u64) {
        if let Some(turn) = &mut self.turn
            && let Some(open) = &mut turn.open
            && open.id == id
        {
            open.dead = true;
        }
        self.lose_context(id);
    }

    /// Takes the context out of the ledger and marks its utterance lost in the book. If that
    /// completes a response that was already waiting only for it, its `TtsEnded` is owed and
    /// goes out at the end of the current `process` call.
    fn lose_context(&mut self, id: u64) {
        let removal = self.shared.lock().remove(id);
        if removal.known {
            bump(&self.counters().contexts_lost);
            self.book.mark_lost(id);
            // A clip waiting for this context in the reader may play now.
            self.shared.owed.notify_one();
        }
        if let Some(turn) = &mut self.turn
            && let Some(open) = &mut turn.open
            && open.id == id
        {
            open.dead = true;
        }
        if let Some(epoch) = removal.ended {
            self.owed_ends.push(epoch);
        }
    }

    /// Pushes the `TtsEnded` frames that became owed while the stage handled a frame.
    async fn emit_owed_ends(&mut self, ctx: &mut ProcessorContext) -> CoreResult<()> {
        let owed = std::mem::take(&mut self.owed_ends);
        for epoch in owed {
            self.tts_ended(epoch, ctx).await?;
        }
        Ok(())
    }

    async fn tts_ended(&self, epoch: TurnEpoch, ctx: &mut ProcessorContext) -> CoreResult<()> {
        bump(&self.counters().tts_ended);
        ctx.push(control(epoch, ControlKind::TtsEnded), Direction::Downstream)
            .await
    }

    /// `LlmResponseEnded`: flush, forward the frame, then decide on `TtsEnded` (see the module
    /// docs for the order and why).
    async fn response_ended(
        &mut self,
        epoch: TurnEpoch,
        frame: Frame,
        ctx: &mut ProcessorContext,
    ) -> CoreResult<()> {
        self.sync_turn(epoch);
        self.flush_open(epoch, ctx).await;
        if let Some(turn) = &mut self.turn {
            turn.open = None;
        }
        ctx.forward(frame, Direction::Downstream).await?;
        let request = self.shared.lock().request_end(epoch);
        match request {
            EndRequest::EmitNow => self.tts_ended(epoch, ctx).await,
            EndRequest::Wait => Ok(()),
            EndRequest::Duplicate => {
                bump(&self.counters().duplicate_ends);
                Ok(())
            }
        }
    }

    /// `Interrupt` to `new_epoch`: everything of older epochs is cancelled at the provider and
    /// forgotten.
    fn interrupt(&mut self, new_epoch: TurnEpoch) {
        let ids = self.shared.lock().cancel_before(new_epoch);
        self.cancel_contexts(&ids);
        self.turn = None;
        // A clip waiting in the reader may be gone now: let the reader see it.
        self.shared.owed.notify_one();
    }
}

/// How a send to the provider ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendOutcome {
    /// The provider's channel took the input.
    Sent,
    /// The epoch ended while the send waited for room: given up, the context is still open.
    Stale,
    /// The channel is closed or the send timed out: the context cannot be completed.
    Failed,
}

/// A control frame of `epoch`.
fn control(epoch: TurnEpoch, kind: ControlKind) -> Frame {
    Frame::Control(ControlFrame { epoch, kind })
}

/// Completes when an interrupt to an epoch newer than `epoch` is queued for the watching stage;
/// never completes without a watch.
pub(crate) async fn stale(watch: Option<&EpochWatch>, epoch: TurnEpoch) {
    match watch {
        Some(watch) => watch.wait_beyond(epoch).await,
        None => std::future::pending::<()>().await,
    }
}

impl<P: TtsProvider> Processor for TtsStage<P> {
    fn name(&self) -> &'static str {
        "tts"
    }

    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::DropStale
    }

    async fn process(
        &mut self,
        frame: Frame,
        direction: Direction,
        ctx: &mut ProcessorContext,
    ) -> CoreResult<()> {
        if direction == Direction::Upstream {
            // Never awaited from `process` (processor.rs, "Pushing upstream without deadlock").
            if let Some(upstream) = ctx.sender(Direction::Upstream)
                && upstream.try_send(frame).is_err()
            {
                bump(&self.counters().upstream_dropped);
            }
            return Ok(());
        }
        match &frame {
            Frame::Data(DataFrame::LlmText(text)) => {
                if !text.text.is_empty() {
                    let (epoch, raw) = (text.epoch, Arc::clone(&text.text));
                    self.text(epoch, raw, ctx).await;
                }
            }
            Frame::Control(ControlFrame {
                epoch,
                kind: ControlKind::LlmResponseStarted,
            }) => {
                let epoch = *epoch;
                self.sync_turn(epoch);
                self.flush_open(epoch, ctx).await;
                // A session that died while the call was quiet (a provider that closes idle
                // sockets) is replaced now, while the LLM is still working on its first token,
                // instead of when the first text of the round arrives. Only a session
                // that existed and is dead: a stage that never connected, is degraded or is in
                // its backoff behaves as before (`ensure_session` applies the same policy).
                if self.session.is_some() && !self.session_alive() {
                    self.ensure_session(ctx, Some(epoch)).await;
                }
                // The caller switched language since the last response: the voice of the new
                // one is connected now, while the LLM works on its first token.
                self.align_language(ctx, epoch).await;
                if let Some(turn) = &mut self.turn {
                    turn.rounds_started = turn.rounds_started.saturating_add(1);
                    turn.open = None;
                }
                ctx.forward(frame, direction).await?;
            }
            Frame::Control(ControlFrame {
                epoch,
                kind: ControlKind::Flush,
            }) => {
                let epoch = *epoch;
                self.sync_turn(epoch);
                self.flush_open(epoch, ctx).await;
                ctx.forward(frame, direction).await?;
            }
            Frame::Control(ControlFrame {
                epoch,
                kind: ControlKind::LlmResponseEnded,
            }) => {
                let epoch = *epoch;
                self.response_ended(epoch, frame, ctx).await?;
            }
            Frame::System(SystemFrame::Start) => {
                ctx.forward(frame, direction).await?;
                // Prewarm: warm the provider and connect now so the first response has no
                // connect latency.
                self.warm_provider().await;
                self.ensure_session(ctx, None).await;
            }
            Frame::System(SystemFrame::Interrupt { new_epoch }) => {
                self.interrupt(*new_epoch);
                ctx.forward(frame, direction).await?;
            }
            _ => ctx.forward(frame, direction).await?,
        }
        self.emit_owed_ends(ctx).await
    }
}

// ---------------------------------------------------------------------------------------------
// The reader task

/// What the reader does after one provider event.
enum Flow {
    Continue,
    /// The provider reported an error: the session is over.
    Failed,
    /// The pipeline behind the stage is gone.
    Gone,
}

/// The task that turns provider events into frames. Owns the framer.
struct Reader {
    generation: u64,
    shared: Arc<Shared>,
    downstream: Option<FrameSender>,
    watch: Option<EpochWatch>,
    clock: SharedClock,
    timeline: TimelineRecorder,
    book: Arc<PlayoutBook>,
    rates: CallRatesCell,
    offload: Offload,
    config: TtsStageConfig,
    healthy: Arc<AtomicBool>,
    died: Arc<Mutex<Option<Instant>>>,
    framer: Option<OutputFramer>,
    /// The context whose audio the framer holds.
    last_context: Option<u64>,
    /// Sequence number of the next audio frame.
    seq: u64,
    /// The epoch whose first audio was marked on the timeline.
    marked: Option<TurnEpoch>,
    /// Frames of the chunk being converted (reused).
    frames: Vec<Bytes>,
    /// The clip taken from the clip queue, waiting for the earlier contexts' audio.
    waiting: Option<ClipPlay>,
    /// A clip is being played: its audio does not prove the provider healthy.
    playing_clip: bool,
}

impl Reader {
    fn counters(&self) -> &Counters {
        &self.shared.counters
    }

    async fn run(
        mut self,
        mut events: mpsc::Receiver<TtsEvent>,
        mut clips: mpsc::Receiver<ClipPlay>,
    ) {
        let idle_timeout = self.config.idle_timeout;
        let mut idle = Box::pin(self.clock.sleep(idle_timeout));
        let mut was_owing = false;
        let mut stalled = false;
        let mut clips_open = true;
        loop {
            if !self.try_clip().await {
                return;
            }
            let owes = self.shared.lock().awaiting(self.generation);
            if owes && !was_owing {
                idle.as_mut().reset(brisvox_core::clock::deadline_after(
                    self.clock.now(),
                    idle_timeout,
                ));
            }
            was_owing = owes;
            let event = tokio::select! {
                biased;
                event = events.recv() => event,
                play = clips.recv(), if clips_open && self.waiting.is_none() => {
                    match play {
                        Some(play) => self.waiting = Some(play),
                        None => clips_open = false,
                    }
                    continue;
                }
                () = &mut idle, if owes => {
                    stalled = true;
                    None
                }
                () = self.shared.owed.notified() => continue,
            };
            let Some(event) = event else {
                break;
            };
            idle.as_mut().reset(brisvox_core::clock::deadline_after(
                self.clock.now(),
                idle_timeout,
            ));
            match self.handle(event).await {
                Flow::Continue => {}
                Flow::Failed => break,
                Flow::Gone => return,
            }
        }
        self.finish_session(stalled).await;
    }

    async fn handle(&mut self, event: TtsEvent) -> Flow {
        let alive = match event {
            TtsEvent::Audio { context, audio } => self.audio(context, audio).await,
            TtsEvent::Word {
                context,
                word,
                start,
            } => self.word(context, word, start).await,
            TtsEvent::Flushed { context } => self.flushed(context).await,
            TtsEvent::Cancelled { context } => {
                if self.last_context == Some(context.0) {
                    self.last_context = None;
                    if let Some(framer) = &mut self.framer {
                        framer.reset();
                    }
                }
                true
            }
            TtsEvent::Error(error) => {
                tracing::warn!(target: "brisvox::engine", %error, "tts session failed");
                return Flow::Failed;
            }
        };
        if alive { Flow::Continue } else { Flow::Gone }
    }

    fn lookup(&self, id: u64) -> Option<Live> {
        self.shared.lock().contexts.get(&id).copied()
    }

    /// [`lookup`](Self::lookup) for an event that answers the text sent so far.
    fn lookup_answered(&self, id: u64) -> Option<Live> {
        self.shared.lock().answered(id)
    }

    fn is_stale(&self, epoch: TurnEpoch) -> bool {
        self.watch
            .as_ref()
            .is_some_and(|watch| watch.latest() > epoch)
    }

    /// Delivers `frame` of `epoch` downstream, waiting for room unless the epoch ends first.
    /// `false` when the pipeline behind the stage is gone.
    async fn send(&mut self, frame: Frame, epoch: TurnEpoch) -> bool {
        let Some(downstream) = &self.downstream else {
            return true;
        };
        // Word timestamps are exempt from the epoch rule (frame.rs): audio that already played
        // needs its words.
        let exempt = matches!(&frame, Frame::Data(DataFrame::Word(_)));
        if !exempt && self.is_stale(epoch) {
            bump(&self.counters().stale_dropped);
            return true;
        }
        let frame = match downstream.try_send(frame) {
            Ok(()) => return true,
            Err(TrySendError::Closed(_)) => return false,
            Err(TrySendError::Full(frame)) => frame,
        };
        if exempt {
            return downstream.send(frame).await.is_ok();
        }
        tokio::select! {
            biased;
            () = stale(self.watch.as_ref(), epoch) => {
                bump(&self.counters().stale_dropped);
                true
            }
            sent = downstream.send(frame) => sent.is_ok(),
        }
    }

    async fn audio(&mut self, context: ContextId, audio: Bytes) -> bool {
        bump(&self.counters().audio_events);
        self.play(context, audio).await
    }

    /// Frames and emits `audio` of `context` (a provider chunk or a whole clip).
    async fn play(&mut self, context: ContextId, audio: Bytes) -> bool {
        let Some(live) = self.lookup_answered(context.0) else {
            bump(&self.counters().unknown_events);
            return true;
        };
        if self.is_stale(live.epoch) {
            bump(&self.counters().stale_dropped);
            return true;
        }
        if audio.is_empty() {
            return true;
        }
        if !self.prepare_framer(context.0).await {
            return true;
        }
        if self.last_context != Some(context.0) && !self.switch_to(context.0, live.epoch).await {
            return false;
        }
        self.mark_first_byte(live.epoch);
        let piece = self.config.piece_bytes();
        let mut rest: &[u8] = &audio;
        while !rest.is_empty() {
            let (now, later) = rest.split_at(piece.min(rest.len()));
            rest = later;
            let Some(framer) = &mut self.framer else {
                return true;
            };
            let frames = &mut self.frames;
            if let Err(error) = framer.push(now, &mut |frame| frames.push(frame)) {
                bump(&self.shared.counters.framing_errors);
                tracing::warn!(target: "brisvox::engine", %error, "tts audio conversion failed");
                framer.reset();
                self.frames.clear();
                self.last_context = None;
                self.book.mark_lost(context.0);
                return true;
            }
            if !self.send_frames(live.epoch).await {
                return false;
            }
            if !rest.is_empty() {
                // One provider chunk never holds the worker for long.
                tokio::task::yield_now().await;
            }
        }
        true
    }

    /// Emits the frames the framer produced as audio frames of `epoch`.
    async fn send_frames(&mut self, epoch: TurnEpoch) -> bool {
        let Some(rate) = self.framer.as_ref().map(OutputFramer::output_rate) else {
            self.frames.clear();
            return true;
        };
        let mut frames = std::mem::take(&mut self.frames);
        let mut alive = true;
        for bytes in frames.drain(..) {
            if !alive {
                continue;
            }
            self.seq = self.seq.saturating_add(1);
            let Ok(frame) = AudioFrame::new(bytes, rate, self.clock.now(), self.seq, epoch) else {
                continue;
            };
            bump(&self.counters().audio_frames);
            self.counters().audio_ms.fetch_add(
                u64::try_from(frame.duration().as_millis()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
            if !self.playing_clip {
                self.healthy.store(true, Ordering::Relaxed);
            }
            alive = self.send(Frame::from(frame), epoch).await;
        }
        self.frames = frames;
        alive
    }

    /// A framer for the call's current output rate.
    async fn prepare_framer(&mut self, context: u64) -> bool {
        let out = self.rates.get().output;
        if self
            .framer
            .as_ref()
            .is_some_and(|framer| framer.output_rate() == out)
        {
            return true;
        }
        let from = self.config.tts_rate;
        let built =
            OutputFramer::build_resampler_on(&self.offload, self.config.offload_timeout, from, out)
                .await;
        match built {
            Ok(resampler) => {
                let samples = self.config.frame_samples(out);
                self.framer = Some(OutputFramer::with_resampler(out, samples, resampler));
                self.last_context = None;
                bump(&self.counters().resampler_builds);
                true
            }
            Err(error) => {
                bump(&self.counters().resampler_failures);
                tracing::warn!(
                    target: "brisvox::engine",
                    ?from,
                    to = ?out,
                    %error,
                    "cannot build the output resampler; its audio is lost"
                );
                self.book.mark_lost(context);
                false
            }
        }
    }

    /// The audio now comes from another context: finish (or drop) the one before and announce
    /// the new utterance.
    async fn switch_to(&mut self, context: u64, epoch: TurnEpoch) -> bool {
        if let Some(previous) = self.last_context.take() {
            match self.lookup(previous) {
                Some(live) => {
                    if !self.finish_tail(live.epoch).await {
                        return false;
                    }
                }
                None => {
                    if let Some(framer) = &mut self.framer {
                        framer.reset();
                    }
                }
            }
        } else if let Some(framer) = &mut self.framer {
            // Nothing of a finished stream stays behind.
            framer.reset();
        }
        self.last_context = Some(context);
        bump(&self.counters().utterance_starts);
        let started = control(epoch, ControlKind::UtteranceStarted { utterance: context });
        self.send(started, epoch).await
    }

    /// Emits what the framer still holds (the resampler's tail and the last partial frame).
    async fn finish_tail(&mut self, epoch: TurnEpoch) -> bool {
        let Some(framer) = &mut self.framer else {
            return true;
        };
        let frames = &mut self.frames;
        if let Err(error) = framer.finish(&mut |frame| frames.push(frame)) {
            bump(&self.shared.counters.framing_errors);
            tracing::warn!(target: "brisvox::engine", %error, "tts audio flush failed");
            framer.reset();
            self.frames.clear();
            return true;
        }
        self.send_frames(epoch).await
    }

    fn mark_first_byte(&mut self, epoch: TurnEpoch) {
        if self.marked != Some(epoch) {
            self.marked = Some(epoch);
            self.timeline
                .mark(epoch, TurnEvent::TtsFirstByte, self.clock.now());
        }
    }

    async fn word(&mut self, context: ContextId, word: Arc<str>, start: Duration) -> bool {
        let Some(live) = self.lookup_answered(context.0) else {
            bump(&self.counters().unknown_events);
            return true;
        };
        bump(&self.counters().words);
        let frame = Frame::Data(DataFrame::Word(Box::new(WordTimestamp {
            epoch: live.epoch,
            utterance: context.0,
            word,
            start,
        })));
        self.send(frame, live.epoch).await
    }

    async fn flushed(&mut self, context: ContextId) -> bool {
        let Some(live) = self.lookup(context.0) else {
            bump(&self.counters().unknown_events);
            return true;
        };
        if self.last_context == Some(context.0) {
            self.last_context = None;
            if !self.finish_tail(live.epoch).await {
                return false;
            }
        }
        // The ledger is updated only now, after the audio of the context was handed on: a
        // `TtsEnded` decided by it can never overtake that audio.
        let removal = self.shared.lock().remove(context.0);
        self.shared.drained.notify_one();
        match removal.ended {
            Some(epoch) => self.tts_ended(epoch).await,
            None => true,
        }
    }

    /// Plays the waiting clip once no earlier context is left in the ledger (their audio is
    /// out, or they were cancelled or lost), and drops it when its own context is gone (an
    /// interrupt, a new epoch). `false` when the pipeline behind the stage is gone.
    async fn try_clip(&mut self) -> bool {
        let Some(id) = self.waiting.as_ref().map(|play| play.context) else {
            return true;
        };
        let (known, blocked) = {
            let ledger = self.shared.lock();
            (ledger.contexts.contains_key(&id), ledger.earlier_open(id))
        };
        if known && blocked {
            return true;
        }
        let Some(play) = self.waiting.take() else {
            return true;
        };
        if !known {
            bump(&self.counters().clips_dropped);
            return true;
        }
        self.play_clip(play).await
    }

    /// The clip's audio, its words and its end, as the provider's events would bring them.
    async fn play_clip(&mut self, play: ClipPlay) -> bool {
        let ClipPlay { context, clip } = play;
        let context = ContextId(context);
        let Some(live) = self.lookup(context.0) else {
            bump(&self.counters().clips_dropped);
            return true;
        };
        if self.is_stale(live.epoch) {
            // The interrupt for the newer epoch removes the context; nothing of it is sent.
            bump(&self.counters().clips_dropped);
            return true;
        }
        self.playing_clip = true;
        let alive = self.play(context, clip.pcm().clone()).await;
        self.playing_clip = false;
        if !alive {
            return false;
        }
        for word in clip.words() {
            if !self.word(context, Arc::clone(&word.word), word.start).await {
                return false;
            }
        }
        if self.is_stale(live.epoch) {
            bump(&self.counters().clips_dropped);
        } else {
            bump(&self.counters().clips_played);
        }
        self.flushed(context).await
    }

    async fn tts_ended(&mut self, epoch: TurnEpoch) -> bool {
        bump(&self.counters().tts_ended);
        self.send(control(epoch, ControlKind::TtsEnded), epoch)
            .await
    }

    /// The session is over (error, closed, silent): what it owed is lost.
    async fn finish_session(&mut self, stalled: bool) {
        *self.died.lock().unwrap_or_else(PoisonError::into_inner) = Some(self.clock.now());
        bump(&self.counters().session_deaths);
        if stalled {
            bump(&self.counters().stalls);
            tracing::warn!(target: "brisvox::engine", "tts provider went silent while owing events");
        }
        // Audio already received is real: hand on what the framer holds.
        if let Some(id) = self.last_context.take()
            && let Some(live) = self.lookup(id)
        {
            let _ = self.finish_tail(live.epoch).await;
        }
        let (lost, ended) = self.shared.lock().fail_generation(self.generation);
        self.shared.drained.notify_one();
        for id in lost {
            bump(&self.counters().contexts_lost);
            self.book.mark_lost(id);
        }
        for epoch in ended {
            let _ = self.tts_ended(epoch).await;
        }
    }
}
