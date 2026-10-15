//! [`SegmenterStage`]: cuts the LLM's text deltas into speakable chunks for TTS
//! (`docs/PIPELINE.md` sections 1 and 5).
//!
//! ```text
//! LlmRunner --LlmText deltas, Flush, LlmResponseStarted/Ended--> SegmenterStage --LlmText chunks--> TtsStage
//! ```
//!
//! # What goes out
//! The stage runs the agent's [`Segmenter`] with [`Segmenter::push_spans`] and emits, for every
//! chunk, the **raw slice** `stream[prev_end..chunk_end]`: the chunk with the whitespace before
//! it, exactly as the LLM wrote it. The slices of one response concatenate to the response text
//! byte for byte (trailing whitespace excepted), so the text the TTS stage records as sent is
//! always a prefix of the assistant message in the LLM context, which is what the played-text
//! truncation (`PlayedText::push_sent`, `apply_truncation`) needs. A trimmed chunk could not
//! promise that (it drops the whitespace between chunks and any run beyond the segmenter's cap),
//! and TTS providers want the leading space anyway (Cartesia's `continue: true` contexts).
//!
//! # When it cuts
//! - a `LlmText` delta is pushed; every chunk that became safe to speak goes out at once;
//! - `Flush` and `LlmResponseEnded` finish the stream: the pending tail goes out as the last
//!   chunk, then the control frame is forwarded, so the TTS stage sees the tail before the
//!   boundary;
//! - `LlmResponseStarted` finishes the stream too (a round that started without a `Flush` before
//!   it must not leave its tail in front of the next round's text);
//! - `Interrupt` resets: pending text was never spoken and is dropped, the next chunk is a first
//!   chunk again;
//! - text of an epoch other than the one being segmented starts a new stream (the old one's
//!   leftovers are dropped).
//!
//! The stage is [`EpochPolicy::DropStale`]: frames of an ended epoch never reach it, and it
//! pushes none.
//!
//! # First chunk
//! The first chunk of a response waits for [`SegmenterConfig::first_min_words`] words, which
//! trades time to first audio against the TTS having enough context for a natural start. The
//! default differs by language ([`SegmenterStageConfig::new`]).
//!
//! # Cost
//! `Segmenter::push_spans` scans the pending text (at most `5 x max_chunk_chars` chars) and
//! allocates the chunks it emits; the stage allocates one `Arc<str>` per chunk and keeps the
//! stream text in a `String` that holds only what has not been emitted. See the cost test in
//! `tests/segmenter_stage.rs` and `docs/LATENCY.md` ("Segmenter push cost in the stage") for the
//! measurement against the 50 us rule.
//!
//! # Memory bound
//! The segmenter bounds its own pending text, but the stage keeps the raw stream text it has not
//! cut yet, and whitespace the segmenter shortens (a run beyond its cap) still lives there. A
//! degenerate generation (a long run of spaces or newlines, with or without a word in front of
//! it) would grow that text without a chunk ever being emitted. The stage therefore ends the
//! stream, exactly like a `Flush`, when the retained text passes [`RAW_CAP_FACTOR`] times
//! `max_chunk_chars` bytes (far above the segmenter's own bound of `5 x max_chunk_chars`
//! chars): the pending tail goes out as a chunk (with the whitespace before it), the whitespace
//! after the last chunk goes out as a chunk of its own (whitespace only: the TTS stage records
//! it as sent text, with the other chunks of the round, but does not speak it; see "Whitespace"
//! in the `tts` module docs), and the next chunk is a first chunk again. Nothing is dropped, so
//! the byte-for-byte promise above holds across the cut and a played-text truncation of such a
//! response still finds the sent text to be a prefix of the message. Counted in
//! [`SegmenterStatsSnapshot::overflow_finishes`].
//!
//! # Upstream
//! Upstream frames are forwarded with a non-blocking `try_send` (counted when dropped), never
//! awaited from `process`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use brisvox_agent::{Language, Segmenter, SegmenterConfig, Span};
use brisvox_core::frame::{ControlFrame, ControlKind, DataFrame, Frame, SystemFrame, TextFrame};
use brisvox_core::{
    Direction, EpochPolicy, Processor, ProcessorContext, Result as CoreResult, SharedClock,
    TimelineRecorder, TurnEpoch, TurnEvent,
};

/// Retained raw text above which [`SegmenterStage`] ends the stream (see "Memory bound" in the
/// [module docs](self)), as a multiple of `max_chunk_chars` bytes.
pub const RAW_CAP_FACTOR: usize = 64;

/// Settings of the [`SegmenterStage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmenterStageConfig {
    /// The segmenter's settings (language, first-chunk words, chunk length limit).
    pub segmenter: SegmenterConfig,
}

impl SegmenterStageConfig {
    /// Words the first chunk of an English response must hold: one, so the first chunk ends at
    /// the earliest clause or sentence boundary. Measured
    /// (`tests/first_chunk_policy.rs`): a voice reply typically opens with a one-word clause
    /// (`Sure,` `Okay,` `Well,`) and then a long sentence; with two or three words the first
    /// chunk waits for that whole sentence. On the chaos suite's 200 reference turns the first
    /// audio came at p50 969 / p95 1262 / max 1366 ms with one word and 1137 / 1485 / 1592 ms
    /// with two or three (9 turns over the 1.5 s SLO). The TTS keeps prosody across chunks: every
    /// chunk of a response goes into the same provider context.
    pub const FIRST_MIN_WORDS_EN: usize = 1;
    /// Words the first chunk of a Turkish response must hold: one, as in English. **An
    /// assumption, not a measurement**: only the English answer shape is measured; a
    /// Turkish reply is assumed to open the same way (`Peki,` `Tamam,` and then a long
    /// sentence). The previous 2 was a starting value chosen without data too. Checked with a
    /// Turkish reference set or the live Turkish TTS tests, which also judge the
    /// prosody of a one-word first chunk.
    pub const FIRST_MIN_WORDS_TR: usize = 1;

    /// The first-chunk word count for `language`.
    pub const fn first_min_words_for(language: Language) -> usize {
        match language {
            Language::En => Self::FIRST_MIN_WORDS_EN,
            Language::Tr => Self::FIRST_MIN_WORDS_TR,
        }
    }

    /// The defaults for `language`: [`Self::first_min_words_for`] words in the first chunk and
    /// the segmenter's own chunk length limit.
    pub fn new(language: Language) -> Self {
        Self {
            segmenter: SegmenterConfig {
                first_min_words: Self::first_min_words_for(language),
                ..SegmenterConfig::new(language)
            },
        }
    }
}

impl Default for SegmenterStageConfig {
    fn default() -> Self {
        Self::new(Language::En)
    }
}

#[derive(Debug, Default)]
struct Counters {
    deltas: AtomicU64,
    chunks: AtomicU64,
    chunk_bytes: AtomicU64,
    finishes: AtomicU64,
    resets: AtomicU64,
    foreign_epoch_resets: AtomicU64,
    overflow_finishes: AtomicU64,
    raw_high_water: AtomicU64,
    upstream_dropped: AtomicU64,
}

/// A point-in-time copy of the [`SegmenterStage`] counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SegmenterStatsSnapshot {
    /// Text deltas received.
    pub deltas: u64,
    /// Chunks emitted.
    pub chunks: u64,
    /// Bytes of the chunks emitted (the raw slices, whitespace included).
    pub chunk_bytes: u64,
    /// Streams ended by `Flush`, `LlmResponseEnded` or `LlmResponseStarted`.
    pub finishes: u64,
    /// Streams dropped by an interrupt.
    pub resets: u64,
    /// Streams dropped because text of another epoch arrived.
    pub foreign_epoch_resets: u64,
    /// Streams ended early because the retained raw text passed its cap ([`RAW_CAP_FACTOR`]).
    pub overflow_finishes: u64,
    /// Most bytes of raw stream text the stage ever held after handling a delta.
    pub raw_high_water: u64,
    /// Upstream frames dropped because the upstream lane was full.
    pub upstream_dropped: u64,
}

/// Shared handle on the counters of one [`SegmenterStage`].
#[derive(Debug, Clone)]
pub struct SegmenterStats {
    counters: Arc<Counters>,
}

impl SegmenterStats {
    /// The counters now.
    pub fn snapshot(&self) -> SegmenterStatsSnapshot {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        SegmenterStatsSnapshot {
            deltas: get(&c.deltas),
            chunks: get(&c.chunks),
            chunk_bytes: get(&c.chunk_bytes),
            finishes: get(&c.finishes),
            resets: get(&c.resets),
            foreign_epoch_resets: get(&c.foreign_epoch_resets),
            overflow_finishes: get(&c.overflow_finishes),
            raw_high_water: get(&c.raw_high_water),
            upstream_dropped: get(&c.upstream_dropped),
        }
    }
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// The segmenter stage. See the [module docs](self).
#[derive(Debug)]
pub struct SegmenterStage {
    segmenter: Segmenter,
    /// The stream text from `base` on: what no chunk has covered yet.
    raw: String,
    /// Stream offset of `raw[0]`.
    base: usize,
    /// Epoch of the stream being segmented.
    epoch: Option<TurnEpoch>,
    /// Retained raw bytes above which the stream is ended early.
    raw_cap: usize,
    /// Scratch for the spans of one call.
    spans: Vec<Span>,
    counters: Arc<Counters>,
    /// Where [`TurnEvent::FirstChunk`] is marked ([`Self::with_timeline`]).
    timeline: Option<(TimelineRecorder, SharedClock)>,
    /// The epoch whose first chunk is marked already: the mark is made once per response, so
    /// the chunks after it pay one comparison and no lock.
    marked: Option<TurnEpoch>,
}

impl SegmenterStage {
    /// A stage cutting with `config`.
    pub fn new(config: SegmenterStageConfig) -> Self {
        Self {
            segmenter: Segmenter::new(config.segmenter),
            raw: String::new(),
            base: 0,
            epoch: None,
            raw_cap: config
                .segmenter
                .max_chunk_chars
                .max(1)
                .saturating_mul(RAW_CAP_FACTOR),
            spans: Vec::new(),
            counters: Arc::new(Counters::default()),
            timeline: None,
            marked: None,
        }
    }

    /// Marks [`TurnEvent::FirstChunk`] on `timeline` (stamped with `clock`) when the first chunk
    /// of a response leaves the stage: once per response, not per chunk. The session assembly
    /// applies the call's.
    #[must_use]
    pub fn with_timeline(mut self, timeline: TimelineRecorder, clock: SharedClock) -> Self {
        self.timeline = Some((timeline, clock));
        self
    }

    /// A handle on this stage's counters.
    pub fn stats(&self) -> SegmenterStats {
        SegmenterStats {
            counters: Arc::clone(&self.counters),
        }
    }

    /// Drops the stream in progress.
    fn reset(&mut self) {
        self.segmenter.reset();
        self.raw.clear();
        self.base = 0;
        self.epoch = None;
    }

    /// Emits the chunks in `self.spans` as raw slices, in order.
    async fn emit_spans(&mut self, epoch: TurnEpoch, ctx: &mut ProcessorContext) -> CoreResult<()> {
        let mut spans = std::mem::take(&mut self.spans);
        let mut result = Ok(());
        for span in spans.drain(..) {
            let end = span.end.saturating_sub(self.base);
            let Some(slice) = self.raw.get(..end) else {
                // Cannot happen (spans lie inside the stream text); never panic on it.
                tracing::warn!(target: "brisvox::engine", "segmenter span outside the stream text");
                continue;
            };
            let text: Arc<str> = Arc::from(slice);
            self.raw.drain(..end);
            self.base = span.end;
            if let Err(error) = self.send_chunk(epoch, text, ctx).await {
                result = Err(error);
                break;
            }
        }
        spans.clear();
        self.spans = spans;
        result
    }

    /// Pushes one chunk downstream.
    async fn send_chunk(
        &mut self,
        epoch: TurnEpoch,
        text: Arc<str>,
        ctx: &mut ProcessorContext,
    ) -> CoreResult<()> {
        bump(&self.counters.chunks);
        if self.marked != Some(epoch) {
            self.marked = Some(epoch);
            if let Some((timeline, clock)) = &self.timeline {
                timeline.mark(epoch, TurnEvent::FirstChunk, clock.now());
            }
        }
        self.counters.chunk_bytes.fetch_add(
            u64::try_from(text.len()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        let frame = Frame::Data(DataFrame::LlmText(TextFrame { epoch, text }));
        ctx.push(frame, Direction::Downstream).await
    }

    /// Ends the stream in progress (`Flush`, response boundaries): the pending tail goes out as
    /// the last chunk.
    async fn finish(&mut self, ctx: &mut ProcessorContext) -> CoreResult<()> {
        self.end_stream(ctx, false).await?;
        bump(&self.counters.finishes);
        Ok(())
    }

    /// The stream ends: the pending tail goes out as the last chunk, the state is cleared. With
    /// `keep_rest` the text after the last chunk (only whitespace, since the tail holds every
    /// word) goes out as a chunk of its own instead of being dropped: the stream is then cut
    /// without losing a byte of the concatenation.
    async fn end_stream(&mut self, ctx: &mut ProcessorContext, keep_rest: bool) -> CoreResult<()> {
        let epoch = self.epoch;
        self.segmenter.finish_spans(&mut self.spans);
        if let Some(epoch) = epoch {
            self.emit_spans(epoch, ctx).await?;
            if keep_rest && !self.raw.is_empty() {
                let rest: Arc<str> = Arc::from(self.raw.as_str());
                self.raw.clear();
                self.send_chunk(epoch, rest, ctx).await?;
            }
        } else {
            self.spans.clear();
        }
        self.raw.clear();
        self.base = 0;
        self.epoch = None;
        Ok(())
    }
}

impl Processor for SegmenterStage {
    fn name(&self) -> &'static str {
        "segmenter"
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
                bump(&self.counters.upstream_dropped);
            }
            return Ok(());
        }
        match &frame {
            Frame::Data(DataFrame::LlmText(text)) => {
                if text.text.is_empty() {
                    return Ok(());
                }
                bump(&self.counters.deltas);
                if self.epoch.is_some_and(|current| current != text.epoch) {
                    bump(&self.counters.foreign_epoch_resets);
                    self.reset();
                }
                self.epoch = Some(text.epoch);
                self.raw.push_str(&text.text);
                self.segmenter.push_spans(&text.text, &mut self.spans);
                self.emit_spans(text.epoch, ctx).await?;
                self.counters.raw_high_water.fetch_max(
                    u64::try_from(self.raw.len()).unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
                if self.raw.len() > self.raw_cap {
                    // Degenerate text (a whitespace flood): no chunk will drain `raw`.
                    bump(&self.counters.overflow_finishes);
                    self.end_stream(ctx, true).await?;
                }
                Ok(())
            }
            Frame::Control(ControlFrame {
                kind:
                    ControlKind::Flush | ControlKind::LlmResponseEnded | ControlKind::LlmResponseStarted,
                ..
            }) => {
                self.finish(ctx).await?;
                ctx.forward(frame, direction).await
            }
            Frame::System(SystemFrame::Interrupt { .. }) => {
                bump(&self.counters.resets);
                self.reset();
                ctx.forward(frame, direction).await
            }
            _ => ctx.forward(frame, direction).await,
        }
    }
}
