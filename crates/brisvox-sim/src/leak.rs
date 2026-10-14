//! Task leak checks.
//!
//! [`TaskCounter`] snapshots how many tasks are alive on the current tokio runtime and later
//! waits until the count is back to that baseline, failing with [`Error::TaskLeak`] if spawned
//! tasks outlive what should have ended them (cancel, stop, session end).

use std::time::Duration;

use brisvox_core::Clock;
use tokio::runtime::Handle;

use crate::error::{Error, Result};

/// Polling interval while waiting for tasks to finish.
const POLL: Duration = Duration::from_millis(1);

/// Counts alive tasks on one runtime relative to a baseline.
#[derive(Debug, Clone)]
pub struct TaskCounter {
    handle: Handle,
    baseline: usize,
}

impl TaskCounter {
    /// Records the current number of alive tasks as the baseline.
    ///
    /// # Errors
    /// [`Error::NoRuntime`] when called outside a tokio runtime.
    pub fn start() -> Result<Self> {
        let handle = Handle::try_current().map_err(|_| Error::NoRuntime)?;
        let baseline = handle.metrics().num_alive_tasks();
        Ok(Self { handle, baseline })
    }

    /// Like [`Self::start`], but first waits until the alive-task count has not changed for
    /// `quiet` (at most `4 * quiet`), so tasks left over from earlier work have finished before
    /// the baseline is taken. A task that exits inside the later window can still offset a leak
    /// there: the count is runtime-wide and cannot tell whose task it was.
    ///
    /// # Errors
    /// [`Error::NoRuntime`] when called outside a tokio runtime.
    pub async fn start_settled(clock: &dyn Clock, quiet: Duration) -> Result<Self> {
        let handle = Handle::try_current().map_err(|_| Error::NoRuntime)?;
        let give_up = brisvox_core::clock::deadline_after(clock.now(), quiet.saturating_mul(4));
        let mut count = handle.metrics().num_alive_tasks();
        let mut stable_since = clock.now();
        while clock.now() < give_up && clock.now().saturating_duration_since(stable_since) < quiet {
            clock.sleep(POLL).await;
            let alive = handle.metrics().num_alive_tasks();
            if alive != count {
                count = alive;
                stable_since = clock.now();
            }
        }
        Ok(Self {
            handle,
            baseline: count,
        })
    }

    /// The baseline recorded by [`Self::start`].
    pub const fn baseline(&self) -> usize {
        self.baseline
    }

    /// Tasks alive right now.
    pub fn alive(&self) -> usize {
        self.handle.metrics().num_alive_tasks()
    }

    /// Waits (on `clock`, so it works under paused time) until no more tasks are alive than at
    /// the baseline.
    ///
    /// # Errors
    /// [`Error::TaskLeak`] if tasks are still alive after `timeout`.
    pub async fn wait_for_baseline(&self, clock: &dyn Clock, timeout: Duration) -> Result<()> {
        let deadline = brisvox_core::clock::deadline_after(clock.now(), timeout);
        loop {
            let alive = self.alive();
            if alive <= self.baseline {
                return Ok(());
            }
            if clock.now() >= deadline {
                return Err(Error::TaskLeak {
                    alive,
                    baseline: self.baseline,
                });
            }
            clock.sleep(POLL).await;
        }
    }
}
