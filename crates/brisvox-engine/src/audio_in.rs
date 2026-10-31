//! [`AudioIn`]: the first stage after the transport pump (`docs/PIPELINE.md` section 1).
//!
//! ```text
//! pump --(REALTIME_INPUT inbox, drop oldest)--> AudioIn --> next stage (STT)
//!                                                  |
//!                                                  +--tee--> VadWorker (helper task) --> VAD events
//! ```
//!
//! For every caller audio frame it
//! 1. converts it to the model rate (16 kHz) with a [`StreamResampler`] when the call runs at
//!    another rate; frames already at 16 kHz pass through untouched (no copy),
//! 2. copies the 16 kHz samples into the VAD tee (bounded, drops oldest, never waits),
//! 3. forwards the 16 kHz frame at once.
//!
//! Everything else (system frames, control frames, upstream traffic) is forwarded unchanged.
//! The stage is epoch-agnostic ([`EpochPolicy::KeepAll`]): the user side stays in epoch zero.
//!
//! # Allocation
//! A steady-state frame allocates nothing: the pass-through forwards the received `Bytes`; the
//! resampled path writes into a ring of reusable [`BytesMut`] buffers (each frame is a frozen
//! split of its slot; a slot is reclaimed without allocating once the frame it produced was
//! dropped, which the downstream stages do within a few frames); the tee recycles its sample
//! buffers. The first frames after construction allocate while the ring and the buffers fill
//! (about one ring cycle, 32 frames). If a downstream stage holds frames longer than a ring
//! cycle the slot allocates a fresh buffer instead (correct, just not free). Resampler
//! construction (10-20 us for the FFT pairs to 16 kHz) happens on the first frame of a rate.
//!
//! # Gaps
//! The first-stage inbox drops the oldest audio when this stage falls behind, which shows as a
//! jump in `AudioFrame::seq`. A jump resets the resampler (its filter state would mix two
//! unrelated stretches of audio) and marks the next tee frame as following a gap, so the VAD
//! closes its stream there instead of running its windows across the hole. The mark is carried
//! until a tee frame is pushed: a frame that yields no resampled output (input not aligned to
//! the resampler block) or fails to convert does not lose it.
//!
//! # Timestamps of resampled audio
//! The resampler emits whole blocks, so the PCM of an output frame can start before the input
//! frame that completed it, and the filter adds a group delay (`StreamResampler::delay`). The
//! output frame is stamped with the time of its FIRST sample: the completing input frame's
//! timestamp, plus the output samples already emitted, minus the input samples already
//! consumed, minus the filter delay (all counted since the last reset). With frame-aligned input
//! and a stable stream this is the input timestamp minus the delay. VAD events backdated from
//! these stamps are therefore dated to the audio itself, not to the moment a block completed.
//! A change of the call's sample rate resets the resampler as well, even when the frames in
//! between were already at the model rate.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use brisvox_audio::StreamResampler;
use brisvox_core::frame::{AudioFrame, DataFrame, Frame, SampleRate, SystemFrame};
use brisvox_core::{
    Direction, EpochPolicy, Processor, ProcessorContext, Result as CoreResult, TurnEpoch,
};
use brisvox_inference::SpeechModel;
use bytes::{BufMut, Bytes, BytesMut};
use tokio::time::Instant;

use crate::config::EngineConfig;
use crate::denoise::DenoiseWiring;
use crate::guard::{GuardedSender, TaskGuard};
use crate::vad::{VadCounters, VadSetup, VadStats, VadTee, spawn_worker};

/// Slots in the output ring of the resampled path: 32 frames of 20 ms = 640 ms of audio in
/// flight before a slot would have to allocate.
const RING_SLOTS: usize = 32;

/// Starts the worker once the stage knows its neighbour.
type WorkerSpawner = Box<
    dyn FnOnce(Arc<VadTee>, GuardedSender, Arc<VadCounters>, Option<DenoiseWiring>) -> TaskGuard
        + Send,
>;

#[derive(Debug, Default)]
struct Counters {
    frames_in: AtomicU64,
    passthrough: AtomicU64,
    resampled: AtomicU64,
    empty_outputs: AtomicU64,
    gaps: AtomicU64,
    resampler_builds: AtomicU64,
    resampler_errors: AtomicU64,
}

/// A point-in-time copy of the [`AudioIn`] counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioInStatsSnapshot {
    /// Audio frames received.
    pub frames_in: u64,
    /// Frames that were already at the model rate and passed through.
    pub passthrough: u64,
    /// Frames converted to the model rate.
    pub resampled: u64,
    /// Resampled frames that produced no output yet (the resampler buffers up to one block).
    pub empty_outputs: u64,
    /// Sequence-number jumps seen (frames lost before this stage).
    pub gaps: u64,
    /// Resamplers built (one per rate seen).
    pub resampler_builds: u64,
    /// Frames dropped because a resampler could not be built or failed.
    pub resampler_errors: u64,
}

/// Shared handle on the counters of one [`AudioIn`].
#[derive(Debug, Clone)]
pub struct AudioInStats {
    counters: Arc<Counters>,
}

impl AudioInStats {
    /// The counters now.
    pub fn snapshot(&self) -> AudioInStatsSnapshot {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        AudioInStatsSnapshot {
            frames_in: get(&c.frames_in),
            passthrough: get(&c.passthrough),
            resampled: get(&c.resampled),
            empty_outputs: get(&c.empty_outputs),
            gaps: get(&c.gaps),
            resampler_builds: get(&c.resampler_builds),
            resampler_errors: get(&c.resampler_errors),
        }
    }
}

/// A ring of reusable buffers that frames are cut from without allocating.
pub(crate) struct FrameRing {
    slots: Vec<BytesMut>,
    next: usize,
}

impl FrameRing {
    pub(crate) fn new() -> Self {
        Self {
            // `BytesMut::new` does not allocate.
            slots: (0..RING_SLOTS).map(|_| BytesMut::new()).collect(),
            next: 0,
        }
    }

    /// `pcm` as little-endian bytes in the next slot, frozen into a `Bytes` that shares the
    /// slot's buffer. The slot gets the buffer back (`reserve`) once every `Bytes` cut from it
    /// was dropped.
    pub(crate) fn freeze(&mut self, pcm: &[i16]) -> Bytes {
        let index = self.next;
        self.next = (self.next + 1) % RING_SLOTS;
        let Some(slot) = self.slots.get_mut(index) else {
            return Bytes::new();
        };
        slot.reserve(pcm.len() * AudioFrame::BYTES_PER_SAMPLE);
        for sample in pcm {
            slot.put_i16_le(*sample);
        }
        slot.split().freeze()
    }
}

/// Decodes little-endian PCM16 into `out` (appending).
fn decode_le(bytes: &[u8], out: &mut Vec<i16>) {
    out.extend(
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| i16::from_le_bytes(*pair)),
    );
}

/// Sees every caller audio frame the stage receives, before any conversion (the
/// recording sink). It runs on the stage's task: it must be cheap and must not block (copy the
/// frame's [`Bytes`] handle into a bounded queue and return; never wait, never allocate per
/// frame beyond that).
///
/// The frame is exactly what the transport delivered: at the call's own rate, with the
/// transport's sequence numbers, so a tap sees the holes the first-stage inbox's drop-oldest
/// policy leaves (a jump in [`AudioFrame::seq`]).
pub trait CallerAudioTap: Send + 'static {
    /// Called with each caller audio frame, in arrival order.
    fn observe(&mut self, audio: &AudioFrame);
}

/// The input chain's audio stage; see the module docs.
pub struct AudioIn {
    tap: Option<Box<dyn CallerAudioTap>>,
    model_rate: SampleRate,
    resampler: Option<StreamResampler>,
    scratch_in: Vec<i16>,
    scratch_out: Vec<i16>,
    ring: FrameRing,
    last_seq: Option<u64>,
    /// Rate of the previous audio frame.
    last_rate: Option<SampleRate>,
    /// A hole precedes the next tee frame (a seq jump, or a frame lost in conversion).
    pending_gap: bool,
    /// Input samples consumed and output samples produced by the resampler since its last reset.
    consumed: u64,
    produced: u64,
    tee: Arc<VadTee>,
    spawner: Option<WorkerSpawner>,
    worker: Option<TaskGuard>,
    /// Whether a VAD was configured (otherwise the tee is not fed).
    vad: bool,
    /// Noise suppression for the VAD worker's streams ([`Self::with_denoise`]).
    denoise: Option<DenoiseWiring>,
    counters: Arc<Counters>,
    vad_stats: VadStats,
}

impl AudioIn {
    /// A stage converting to `config.rates.model`, with no VAD yet (see [`Self::with_vad`]).
    pub fn new(config: &EngineConfig) -> Self {
        let frame_samples = usize::try_from(
            u128::from(config.rates.model.hz()) * config.rates.frame.as_nanos() / 1_000_000_000,
        )
        .unwrap_or(320);
        let vad_stats = VadStats::new();
        Self {
            tap: None,
            model_rate: config.rates.model,
            resampler: None,
            scratch_in: Vec::new(),
            scratch_out: Vec::new(),
            ring: FrameRing::new(),
            last_seq: None,
            last_rate: None,
            pending_gap: false,
            consumed: 0,
            produced: 0,
            tee: Arc::new(VadTee::new(
                config.queues.input.data_capacity,
                frame_samples.max(1),
                vad_stats.counters(),
            )),
            spawner: None,
            worker: None,
            vad: false,
            denoise: None,
            counters: Arc::new(Counters::default()),
            vad_stats,
        }
    }

    /// Runs voice activity detection on the audio this stage forwards. The worker task starts
    /// when the pipeline's `Start` reaches the stage and is aborted when the stage is dropped.
    #[must_use]
    pub fn with_vad<M: SpeechModel + 'static>(mut self, setup: VadSetup<M>) -> Self {
        self.vad = true;
        self.spawner = Some(Box::new(move |tee, out, counters, denoise| {
            spawn_worker(setup, tee, out, counters, denoise)
        }));
        self
    }

    /// Suppresses noise in the audio the VAD worker hears and hands the enhanced audio to the
    /// feeds of `wiring` (see `crate::denoise`). Needs [`Self::with_vad`]; the frames this stage
    /// forwards are never changed.
    #[must_use]
    pub(crate) fn with_denoise(mut self, wiring: DenoiseWiring) -> Self {
        self.denoise = Some(wiring);
        self
    }

    /// Shows `tap` every caller audio frame (see [`CallerAudioTap`]); without one the stage
    /// does no extra work per frame.
    #[must_use]
    pub fn with_tap(self, tap: impl CallerAudioTap) -> Self {
        self.with_boxed_tap(Box::new(tap))
    }

    /// [`Self::with_tap`] for a tap that is already boxed.
    #[must_use]
    pub(crate) fn with_boxed_tap(mut self, tap: Box<dyn CallerAudioTap>) -> Self {
        self.tap = Some(tap);
        self
    }

    /// Whether a VAD worker was configured ([`Self::with_vad`]).
    pub(crate) const fn has_vad(&self) -> bool {
        self.vad
    }

    /// Counters of this stage (usable after the stage moved into a pipeline).
    pub fn stats(&self) -> AudioInStats {
        AudioInStats {
            counters: Arc::clone(&self.counters),
        }
    }

    /// Counters of the VAD worker.
    pub fn vad_stats(&self) -> VadStats {
        self.vad_stats.clone()
    }

    fn start_worker(&mut self, ctx: &ProcessorContext) {
        if self.worker.is_some() {
            return;
        }
        let Some(spawner) = self.spawner.take() else {
            return;
        };
        let (Some(downstream), Some(watch)) =
            (ctx.sender(Direction::Downstream), ctx.epoch_watch())
        else {
            tracing::warn!(
                target: "brisvox::engine",
                "audio_in has no downstream neighbour: no vad worker started"
            );
            return;
        };
        let out = GuardedSender::new(downstream, watch.clone(), TurnEpoch::ZERO);
        self.worker = Some(spawner(
            Arc::clone(&self.tee),
            out,
            self.vad_stats.counters(),
            self.denoise.clone(),
        ));
    }

    /// Clears the resampler's filter state and the sample counters that stamp its output.
    fn reset_resampler(&mut self) {
        if let Some(resampler) = self.resampler.as_mut() {
            resampler.reset();
        }
        self.consumed = 0;
        self.produced = 0;
    }

    /// Time of the first sample of the next resampler output, given that `audio` completes it.
    fn output_start(&self, audio: &AudioFrame, in_hz: u32, delay: Duration) -> Instant {
        let nanos =
            |samples: u64, hz: u32| i128::from(samples) * 1_000_000_000 / i128::from(hz.max(1));
        let offset = nanos(self.produced, self.model_rate.hz())
            - nanos(self.consumed, in_hz)
            - i128::try_from(delay.as_nanos()).unwrap_or(0);
        let magnitude = Duration::from_nanos(u64::try_from(offset.unsigned_abs()).unwrap_or(0));
        if offset >= 0 {
            audio.timestamp() + magnitude
        } else {
            audio
                .timestamp()
                .checked_sub(magnitude)
                .unwrap_or_else(|| audio.timestamp())
        }
    }

    /// Converts `audio` to the model rate. `None` when nothing came out (the resampler buffers
    /// up to one block) or the conversion failed (counted). Also tees the converted samples.
    /// `reset` drops the filter state first (a hole or a rate change before this frame).
    fn convert(&mut self, audio: &AudioFrame, reset: bool) -> Option<AudioFrame> {
        let rate = audio.sample_rate();
        let reusable = self
            .resampler
            .as_ref()
            .is_some_and(|resampler| resampler.from_rate() == rate);
        if !reusable {
            self.consumed = 0;
            self.produced = 0;
            match StreamResampler::new(rate, self.model_rate) {
                Ok(resampler) => {
                    self.counters
                        .resampler_builds
                        .fetch_add(1, Ordering::Relaxed);
                    self.resampler = Some(resampler);
                }
                Err(error) => {
                    self.counters
                        .resampler_errors
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(target: "brisvox::engine", %error, ?rate, "audio_in cannot resample");
                    self.resampler = None;
                    // The frame is lost: audio after it does not follow the audio before it.
                    self.pending_gap = true;
                    return None;
                }
            }
        } else if reset {
            self.reset_resampler();
        }
        self.scratch_in.clear();
        decode_le(audio.data(), &mut self.scratch_in);
        let resampler = self.resampler.as_mut()?;
        let delay = resampler.delay();
        let needed = resampler.max_output_len(self.scratch_in.len());
        if self.scratch_out.len() < needed {
            self.scratch_out.resize(needed, 0);
        }
        let written = match resampler.process(&self.scratch_in, &mut self.scratch_out) {
            Ok(written) => written,
            Err(error) => {
                self.counters
                    .resampler_errors
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(target: "brisvox::engine", %error, "audio_in resampler failed");
                self.reset_resampler();
                self.pending_gap = true;
                return None;
            }
        };
        let timestamp = self.output_start(audio, rate.hz(), delay);
        self.consumed = self
            .consumed
            .saturating_add(u64::try_from(self.scratch_in.len()).unwrap_or(u64::MAX));
        self.produced = self
            .produced
            .saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
        if written == 0 {
            self.counters.empty_outputs.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let pcm = self.scratch_out.get(..written)?;
        if self.vad {
            let gap = std::mem::take(&mut self.pending_gap);
            self.tee
                .push_with(timestamp, gap, |samples| samples.extend_from_slice(pcm));
        }
        let bytes = self.ring.freeze(pcm);
        AudioFrame::new(
            bytes,
            self.model_rate,
            timestamp,
            audio.seq(),
            audio.epoch(),
        )
        .ok()
    }

    /// Noise suppression is for calls at or above the filter's minimum rate: the first frame below
    /// it switches the filter off for the call (and says so once).
    fn check_denoise_rate(&mut self, rate: SampleRate) {
        let Some(wiring) = &self.denoise else {
            return;
        };
        if rate.hz() < wiring.min_call_hz
            && !wiring.counters.narrowband.swap(true, Ordering::Relaxed)
        {
            wiring.counters.down.store(true, Ordering::Relaxed);
            tracing::warn!(
                target: "brisvox::engine",
                rate_hz = rate.hz(),
                min_hz = wiring.min_call_hz,
                "noise suppression is not used on this call: its audio is below the filter's minimum \
                 sample rate"
            );
        }
    }

    async fn audio(&mut self, audio: AudioFrame, ctx: &mut ProcessorContext) -> CoreResult<()> {
        self.counters.frames_in.fetch_add(1, Ordering::Relaxed);
        self.check_denoise_rate(audio.sample_rate());
        if let Some(tap) = self.tap.as_mut() {
            tap.observe(&audio);
        }
        let seq = audio.seq();
        let gap = self
            .last_seq
            .is_some_and(|last| seq != last.wrapping_add(1));
        self.last_seq = Some(seq);
        let rate_changed = self
            .last_rate
            .replace(audio.sample_rate())
            .is_some_and(|previous| previous != audio.sample_rate());
        if gap {
            self.counters.gaps.fetch_add(1, Ordering::Relaxed);
            self.pending_gap = true;
        }
        let out = if audio.sample_rate() == self.model_rate {
            self.counters.passthrough.fetch_add(1, Ordering::Relaxed);
            if rate_changed || gap {
                // Whatever the resampler still holds belongs to another stretch of audio.
                self.reset_resampler();
            }
            if self.vad {
                let gap = std::mem::take(&mut self.pending_gap);
                self.tee.push_with(audio.timestamp(), gap, |samples| {
                    decode_le(audio.data(), samples);
                });
            }
            Some(audio)
        } else {
            let converted = self.convert(&audio, gap || rate_changed);
            if converted.is_some() {
                self.counters.resampled.fetch_add(1, Ordering::Relaxed);
            }
            converted
        };
        match out {
            Some(frame) => {
                ctx.push(Frame::Data(DataFrame::Audio(frame)), Direction::Downstream)
                    .await
            }
            None => Ok(()),
        }
    }
}

impl std::fmt::Debug for AudioIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioIn")
            .field("model_rate", &self.model_rate)
            .field("vad", &self.vad)
            .finish_non_exhaustive()
    }
}

impl Processor for AudioIn {
    fn name(&self) -> &'static str {
        "audio_in"
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
        match frame {
            Frame::Data(DataFrame::Audio(audio)) if direction == Direction::Downstream => {
                self.audio(audio, ctx).await
            }
            Frame::System(SystemFrame::Start) if direction == Direction::Downstream => {
                self.start_worker(ctx);
                ctx.forward(Frame::System(SystemFrame::Start), direction)
                    .await
            }
            other => ctx.forward(other, direction).await,
        }
    }
}
