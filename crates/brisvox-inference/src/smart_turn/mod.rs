//! Smart Turn v3.2 end-of-turn detection.
//!
//! The pinned `smart-turn-v3.2-cpu.onnx` (int8, 8 679 182 bytes, SHA-256 `2bb02631...e4f`,
//! BSD-2-Clause with Whisper MIT attribution: `docs/providers/smart-turn.md`) is not
//! committed; `scripts/models/fetch_models.py` downloads it into `models/`. Contract:
//! `input_features` f32 `[1, 80, 800]` (the [`crate::mel`] features of the last 8 s of 16 kHz
//! audio, zero-padded at the start) -> `logits` f32 `[1, 1]`, already a sigmoid probability
//! that the user finished their turn (Pipecat: complete when > 0.5).
//!
//! Backends implement [`TurnModel`]; the production one is [`ort_backend`] (feature
//! `smart-turn-ort`, ONNX Runtime, what Pipecat runs). tract's optimizer moves this int8
//! graph's output far (up to 0.83 off: lossy u8 Softmax tables, amplified rewrites); without it tract keeps every verdict but is up to 0.18 off and ~4x
//! slower, so there is no pure-Rust backend yet.
//!
//! **Exactness.** This int8 model moves by up to 0.13 for feature changes of 1e-6 and by up to
//! 0.16 between ONNX Runtime 1.28 and 1.30 on identical input (its uint8 activation
//! quantization dominates: removing it changes outputs by up to 0.92). [`crate::mel`] therefore reproduces the reference
//! features bit for bit, and the goldens are made with the ONNX Runtime version `ort` bundles
//! (1.28.0).
//!
//! **Where the work runs.** Only at a candidate end of turn (a VAD stop), never per frame.
//! [`SmartTurn::push`] copies the user's PCM16 into a ring of the last 8 s (no allocation).
//! [`SmartTurn::analyze`] copies that ring (256 KB of i16) into a snapshot that moves
//! to the inference pool; the copy into a REUSED buffer takes ~2 us in release (measured by
//! `the_caller_side_copy_is_cheap`, which also prints the cold path: a fresh 256 KB buffer, the
//! first pause of a session, costs about as much again in page faults and `malloc`). The engine's
//! turn driver reuses one buffer per session through [`SmartTurn::snapshot_into`], so only the
//! first pause allocates on the async worker; everything else (i16 -> f32 window, features ~1.2 ms, model ~26-28
//! ms) runs on the pool thread, with scratch buffers that belong to that thread. The result
//! carries the pool's wait and run times.
//!
//! **Memory.** A session holds its ring (256 KB) from [`SmartTurn::new`]. The snapshot buffer
//! (another 256 KB) is allocated lazily, by the first analysis, and then reused; a session that
//! never pauses never pays for it. Each pool thread that
//! has run an analysis holds one scratch set (~3.4 MB: extractor buffers, window, features).

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::mel::{FEATURES, WINDOW_SAMPLES, WhisperLogMel};
use crate::pool::{JobTiming, PoolHandle};

#[cfg(feature = "smart-turn-ort")]
pub mod fuse;
#[cfg(feature = "smart-turn-ort")]
pub mod ort_backend;

/// Size of the pinned `smart-turn-v3.2-cpu.onnx`.
pub const MODEL_BYTES: u64 = 8_679_182;
/// File name of the pinned model under `models/`.
pub const MODEL_FILE: &str = "smart-turn-v3.2-cpu.onnx";
/// Pipecat's decision threshold: complete when the probability is above it.
pub const THRESHOLD: f32 = 0.5;

/// A loaded end-of-turn model, shared by all sessions.
pub trait TurnModel: Send + Sync {
    /// Probability that the turn is complete, from `[80, 800]` features ([`FEATURES`]
    /// values). CPU-heavy: call it on the inference pool.
    ///
    /// # Errors
    /// [`Error::Model`] on a wrong input length, a failed run, or a malformed output.
    fn predict(&self, features: &[f32]) -> Result<f32>;

    /// Prepares `sessions` concurrent analyses, so none of them pays a cold start (the ONNX
    /// Runtime backend creates one session per concurrent call, about 100 ms each). The engine
    /// calls it with the number of inference pool threads when a session starts. The default
    /// does nothing (models without per-call state).
    ///
    /// # Errors
    /// [`Error::Model`] if the runtime rejects the model.
    fn warm(&self, _sessions: usize) -> Result<()> {
        Ok(())
    }

    /// What the state [`Self::warm`] prepares belongs to, as a key the engine claims the
    /// warm-up under (once per pool for the whole process): unique per model instance whose
    /// warm state lives in the instance. `None` (the default): the engine keys the claim on the
    /// instance's address, which a later model allocated at the same address would share.
    fn warm_key(&self) -> Option<String> {
        None
    }
}

/// Checks a model output: one finite value in 0..=1.
#[cfg_attr(not(feature = "smart-turn-ort"), allow(dead_code))] // used by the backends
pub(crate) fn check_probability(output: &[f32]) -> Result<f32> {
    match output {
        [p] if (0.0..=1.0).contains(p) => Ok(*p),
        [p] => Err(Error::Model(format!(
            "smart turn output {p} is outside 0..=1"
        ))),
        _ => Err(Error::Model(format!(
            "smart turn output has {} values, expected 1",
            output.len()
        ))),
    }
}

/// Checks the input length every backend expects.
#[cfg_attr(not(feature = "smart-turn-ort"), allow(dead_code))] // used by the backends
pub(crate) fn check_features(features: &[f32]) -> Result<()> {
    if features.len() == FEATURES {
        Ok(())
    } else {
        Err(Error::Model(format!(
            "{} feature values, expected {FEATURES}",
            features.len()
        )))
    }
}

/// Checks a model file's size before a backend loads it.
#[cfg_attr(not(feature = "smart-turn-ort"), allow(dead_code))] // used by the backends
pub(crate) fn check_size(size: u64) -> Result<()> {
    if size == MODEL_BYTES {
        Ok(())
    } else {
        Err(Error::Model(format!(
            "{size} bytes, expected the pinned {MODEL_FILE} ({MODEL_BYTES} bytes, \
             scripts/models/fetch_models.py)"
        )))
    }
}

/// An end-of-turn verdict.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Verdict {
    /// Probability that the user finished their turn.
    pub probability: f32,
    /// `probability > THRESHOLD`.
    pub complete: bool,
    /// Pool queue wait and run time (window, features and model).
    pub timing: JobTiming,
}

/// Per-thread buffers of an analysis (built on first use on that thread).
struct Scratch {
    mel: WhisperLogMel,
    window: Vec<f32>,
    features: Vec<f32>,
}

thread_local! {
    static SCRATCH: RefCell<Option<Scratch>> = const { RefCell::new(None) };
}

/// Builds the model input from `audio` (oldest first, at most 8 s) and runs the model, with
/// this thread's scratch.
fn run_on_this_thread(audio: &[i16], model: &dyn TurnModel) -> Result<f32> {
    SCRATCH.with(|cell| {
        let mut slot = cell
            .try_borrow_mut()
            .map_err(|_| Error::Model("smart turn scratch is busy (reentrant call)".into()))?;
        let scratch = slot.get_or_insert_with(|| Scratch {
            mel: WhisperLogMel::new(),
            window: vec![0.0; WINDOW_SAMPLES],
            features: vec![0.0; FEATURES],
        });
        // Pipecat pads short audio with zeros at the start.
        let pad = WINDOW_SAMPLES.saturating_sub(audio.len());
        let (zeros, samples) = scratch.window.split_at_mut(pad);
        zeros.fill(0.0);
        for (dst, &src) in samples.iter_mut().zip(audio) {
            *dst = f32::from(src) / 32_768.0;
        }
        scratch
            .mel
            .compute(&scratch.window, &mut scratch.features)?;
        model.predict(&scratch.features)
    })
}

/// One session's end-of-turn analyzer: the last 8 s of the user's 16 kHz audio.
pub struct SmartTurn {
    model: Arc<dyn TurnModel>,
    /// Ring buffer of the most recent PCM16 samples.
    history: Vec<i16>,
    /// Next write position in `history`.
    write: usize,
    /// Valid samples in `history` (up to its length).
    filled: usize,
    /// Buffer the ring is copied into for a pool job; `None` before the first [`Self::analyze`],
    /// while a job holds it, or after a failed job dropped it (re-allocated, not zeroed, on the
    /// next call). Lazy: a caller that takes its own snapshots (the engine's turn driver) never
    /// allocates it.
    snapshot: Option<Vec<i16>>,
}

impl std::fmt::Debug for SmartTurn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmartTurn")
            .field("filled", &self.filled)
            .finish_non_exhaustive()
    }
}

impl SmartTurn {
    /// A new analyzer over the shared model (allocates 256 KB: the ring. The snapshot buffer of
    /// [`Self::analyze`] is allocated by its first call).
    #[must_use]
    pub fn new(model: Arc<dyn TurnModel>) -> Self {
        Self {
            model,
            history: vec![0; WINDOW_SAMPLES],
            write: 0,
            filled: 0,
            snapshot: None,
        }
    }

    /// Appends 16 kHz mono PCM16 of the user; only the last 8 s are kept. Cheap (a copy).
    pub fn push(&mut self, pcm: &[i16]) {
        // Only the last 8 s of a very long push can survive.
        let pcm = pcm
            .get(pcm.len().saturating_sub(WINDOW_SAMPLES)..)
            .unwrap_or(pcm);
        let mut rest = pcm;
        while !rest.is_empty() {
            let Some(room) = self.history.get_mut(self.write..) else {
                break;
            };
            let n = room.len().min(rest.len());
            let (now, later) = rest.split_at(n);
            if let Some(dst) = room.get_mut(..n) {
                dst.copy_from_slice(now);
            }
            rest = later;
            self.write = (self.write + n) % WINDOW_SAMPLES;
        }
        self.filled = self.filled.saturating_add(pcm.len()).min(WINDOW_SAMPLES);
    }

    /// Replaces audio already in the ring with `pcm`: the `pcm.len()` samples that end `back`
    /// samples before the newest one (`back == 0`: the newest samples). For a stage that
    /// improves audio after it was pushed (noise suppression runs on the pool and finishes a
    /// few milliseconds after the plain samples arrived). Samples older than what the ring
    /// holds are skipped; nothing is added, so the held duration does not change.
    pub fn overwrite(&mut self, back: usize, pcm: &[i16]) {
        let len = pcm.len();
        for (j, sample) in pcm.iter().enumerate() {
            // Age of this sample: 0 is the newest one in the ring.
            let age = back.saturating_add(len - 1 - j);
            if age >= self.filled {
                continue; // older than the ring holds
            }
            let index = (self.write + WINDOW_SAMPLES - 1 - age) % WINDOW_SAMPLES;
            if let Some(slot) = self.history.get_mut(index) {
                *slot = *sample;
            }
        }
    }

    /// Audio held (at most 8 s).
    #[must_use]
    pub fn buffered(&self) -> Duration {
        Duration::from_micros(u64::try_from(self.filled).unwrap_or(0) * 1_000_000 / 16_000)
    }

    /// Forgets the audio (call when a turn has been decided, like Pipecat does).
    pub fn clear(&mut self) {
        self.write = 0;
        self.filled = 0;
    }

    /// The held audio, oldest first, copied into `snapshot` (cleared first; its capacity is
    /// reused, so a caller that keeps one buffer allocates only when the audio outgrows it).
    /// Like [`Self::snapshot`] but without the allocation: the engine's turn driver recycles
    /// one buffer per session.
    pub fn snapshot_into(&self, snapshot: &mut Vec<i16>) {
        snapshot.clear();
        snapshot.reserve(self.filled);
        // The oldest sample sits at `write - filled` (mod the ring length).
        let oldest = (self.write + WINDOW_SAMPLES - self.filled) % WINDOW_SAMPLES;
        let (newer, older) = self.history.split_at(oldest);
        let older = older.get(..self.filled.min(older.len())).unwrap_or(older);
        snapshot.extend_from_slice(older);
        let missing = self.filled.saturating_sub(older.len());
        snapshot.extend_from_slice(newer.get(..missing).unwrap_or(newer));
    }

    /// Runs Smart Turn on the held audio on the pool (call at a candidate end of turn). Only
    /// the ring copy runs on the calling thread. Dropping the future abandons the job (see
    /// [`PoolHandle::run`]).
    ///
    /// # Errors
    /// The pool's errors ([`Error::PoolSaturated`], timeout, closed, panic) and
    /// [`Error::Model`].
    pub async fn analyze(&mut self, pool: &PoolHandle, timeout: Duration) -> Result<Verdict> {
        let mut snapshot = self
            .snapshot
            .take()
            .unwrap_or_else(|| Vec::with_capacity(WINDOW_SAMPLES));
        self.snapshot_into(&mut snapshot);
        let model = Arc::clone(&self.model);
        let done = pool
            .run(timeout, move || {
                let probability = run_on_this_thread(&snapshot, model.as_ref());
                (snapshot, probability)
            })
            .await?;
        let (snapshot, probability) = done.value;
        self.snapshot = Some(snapshot);
        let probability = probability?;
        Ok(Verdict {
            probability,
            complete: probability > THRESHOLD,
            timing: done.timing,
        })
    }

    /// An owned copy of the held audio, oldest first (at most 8 s).
    ///
    /// Unlike [`Self::analyze`], which borrows the analyzer for the whole job, this lets the
    /// ring keep filling while an analysis runs: take the snapshot at the candidate end of turn,
    /// hand it to [`Self::analyze_snapshot`] (or to [`Self::analyze_snapshot_blocking`] inside a
    /// job of the caller's own pool wrapper) and go on pushing audio. The copy allocates
    /// `buffered()` worth of samples (up to 256 KB); it happens once per pause, not per frame. A
    /// caller that takes one per pause should recycle a buffer with [`Self::snapshot_into`].
    #[must_use]
    pub fn snapshot(&self) -> Vec<i16> {
        let mut out = Vec::new();
        self.snapshot_into(&mut out);
        out
    }

    /// The model this analyzer runs, for [`Self::analyze_snapshot`] and
    /// [`Self::analyze_snapshot_blocking`] (shared: cloning it is a reference count).
    #[must_use]
    pub fn model(&self) -> Arc<dyn TurnModel> {
        Arc::clone(&self.model)
    }

    /// Runs Smart Turn on a [`Self::snapshot`] on the pool. An associated function (no `&self`):
    /// the analyzer stays free to receive audio while this runs. Dropping the future abandons
    /// the job (see [`PoolHandle::run`]).
    ///
    /// # Errors
    /// The pool's errors ([`Error::PoolSaturated`], timeout, closed, panic) and
    /// [`Error::Model`].
    pub async fn analyze_snapshot(
        pool: &PoolHandle,
        model: Arc<dyn TurnModel>,
        samples: Vec<i16>,
        timeout: Duration,
    ) -> Result<Verdict> {
        let done = pool
            .run(timeout, move || {
                Self::analyze_snapshot_blocking(model.as_ref(), &samples)
            })
            .await?;
        let probability = done.value?;
        Ok(Verdict {
            probability,
            complete: probability > THRESHOLD,
            timing: done.timing,
        })
    }

    /// The probability that the turn is complete for `samples` (a [`Self::snapshot`]: at most
    /// 8 s of 16 kHz audio, shorter audio is zero-padded at the start), computed on the calling
    /// thread with that thread's scratch. CPU-heavy: for pool jobs, tests and offline tools,
    /// never a tokio worker. A caller with its own pool wrapper (the engine's `Offload`) calls
    /// this inside its job.
    ///
    /// # Errors
    /// [`Error::Model`].
    pub fn analyze_snapshot_blocking(model: &dyn TurnModel, samples: &[i16]) -> Result<f32> {
        run_on_this_thread(samples, model)
    }

    /// The same as [`Self::analyze`], inline on the calling thread (tests and offline tools;
    /// never on a tokio worker).
    ///
    /// # Errors
    /// [`Error::Model`].
    pub fn analyze_blocking(&mut self) -> Result<f32> {
        let mut snapshot = self
            .snapshot
            .take()
            .unwrap_or_else(|| Vec::with_capacity(WINDOW_SAMPLES));
        self.snapshot_into(&mut snapshot);
        let probability = run_on_this_thread(&snapshot, self.model.as_ref());
        self.snapshot = Some(snapshot);
        probability
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_probability_accepts_one_value_in_range() {
        assert_eq!(check_probability(&[0.0]).ok(), Some(0.0));
        assert_eq!(check_probability(&[0.7]).ok(), Some(0.7));
        assert_eq!(check_probability(&[1.0]).ok(), Some(1.0));
        assert!(check_probability(&[]).is_err());
        assert!(check_probability(&[0.1, 0.2]).is_err());
        assert!(check_probability(&[f32::NAN]).is_err());
        assert!(check_probability(&[1.01]).is_err());
        assert!(check_probability(&[-0.01]).is_err());
    }

    struct Fixed;

    impl TurnModel for Fixed {
        /// A fixed probability for any well-formed input.
        fn predict(&self, features: &[f32]) -> Result<f32> {
            check_features(features)?;
            Ok(0.75)
        }
    }

    fn snapshot(turn: &SmartTurn) -> Vec<i16> {
        let mut out = Vec::new();
        turn.snapshot_into(&mut out);
        out
    }

    #[test]
    fn overwrite_replaces_samples_counted_back_from_the_newest() {
        let mut turn = SmartTurn::new(Arc::new(Fixed));
        turn.push(&[1, 2, 3, 4, 5, 6]);
        turn.overwrite(0, &[50, 60]);
        assert_eq!(snapshot(&turn), [1, 2, 3, 4, 50, 60]);
        turn.overwrite(2, &[30, 40]);
        assert_eq!(snapshot(&turn), [1, 2, 30, 40, 50, 60]);
        // Older than the ring holds (or beyond its start): skipped, never added.
        turn.overwrite(5, &[9, 9]);
        assert_eq!(snapshot(&turn), [9, 2, 30, 40, 50, 60]);
        turn.overwrite(100, &[7]);
        assert_eq!(snapshot(&turn), [9, 2, 30, 40, 50, 60]);
    }

    #[test]
    fn overwrite_follows_the_ring_across_its_wrap_point() {
        let mut turn = SmartTurn::new(Arc::new(Fixed));
        let long: Vec<i16> = (0..WINDOW_SAMPLES + 5).map(|i| (i % 100) as i16).collect();
        turn.push(&long);
        turn.overwrite(0, &[-1, -2, -3]);
        let snap = snapshot(&turn);
        assert_eq!(snap.len(), WINDOW_SAMPLES);
        assert_eq!(snap[WINDOW_SAMPLES - 3..], [-1, -2, -3]);
        assert_eq!(snap[WINDOW_SAMPLES - 4], long[WINDOW_SAMPLES + 5 - 4]);
    }

    #[test]
    fn the_ring_hands_out_the_last_8_seconds_oldest_first() {
        let mut turn = SmartTurn::new(Arc::new(Fixed));
        assert!(snapshot(&turn).is_empty(), "empty history");

        turn.push(&[16_384, -16_384]);
        assert_eq!(snapshot(&turn), vec![16_384, -16_384]);

        // Wrap the ring: 8 s + 3 samples of a ramp in uneven pieces.
        turn.clear();
        let ramp: Vec<i16> = (0..WINDOW_SAMPLES + 3)
            .map(|i| i16::try_from(i % 30_000).unwrap_or(0))
            .collect();
        for piece in ramp.chunks(317) {
            turn.push(piece);
        }
        assert_eq!(turn.buffered(), Duration::from_secs(8));
        assert_eq!(Some(&snapshot(&turn)[..]), ramp.get(3..));

        // One push longer than the ring keeps its last 8 s.
        let mut long = SmartTurn::new(Arc::new(Fixed));
        long.push(&ramp);
        assert_eq!(Some(&snapshot(&long)[..]), ramp.get(3..));

        assert_eq!(turn.analyze_blocking().ok(), Some(0.75));
        turn.clear();
        assert_eq!(turn.buffered(), Duration::ZERO);
        assert!(snapshot(&turn).is_empty());
    }

    #[test]
    fn a_snapshot_is_an_owned_copy_and_the_ring_keeps_filling() {
        let mut turn = SmartTurn::new(Arc::new(Fixed));
        assert!(turn.snapshot().is_empty());
        turn.push(&[1, 2, 3]);
        let first = turn.snapshot();
        assert_eq!(first, vec![1, 2, 3]);
        // Audio pushed after the snapshot does not change it, and shows in the next one.
        turn.push(&[4, 5]);
        assert_eq!(first, vec![1, 2, 3]);
        assert_eq!(turn.snapshot(), vec![1, 2, 3, 4, 5]);
        // A snapshot of a wrapped ring is oldest first, like the internal copy.
        let ramp: Vec<i16> = (0..WINDOW_SAMPLES + 5)
            .map(|i| i16::try_from(i % 30_000).unwrap_or(0))
            .collect();
        turn.clear();
        turn.push(&ramp);
        assert_eq!(Some(&turn.snapshot()[..]), ramp.get(5..));
        assert!(Arc::strong_count(&turn.model()) >= 2);
    }

    #[test]
    fn new_allocates_only_the_ring_and_snapshot_into_reuses_its_buffer() {
        let mut turn = SmartTurn::new(Arc::new(Fixed));
        assert!(turn.snapshot.is_none(), "the pool-job buffer is lazy");
        turn.push(&[1, 2, 3, 4]);
        let mut buffer = Vec::new();
        turn.snapshot_into(&mut buffer);
        assert_eq!(buffer, vec![1, 2, 3, 4]);
        // A longer clip then a shorter one: the capacity is kept and the content replaced.
        let long = vec![5; 10_000];
        turn.clear();
        turn.push(&long);
        turn.snapshot_into(&mut buffer);
        let capacity = buffer.capacity();
        assert_eq!(buffer, long);
        turn.clear();
        turn.push(&[9, 8]);
        turn.snapshot_into(&mut buffer);
        assert_eq!(buffer, vec![9, 8]);
        assert_eq!(buffer.capacity(), capacity, "no reallocation");
        // `analyze_blocking` still works and creates the buffer on first use.
        assert_eq!(turn.analyze_blocking().ok(), Some(0.75));
        assert!(turn.snapshot.is_some());
    }

    #[test]
    fn analyze_snapshot_matches_analyze_and_frees_the_analyzer() {
        use crate::pool::{InferencePool, PoolConfig};
        use brisvox_core::SystemClock;
        let mut turn = SmartTurn::new(Arc::new(Fixed));
        turn.push(&[100; 1_600]);
        let snapshot = turn.snapshot();
        assert_eq!(
            SmartTurn::analyze_snapshot_blocking(turn.model().as_ref(), &snapshot).ok(),
            turn.analyze_blocking().ok()
        );
        let pool = InferencePool::new(
            &PoolConfig {
                threads: 1,
                queue_capacity: 4,
                name: "smart-turn-snapshot-test".to_owned(),
            },
            SystemClock::shared(),
        )
        .unwrap();
        let handle = pool.handle();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let model = turn.model();
        // The analyzer takes audio while the job is pending.
        let verdict = runtime.block_on(async {
            let job = SmartTurn::analyze_snapshot(&handle, model, snapshot, Duration::from_secs(5));
            turn.push(&[7; 320]);
            job.await
        });
        let verdict = verdict.unwrap();
        assert!((verdict.probability - 0.75).abs() < f32::EPSILON);
        assert!(verdict.complete);
        assert_eq!(
            turn.buffered(),
            Duration::from_micros(1_920 * 1_000_000 / 16_000)
        );
        drop(handle);
        pool.shutdown();
    }

    /// The only part of `analyze` that runs on the caller (a tokio worker): copying the full
    /// ring. Run with --release for the number in the module docs.
    #[test]
    fn the_caller_side_copy_is_cheap() {
        use brisvox_core::{Clock, SystemClock};
        let mut turn = SmartTurn::new(Arc::new(Fixed));
        turn.push(&vec![7; WINDOW_SAMPLES + 1_000]);
        let mut out = Vec::with_capacity(WINDOW_SAMPLES);
        let clock = SystemClock;
        let mut runs: Vec<Duration> = (0..21)
            .map(|_| {
                let start = clock.now();
                turn.snapshot_into(&mut out);
                clock.now().saturating_duration_since(start)
            })
            .collect();
        runs.sort();
        let median = runs.get(10).copied().unwrap_or_default();
        println!("ring copy (8 s, reused buffer): median {median:?}");
        assert_eq!(out.len(), WINDOW_SAMPLES);
        assert!(median < Duration::from_millis(2), "{median:?}");
        // The cold path: a fresh buffer, as the first pause of a session (and `snapshot()`).
        let mut cold: Vec<Duration> = (0..21)
            .map(|_| {
                let start = clock.now();
                let fresh = turn.snapshot();
                let took = clock.now().saturating_duration_since(start);
                assert_eq!(fresh.len(), WINDOW_SAMPLES);
                took
            })
            .collect();
        cold.sort();
        let cold_median = cold.get(10).copied().unwrap_or_default();
        println!("ring copy (8 s, fresh allocation): median {cold_median:?}");
        assert!(cold_median < Duration::from_millis(2), "{cold_median:?}");
    }

    #[test]
    fn the_window_is_zero_padded_at_the_start() {
        struct Probe;
        impl TurnModel for Probe {
            fn predict(&self, _features: &[f32]) -> Result<f32> {
                Ok(0.5)
            }
        }
        run_on_this_thread(&[16_384, -32_768], &Probe).ok();
        SCRATCH.with(|cell| {
            let slot = cell.borrow();
            let window = slot.as_ref().map(|s| s.window.clone()).unwrap_or_default();
            assert_eq!(window.get(WINDOW_SAMPLES - 2..), Some(&[0.5, -1.0][..]));
            assert!(
                window
                    .get(..WINDOW_SAMPLES - 2)
                    .is_some_and(|z| z.iter().all(|&v| v == 0.0))
            );
        });
    }
}
