//! Noise suppression of the caller's audio (`docs/PIPELINE.md`; agent file `[audio]`).
//!
//! The audio path never waits for the suppressor. The caller's frames reach the speech-to-text
//! stage and the turn driver at once, exactly as without it. The VAD worker ([`crate::vad`])
//! runs each batch through a [`brisvox_inference::Denoiser`] on the inference pool before the
//! speech model hears it, and hands the enhanced samples to the consumers through a
//! [`DenoiseFeed`] each:
//!
//! - the **turn driver** replaces the plain samples in its Smart Turn ring with the enhanced
//!   ones (the ring is only read at a candidate end of turn, after the job that enhanced the
//!   audio up to the VAD stop has finished, because the enhanced chunks are pushed before the
//!   stop is emitted);
//! - with `denoise_stt`, the **STT stage** sends the enhanced samples to the provider instead
//!   of the plain frames, and falls back to the plain frames when the enhanced audio is late.
//!
//! # Overload
//! Nothing is ever dropped to protect the suppressor:
//! - a batch the pool refuses or times out is kept and retried by the VAD worker, which keeps
//!   the audio (the worker's own policy, `vad.rs`); meanwhile the consumers use the plain audio;
//! - a feed that fills up (a consumer that stopped draining) drops its OLDEST enhanced chunk,
//!   counted in [`DenoiseStatsSnapshot::feed_dropped`]; the plain audio is still there, so the
//!   consumer simply keeps it for that stretch (Smart Turn: the ring keeps the plain samples;
//!   STT: the held plain frames are sent);
//! - a suppressor that fails on a push is reset and its audio goes through as it was
//!   ([`DenoiseStatsSnapshot::errors`]);
//! - a VAD worker that failed open stops producing enhanced audio altogether; everything then
//!   runs on the plain audio.
//!
//! # Allocation
//! Chunks are copied into recycled buffers; a steady state allocates nothing.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_core::frame::AudioFrame;
use brisvox_inference::BoxedDenoiser;
use bytes::Bytes;
use tokio::time::Instant;

use crate::audio_in::FrameRing;

/// Builds one call's denoiser (state per call). Runs inside the VAD batch job on the inference
/// pool (building can load a model file).
pub type DenoiserFactory = Arc<dyn Fn() -> brisvox_inference::Result<BoxedDenoiser> + Send + Sync>;

/// What a call needs to suppress noise: how to make a denoiser and where its output goes.
/// Install it with [`SessionBuilder::with_denoise`](crate::SessionBuilder::with_denoise).
#[derive(Clone)]
pub struct DenoiseSetup {
    pub(crate) make: DenoiserFactory,
    /// Whether the speech-to-text provider hears the enhanced audio too (default `false`:
    /// only the VAD and the turn detector do).
    pub(crate) to_stt: bool,
    /// The lowest call sample rate the filter is used at (0: any). A call whose transport delivers
    /// a lower rate runs without it.
    pub(crate) min_call_hz: u32,
}

impl std::fmt::Debug for DenoiseSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DenoiseSetup")
            .field("to_stt", &self.to_stt)
            .finish_non_exhaustive()
    }
}

impl DenoiseSetup {
    /// A setup whose denoisers come from `make` (one per stream: the first and every rebuild
    /// after a lost job). It must produce denoisers at the model rate, 16 kHz.
    pub fn new<F>(make: F) -> Self
    where
        F: Fn() -> brisvox_inference::Result<BoxedDenoiser> + Send + Sync + 'static,
    {
        Self {
            make: Arc::new(make),
            to_stt: false,
            min_call_hz: 0,
        }
    }

    /// Uses the filter only on calls whose transport delivers at least `hz` samples a second. A
    /// call at a lower rate (a telephone call at 8 kHz) runs without it: a warning is logged once
    /// and [`DenoiseStatsSnapshot::bypassed_narrowband`] is set. For a model that does harm to
    /// band-limited audio (Hush, which attenuated clean 8 kHz speech by 12 dB and turned it into
    /// noise for the recognizer).
    #[must_use]
    pub const fn with_min_call_rate(mut self, hz: u32) -> Self {
        self.min_call_hz = hz;
        self
    }

    /// Also sends the enhanced audio to the speech-to-text provider (default: only to the VAD
    /// and the turn detector). Independent studies found that denoised audio often raises the
    /// word error rate of Whisper-class recognizers; measure before enabling.
    #[must_use]
    pub const fn with_stt(mut self, on: bool) -> Self {
        self.to_stt = on;
        self
    }

    /// Whether the speech-to-text provider hears the enhanced audio.
    pub const fn to_stt(&self) -> bool {
        self.to_stt
    }
}

/// Counters of one call's noise suppression.
#[derive(Debug, Default)]
pub(crate) struct DenoiseCounters {
    /// Enhanced chunks produced by the VAD worker's jobs.
    pub(crate) chunks: AtomicU64,
    /// Pushes the suppressor failed on (their audio went through as it was).
    pub(crate) errors: AtomicU64,
    /// Times the suppressor could not be built and the call's VAD stream was built without it.
    pub(crate) build_failures: AtomicU64,
    /// Chunks the feeds dropped because they were full.
    pub(crate) feed_dropped: AtomicU64,
    /// Plain frames the STT stage sent because the enhanced audio was late (`denoise_stt`).
    pub(crate) stt_fallback_frames: AtomicU64,
    /// Samples of enhanced audio sent to the STT provider (`denoise_stt`).
    pub(crate) stt_denoised_samples: AtomicU64,
    /// The VAD worker failed open, or the call is below the filter's minimum rate: no enhanced
    /// audio is coming (until the worker recovers).
    pub(crate) down: AtomicBool,
    /// The call's audio is below the filter's minimum sample rate: the filter is not used.
    pub(crate) narrowband: AtomicBool,
}

/// A point-in-time copy of the noise suppression counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DenoiseStatsSnapshot {
    /// Enhanced chunks produced.
    pub chunks: u64,
    /// Pushes the suppressor failed on; their audio went through unprocessed.
    pub errors: u64,
    /// Times the suppressor could not be built: the call's VAD ran on the plain audio.
    pub build_failures: u64,
    /// Enhanced chunks dropped because a consumer's feed was full (the plain audio was used).
    pub feed_dropped: u64,
    /// Plain frames sent to the STT provider instead of enhanced audio (`denoise_stt` only).
    pub stt_fallback_frames: u64,
    /// Samples of enhanced audio sent to the STT provider (`denoise_stt` only).
    pub stt_denoised_samples: u64,
    /// The call runs below the filter's minimum sample rate and without the filter.
    pub bypassed_narrowband: bool,
}

/// Shared, cheap-to-clone handle on the noise suppression counters of one call.
#[derive(Debug, Clone, Default)]
pub struct DenoiseStats {
    pub(crate) counters: Arc<DenoiseCounters>,
}

impl DenoiseStats {
    /// The counters now.
    pub fn snapshot(&self) -> DenoiseStatsSnapshot {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        DenoiseStatsSnapshot {
            chunks: get(&c.chunks),
            errors: get(&c.errors),
            build_failures: get(&c.build_failures),
            feed_dropped: get(&c.feed_dropped),
            stt_fallback_frames: get(&c.stt_fallback_frames),
            stt_denoised_samples: get(&c.stt_denoised_samples),
            bypassed_narrowband: c.narrowband.load(Ordering::Relaxed),
        }
    }
}

/// What the VAD worker needs once noise suppression is on: the denoiser factory and the feeds
/// to fill.
#[derive(Clone)]
pub(crate) struct DenoiseWiring {
    pub(crate) make: DenoiserFactory,
    pub(crate) sinks: Arc<[Arc<DenoiseFeed>]>,
    pub(crate) counters: Arc<DenoiseCounters>,
    /// See [`DenoiseSetup::with_min_call_rate`].
    pub(crate) min_call_hz: u32,
}

/// Most chunks a feed holds. A chunk is one VAD batch (usually one 20 ms frame, up to the
/// tee's 160 ms), so this is more than a second of audio at the usual size.
const FEED_CAPACITY: usize = 64;

struct Chunk {
    start: Instant,
    samples: Vec<i16>,
}

struct FeedState {
    chunks: VecDeque<Chunk>,
    /// Sample buffers ready for reuse.
    spare: Vec<Vec<i16>>,
}

/// A bounded queue of enhanced audio from the VAD worker's jobs (producer) to one consumer
/// stage. Overflow policy: DROP OLDEST, counted; the consumer has the plain audio for that
/// stretch (see the module docs).
pub struct DenoiseFeed {
    state: Mutex<FeedState>,
    counters: Arc<DenoiseCounters>,
    samples_hint: usize,
}

impl std::fmt::Debug for DenoiseFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DenoiseFeed").finish_non_exhaustive()
    }
}

impl DenoiseFeed {
    pub(crate) fn new(counters: Arc<DenoiseCounters>, samples_hint: usize) -> Self {
        let spare = (0..FEED_CAPACITY + 2)
            .map(|_| Vec::with_capacity(samples_hint))
            .collect();
        Self {
            state: Mutex::new(FeedState {
                chunks: VecDeque::with_capacity(FEED_CAPACITY),
                spare,
            }),
            counters,
            samples_hint,
        }
    }

    fn lock(&self) -> MutexGuard<'_, FeedState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `samples` (16 kHz) whose first sample belongs to the input at `start`.
    pub(crate) fn push(&self, start: Instant, samples: &[i16]) {
        if samples.is_empty() {
            return;
        }
        let mut state = self.lock();
        let mut buffer = state
            .spare
            .pop()
            .unwrap_or_else(|| Vec::with_capacity(self.samples_hint));
        buffer.clear();
        buffer.extend_from_slice(samples);
        if state.chunks.len() >= FEED_CAPACITY
            && let Some(oldest) = state.chunks.pop_front()
        {
            state.spare.push(oldest.samples);
            self.counters.feed_dropped.fetch_add(1, Ordering::Relaxed);
        }
        state.chunks.push_back(Chunk {
            start,
            samples: buffer,
        });
    }

    /// Calls `f(start, samples)` for every queued chunk, oldest first, and empties the queue.
    /// `f` runs under the feed's lock (a few microseconds): it must not block or call back.
    pub(crate) fn drain(&self, mut f: impl FnMut(Instant, &[i16])) {
        let mut state = self.lock();
        while let Some(chunk) = state.chunks.pop_front() {
            f(chunk.start, &chunk.samples);
            state.spare.push(chunk.samples);
        }
    }

    /// Like [`Self::drain`], but only the leading chunks for which `ready(start, samples)` holds:
    /// the rest stay queued. A consumer whose own audio lags the enhanced audio (the enhanced
    /// chunk of a frame can be ready before the plain frame has reached the consumer) takes a
    /// chunk once its audio is there.
    pub(crate) fn drain_ready(
        &self,
        ready: impl Fn(Instant, usize) -> bool,
        mut f: impl FnMut(Instant, &[i16]),
    ) {
        let mut state = self.lock();
        while state
            .chunks
            .front()
            .is_some_and(|c| ready(c.start, c.samples.len()))
        {
            if let Some(chunk) = state.chunks.pop_front() {
                f(chunk.start, &chunk.samples);
                state.spare.push(chunk.samples);
            }
        }
    }

    /// Whether anything is queued.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.lock().chunks.is_empty()
    }
}

/// Frames the STT side holds while it waits for the enhanced version: 100 ms of audio. Beyond
/// that the oldest plain frame is sent as it is.
const MAX_HELD_FRAMES: usize = 5;

/// Rounding slack when matching the end of a held frame to the end of an enhanced chunk.
const MATCH_SLACK: Duration = Duration::from_millis(1);

/// A plain frame the speech-to-text stage holds until its enhanced version arrives.
struct HeldFrame {
    end: Instant,
    data: Bytes,
}

/// What the speech-to-text stage sends to its provider when it hears the enhanced audio
/// (`denoise_stt`). The plain frames arrive at once; each is held until the enhanced samples
/// covering it are in the feed, then the enhanced samples go to the provider instead. A frame
/// that waits more than [`MAX_HELD_FRAMES`] frames, or is flushed at a VAD stop (the
/// `Finalize` must follow ALL the audio of the utterance), goes out as it was, counted in
/// [`DenoiseStatsSnapshot::stt_fallback_frames`]; enhanced samples for a stretch already sent
/// that way are discarded. Audio is therefore never dropped and never sent twice.
pub(crate) struct SttDenoisedInput {
    feed: Arc<DenoiseFeed>,
    counters: Arc<DenoiseCounters>,
    held: VecDeque<HeldFrame>,
    /// The plain audio was sent up to here: enhanced samples before it are stale.
    sent_until: Option<Instant>,
    ring: FrameRing,
    hz: u64,
}

impl SttDenoisedInput {
    pub(crate) fn new(feed: Arc<DenoiseFeed>, counters: Arc<DenoiseCounters>, hz: u32) -> Self {
        Self {
            feed,
            counters,
            held: VecDeque::with_capacity(MAX_HELD_FRAMES + 2),
            sent_until: None,
            ring: FrameRing::new(),
            hz: u64::from(hz.max(1)),
        }
    }

    fn samples_duration(&self, samples: usize) -> Duration {
        Duration::from_micros(
            u64::try_from(samples)
                .unwrap_or(0)
                .saturating_mul(1_000_000)
                / self.hz,
        )
    }

    /// A plain frame arrived: hold it and send what the feed has enhanced meanwhile.
    pub(crate) fn on_frame(&mut self, audio: &AudioFrame, send: &mut dyn FnMut(Bytes)) {
        let end = audio.timestamp()
            + self.samples_duration(audio.data().len() / AudioFrame::BYTES_PER_SAMPLE);
        self.held.push_back(HeldFrame {
            end,
            data: audio.data().clone(),
        });
        // An enhanced chunk can be ahead of the plain frame (it was sent when it arrived).
        if let Some(until) = self.sent_until {
            while self
                .held
                .front()
                .is_some_and(|f| f.end <= until + MATCH_SLACK)
            {
                self.held.pop_front();
            }
        }
        if self.counters.down.load(Ordering::Relaxed) {
            // No enhanced audio is coming (the VAD worker failed open): do not delay the plain.
            self.flush(send);
            return;
        }
        self.pump(send);
        while self.held.len() > MAX_HELD_FRAMES {
            self.fall_back(send);
        }
    }

    /// Sends everything still held as it is (the stretch up to a VAD stop is complete before the
    /// `Finalize`; a stream that ends).
    pub(crate) fn flush(&mut self, send: &mut dyn FnMut(Bytes)) {
        self.pump(send);
        while !self.held.is_empty() {
            self.fall_back(send);
        }
    }

    fn fall_back(&mut self, send: &mut dyn FnMut(Bytes)) {
        if let Some(frame) = self.held.pop_front() {
            send_plain(
                &frame,
                None,
                &mut self.sent_until,
                self.hz,
                &self.counters,
                send,
            );
        }
    }

    /// Sends the enhanced chunks the feed holds, skipping what the plain audio already covered,
    /// and releases the held frames they cover. A chunk that starts after plain frames that were
    /// never enhanced (a hole in the VAD stream, a lost job, a failed push) sends those frames
    /// plain first: a frame is released only by audio that was sent in its place.
    fn pump(&mut self, send: &mut dyn FnMut(Bytes)) {
        let hz = self.hz;
        let micros = |samples: usize| {
            Duration::from_micros(
                u64::try_from(samples)
                    .unwrap_or(0)
                    .saturating_mul(1_000_000)
                    / hz,
            )
        };
        let Self {
            feed,
            counters,
            held,
            sent_until,
            ring,
            ..
        } = self;
        feed.drain(|start, samples| {
            let mut start = start;
            let mut samples = samples;
            let end = start + micros(samples.len());
            if let Some(until) = *sent_until {
                if end <= until {
                    return; // the plain audio of this stretch is already with the provider
                }
                if start < until {
                    let stale = usize::try_from(
                        until.saturating_duration_since(start).as_micros() * u128::from(hz)
                            / 1_000_000,
                    )
                    .unwrap_or(usize::MAX);
                    samples = samples.get(stale.min(samples.len())..).unwrap_or_default();
                    start = until;
                }
            }
            if samples.is_empty() {
                return;
            }
            // Plain frames before this chunk were not enhanced: they go out as they were.
            while let Some(front) = held.front() {
                if front.end <= start + MATCH_SLACK {
                    if let Some(frame) = held.pop_front() {
                        send_plain(&frame, None, sent_until, hz, counters, send);
                    }
                } else {
                    let frame_start = front.end.checked_sub(micros(front.data.len() / 2));
                    if frame_start.is_some_and(|f| f + MATCH_SLACK < start) {
                        // The chunk starts inside the frame: its first part goes out plain, the
                        // rest is covered by the chunk.
                        send_plain(front, Some(start), sent_until, hz, counters, send);
                    }
                    break;
                }
            }
            counters
                .stt_denoised_samples
                .fetch_add(u64::try_from(samples.len()).unwrap_or(0), Ordering::Relaxed);
            send(ring.freeze(samples));
            *sent_until = Some(end);
            while held.front().is_some_and(|f| f.end <= end + MATCH_SLACK) {
                held.pop_front();
            }
        });
    }
}

/// Sends the part of the plain `frame` that has not gone out yet: from `sent_until` (or its own
/// start) up to `upto` (or its end), and moves `sent_until` there.
fn send_plain(
    frame: &HeldFrame,
    upto: Option<Instant>,
    sent_until: &mut Option<Instant>,
    hz: u64,
    counters: &DenoiseCounters,
    send: &mut dyn FnMut(Bytes),
) {
    let samples = frame.data.len() / AudioFrame::BYTES_PER_SAMPLE;
    let micros = |samples: usize| {
        Duration::from_micros(
            u64::try_from(samples)
                .unwrap_or(0)
                .saturating_mul(1_000_000)
                / hz,
        )
    };
    let frame_start = frame.end.checked_sub(micros(samples)).unwrap_or(frame.end);
    let from = sent_until.map_or(frame_start, |u| u.max(frame_start));
    let to = upto.map_or(frame.end, |u| u.min(frame.end));
    let to_sample = |t: Instant| {
        usize::try_from(
            t.saturating_duration_since(frame_start).as_micros() * u128::from(hz) / 1_000_000,
        )
        .unwrap_or(usize::MAX)
        .min(samples)
    };
    let (a, b) = (to_sample(from), to_sample(to));
    *sent_until = Some(sent_until.map_or(to, |u| u.max(to)));
    if b > a {
        counters.stt_fallback_frames.fetch_add(1, Ordering::Relaxed);
        send(
            frame
                .data
                .slice(a * AudioFrame::BYTES_PER_SAMPLE..b * AudioFrame::BYTES_PER_SAMPLE),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed time origin (the paused simulation clock).
    fn origin() -> Instant {
        use brisvox_core::Clock;
        brisvox_core::SimClock::new().now()
    }

    fn feed() -> (DenoiseFeed, Arc<DenoiseCounters>) {
        let counters = Arc::new(DenoiseCounters::default());
        (DenoiseFeed::new(Arc::clone(&counters), 320), counters)
    }

    #[tokio::test(start_paused = true)]
    async fn chunks_come_out_in_order_and_the_queue_empties() {
        let (feed, _) = feed();
        let t = origin();
        feed.push(t, &[1, 2]);
        feed.push(t, &[]);
        feed.push(t, &[3]);
        let mut seen = Vec::new();
        feed.drain(|_, s| seen.push(s.to_vec()));
        assert_eq!(seen, [vec![1, 2], vec![3]]);
        assert!(feed.is_empty());
    }

    fn frame(t0: Instant, i: u64, value: i16) -> AudioFrame {
        let mut pcm = Vec::new();
        for _ in 0..320 {
            pcm.extend_from_slice(&value.to_le_bytes());
        }
        AudioFrame::new(
            Bytes::from(pcm),
            brisvox_core::frame::SampleRate::Hz16000,
            t0 + Duration::from_millis(20 * i),
            i,
            brisvox_core::TurnEpoch::ZERO,
        )
        .unwrap()
    }

    fn stt_input() -> (SttDenoisedInput, Arc<DenoiseFeed>, Arc<DenoiseCounters>) {
        let (feed, counters) = feed();
        let feed = Arc::new(feed);
        let input = SttDenoisedInput::new(Arc::clone(&feed), Arc::clone(&counters), 16_000);
        (input, feed, counters)
    }

    /// The samples of the `Bytes` sent so far.
    fn samples(sent: &[Bytes]) -> Vec<i16> {
        sent.iter()
            .flat_map(|b| b.as_chunks::<2>().0.iter().map(|p| i16::from_le_bytes(*p)))
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn enhanced_audio_replaces_the_plain_frame_it_covers() {
        let (mut input, feed, counters) = stt_input();
        let t0 = origin();
        let mut sent = Vec::new();
        feed.push(t0, &[7; 320]);
        input.on_frame(&frame(t0, 0, 1), &mut |b| sent.push(b));
        assert_eq!(
            samples(&sent),
            vec![7; 320],
            "the enhanced samples, not the plain"
        );
        assert!(input.held.is_empty());
        assert_eq!(counters.stt_fallback_frames.load(Ordering::Relaxed), 0);
        assert_eq!(counters.stt_denoised_samples.load(Ordering::Relaxed), 320);
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_frame_goes_out_plain_and_its_late_enhanced_audio_is_discarded() {
        let (mut input, feed, counters) = stt_input();
        let t0 = origin();
        let mut sent = Vec::new();
        // No enhanced audio for six frames: the first goes out as it was.
        for i in 0..=MAX_HELD_FRAMES as u64 {
            input.on_frame(&frame(t0, i, 1), &mut |b| sent.push(b));
        }
        assert_eq!(samples(&sent), vec![1; 320]);
        assert_eq!(counters.stt_fallback_frames.load(Ordering::Relaxed), 1);
        // The enhanced version of frames 0 and 1 arrives late: frame 0 is stale, frame 1 is used.
        feed.push(t0, &[9; 640]);
        input.on_frame(&frame(t0, 6, 1), &mut |b| sent.push(b));
        let all = samples(&sent);
        assert_eq!(
            all.len(),
            320 + 320,
            "frame 0 plain, then the enhanced half that is new"
        );
        assert_eq!(&all[320..], &[9; 320][..]);
        // Frame 1 is covered; frames 2 to 6 still wait.
        assert_eq!(input.held.len(), 5);
    }

    #[tokio::test(start_paused = true)]
    async fn a_flush_sends_only_what_the_enhanced_audio_did_not_cover() {
        let (mut input, feed, _) = stt_input();
        let t0 = origin();
        let mut sent = Vec::new();
        input.on_frame(&frame(t0, 0, 1), &mut |b| sent.push(b));
        input.on_frame(&frame(t0, 1, 2), &mut |b| sent.push(b));
        // The enhanced audio covers frame 0 and the first half of frame 1.
        feed.push(t0, &[5; 480]);
        input.flush(&mut |b| sent.push(b));
        let all = samples(&sent);
        assert_eq!(all.len(), 640, "every sample exactly once");
        assert_eq!(&all[..480], &[5; 480][..]);
        assert_eq!(&all[480..], &[2; 160][..], "the rest of frame 1, plain");
        assert!(input.held.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn drain_ready_leaves_chunks_from_the_future_queued() {
        let (feed, _) = feed();
        let t = origin();
        let ms = Duration::from_millis;
        feed.push(t, &[1; 160]); // 10 ms
        feed.push(t + ms(10), &[2; 160]);
        feed.push(t + ms(20), &[3; 160]);
        // The consumer has audio up to 22 ms: the third chunk ends at 30 ms.
        let limit = t + ms(22);
        let mut seen = Vec::new();
        feed.drain_ready(
            |start, len| start + ms(u64::try_from(len / 16).unwrap()) <= limit,
            |_, s| seen.push(s[0]),
        );
        assert_eq!(seen, [1, 2]);
        assert!(!feed.is_empty());
        let mut rest = Vec::new();
        feed.drain(|_, s| rest.push(s[0]));
        assert_eq!(rest, [3]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_plain_frame_behind_the_enhanced_audio_is_not_held_or_sent_again() {
        let (mut input, feed, counters) = stt_input();
        let t0 = origin();
        let mut sent = Vec::new();
        // The enhanced chunk for frame 0 is in the feed before the plain frame arrives.
        feed.push(t0, &[7; 320]);
        input.pump(&mut |b| sent.push(b));
        assert_eq!(samples(&sent), vec![7; 320]);
        input.on_frame(&frame(t0, 0, 1), &mut |b| sent.push(b));
        assert_eq!(samples(&sent).len(), 320, "the plain frame is not sent too");
        assert!(input.held.is_empty());
        assert_eq!(counters.stt_fallback_frames.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hole_in_the_enhanced_stream_costs_no_audio_and_doubles_none() {
        // Ten plain frames, value i + 1. The suppressor delays by half a frame, so its chunks lag;
        // frames 4 to 6 are never enhanced (a gap in the VAD stream, a lost job), and the stream
        // restarts in the middle of frame 7 (its fade-in dropped).
        let (mut input, feed, counters) = stt_input();
        let t0 = origin();
        let ms = Duration::from_millis;
        let mut sent = Vec::new();
        let value = |i: i16| 1000 + i;
        for i in 0..10u64 {
            // Enhanced chunks arrive a frame late.
            if (1..=4).contains(&i) {
                feed.push(
                    t0 + ms(20 * (i - 1)),
                    &[value(i16::try_from(i - 1).unwrap()); 320],
                );
            }
            if i == 9 {
                // The restarted stream: from 10 ms into frame 7 to the end of frame 8.
                feed.push(t0 + ms(150), &[7777; 480]);
            }
            input.on_frame(&frame(t0, i, i16::try_from(i).unwrap() + 1), &mut |b| {
                sent.push(b)
            });
        }
        input.flush(&mut |b| sent.push(b));
        let all = samples(&sent);
        assert_eq!(
            all.len(),
            10 * 320,
            "every sample exactly once: {}",
            all.len()
        );
        let count = |v: i16| all.iter().filter(|x| **x == v).count();
        // Frames 0 to 3 enhanced, frames 4 to 6 plain (values 5, 6, 7), frame 7: its first half
        // plain (value 8), the rest enhanced; frame 8 enhanced (the chunk covers it); frame 9 plain.
        for i in 0..4 {
            assert_eq!(count(value(i)), 320, "frame {i}");
        }
        assert_eq!(count(5), 320);
        assert_eq!(count(6), 320);
        assert_eq!(count(7), 320);
        assert_eq!(count(8), 160, "the first half of frame 7");
        assert_eq!(count(7777), 480);
        assert_eq!(count(10), 320, "frame 9, never enhanced");
        assert!(counters.stt_fallback_frames.load(Ordering::Relaxed) >= 5);
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_feed_drops_its_oldest_chunk_and_counts_it() {
        let (feed, counters) = feed();
        let t = origin();
        for i in 0..(FEED_CAPACITY + 3) {
            feed.push(t, &[i16::try_from(i).unwrap()]);
        }
        let mut first = None;
        let mut n = 0;
        feed.drain(|_, s| {
            first.get_or_insert(s[0]);
            n += 1;
        });
        assert_eq!(n, FEED_CAPACITY);
        assert_eq!(first, Some(3));
        assert_eq!(counters.feed_dropped.load(Ordering::Relaxed), 3);
    }
}
