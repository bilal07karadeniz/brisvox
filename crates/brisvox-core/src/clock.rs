//! The injected time source.
//!
//! Engine logic never reads time directly (`Instant::now`, `SystemTime::now` and tokio sleeps are
//! `disallowed-methods` in `clippy.toml`); it asks a [`Clock`]. Production uses [`SystemClock`];
//! simulations use [`SimClock`] on a tokio runtime with paused time, where a 10-minute session
//! runs in milliseconds and every run produces the same timeline.
//!
//! Both clocks are built on tokio's timer, so [`Clock::sleep_until`] returns a concrete
//! [`Sleep`] (no boxing, no per-call allocation) and timeouts/intervals from `tokio::time` stay
//! consistent with `now()`. Timer granularity is 1 ms (also under paused time): deadlines are
//! rounded up to the next millisecond.
//!
//! Periodic loops (output pacing) should keep an absolute schedule and reuse one pinned `Sleep`
//! rather than calling `sleep(period)` each tick, which drifts:
//!
//! ```ignore
//! let sleep = clock.sleep_until(next);
//! tokio::pin!(sleep);
//! loop {
//!     sleep.as_mut().await;
//!     // ... send one frame ...
//!     next += FRAME;
//!     sleep.as_mut().reset(next);
//! }
//! ```

use std::fmt::Debug;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::{Instant, Sleep};

/// A clock shared by every task of a session.
pub type SharedClock = Arc<dyn Clock>;

/// Roughly 30 years: the deadline used when `now + duration` overflows ("sleep forever").
const FAR_FUTURE: Duration = Duration::from_secs(86_400 * 365 * 30);

/// Source of monotonic time, wall-clock time and timers.
pub trait Clock: Debug + Send + Sync + 'static {
    /// Monotonic now. Use for every latency measurement and deadline.
    fn now(&self) -> Instant;

    /// Wall-clock time. Use only for human-facing timestamps (logs, records), never for
    /// measuring durations.
    fn wall(&self) -> SystemTime;

    /// A future that completes at `deadline` (immediately if it is in the past).
    ///
    /// # Panics
    /// The tokio implementations panic if called outside a tokio runtime with the time driver
    /// enabled. Engine code always runs inside the session runtime, which enables it.
    fn sleep_until(&self, deadline: Instant) -> Sleep;

    /// A future that completes after `duration`. An overflowing deadline sleeps "forever".
    fn sleep(&self, duration: Duration) -> Sleep {
        self.sleep_until(deadline_after(self.now(), duration))
    }
}

/// Waits until `deadline` on `clock`; returns at once, without touching the timer, if the
/// deadline has already come.
///
/// Use this instead of [`Clock::sleep_until`] wherever a deadline may already be due (paced
/// output, catch-up loops): tokio rounds every sleep up to its next timer tick, ~15.6 ms of real
/// time on Windows, so sleeping on a due deadline delays the caller by up to a tick.
pub async fn wait_until(clock: &dyn Clock, deadline: Instant) {
    if deadline > clock.now() {
        clock.sleep_until(deadline).await;
    }
}

/// Waits `duration` on `clock`; returns at once, without touching the timer, for a zero
/// duration (see [`wait_until`]).
pub async fn wait_for(clock: &dyn Clock, duration: Duration) {
    if !duration.is_zero() {
        clock.sleep(duration).await;
    }
}

/// `now + duration`, saturating to a far-future deadline instead of panicking on overflow.
pub fn deadline_after(now: Instant, duration: Duration) -> Instant {
    // The last fallback (`now`) needs `now + 30 years` to overflow too, which cannot happen on
    // supported platforms; it exists only to avoid a panic path.
    now.checked_add(duration)
        .or_else(|| now.checked_add(FAR_FUTURE))
        .unwrap_or(now)
}

/// Real time: tokio's monotonic clock and the operating system's wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl SystemClock {
    /// A [`SharedClock`] backed by real time.
    pub fn shared() -> SharedClock {
        Arc::new(Self)
    }
}

#[allow(clippy::disallowed_methods)] // the one place real time is read
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wall(&self) -> SystemTime {
        SystemTime::now()
    }

    fn sleep_until(&self, deadline: Instant) -> Sleep {
        tokio::time::sleep_until(deadline)
    }
}

/// Simulated time for deterministic tests.
///
/// Create it **inside** a tokio runtime whose time is paused (`Builder::start_paused(true)`,
/// `#[tokio::test(start_paused = true)]` or `tokio::time::pause()`). Time then advances only when
/// every task is idle, jumping straight to the next timer, so runs are fast and reproducible.
/// `wall()` is a fixed origin plus elapsed simulated time, so log timestamps are reproducible too.
///
/// # Caveats
/// - Only current-thread runtimes can be paused (`start_paused(true)` panics on multi-thread).
/// - Misuse is silent: created outside a paused runtime, it follows real time. tokio offers no
///   "is paused" query, so this cannot be checked; tests should assert on simulated elapsed time.
#[derive(Debug, Clone, Copy)]
pub struct SimClock {
    origin: Instant,
    wall_origin: SystemTime,
}

impl SimClock {
    /// Default wall-clock origin in seconds since the Unix epoch: 2026-01-01T00:00:00Z.
    pub const DEFAULT_WALL_ORIGIN_UNIX_SECS: u64 = 1_767_225_600;

    /// The default wall-clock origin as a `SystemTime`.
    pub fn default_wall_origin() -> SystemTime {
        UNIX_EPOCH
            .checked_add(Duration::from_secs(Self::DEFAULT_WALL_ORIGIN_UNIX_SECS))
            .unwrap_or(UNIX_EPOCH)
    }

    /// A simulation clock starting now, with the [default wall origin](Self::default_wall_origin).
    pub fn new() -> Self {
        Self::with_wall_origin(Self::default_wall_origin())
    }

    /// A simulation clock starting now whose `wall()` begins at `wall_origin`.
    #[allow(clippy::disallowed_methods)] // reads tokio's (paused) clock once, as the origin
    pub fn with_wall_origin(wall_origin: SystemTime) -> Self {
        Self {
            origin: Instant::now(),
            wall_origin,
        }
    }

    /// A [`SharedClock`] with the default wall origin.
    pub fn shared() -> SharedClock {
        Arc::new(Self::new())
    }

    /// Simulated time elapsed since this clock was created.
    pub fn elapsed(&self) -> Duration {
        self.now().saturating_duration_since(self.origin)
    }
}

impl Default for SimClock {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::disallowed_methods)] // paused tokio time is the simulated time source
impl Clock for SimClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wall(&self) -> SystemTime {
        // Overflow would need ~584 billion years of simulated time; fall back to the origin.
        self.wall_origin
            .checked_add(self.elapsed())
            .unwrap_or(self.wall_origin)
    }

    fn sleep_until(&self, deadline: Instant) -> Sleep {
        tokio::time::sleep_until(deadline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn wait_until_skips_the_timer_for_due_deadlines() {
        let clock = SimClock::new();
        let start = clock.now();
        // Due (now and in the past): completes on the first poll, time does not move.
        let mut due = std::pin::pin!(wait_until(&clock, start));
        assert!(futures_poll_once(due.as_mut()));
        wait_until(&clock, start).await;
        wait_for(&clock, Duration::ZERO).await;
        assert_eq!(clock.now(), start);
        // A future deadline waits for it.
        let later = start + Duration::from_millis(30);
        wait_until(&clock, later).await;
        assert_eq!(clock.now(), later);
        wait_for(&clock, Duration::from_millis(5)).await;
        assert_eq!(clock.now(), later + Duration::from_millis(5));
    }

    /// Polls a future once with a no-op waker; true if it completed.
    fn futures_poll_once<F: std::future::Future>(future: std::pin::Pin<&mut F>) -> bool {
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        future.poll(&mut cx).is_ready()
    }

    const FRAME: Duration = Duration::from_millis(20);
    const SESSION: Duration = Duration::from_secs(600);

    /// One trace entry: (simulated ms since session start, event).
    type Trace = Vec<(u128, &'static str)>;

    /// A 10-minute call: a 20 ms output pacer (30 000 frames) and a scripted caller who talks
    /// for 2.5 s every 7 s, all reporting into one bounded channel.
    async fn simulated_session(clock: SharedClock) -> Trace {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(u128, &'static str)>(64);
        let start = clock.now();

        let pacer = {
            let (clock, tx) = (clock.clone(), tx.clone());
            tokio::spawn(async move {
                let mut next = start;
                for frame in 1..=SESSION.as_millis() / FRAME.as_millis() {
                    next += FRAME;
                    clock.sleep_until(next).await;
                    if frame % 1_000 == 0 {
                        let at = (clock.now() - start).as_millis();
                        tx.send((at, "1000 frames out")).await.unwrap();
                    }
                }
            })
        };

        let caller = {
            let clock = clock.clone();
            tokio::spawn(async move {
                let mut turn_start = start + Duration::from_millis(1_500);
                while turn_start + Duration::from_millis(2_500) <= start + SESSION {
                    clock.sleep_until(turn_start).await;
                    tx.send(((clock.now() - start).as_millis(), "user speech start"))
                        .await
                        .unwrap();
                    clock.sleep(Duration::from_millis(2_500)).await;
                    tx.send(((clock.now() - start).as_millis(), "user speech end"))
                        .await
                        .unwrap();
                    turn_start += Duration::from_secs(7);
                }
            })
        };

        let mut trace = Trace::new();
        while let Some(event) = rx.recv().await {
            trace.push(event);
        }
        pacer.await.unwrap();
        caller.await.unwrap();
        trace
    }

    /// Runs one simulated session on a fresh paused runtime.
    fn run_simulation() -> (Trace, Duration, SystemTime) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        rt.block_on(async {
            let sim = Arc::new(SimClock::new());
            let trace = simulated_session(sim.clone()).await;
            (trace, sim.elapsed(), sim.wall())
        })
    }

    #[test]
    #[allow(clippy::disallowed_methods)] // measuring real elapsed time is the point
    fn ten_minute_session_runs_fast_and_deterministically() {
        let wall_start = std::time::Instant::now();
        let (first, elapsed, wall_end) = run_simulation();
        let (second, _, _) = run_simulation();
        let real = wall_start.elapsed();

        assert!(real < Duration::from_secs(1), "two runs took {real:?}");
        assert_eq!(elapsed, SESSION);
        assert_eq!(wall_end, SimClock::default_wall_origin() + SESSION);
        assert_eq!(first, second, "simulated timelines differ between runs");

        // 30 pacer checkpoints + 86 caller turns (1.5 s + 7 s·k, k = 0..=85) × 2 events.
        assert_eq!(first.len(), 30 + 86 * 2);
        assert_eq!(first.first(), Some(&(1_500, "user speech start")));
        assert_eq!(first.last(), Some(&(600_000, "1000 frames out")));
        assert!(first.contains(&(4_000, "user speech end")));
        assert!(
            first.windows(2).all(|w| w[0].0 <= w[1].0),
            "time went backwards"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sim_clock_follows_paused_time() {
        let sim = SimClock::new();
        let t0 = sim.now();
        tokio::time::advance(Duration::from_millis(250)).await;
        assert_eq!(sim.now() - t0, Duration::from_millis(250));
        assert_eq!(sim.elapsed(), Duration::from_millis(250));
        sim.sleep(Duration::from_secs(3)).await;
        assert_eq!(sim.elapsed(), Duration::from_millis(3_250));
        assert_eq!(
            sim.wall(),
            SimClock::default_wall_origin() + Duration::from_millis(3_250)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn overflowing_sleep_saturates_instead_of_panicking() {
        let sim = SimClock::new();
        let now = sim.now();
        assert!(deadline_after(now, Duration::MAX) > now + Duration::from_secs(86_400 * 365));
        let sleep = sim.sleep(Duration::MAX);
        assert!(sleep.deadline() > now);
    }

    /// Deliberately uses real time (5 ms): the exception to "no wall-time sleeps in tests".
    #[tokio::test]
    async fn system_clock_reads_real_time() {
        let clock = SystemClock::shared();
        let t0 = clock.now();
        clock.sleep(Duration::from_millis(5)).await;
        assert!(clock.now() - t0 >= Duration::from_millis(5));
        #[allow(clippy::disallowed_methods)]
        let os_now = SystemTime::now();
        let skew = os_now
            .duration_since(clock.wall())
            .unwrap_or_else(|e| e.duration());
        assert!(skew < Duration::from_secs(1));
    }

    #[test]
    fn default_wall_origin_is_2026_01_01_utc() {
        let secs = SimClock::default_wall_origin()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(secs, 1_767_225_600);
    }
}
