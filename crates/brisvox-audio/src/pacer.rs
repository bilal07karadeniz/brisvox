//! Real-time output pacing.
//!
//! [`OutputPacer`] sits between the TTS stage and the output transport. It releases every audio
//! frame downstream one frame duration after the previous one, on the injected [`Clock`](brisvox_core::Clock), so the
//! transport sees playout timing and the synthesis side can run ahead without flooding it.
//!
//! Scheduling rules:
//! - **Absolute schedule.** The release deadline of a frame is the previous deadline plus the
//!   previous frame's own duration (`next += duration`), never "now plus a period". Sleep
//!   overshoot therefore does not accumulate: frames stay on one grid for the whole turn.
//! - **Bounded lag.** A frame that finds its deadline more than `max_lag` in the past means the
//!   audio source starved the output. The pacer resyncs to the clock (the frame is released at
//!   once and the grid restarts from there) and counts an underrun. A smaller lag is caught up
//!   by releasing the late frames immediately, which keeps the grid.
//! - **Due deadlines never touch the timer.** A deadline already reached is released without
//!   calling the clock's timer: tokio rounds every sleep up to its next timer tick (~15.6 ms on
//!   Windows), so sleeping on a due deadline would delay it by a tick.
//! - **One timer per session.** The pacer owns one pinned [`Sleep`], created on first use and
//!   re-armed with `reset` for every later wait.
//! - **Interrupts flush at once.** A wait is abandoned as soon as an interrupt for a newer turn
//!   is queued for the pacer (its [`EpochWatch`](brisvox_core::EpochWatch)); a frame doomed that way is neither released
//!   nor counted. A push that is already blocked on a full downstream inbox is abandoned the same
//!   way (the frame is dropped by the framework, detected through `stale_dropped`), and such a
//!   frame is not counted either. An `Interrupt` resets the schedule, so the next turn starts at
//!   once.
//! - **Turn ends are not underruns.** A `TtsEnded` is held until the turn's last frame has played
//!   out, then passed on, and the schedule is reset: the silence before the next turn is not
//!   starvation, so its first frame starts at once and counts no underrun.
//! - **First frame at once.** The pacing clock starts at the first audio frame after an idle
//!   period (session start, an interrupt, a finished turn): that frame leaves the instant it
//!   arrives and the grid runs from there. A voice agent's first answer is latency, not a stream
//!   to smooth (the former per-session phase cost it 0-19 ms, 9.5 ms on average).
//! - **Optional per-session phase** ([`OutputPacer::with_first_frame_phase`], off by default and in
//!   production). The first frame of a session is then released [`phase_offset`] after the clock
//!   reads its arrival, a whole number of milliseconds in `[0, 20 ms)` derived from the session
//!   seed, so co-started sessions wake in different milliseconds instead of all at once. It
//!   applies to the session's first frame only; later turns start at once. It is the original
//!   behaviour, kept for load experiments that start many sessions in the same millisecond.
//! - **A frame counts as played when it is released.** `frames_played`, and a played position
//!   derived from the frames the pacer has handed downstream, advance at the release instant, not
//!   when the frame's audio has left the transport. The reported position can therefore run up to
//!   one frame (20 ms) ahead of the wire. A frame doomed by an interrupt before its release is
//!   never counted.
//!
//! Transport-side buffers are not drained here: after an interrupt, the transport clears what it
//! already holds. Only the frame already handed downstream can still play.
//!
//! This is the production counterpart of the reference pacer in `brisvox-sim` (which is the
//! behavioural spec, including its regression tests).

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use brisvox_core::clock::deadline_after;
use brisvox_core::frame::{ControlFrame, ControlKind, DataFrame, Frame, SystemFrame};
use brisvox_core::{Direction, Processor, ProcessorContext, Result, SharedClock, TurnEpoch};
use tokio::time::{Instant, Sleep};

/// Nominal frame duration of the audio path, in milliseconds.
const FRAME_MS: u64 = 20;

/// Nominal frame duration of the audio path (20 ms). Frames of other durations are paced by
/// their own duration; this constant only bounds the session phase.
pub const FRAME: Duration = Duration::from_millis(FRAME_MS);

/// Default lag beyond which the pacer resyncs instead of catching up.
pub const DEFAULT_MAX_LAG: Duration = Duration::from_millis(60);

/// Advances a splitmix64 state and returns the next output (Steele, Lea and Flood, 2014).
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The phase offset of a session with `seed`: a whole number of milliseconds in `[0, 20 ms)`.
///
/// Pure and deterministic: the same seed always gives the same offset. Whole milliseconds match
/// the clock's timer granularity, so offsets of different sessions land in different
/// milliseconds (1 in 20 pairs still collides, which is harmless).
pub fn phase_offset(seed: u64) -> Duration {
    let mut state = seed;
    Duration::from_millis(splitmix64(&mut state) % FRAME_MS)
}

/// Pacing metrics of one session, as read by [`PacerMetrics::snapshot`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PacerStats {
    /// Audio frames released downstream.
    pub frames_played: u64,
    /// Times the pacer fell behind by more than its `max_lag` and resynced to the clock.
    pub underruns: u64,
    /// The largest lag seen among delivered frames: how far past its grid point a frame was when
    /// it was handed downstream. Includes timer overshoot and time blocked in the hand-off, but
    /// not the underrun decision, which uses the lag at arrival.
    pub worst_lag: Duration,
}

#[derive(Debug, Default)]
struct Counters {
    frames_played: AtomicU64,
    underruns: AtomicU64,
    worst_lag_nanos: AtomicU64,
}

/// A handle that reads an [`OutputPacer`]'s metrics. Clone it before the pacer moves into its
/// task; reads are lock-free.
#[derive(Debug, Clone, Default)]
pub struct PacerMetrics {
    counters: Arc<Counters>,
}

impl PacerMetrics {
    /// The current counters.
    pub fn snapshot(&self) -> PacerStats {
        PacerStats {
            frames_played: self.counters.frames_played.load(Ordering::Relaxed),
            underruns: self.counters.underruns.load(Ordering::Relaxed),
            worst_lag: Duration::from_nanos(self.counters.worst_lag_nanos.load(Ordering::Relaxed)),
        }
    }

    fn count_frame(&self) {
        self.counters.frames_played.fetch_add(1, Ordering::Relaxed);
    }

    fn count_underrun(&self) {
        self.counters.underruns.fetch_add(1, Ordering::Relaxed);
    }

    fn record_lag(&self, lag: Duration) {
        let nanos = u64::try_from(lag.as_nanos()).unwrap_or(u64::MAX);
        self.counters
            .worst_lag_nanos
            .fetch_max(nanos, Ordering::Relaxed);
    }
}

/// Releases audio frames downstream at real time. See the [module docs](self) for the rules.
///
/// Create one per session and direction of audio output, and add it to the pipeline in front of
/// the transport. Its clock must be the session's clock.
pub struct OutputPacer {
    clock: SharedClock,
    phase: Duration,
    max_lag: Duration,
    /// Release deadline of the next frame; `None` when the next frame starts at once.
    next_play: Option<Instant>,
    /// Whether the session's first frame has been released (the phase applies only before).
    started: bool,
    /// Whether the session's first frame waits for `phase` (off by default).
    first_frame_phase: bool,
    /// The pacer's one timer, created on first use and re-armed with `reset` for every wait.
    sleep: Option<Pin<Box<Sleep>>>,
    metrics: PacerMetrics,
}

impl OutputPacer {
    /// A pacer on `clock` for one session. `seed` fixes the session's phase offset
    /// ([`phase_offset`]), which only matters with [`with_first_frame_phase`]; use a different
    /// seed per session.
    ///
    /// [`with_first_frame_phase`]: Self::with_first_frame_phase
    pub fn new(clock: SharedClock, seed: u64) -> Self {
        Self {
            clock,
            phase: phase_offset(seed),
            max_lag: DEFAULT_MAX_LAG,
            next_play: None,
            started: false,
            first_frame_phase: false,
            sleep: None,
            metrics: PacerMetrics::default(),
        }
    }

    /// Sets the lag beyond which the pacer resyncs instead of catching up (default
    /// [`DEFAULT_MAX_LAG`]).
    #[must_use]
    pub fn with_max_lag(mut self, max_lag: Duration) -> Self {
        self.max_lag = max_lag;
        self
    }

    /// Delays the session's first frame by the session's phase ([`phase_offset`]) so that
    /// co-started sessions wake in different milliseconds. Off by default: the first frame of the
    /// first answer then costs the phase (0-19 ms) in voice-to-voice latency.
    #[must_use]
    pub fn with_first_frame_phase(mut self, on: bool) -> Self {
        self.first_frame_phase = on;
        self
    }

    /// A handle that reads this pacer's metrics after it has moved into its task.
    pub fn metrics(&self) -> PacerMetrics {
        self.metrics.clone()
    }

    /// The session's phase offset (see [`phase_offset`]).
    pub const fn phase(&self) -> Duration {
        self.phase
    }

    /// The lag beyond which the pacer resyncs.
    pub const fn max_lag(&self) -> Duration {
        self.max_lag
    }

    /// Waits until `at`, unless an interrupt for a newer epoch than `epoch` is queued for this
    /// processor. Returns `false` if the wait was abandoned (the frame is stale now) and `true`
    /// once `at` has come. A deadline that is already due returns at once and never touches the
    /// timer.
    async fn wait(&mut self, at: Instant, epoch: TurnEpoch, ctx: &ProcessorContext) -> bool {
        let watch = ctx.epoch_watch();
        if at <= self.clock.now() {
            // Same verdict the select below would give, without the timer.
            return !watch.is_some_and(|watch| epoch.is_stale(watch.latest()));
        }
        // Take the timer out of `self` for the wait and put it back afterwards: the box is moved,
        // never reallocated, so a session allocates its timer once.
        let mut sleep = match self.sleep.take() {
            Some(mut sleep) => {
                sleep.as_mut().reset(at);
                sleep
            }
            None => Box::pin(self.clock.sleep_until(at)),
        };
        let released = match watch {
            Some(watch) => tokio::select! {
                biased;
                () = watch.wait_beyond(epoch) => false,
                () = sleep.as_mut() => true,
            },
            None => {
                sleep.as_mut().await;
                true
            }
        };
        self.sleep = Some(sleep);
        released
    }

    /// Releases one audio frame at its deadline, or drops it if an interrupt made it stale.
    async fn release_audio(
        &mut self,
        frame: Frame,
        duration: Duration,
        epoch: TurnEpoch,
        ctx: &mut ProcessorContext,
    ) -> Result<()> {
        let now = self.clock.now();
        // `due` is the grid point the frame should be released at; `at` is the instant the pacer
        // waits for. They differ only when a resync restarts the grid at `now`.
        let (at, due, resync) = match self.next_play {
            // Session start, or the first frame after an interrupt or a finished turn: start at
            // once (plus the phase on the session's first frame only, when it is enabled).
            None => {
                let offset = if self.started || !self.first_frame_phase {
                    Duration::ZERO
                } else {
                    self.phase
                };
                let at = deadline_after(now, offset);
                (at, at, false)
            }
            // On schedule: wait for the grid.
            Some(next) if next > now => (next, next, false),
            // Late: catch up within `max_lag`, otherwise resync to now and count an underrun.
            // The underrun decision uses the lag at arrival (the source-starvation signal).
            Some(next) => {
                let lag = now.saturating_duration_since(next);
                if lag > self.max_lag {
                    (now, next, true)
                } else {
                    (next, next, false)
                }
            }
        };
        if !self.wait(at, epoch, ctx).await {
            // Doomed by a queued interrupt: neither played nor counted.
            return Ok(());
        }
        // Hand the frame on first. The push can block on a full downstream inbox, and an
        // interrupt can abandon that wait: the framework then drops the frame and counts it in
        // `stale_dropped`. Such a frame was not played, so it is neither counted nor scheduled.
        let dropped_before = ctx.stale_dropped();
        ctx.forward(frame, Direction::Downstream).await?;
        if ctx.stale_dropped() != dropped_before {
            return Ok(());
        }
        // How late the frame was actually handed on, against its grid point. Measured after the
        // hand-off, not at dequeue: the timer's overshoot (tokio wakes on its tick) and a
        // push blocked on a full downstream inbox both make the frame late, and both are counted.
        self.metrics
            .record_lag(self.clock.now().saturating_duration_since(due));
        if resync {
            self.metrics.count_underrun();
        }
        self.metrics.count_frame();
        self.started = true;
        self.next_play = Some(deadline_after(at, duration));
        Ok(())
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
    ) -> Result<()> {
        match (&frame, dir) {
            (Frame::Data(DataFrame::Audio(audio)), Direction::Downstream) => {
                let (duration, epoch) = (audio.duration(), audio.epoch());
                self.release_audio(frame, duration, epoch, ctx).await
            }
            // The turn's end goes out once its last frame has played out (the reference pacer
            // does the same), and the schedule then restarts: the next turn is not late.
            (
                Frame::Control(ControlFrame {
                    epoch,
                    kind: ControlKind::TtsEnded,
                }),
                Direction::Downstream,
            ) => {
                let epoch = *epoch;
                if let Some(next) = self.next_play
                    && !self.wait(next, epoch, ctx).await
                {
                    return Ok(()); // doomed by a queued interrupt
                }
                self.next_play = None;
                ctx.forward(frame, dir).await
            }
            // An interrupt flushes the schedule: the next turn starts at once.
            (Frame::System(SystemFrame::Interrupt { .. }), _) => {
                self.next_play = None;
                ctx.forward(frame, dir).await
            }
            _ => ctx.forward(frame, dir).await,
        }
    }
}
