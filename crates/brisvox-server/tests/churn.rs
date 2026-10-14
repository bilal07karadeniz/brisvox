//! Leak test: 10 000 sessions of every ending (completed, failed, panicked, cancelled,
//! detached) in concurrent waves on a multi-thread runtime. Afterwards the registry is empty,
//! the stats add up, the runtime's task count is back to its baseline and the heap did not grow.
//!
//! One test per binary on purpose: the counting global allocator is process-wide.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
// test helpers
// The counting allocator is the only unsafe code: it forwards to the system allocator.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Duration;

use brisvox_core::SystemClock;
use brisvox_server::{ManagerConfig, SessionContext, SessionEnded, SessionManager};
use brisvox_sim::leak::TaskCounter;

struct Counting;

/// Bytes currently allocated (allocations minus frees), process-wide.
static LIVE_BYTES: AtomicIsize = AtomicIsize::new(0);

fn to_isize(n: usize) -> isize {
    isize::try_from(n).unwrap_or(isize::MAX)
}

// SAFETY: forwards every call unchanged to the system allocator; only keeps a byte balance.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: same contract as the caller's.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            LIVE_BYTES.fetch_add(to_isize(layout.size()), Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(to_isize(layout.size()), Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: same contract as the caller's.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            LIVE_BYTES.fetch_add(
                to_isize(new_size) - to_isize(layout.size()),
                Ordering::Relaxed,
            );
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const SESSIONS: usize = 10_000;
const WAVE: usize = 250;
const WARMUP_WAVES: usize = 4;
/// Heap growth allowed over the whole measured window. A leaked entry + task + token costs
/// several hundred bytes, so a per-session leak of even 10 bytes would be ~100 KB.
const MAX_GROWTH_BYTES: isize = 64 * 1024;

/// One wave of `WAVE` sessions, a mix of every ending, all running concurrently.
async fn wave(mgr: &SessionManager, first: usize) {
    let mut handles = Vec::with_capacity(WAVE);
    // Sessions park on this gate, so the whole wave is registered at the same time.
    let (open, gate) = tokio::sync::watch::channel(false);
    for i in first..first + WAVE {
        let kind = i % 10;
        let mut gate = gate.clone();
        let h = mgr
            .start(move |ctx: SessionContext| async move {
                let _ = gate.wait_for(|open| *open).await;
                match kind {
                    0 => panic!("churn panic {i}"),
                    1 | 2 => Err(format!("churn failure {i}")),
                    3 | 4 => {
                        // Waits for the cancel the driver sends right after starting it.
                        ctx.cancel.cancelled().await;
                        Ok(())
                    }
                    _ => {
                        tokio::task::yield_now().await;
                        Ok(())
                    }
                }
            })
            .expect("capacity is above the wave size");
        if kind == 3 || kind == 4 {
            h.cancel();
        }
        handles.push((kind, h));
    }
    open.send_replace(true);
    for (kind, h) in handles {
        if kind == 9 {
            continue; // detached: nobody awaits it, cleanup must still happen
        }
        let ended = h.ended().await;
        match kind {
            0 => assert!(matches!(ended, SessionEnded::Panicked(_))),
            1 | 2 => assert!(matches!(ended, SessionEnded::Failed(_))),
            3 | 4 => assert_eq!(ended, SessionEnded::Cancelled),
            _ => assert_eq!(ended, SessionEnded::Completed),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_thousand_sessions_leave_nothing_behind() {
    // Panics are expected here; keep the test output readable.
    std::panic::set_hook(Box::new(|_| {}));

    let clock = SystemClock::shared();
    let mgr =
        SessionManager::new(ManagerConfig::new(WAVE + 16).unwrap(), Arc::clone(&clock)).unwrap();
    let counter = TaskCounter::start_settled(&*clock, Duration::from_millis(50))
        .await
        .unwrap();

    // Warm-up: grows the registry map, the runtime's queues and the tracing machinery to their
    // steady-state size, so the measured window sees only per-session leaks.
    let mut done = 0;
    for w in 0..WARMUP_WAVES {
        wave(&mgr, w * WAVE).await;
        done += WAVE;
    }
    assert!(mgr.wait_idle(Duration::from_secs(10)).await);
    let before = LIVE_BYTES.load(Ordering::Relaxed);

    while done < SESSIONS {
        wave(&mgr, done).await;
        done += WAVE;
    }
    assert!(
        mgr.wait_idle(Duration::from_secs(10)).await,
        "detached sessions must also be cleaned up"
    );
    counter
        .wait_for_baseline(&*clock, Duration::from_secs(10))
        .await
        .expect("task count is back to the baseline");
    let after = LIVE_BYTES.load(Ordering::Relaxed);

    assert!(mgr.active_ids().is_empty(), "registry is empty");
    let stats = mgr.stats();
    assert_eq!(stats.active, 0);
    assert_eq!(stats.reserved, 0);
    assert_eq!(stats.started, SESSIONS as u64);
    assert_eq!(stats.ended.total(), SESSIONS as u64);
    assert_eq!(stats.ended.panicked, (SESSIONS / 10) as u64);
    assert_eq!(stats.ended.failed, (SESSIONS / 5) as u64);
    assert_eq!(stats.ended.cancelled, (SESSIONS / 5) as u64);
    assert_eq!(stats.ended.completed, (SESSIONS / 2) as u64);
    assert_eq!(stats.ended.aborted, 0);
    assert_eq!(stats.rejected_at_capacity, 0);
    assert!(stats.peak_active <= WAVE + 16);

    let growth = after - before;
    eprintln!(
        "churn: {SESSIONS} sessions, peak {} active, heap growth over the last {} sessions: {growth} B",
        stats.peak_active,
        SESSIONS - WARMUP_WAVES * WAVE
    );
    assert!(
        growth < MAX_GROWTH_BYTES,
        "heap grew by {growth} bytes over the measured window"
    );
}
