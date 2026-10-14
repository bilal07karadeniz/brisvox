//! Voice activity detection off the audio path (`docs/PIPELINE.md` sections 1 and 7).
//!
//! Model inference is CPU work, so it never runs in the stage that forwards audio. [`AudioIn`](crate::AudioIn)
//! copies each 16 kHz frame into a `VadTee` (a small bounded queue) and goes on; the
//! `VadWorker` helper task takes everything queued and runs it through a
//! [`VadStream`] as ONE job on the [`Offload`] (the inference pool in production, the calling
//! task in paused-time simulations). It emits the backdated `UserStartedSpeaking` and
//! `UserStoppedSpeaking` system frames. The audio frames themselves never wait for it.
//!
//! # One job in flight, batching
//! While a job runs, newer frames queue in the tee. When the job returns, the worker takes all of
//! them as the next batch, so a slow model costs latency in the VAD events, never audio. The tee
//! holds at most `capacity` frames (8 = 160 ms by default). When it is full the OLDEST frame is
//! dropped and counted, and the next batch is marked as following a gap.
//!
//! # Priority
//! The batch job goes in the pool's [`Priority::High`] line: it is short (microseconds with the
//! energy model, a fraction of a millisecond with Silero) and every call needs one every 20-32
//! ms, while a Smart Turn analysis runs for 1.5-25 ms. Queued behind a burst of analyses (a few
//! hundred calls whose turns end together) the batches of all calls waited their turn, overflowed
//! the shared queue and were rejected, which loses the call's VAD stream. With one batch
//! in flight per call, the high line needs room for one job per call.
//!
//! # Gaps
//! Audio is not contiguous after a drop in the tee or after a sequence-number jump before
//! [`AudioIn`](crate::AudioIn) (the first-stage queue drops oldest too). The model's windows
//! and recurrent state would straddle the hole, so the worker closes the stream there with
//! [`VadStream::finish`]: speech in progress is ended with a stop dated to the last speech seen,
//! the stream is reset and the audio after the hole starts a fresh stream. A short loss in the
//! middle of an utterance therefore reads as a short pause (stop, then start), which the turn
//! machine already handles ("user resumes"), instead of a stop that never comes.
//!
//! # Falling hint (opt-in)
//! [`VadSetup::with_fall_hint`] (or [`VadSetup::with_early_analysis`], from the engine
//! configuration) makes the worker send `SystemFrame::UserSpeechFalling` once per fall: when the
//! probability has stayed below the negative threshold for the configured delay after the last
//! speech window and the stop is not confirmed yet. It is dated to the time the stop will carry,
//! always precedes that stop (with exactly its time, which needs `speech_pad <= min_silence`;
//! otherwise none is sent), and lets the [`TurnDriver`](crate::TurnDriver) start its Smart
//! Turn analysis early (`docs/LATENCY.md`, "Early analysis"). Without it no such frame is ever
//! produced and the worker behaves exactly as before.
//!
//! # Failures
//! A job can fail with `PoolSaturated`, a timeout, a panic or a closed pool, and the model can
//! report errors for single windows. Each is counted (see [`VadStats`]).
//!
//! A job the pool REFUSED (`PoolSaturated`) or that timed out before a pool thread started it
//! never ran, so nothing is lost: the stream and the batch are parked in a slot the job takes
//! them from, and the worker takes them back. It keeps the stream, waits 10 ms
//! (`RETRY_DELAY`) on the pool's clock, and runs the same audio again together with whatever
//! arrived meanwhile (`retries`): overload becomes delay, not a lost turn. The kept audio never
//! exceeds the tee's capacity or 50 frames (`RETAIN_FRAMES`), whichever is larger (new audio
//! joins it only up to that limit and waits in the tee otherwise); once the kept audio has
//! reached the limit it is given up like a tee overflow (`tee_dropped`, and the stream closes
//! at the hole). Every
//! refusal still counts toward the fail-open streak below.
//!
//! A job that DID start and was lost (a panic, a timeout while running) takes the stream with it
//! (the closure owned it), so the stream is rebuilt from the setup's model factory for the next
//! batch; if speech was in progress, a stop is synthesized first so no turn waits for a stop that
//! cannot come. After `max_consecutive_failures` failed batches in a row, spanning at
//! least `failure_window` of audio, the worker FAILS OPEN: it sends one non-fatal `Error` frame
//! and from then on only drains the tee, so audio keeps flowing to STT and no VAD event is
//! produced (the turn machine then relies on its own fallbacks). A call that needs VAD events to
//! work must treat that frame as a degradation.
//!
//! # Warm-up
//! With [`VadSetup::with_warm_up`] (which [`SessionBuilder`](crate::SessionBuilder) always
//! applies), a helper task runs [`SpeechModel::warm`] when the worker starts (the pipeline's
//! `Start`) once on every thread of the [`Offload`] (once inline), on a model of its own from the
//! setup's factory, each job bounded by the warm-up timeout: the ONNX Runtime Silero keeps one
//! session per pool thread and loads it on its first call (about 80 ms), which would otherwise
//! land on the first speech of the call. On a pool this happens once per pool and model for the
//! whole process ([`Offload::claim_warm`], keyed by [`VadSetup::with_model_id`], else the
//! model's own [`SpeechModel::warm_key`] for a setup made with [`VadSetup::from_model`] (the
//! Silero's file), else the model type, which the warm job flags with a warning when the model
//! names its own key): the first call warms, later calls skip it (their `warms` stay zero), so a burst of call
//! starts cannot fill the shared queue with warm jobs and make running calls' VAD jobs fail as
//! saturated. A pool whose queue is deep is not warmed at all (`warm_skips`; live VAD windows go
//! first). A failure is counted (`warm_failures`) and never fails the call; the claim comes back
//! when the last warm job has finished (a job still running after its timeout keeps it, so the
//! model is never warmed twice at once), after [`crate::offload::WARM_COOLDOWN`] when the pool
//! was busy. The task is aborted with the worker (an aborted warm-up gives the claim back too)
//! and traced as the span `vad_warm` on `brisvox::engine`. Without it (the default of a bare
//! setup) the factory is only called for the worker's own streams.
//!
//! The worker's own streams are built inside the batch job, on the offload (the first batch, and
//! the first after a lost job): a factory may touch the file system (`SileroVad::load` checks
//! the model file), which must not happen on a tokio worker.
//!
//! # Recovery from fail-open
//! Failing open is not permanent: a pool that was saturated for a few seconds is usually healthy
//! again afterwards, and a call that lost barge-in and turn end for its remaining minutes would
//! be a bad trade. While failed open, the worker probes once per `recovery_interval` of AUDIO
//! time (default 5 s; `None` disables it): the next batch after the interval is run through a
//! FRESH stream (built from the model factory) as a normal job. If it succeeds, the worker
//! recovers: the failed-open flag and the failure streak are cleared, `recoveries` is counted
//! and VAD events resume (speech already in progress at that point simply starts a new
//! utterance with a backdated start). If it fails, the worker stays failed open (speech the
//! failed probe may have started is closed again) and waits another interval. The single `Error`
//! frame has no counterpart; observers read `VadStatsSnapshot::failed_open` and `recoveries`.
//!
//! The worker forms a batch per audio frame (20 ms apart) when the model keeps up, so a count
//! alone would turn a 100 ms burst of `PoolSaturated` (which is rejected at once, the very case
//! the bounded pool exists for) into a call-long loss of VAD. The streak therefore also has to
//! last `failure_window` (default 1 s) of AUDIO time (the batch timestamps, which come from the
//! injected clock): a burst shorter than that heals itself with the next successful job, which
//! resets the streak. Only the first failure of a streak is logged at `warn`; the rest are
//! `debug`, so a burst does not log at 50 Hz.

use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_core::frame::{ErrorFrame, Frame, SystemFrame};
use brisvox_core::{SharedClock, TimelineRecorder, TurnEvent};
use brisvox_inference::{
    Done, Error as InferenceError, Priority, SpeechModel, VadEvent, VadParams, VadSignal, VadStream,
};
use tokio::sync::Notify;
use tokio::time::Instant;
use tracing::Instrument;

use crate::config::EngineConfig;
use crate::guard::{GuardedSender, TaskGuard};
use crate::offload::Offload;

/// Sample rate of the VAD chain (the only rate the Silero wrapper accepts).
const MODEL_HZ: u32 = 16_000;

/// How long the worker waits before it offers a batch the pool refused again (half a frame: the
/// next frames have usually arrived and join the retry).
const RETRY_DELAY: Duration = Duration::from_millis(10);

/// Most frames (about a second of audio) the worker keeps for a retry; a longer refusal sheds
/// the kept audio like a tee overflow.
const RETAIN_FRAMES: usize = 50;

/// A deterministic stand-in for a speech model: energy against a threshold.
///
/// The probability is `1.0` for a window whose RMS level is at or above the threshold and `0.0`
/// below it, so with the default [`VadParams`] hysteresis it behaves like the simulator's
/// energy VAD. It exists so paused-time simulations can run the production VAD path
/// ([`VadStream`], the worker, batching, gaps) without a neural network. Windows default to
/// 320 samples (20 ms at 16 kHz), the engine's frame, so detection time resolution equals the
/// simulator's per-frame VAD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnergyModel {
    /// RMS threshold on samples scaled to -1..1.
    threshold: f32,
    window: usize,
}

impl EnergyModel {
    /// Default window: 20 ms at 16 kHz.
    pub const DEFAULT_WINDOW: usize = 320;

    /// A model with `threshold` as an RMS level on samples scaled to -1..1.
    pub const fn new(threshold: f32) -> Self {
        Self {
            threshold,
            window: Self::DEFAULT_WINDOW,
        }
    }

    /// A model with `threshold` as an RMS level in PCM16 units (500 is a soft voice).
    pub fn from_pcm_rms(threshold: f32) -> Self {
        Self::new(threshold / 32_768.0)
    }

    /// The same model with a window of `window` samples (at least 1).
    #[must_use]
    pub const fn with_window(mut self, window: usize) -> Self {
        self.window = if window == 0 { 1 } else { window };
        self
    }
}

impl SpeechModel for EnergyModel {
    fn window(&self) -> usize {
        self.window
    }

    fn context(&self) -> usize {
        0
    }

    fn sample_rate(&self) -> u32 {
        MODEL_HZ
    }

    fn infer(&mut self, input: &[f32]) -> brisvox_inference::Result<f32> {
        if input.is_empty() {
            return Ok(0.0);
        }
        let sum: f64 = input
            .iter()
            .map(|x| {
                let v = f64::from(*x);
                v * v
            })
            .sum();
        #[allow(clippy::cast_precision_loss)] // window lengths are far below 2^52
        let rms = (sum / input.len() as f64).sqrt();
        Ok(if rms >= f64::from(self.threshold) {
            1.0
        } else {
            0.0
        })
    }

    fn reset(&mut self) {}
}

/// How the VAD worker behaves.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VadWorkerConfig {
    /// Detector settings (thresholds, minimum speech and silence, padding).
    pub params: VadParams,
    /// Longest one job may take on the offload before it counts as a failure. Default 250 ms
    /// ([`Timeouts::vad_job`](crate::Timeouts::vad_job)).
    pub job_timeout: Duration,
    /// Failed batches in a row after which the worker may fail open (see the module docs).
    /// Default 5.
    pub max_consecutive_failures: u32,
    /// Shortest span of audio that the failing batches must cover before the worker fails open.
    /// A burst of failures shorter than this recovers with the next successful job. Default 1 s;
    /// [`Duration::ZERO`] fails open as soon as `max_consecutive_failures` is reached.
    pub failure_window: Duration,
    /// While failed open, how much audio time passes between recovery probes (see the module
    /// docs). `None` keeps the worker failed open for the rest of the call. Default 5 s.
    pub recovery_interval: Option<Duration>,
}

impl VadWorkerConfig {
    /// Defaults: [`VadParams::default`], the engine's `vad_job` timeout, 5 failures over 1 s.
    pub fn from_engine(config: &EngineConfig) -> Self {
        Self {
            params: VadParams::default(),
            job_timeout: config.timeouts.vad_job,
            max_consecutive_failures: 5,
            failure_window: Duration::from_secs(1),
            recovery_interval: Some(Duration::from_secs(5)),
        }
    }
}

impl Default for VadWorkerConfig {
    fn default() -> Self {
        Self::from_engine(&EngineConfig::DEFAULT)
    }
}

type StreamFactory<M> = Arc<dyn Fn() -> brisvox_inference::Result<VadStream<M>> + Send + Sync>;

/// Everything the VAD worker of an [`AudioIn`](crate::AudioIn) needs: where jobs run, how to
/// make a model, and the settings.
pub struct VadSetup<M: SpeechModel + 'static> {
    factory: StreamFactory<M>,
    offload: Offload,
    /// Where the worker marks [`TurnEvent::VadStop`] (see [`Self::with_timeline`]).
    timeline: Option<(TimelineRecorder, SharedClock)>,
    config: VadWorkerConfig,
    /// The bound of each warm-up job, when the worker warms the model at start.
    warm_up: Option<Duration>,
    /// What the warm-up is claimed under (see [`Self::with_model_id`]).
    model_id: Option<Arc<str>>,
}

impl<M: SpeechModel + 'static> VadSetup<M> {
    /// A setup running jobs on `offload`. `make_model` builds a fresh model (one per stream:
    /// the first, and every rebuild after a lost job); it runs inside the batch job on the
    /// offload, and should still be cheap (share loaded weights behind an `Arc` and only create
    /// the per-stream state): the batch waits for it.
    pub fn new<F>(offload: Offload, config: VadWorkerConfig, make_model: F) -> Self
    where
        F: Fn() -> brisvox_inference::Result<M> + Send + Sync + 'static,
    {
        let params = config.params;
        Self {
            factory: Arc::new(move || make_model().map(|model| VadStream::new(model, params))),
            offload,
            config,
            warm_up: None,
            model_id: None,
            timeline: None,
        }
    }

    /// A setup whose streams are copies of `model` (reset before use), for a model that is
    /// loaded once and cheap to copy (the ONNX Runtime Silero: a file path and its state).
    /// Building a stream then never touches the file system, and the warm-up claim is keyed on
    /// the model's own [`SpeechModel::warm_key`] (its file for the Silero), so two setups over
    /// different files each warm their own and nobody has to remember
    /// [`Self::with_model_id`] (which still wins when set).
    pub fn from_model(offload: Offload, config: VadWorkerConfig, model: M) -> Self
    where
        M: Clone + Send + Sync,
    {
        let model_id = model.warm_key().map(Arc::from);
        let setup = Self::new(offload, config, move || {
            let mut copy = model.clone();
            copy.reset();
            Ok(copy)
        });
        Self { model_id, ..setup }
    }

    /// Names the model the warm-up prepares, for the once-per-pool claim
    /// ([`Offload::claim_warm`]): the model file for a model that keeps its state per pool thread
    /// (the ONNX Runtime Silero keeps one session per thread and file). Without it the claim is
    /// keyed by the model type, so two setups of the same type with different files would share
    /// one warm-up and the second file would stay cold.
    #[must_use]
    pub fn with_model_id(mut self, id: impl Into<Arc<str>>) -> Self {
        self.model_id = Some(id.into());
        self
    }

    /// Runs the jobs on `offload` instead of the one given to [`Self::new`]
    /// ([`SessionBuilder::with_vad`](crate::SessionBuilder::with_vad) uses the builder's).
    #[must_use]
    pub(crate) fn with_offload(mut self, offload: Offload) -> Self {
        self.offload = offload;
        self
    }

    /// Warms the model on every offload thread when the worker starts (see the module docs,
    /// "Warm-up"), each job bounded by `timeout`, once per pool and model type for the whole
    /// process ([`Offload::claim_warm`]): later calls on a warm pool skip it.
    /// [`SessionBuilder`](crate::SessionBuilder) applies it with the engine's `timeouts.connect`.
    #[must_use]
    pub const fn with_warm_up(mut self, timeout: Duration) -> Self {
        self.warm_up = Some(timeout);
        self
    }

    /// Marks [`TurnEvent::VadStop`] on `timeline` (the open user turn,
    /// [`TimelineRecorder::mark_open_turn`]) at `clock`'s now whenever the worker confirms a
    /// stop: the moment of the decision, where `UserSpeechEnd` is backdated to the speech.
    /// [`SessionBuilder::with_vad`](crate::SessionBuilder::with_vad) applies the call's.
    #[must_use]
    pub fn with_timeline(mut self, timeline: TimelineRecorder, clock: SharedClock) -> Self {
        self.timeline = Some((timeline, clock));
        self
    }
}

impl<M: SpeechModel + 'static> VadSetup<M> {
    /// Sends the opt-in `UserSpeechFalling` hint: once per fall, when the probability
    /// has stayed below the negative threshold for `after` following the last speech window and
    /// the stop is not confirmed yet (see [`brisvox_inference::VadDetector::with_falling_hint`]).
    /// Without it (the default) the worker never sends one and behaves exactly as before.
    ///
    /// A hint can only precede a stop when `after` (rounded up to whole windows, at least one)
    /// is shorter than the detector's `min_silence` and `speech_pad <= min_silence`
    /// ([`VadParams::falling_hint_possible`]). Otherwise the setup is inert (no hint is ever
    /// sent, [`VadStatsSnapshot::falls`] stays zero) and a warning is logged once, when the
    /// worker builds its first stream: the check needs the model's window, and the model is only
    /// ever built inside a job on the offload, never on the caller's thread (building one can
    /// load a model file).
    #[must_use]
    pub fn with_fall_hint(mut self, after: Option<Duration>) -> Self {
        let inner = Arc::clone(&self.factory);
        let params = self.config.params;
        let checked = Arc::new(AtomicBool::new(false));
        self.factory = Arc::new(move || {
            inner().map(|stream| {
                if let Some(after) = after
                    && !checked.swap(true, Ordering::Relaxed)
                    && !stream.falling_hint_possible(after)
                {
                    tracing::warn!(
                        target: "brisvox::engine",
                        ?after,
                        ?params,
                        "falling hint delay cannot precede the VAD stop (delay not below                          min_silence, or speech_pad above min_silence): no UserSpeechFalling                          will be sent"
                    );
                }
                stream.with_falling_hint(after)
            })
        });
        self
    }

    /// [`Self::with_fall_hint`] as the engine configuration says: the hint is sent when
    /// [`EngineConfig::early_analysis`] is enabled, after its `fall_delay`.
    ///
    /// **Both halves are needed.** Enabling `EngineConfig::early_analysis` only switches on the
    /// `TurnDriver` half (it starts an early analysis when a hint arrives). The VAD half is this
    /// call: an assembly that sets the flag but builds a plain `VadSetup` gets no hints, so the
    /// feature does nothing (`early_hints` stays zero in the driver's statistics).
    #[must_use]
    pub fn with_early_analysis(self, config: &EngineConfig) -> Self {
        let early = config.early_analysis;
        self.with_fall_hint(early.enabled.then_some(early.fall_delay))
    }
}

impl<M: SpeechModel + 'static> std::fmt::Debug for VadSetup<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VadSetup")
            .field("offload", &self.offload)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
pub(crate) struct VadCounters {
    frames: AtomicU64,
    batches: AtomicU64,
    jobs_ok: AtomicU64,
    saturated: AtomicU64,
    timeouts: AtomicU64,
    panics: AtomicU64,
    other_failures: AtomicU64,
    model_errors: AtomicU64,
    rebuilds: AtomicU64,
    retries: AtomicU64,
    build_failures: AtomicU64,
    gap_resets: AtomicU64,
    tee_dropped: AtomicU64,
    discarded_failed_open: AtomicU64,
    starts: AtomicU64,
    stops: AtomicU64,
    synthesized_stops: AtomicU64,
    falls: AtomicU64,
    recoveries: AtomicU64,
    warms: AtomicU64,
    warm_failures: AtomicU64,
    warm_skips: AtomicU64,
    failed_open: AtomicBool,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

fn add(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

/// Shared, cheap-to-clone handle on the counters of one VAD worker.
#[derive(Debug, Clone)]
pub struct VadStats {
    counters: Arc<VadCounters>,
}

/// A point-in-time copy of the VAD counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VadStatsSnapshot {
    /// Frames the worker took from the tee.
    pub frames: u64,
    /// Batches taken (one job each, unless the worker had failed open).
    pub batches: u64,
    /// Jobs that completed.
    pub jobs_ok: u64,
    /// Jobs rejected by a saturated pool.
    pub saturated: u64,
    /// Jobs that timed out.
    pub timeouts: u64,
    /// Jobs that panicked.
    pub panics: u64,
    /// Jobs lost for another reason (a closed pool).
    pub other_failures: u64,
    /// Batches in which the model reported an error for a window.
    pub model_errors: u64,
    /// Streams rebuilt after a lost one.
    pub rebuilds: u64,
    /// Batches that never ran (the pool refused them, or they timed out unstarted) and were kept,
    /// stream included, to be run again with the audio that arrived meanwhile.
    pub retries: u64,
    /// Stream rebuilds that failed.
    pub build_failures: u64,
    /// Times the stream was closed and restarted because the audio had a hole.
    pub gap_resets: u64,
    /// Frames dropped by the tee because the worker fell behind (oldest first).
    pub tee_dropped: u64,
    /// Frames drained without detection while the worker was failed open (probe batches are not
    /// counted).
    pub discarded_failed_open: u64,
    /// `UserStartedSpeaking` frames sent.
    pub starts: u64,
    /// `UserStoppedSpeaking` frames sent (including synthesized ones).
    pub stops: u64,
    /// Stops sent because the stream that knew about the speech was lost.
    pub synthesized_stops: u64,
    /// `UserSpeechFalling` hints sent (always zero unless [`VadSetup::with_fall_hint`] or
    /// [`VadSetup::with_early_analysis`] was applied, and zero when the delay cannot precede the
    /// stop).
    pub falls: u64,
    /// Times the worker recovered from failing open (a probe job succeeded).
    pub recoveries: u64,
    /// Model warm-up jobs that succeeded at the start of the call (one per inference pool
    /// thread, or one inline; see the module docs, "Warm-up").
    pub warms: u64,
    /// Model warm-up jobs that failed or timed out (the call goes on; the first real job on
    /// that thread pays the cold start).
    pub warm_failures: u64,
    /// Model warm-up jobs not submitted because the pool's queue was deep (the warm-up cools
    /// down and a later call warms; [`crate::offload::WARM_COOLDOWN`]).
    pub warm_skips: u64,
    /// Whether the worker is failed open right now.
    pub failed_open: bool,
}

impl VadStats {
    pub(crate) fn new() -> Self {
        Self {
            counters: Arc::new(VadCounters::default()),
        }
    }

    pub(crate) fn counters(&self) -> Arc<VadCounters> {
        Arc::clone(&self.counters)
    }

    /// The counters now.
    pub fn snapshot(&self) -> VadStatsSnapshot {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        VadStatsSnapshot {
            frames: get(&c.frames),
            batches: get(&c.batches),
            jobs_ok: get(&c.jobs_ok),
            saturated: get(&c.saturated),
            timeouts: get(&c.timeouts),
            panics: get(&c.panics),
            other_failures: get(&c.other_failures),
            model_errors: get(&c.model_errors),
            rebuilds: get(&c.rebuilds),
            retries: get(&c.retries),
            build_failures: get(&c.build_failures),
            gap_resets: get(&c.gap_resets),
            tee_dropped: get(&c.tee_dropped),
            discarded_failed_open: get(&c.discarded_failed_open),
            starts: get(&c.starts),
            stops: get(&c.stops),
            synthesized_stops: get(&c.synthesized_stops),
            falls: get(&c.falls),
            recoveries: get(&c.recoveries),
            warms: get(&c.warms),
            warm_failures: get(&c.warm_failures),
            warm_skips: get(&c.warm_skips),
            failed_open: c.failed_open.load(Ordering::Relaxed),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The tee

/// One frame of model-rate audio waiting for the worker.
struct TeeFrame {
    samples: Vec<i16>,
    timestamp: Instant,
    /// The audio before this frame is not contiguous with the audio before it.
    gap_before: bool,
}

struct TeeState {
    frames: VecDeque<TeeFrame>,
    /// Sample buffers ready for reuse, so a steady-state push allocates nothing.
    spare: Vec<Vec<i16>>,
    /// The oldest queued frame was dropped: the next frame taken follows a hole.
    gap_next: bool,
}

/// The bounded queue between [`AudioIn`](crate::AudioIn) and the VAD worker.
///
/// Overflow policy: DROP OLDEST (counted in `tee_dropped`); the audio path never waits for the
/// worker. Frames are copied in (about 640 bytes), so the queue neither keeps the transport's
/// buffers alive nor needs a refcounted clone that could allocate.
pub(crate) struct VadTee {
    state: Mutex<TeeState>,
    notify: Notify,
    capacity: usize,
    samples_hint: usize,
    counters: Arc<VadCounters>,
}

impl VadTee {
    pub(crate) fn new(capacity: usize, samples_hint: usize, counters: Arc<VadCounters>) -> Self {
        let capacity = capacity.max(1);
        // `capacity` queued + one being filled + one being recycled: enough that a steady
        // state never allocates a sample buffer.
        let spare = (0..capacity + 2)
            .map(|_| Vec::with_capacity(samples_hint))
            .collect();
        Self {
            state: Mutex::new(TeeState {
                frames: VecDeque::with_capacity(capacity),
                spare,
                gap_next: false,
            }),
            notify: Notify::new(),
            capacity,
            samples_hint,
            counters,
        }
    }

    fn lock(&self) -> MutexGuard<'_, TeeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues a frame whose samples `fill` writes into the (cleared) buffer it is given. `gap`
    /// says the audio before this frame is not contiguous with the previous frame.
    pub(crate) fn push_with(
        &self,
        timestamp: Instant,
        gap: bool,
        fill: impl FnOnce(&mut Vec<i16>),
    ) {
        {
            let mut state = self.lock();
            let mut samples = state
                .spare
                .pop()
                .unwrap_or_else(|| Vec::with_capacity(self.samples_hint));
            samples.clear();
            fill(&mut samples);
            if state.frames.len() >= self.capacity
                && let Some(oldest) = state.frames.pop_front()
            {
                state.spare.push(oldest.samples);
                state.gap_next = true;
                bump(&self.counters.tee_dropped);
            }
            state.frames.push_back(TeeFrame {
                samples,
                timestamp,
                gap_before: gap,
            });
        }
        self.notify.notify_one();
    }

    /// Moves everything queued into `batch`. The batch is cleared first unless `append` is set
    /// (a batch kept after a rejected job: the new audio joins it, contiguous frames extending
    /// its last segment). At most `room` frames are moved; the rest stay queued for the next
    /// take (or are shed by the tee's own overflow policy). Returns whether the batch holds
    /// anything.
    fn take(&self, batch: &mut Batch, append: bool, room: u64) -> bool {
        if !append {
            batch.clear();
        }
        let mut state = self.lock();
        let mut taken = 0u64;
        while taken < room
            && let Some(frame) = state.frames.pop_front()
        {
            taken += 1;
            // Both operands are evaluated on purpose (`|`, not `||`): a frame that already carries
            // its own hole must still consume the overflow hole, or the flag would leak onto the
            // next, contiguous frame and split the stream in the middle of speech.
            let from_drop = std::mem::take(&mut state.gap_next);
            let gap = frame.gap_before | from_drop;
            let contiguous = !gap && !batch.segments.is_empty();
            match batch.segments.last_mut() {
                Some(segment) if contiguous => segment.samples += frame.samples.len(),
                _ => batch.segments.push(Segment {
                    start: frame.timestamp,
                    samples: frame.samples.len(),
                    gap_before: gap,
                }),
            }
            batch.pcm.extend_from_slice(&frame.samples);
            batch.frames += 1;
            batch.end = Some(frame.timestamp + frames_duration(frame.samples.len()));
            state.spare.push(frame.samples);
        }
        batch.frames > 0
    }

    async fn wait(&self) {
        self.notify.notified().await;
    }

    /// Marks a hole before the next frame taken (audio the worker gave up on).
    fn mark_hole(&self) {
        self.lock().gap_next = true;
    }
}

fn frames_duration(samples: usize) -> Duration {
    let samples = u64::try_from(samples).unwrap_or(u64::MAX);
    Duration::from_micros(samples.saturating_mul(1_000_000) / u64::from(MODEL_HZ))
}

/// A run of contiguous samples inside a batch.
#[derive(Debug, Clone, Copy)]
struct Segment {
    start: Instant,
    samples: usize,
    /// Close the stream before this segment (a hole precedes it).
    gap_before: bool,
}

/// What one job processes. The buffers travel into the job and come back, so a steady state
/// reuses them.
#[derive(Default)]
struct Batch {
    pcm: Vec<i16>,
    segments: Vec<Segment>,
    events: Vec<VadSignal>,
    frames: u64,
    /// End of the audio in the batch.
    end: Option<Instant>,
}

impl Batch {
    fn clear(&mut self) {
        self.pcm.clear();
        self.segments.clear();
        self.events.clear();
        self.frames = 0;
        self.end = None;
    }
}

/// What a batch job works on. The worker parks it in a slot before the job is submitted and the
/// job takes it out: a job the pool never started leaves it there for the worker to take back.
struct Payload<M: SpeechModel> {
    /// The attempt this payload was parked for (only that attempt's job may take it).
    attempt: u64,
    /// `None` when the job has to build the stream first.
    stream: Option<VadStream<M>>,
    batch: Batch,
}

/// The parked [`Payload`] of the batch in flight (one per worker; locked for a few nanoseconds).
type Slot<M> = Arc<Mutex<Option<Payload<M>>>>;

/// Takes the payload parked for `attempt`; a payload of another attempt stays where it is.
fn take_attempt<M: SpeechModel>(slot: &Slot<M>, attempt: u64) -> Option<Payload<M>> {
    let mut guard = lock_slot(slot);
    if guard.as_ref().is_some_and(|p| p.attempt == attempt) {
        guard.take()
    } else {
        None
    }
}

fn lock_slot<M: SpeechModel>(slot: &Slot<M>) -> MutexGuard<'_, Option<Payload<M>>> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a batch job returns: `None` when the slot was empty (an abandoned job whose payload the
/// worker already took back), else the finished batch or the factory's error with the buffers.
type JobResult<M> = Option<Result<JobOutput<M>, Box<(Batch, InferenceError)>>>;

struct JobOutput<M: SpeechModel> {
    stream: VadStream<M>,
    batch: Batch,
    error: Option<InferenceError>,
}

/// Runs a batch through the stream (on the pool thread, or inline).
fn run_batch<M: SpeechModel>(mut stream: VadStream<M>, mut batch: Batch) -> JobOutput<M> {
    batch.events.clear();
    let mut error = None;
    let mut offset = 0usize;
    for segment in &batch.segments {
        if segment.gap_before
            && let Some(event) = stream.finish()
        {
            batch.events.push(VadSignal::Event(event));
        }
        let end = offset.saturating_add(segment.samples);
        if let Some(pcm) = batch.pcm.get(offset..end)
            && let Err(e) = stream.push_signals(pcm, segment.start, &mut batch.events)
            && error.is_none()
        {
            error = Some(e);
        }
        offset = end;
    }
    JobOutput {
        stream,
        batch,
        error,
    }
}

// ---------------------------------------------------------------------------------------------
// The worker

/// Starts the VAD worker task for `setup`.
pub(crate) fn spawn_worker<M: SpeechModel + 'static>(
    setup: VadSetup<M>,
    tee: Arc<VadTee>,
    out: GuardedSender,
    counters: Arc<VadCounters>,
) -> TaskGuard {
    // Once per pool and model for the process: the sessions are per pool thread.
    let keyed_by_type = setup.model_id.is_none();
    let key = setup.model_id.as_deref().map_or_else(
        || format!("vad:{}", std::any::type_name::<M>()),
        |id| format!("vad:{id}"),
    );
    let claim = setup
        .warm_up
        .and_then(|timeout| Some((timeout, setup.offload.claim_warm(&key)?)));
    let warm = claim.map(|(timeout, claim)| {
        let factory = Arc::clone(&setup.factory);
        let (offload, counters) = (setup.offload.clone(), Arc::clone(&counters));
        let span = tracing::debug_span!(target: "brisvox::engine", "vad_warm", key = %key);
        TaskGuard::spawn(
            "vad_warm",
            async move {
                let jobs = offload.threads();
                let report = offload
                    .warm(claim, timeout, jobs, move || {
                        let stream = factory()?;
                        if keyed_by_type && let Some(own) = stream.warm_key() {
                            // Another setup of this type over a different file would find the
                            // claim taken and stay cold (see `VadSetup::from_model`).
                            tracing::warn!(
                                target: "brisvox::engine",
                                model = %own,
                                "vad warm-up claimed by model type although the model names                                  its own key: build the setup with VadSetup::from_model or                                  with_model_id"
                            );
                        }
                        stream.warm()
                    })
                    .await;
                add(&counters.warms, report.ok as u64);
                add(&counters.warm_failures, report.failed as u64);
                add(&counters.warm_skips, report.skipped as u64);
                if report.skipped > 0 {
                    tracing::debug!(target: "brisvox::engine", "vad warm-up skipped: pool busy");
                } else if report.failed > 0 {
                    tracing::debug!(
                        target: "brisvox::engine",
                        failed = report.failed,
                        "vad model warm-up failed on some threads"
                    );
                } else {
                    tracing::debug!(target: "brisvox::engine", jobs, "vad model warm");
                }
            }
            .instrument(span),
        )
    });
    let worker = Worker {
        _warm: warm,
        factory: setup.factory,
        offload: setup.offload,
        config: setup.config,
        tee,
        out,
        counters,
        stream: None,
        built_before: false,
        batch: Batch::default(),
        slot: Arc::new(Mutex::new(None)),
        attempt: 0,
        retain: false,
        counted: 0,
        timeline: setup.timeline,
        speaking: false,
        last_end: None,
        consecutive_failures: 0,
        streak_start: None,
        failed_open: false,
        last_probe: None,
    };
    TaskGuard::spawn("vad_worker", worker.run())
}

struct Worker<M: SpeechModel + 'static> {
    /// The start-of-call model warm-up, if enabled; aborted with the worker.
    _warm: Option<TaskGuard>,
    factory: StreamFactory<M>,
    offload: Offload,
    config: VadWorkerConfig,
    tee: Arc<VadTee>,
    out: GuardedSender,
    counters: Arc<VadCounters>,
    /// `None` before the first batch and after a lost job.
    stream: Option<VadStream<M>>,
    built_before: bool,
    batch: Batch,
    /// Where the batch in flight waits for its job (see [`Payload`]).
    slot: Slot<M>,
    /// Number of the last attempt parked in `slot`.
    attempt: u64,
    /// `batch` holds audio the pool refused: the next take appends to it.
    retain: bool,
    /// Frames of `batch` already added to the `frames` counter.
    counted: u64,
    /// Where confirmed stops are marked ([`VadSetup::with_timeline`]).
    timeline: Option<(TimelineRecorder, SharedClock)>,
    /// The last event sent was a start.
    speaking: bool,
    /// End of the audio of the last batch that went through the stream.
    last_end: Option<Instant>,
    consecutive_failures: u32,
    /// Audio time of the end of the first batch of the current failure streak.
    streak_start: Option<Instant>,
    failed_open: bool,
    /// While failed open: audio time of the last probe (or of the moment of failing open).
    last_probe: Option<Instant>,
}

/// Why a batch counts as failed.
enum Failure {
    /// The job was lost with the stream.
    Job(InferenceError),
    /// The job never ran: the stream and the batch were kept.
    Kept(InferenceError),
    Build(InferenceError),
    Model(InferenceError),
}

/// How many frames the next take may move into a batch that already holds `kept` frames
/// (`append`: a batch kept for a retry) under `limit`; a fresh batch is bounded by the tee only.
fn take_room(append: bool, limit: u64, kept: u64) -> u64 {
    if append {
        limit.saturating_sub(kept).max(1)
    } else {
        u64::MAX
    }
}

impl<M: SpeechModel + 'static> Worker<M> {
    async fn run(mut self) {
        loop {
            if self.retain {
                self.offload.sleep(RETRY_DELAY).await;
            }
            let mut batch = std::mem::take(&mut self.batch);
            let mut append = self.retain;
            let limit = u64::try_from(self.tee.capacity.max(RETAIN_FRAMES)).unwrap_or(u64::MAX);
            if append && batch.frames >= limit {
                // The pool has refused this audio for as long as the tee holds: shed it like a
                // tee overflow (the stream closes at the hole) instead of growing.
                add(&self.counters.tee_dropped, batch.frames);
                self.tee.mark_hole();
                append = false;
            }
            if !append {
                self.counted = 0;
            }
            // A kept batch grows only up to the limit, so the audio held for a retry never
            // exceeds it (the rest waits in the tee).
            let room = take_room(append, limit, batch.frames);
            let any = self.tee.take(&mut batch, append, room);
            self.batch = batch;
            self.retain = false;
            if !any {
                self.tee.wait().await;
                continue;
            }
            if self.handle_batch().await.is_break() {
                return;
            }
        }
    }

    async fn handle_batch(&mut self) -> ControlFlow<()> {
        add(
            &self.counters.frames,
            self.batch.frames.saturating_sub(self.counted),
        );
        if self.counted == 0 {
            bump(&self.counters.batches);
        }
        self.counted = self.batch.frames;
        if self.failed_open {
            if !self.probe_due() {
                add(&self.counters.discarded_failed_open, self.batch.frames);
                return ControlFlow::Continue(());
            }
            // A probe: this batch goes through a fresh stream like any other. The stream left
            // from before (if any) does not know about the audio drained since.
            self.last_probe = self.batch.end;
            self.stream = None;
        }
        // A missing stream (the first batch, or after a lost job) is built in the job, on the
        // offload: a model factory may touch the file system (`SileroVad::load` checks the
        // file), which must not run on a tokio worker.
        let stream = self.stream.take();
        // A stream built after an earlier attempt is a rebuild (counted when it ran).
        let rebuild = stream.is_none() && self.built_before;
        let built_before = self.built_before;
        self.built_before |= stream.is_none();
        let factory = Arc::clone(&self.factory);
        let batch = std::mem::take(&mut self.batch);
        let end = batch.end;
        // The payload carries the number of its attempt: an abandoned job of an earlier attempt
        // that a pool thread starts late finds another attempt's payload (or none) and takes
        // nothing, so it cannot steal the payload parked for a later attempt. (A slot per attempt
        // would allocate on every batch.)
        self.attempt = self.attempt.wrapping_add(1);
        let attempt = self.attempt;
        *lock_slot(&self.slot) = Some(Payload {
            attempt,
            stream,
            batch,
        });
        let slot = Arc::clone(&self.slot);
        // A VAD batch is short and a call needs one every 20-32 ms: it goes in the pool's
        // high-priority line, ahead of the multi-millisecond jobs (Smart Turn analyses) that
        // would otherwise queue in front of it and overflow the queue.
        let result = self
            .offload
            .run_with(
                Priority::High,
                self.config.job_timeout,
                move || -> JobResult<M> {
                    let Payload { stream, batch, .. } = take_attempt(&slot, attempt)?;
                    let stream = match stream {
                        Some(stream) => stream,
                        None => match factory() {
                            Ok(stream) => stream,
                            // Boxed: the failure path is rare, the batch is large.
                            Err(error) => return Some(Err(Box::new((batch, error)))),
                        },
                    };
                    Some(Ok(run_batch(stream, batch)))
                },
            )
            .await;
        match result {
            Ok(Done { value: None, .. }) => {
                // Nothing to run: the payload was taken by an earlier, abandoned job.
                let error = InferenceError::Core(brisvox_core::Error::ChannelClosed("vad batch"));
                self.fail(Failure::Job(error), end).await
            }
            Ok(Done {
                value: Some(Err(failed)),
                ..
            }) => {
                // The batch buffers come back for reuse; its audio is not run again.
                let (batch, error) = *failed;
                self.batch = batch;
                self.fail(Failure::Build(error), end).await
            }
            Ok(Done {
                value: Some(Ok(output)),
                ..
            }) => {
                if rebuild {
                    bump(&self.counters.rebuilds);
                }
                bump(&self.counters.jobs_ok);
                let JobOutput {
                    stream,
                    mut batch,
                    error,
                } = output;
                self.stream = Some(stream);
                self.last_end = end.or(self.last_end);
                let gaps = batch.segments.iter().filter(|s| s.gap_before).count();
                add(&self.counters.gap_resets, u64::try_from(gaps).unwrap_or(0));
                let mut flow = ControlFlow::Continue(());
                for event in batch.events.drain(..) {
                    if self.emit(event).await.is_break() {
                        flow = ControlFlow::Break(());
                        break;
                    }
                }
                self.batch = batch;
                if flow.is_break() {
                    return flow;
                }
                match error {
                    None => {
                        self.consecutive_failures = 0;
                        self.streak_start = None;
                        if self.failed_open {
                            self.recover();
                        }
                        ControlFlow::Continue(())
                    }
                    Some(error) => self.fail(Failure::Model(error), end).await,
                }
            }
            Err(error) => {
                let parked = take_attempt(&self.slot, attempt);
                let refused = matches!(
                    error,
                    InferenceError::PoolSaturated { .. }
                        | InferenceError::Core(brisvox_core::Error::Timeout { .. })
                );
                match parked {
                    // The job never started (refused, or timed out in the queue and skipped):
                    // the stream and the audio are still here. Run them again, later.
                    Some(Payload { stream, batch, .. }) if refused => {
                        self.stream = stream;
                        self.batch = batch;
                        self.built_before = built_before;
                        self.retain = true;
                        bump(&self.counters.retries);
                        self.fail(Failure::Kept(error), end).await
                    }
                    // The closure owned the stream and the batch buffers: both are gone. The
                    // audio after this hole starts a fresh stream, so the speech the old one
                    // knew about must be closed (below) rather than left open.
                    _ => self.fail(Failure::Job(error), end).await,
                }
            }
        }
    }

    /// Counts a failed batch (whose audio ends at `end`), closes speech the lost stream knew
    /// about, and fails open after too many in a row over long enough a span of audio.
    async fn fail(&mut self, failure: Failure, end: Option<Instant>) -> ControlFlow<()> {
        let lost_stream = matches!(failure, Failure::Job(_) | Failure::Build(_));
        match &failure {
            Failure::Job(error) | Failure::Kept(error) => match error {
                InferenceError::PoolSaturated { .. } => bump(&self.counters.saturated),
                InferenceError::JobPanicked => bump(&self.counters.panics),
                InferenceError::Core(brisvox_core::Error::Timeout { .. }) => {
                    bump(&self.counters.timeouts);
                }
                _ => bump(&self.counters.other_failures),
            },
            Failure::Build(_) => bump(&self.counters.build_failures),
            Failure::Model(_) => bump(&self.counters.model_errors),
        }
        let (Failure::Job(error)
        | Failure::Kept(error)
        | Failure::Build(error)
        | Failure::Model(error)) = &failure;
        if self.consecutive_failures == 0 {
            tracing::warn!(target: "brisvox::engine", stage = "vad", %error, "vad batch failed");
        } else {
            tracing::debug!(
                target: "brisvox::engine",
                stage = "vad",
                %error,
                consecutive = self.consecutive_failures + 1,
                "vad batch failed again"
            );
        }
        if self.consecutive_failures == 0 {
            self.streak_start = end;
        }
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        // A failed probe (already failed open) must not leave speech open or a stale stream.
        if lost_stream || self.failed_open {
            // Drop the buffers that went with the job; the next take starts empty.
            self.stream = None;
            self.close_speech().await?;
        }
        let span = match (self.streak_start, end) {
            (Some(start), Some(end)) => end.saturating_duration_since(start),
            _ => Duration::ZERO,
        };
        if self.consecutive_failures >= self.config.max_consecutive_failures
            && span >= self.config.failure_window
        {
            self.fail_open(end).await?;
        }
        if self.failed_open {
            // Failed open drains the tee: audio kept for a retry is not run late.
            self.retain = false;
            self.batch.clear();
            self.counted = 0;
        }
        ControlFlow::Continue(())
    }

    /// Whether the batch in hand is the first one at least `recovery_interval` of audio after
    /// the last probe (or after failing open).
    fn probe_due(&self) -> bool {
        match (
            self.config.recovery_interval,
            self.last_probe,
            self.batch.end,
        ) {
            (Some(interval), Some(last), Some(end)) => {
                end.saturating_duration_since(last) >= interval
            }
            _ => false,
        }
    }

    /// A probe job succeeded: VAD events resume.
    fn recover(&mut self) {
        self.failed_open = false;
        self.counters.failed_open.store(false, Ordering::Relaxed);
        self.last_probe = None;
        bump(&self.counters.recoveries);
        tracing::warn!(
            target: "brisvox::engine",
            stage = "vad",
            "vad recovered after failing open: events resume"
        );
    }

    /// Ends speech in progress with a stop dated to the end of the last audio the lost stream
    /// processed.
    async fn close_speech(&mut self) -> ControlFlow<()> {
        if !self.speaking {
            return ControlFlow::Continue(());
        }
        let Some(at) = self.last_end else {
            self.speaking = false;
            return ControlFlow::Continue(());
        };
        bump(&self.counters.synthesized_stops);
        self.emit(VadSignal::Event(VadEvent::Stop { at })).await
    }

    async fn fail_open(&mut self, end: Option<Instant>) -> ControlFlow<()> {
        if self.failed_open {
            return ControlFlow::Continue(());
        }
        self.failed_open = true;
        self.last_probe = end;
        self.stream = None;
        self.counters.failed_open.store(true, Ordering::Relaxed);
        tracing::error!(
            target: "brisvox::engine",
            stage = "vad",
            failures = self.consecutive_failures,
            "vad failed open: audio keeps flowing, no more vad events"
        );
        self.close_speech().await?;
        let frame = Frame::System(SystemFrame::Error(Box::new(ErrorFrame {
            stage: "vad",
            message: format!(
                "voice activity detection failed {} batches in a row; continuing without it",
                self.consecutive_failures
            ),
            fatal: false,
        })));
        send_downstream(&self.out, frame).await
    }

    async fn emit(&mut self, signal: VadSignal) -> ControlFlow<()> {
        let frame = match signal {
            VadSignal::Event(VadEvent::Start { at }) => {
                self.speaking = true;
                bump(&self.counters.starts);
                SystemFrame::UserStartedSpeaking { at }
            }
            VadSignal::Event(VadEvent::Stop { at }) => {
                self.speaking = false;
                bump(&self.counters.stops);
                if let Some((timeline, clock)) = &self.timeline {
                    timeline.mark_open_turn(TurnEvent::VadStop, clock.now());
                }
                SystemFrame::UserStoppedSpeaking { at }
            }
            VadSignal::Falling { at } => {
                bump(&self.counters.falls);
                SystemFrame::UserSpeechFalling { at }
            }
        };
        send_downstream(&self.out, Frame::System(frame)).await
    }
}

/// Sends to the stage after `AudioIn`; `Break` when it is gone (the pipeline ended). A free
/// function so the future borrows only the sender (`Worker` is `Send` but not `Sync`).
async fn send_downstream(out: &GuardedSender, frame: Frame) -> ControlFlow<()> {
    match out.send(frame).await {
        Ok(_) => ControlFlow::Continue(()),
        Err(_) => ControlFlow::Break(()),
    }
}

#[cfg(test)]
mod tests {
    use brisvox_core::{Clock, SimClock};

    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn the_energy_model_is_a_threshold_on_the_rms_level() {
        let mut model = EnergyModel::from_pcm_rms(500.0);
        assert_eq!((model.window(), model.context()), (320, 0));
        assert_eq!(model.sample_rate(), 16_000);
        let level = |pcm: f32| vec![pcm / 32_768.0; 320];
        assert_eq!(model.infer(&level(499.0)).unwrap(), 0.0);
        assert_eq!(model.infer(&level(501.0)).unwrap(), 1.0);
        assert_eq!(
            model.infer(&level(-3_000.0)).unwrap(),
            1.0,
            "the sign does not matter"
        );
        assert_eq!(model.infer(&[]).unwrap(), 0.0);
        assert_eq!(
            EnergyModel::new(0.1).with_window(0).window(),
            1,
            "never an empty window"
        );
        assert_eq!(EnergyModel::new(0.1).with_window(512).window(), 512);
    }

    fn tee(capacity: usize) -> (VadTee, VadStats) {
        let stats = VadStats::new();
        (VadTee::new(capacity, 4, stats.counters()), stats)
    }

    fn origin() -> Instant {
        SimClock::new().now()
    }

    fn push(tee: &VadTee, n: u64, hole: bool) {
        let at = origin() + 20 * MS * u32::try_from(n).unwrap();
        tee.push_with(at, hole, |samples| {
            samples.extend_from_slice(&[i16::try_from(n).unwrap(); 4]);
        });
    }

    #[tokio::test(start_paused = true)]
    async fn a_take_with_room_moves_only_that_many_frames_and_the_rest_waits() {
        let (tee, stats) = tee(10);
        for n in 0..8 {
            push(&tee, n, false);
        }
        let mut batch = Batch::default();
        assert!(tee.take(&mut batch, false, 3));
        assert_eq!(batch.frames, 3);
        // A kept batch grows up to its limit and no further: 3 kept + room 2 = 5.
        let room = take_room(true, 5, batch.frames);
        assert!(tee.take(&mut batch, true, room));
        assert_eq!(batch.frames, 5);
        assert_eq!(
            batch.segments.len(),
            1,
            "contiguous frames extend the segment"
        );
        assert_eq!(batch.pcm.len(), 5 * 4);
        // The rest is still queued, in order, and nothing was dropped.
        assert!(tee.take(&mut batch, false, u64::MAX));
        assert_eq!(batch.frames, 3);
        assert_eq!(batch.pcm, [5, 5, 5, 5, 6, 6, 6, 6, 7, 7, 7, 7]);
        assert_eq!(stats.snapshot().tee_dropped, 0);
    }

    #[test]
    fn a_stale_job_cannot_take_the_payload_parked_for_a_later_attempt() {
        let slot: Slot<EnergyModel> = Arc::new(Mutex::new(None));
        let park = |attempt| {
            *lock_slot(&slot) = Some(Payload {
                attempt,
                stream: None,
                batch: Batch::default(),
            });
        };
        // Attempt 1 timed out unstarted; the worker took its payload back and parked attempt 2.
        park(2);
        assert!(
            take_attempt(&slot, 1).is_none(),
            "the abandoned job of attempt 1"
        );
        assert!(
            lock_slot(&slot).is_some(),
            "attempt 2's payload is untouched"
        );
        assert!(take_attempt(&slot, 2).is_some());
        assert!(take_attempt(&slot, 2).is_none(), "taken once");
    }

    #[test]
    fn the_room_of_a_kept_batch_is_what_is_left_under_the_limit() {
        assert_eq!(take_room(false, 50, 30), u64::MAX);
        assert_eq!(take_room(true, 50, 30), 20);
        assert_eq!(take_room(true, 50, 49), 1);
        // Never zero: the take must make progress.
        assert_eq!(take_room(true, 50, 50), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_tee_drops_the_oldest_frame_when_full_and_marks_the_hole() {
        let (tee, stats) = tee(3);
        for n in 0..5 {
            push(&tee, n, false);
        }
        assert_eq!(stats.snapshot().tee_dropped, 2);
        let mut batch = Batch::default();
        assert!(tee.take(&mut batch, false, u64::MAX));
        // Frames 2, 3, 4 survive; the hole before frame 2 starts the segment.
        assert_eq!(batch.frames, 3);
        assert_eq!(batch.pcm, [2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4]);
        assert_eq!(batch.segments.len(), 1);
        assert!(batch.segments[0].gap_before);
        assert_eq!(batch.segments[0].samples, 12);
        assert_eq!(batch.segments[0].start, origin() + 40 * MS);
        // The next batch follows the first without a hole.
        push(&tee, 5, false);
        assert!(tee.take(&mut batch, false, u64::MAX));
        assert!(!batch.segments[0].gap_before);
        assert!(!tee.take(&mut batch, false, u64::MAX), "nothing queued");
    }

    #[tokio::test(start_paused = true)]
    async fn a_flagged_frame_splits_a_batch_into_segments() {
        let (tee, _stats) = tee(8);
        push(&tee, 0, false);
        push(&tee, 1, false);
        push(&tee, 4, true);
        push(&tee, 5, false);
        let mut batch = Batch::default();
        assert!(tee.take(&mut batch, false, u64::MAX));
        let shape: Vec<(usize, bool)> = batch
            .segments
            .iter()
            .map(|s| (s.samples, s.gap_before))
            .collect();
        assert_eq!(shape, [(8, false), (8, true)]);
    }

    /// An overflow drop and a hole the first stage already flagged can meet in the same frame
    /// (the new head was flagged AND follows the dropped one). Both are the one hole before the
    /// head: the contiguous frame behind it must not inherit a second hole.
    #[tokio::test(start_paused = true)]
    async fn an_overflow_drop_and_an_upstream_hole_on_the_head_make_one_gap_not_two() {
        let (tee, stats) = tee(2);
        push(&tee, 0, false);
        push(&tee, 1, true); // frames lost before this one (first-stage drop)
        push(&tee, 2, false); // overflow: drops frame 0
        assert_eq!(stats.snapshot().tee_dropped, 1);
        let mut batch = Batch::default();
        assert!(tee.take(&mut batch, false, u64::MAX));
        let shape: Vec<(usize, bool)> = batch
            .segments
            .iter()
            .map(|s| (s.samples, s.gap_before))
            .collect();
        assert_eq!(
            shape,
            [(8, true)],
            "frames 1 and 2 are contiguous: one segment, not a split in the middle of speech"
        );
        // Nor does the overflow flag wait to land on the next batch.
        push(&tee, 3, false);
        assert!(tee.take(&mut batch, false, u64::MAX));
        assert!(!batch.segments[0].gap_before);
    }

    #[tokio::test(start_paused = true)]
    async fn the_tee_reuses_its_sample_buffers() {
        let (tee, _stats) = tee(2);
        let mut batch = Batch::default();
        for round in 0..20 {
            push(&tee, round, false);
            push(&tee, round + 100, false);
            assert!(tee.take(&mut batch, false, u64::MAX));
        }
        let state = tee.lock();
        assert_eq!(
            state.spare.len(),
            4,
            "capacity + 2 buffers circulate; none was lost or added"
        );
        assert!(state.spare.iter().all(|v| v.capacity() >= 4));
    }
}
