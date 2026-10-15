//! Counters of the fakes and the `/metrics` text (Prometheus exposition format).

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A gauge that also remembers its peak and counts how often it was raised.
#[derive(Debug, Default)]
pub struct Gauge {
    open: AtomicU64,
    peak: AtomicU64,
    total: AtomicU64,
}

impl Gauge {
    /// Raises the gauge until the returned guard is dropped.
    pub fn enter(self: &Arc<Self>) -> GaugeGuard {
        let now = self.open.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        self.peak.fetch_max(now, Ordering::Relaxed);
        self.total.fetch_add(1, Ordering::Relaxed);
        GaugeGuard(Arc::clone(self))
    }

    /// Currently open.
    pub fn open(&self) -> u64 {
        self.open.load(Ordering::Relaxed)
    }

    /// Highest `open` seen.
    pub fn peak(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }

    /// Times `enter` was called.
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
}

/// Lowers its gauge on drop.
#[derive(Debug)]
pub struct GaugeGuard(Arc<Gauge>);

impl Drop for GaugeGuard {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::Relaxed);
    }
}

/// All counters of one server process.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Open TCP connections.
    pub connections: Arc<Gauge>,
    /// Open Deepgram live STT websockets.
    pub stt_streams: Arc<Gauge>,
    /// Open Deepgram Aura TTS websockets.
    pub tts_streams: Arc<Gauge>,
    /// Chat Completions requests being answered.
    pub llm_requests: Arc<Gauge>,
    /// STT audio bytes received.
    pub stt_audio_bytes: AtomicU64,
    /// TTS audio bytes sent.
    pub tts_audio_bytes: AtomicU64,
    /// STT result messages sent.
    pub stt_results: AtomicU64,
    /// TLS handshakes that completed.
    pub tls_handshakes: AtomicU64,
    /// TLS handshakes that failed or timed out.
    pub tls_failures: AtomicU64,
    /// Connections closed at accept because the limit was reached.
    pub refused: AtomicU64,
    /// Requests that were not understood (bad path, bad body, bad upgrade).
    pub bad_requests: AtomicU64,
}

impl Metrics {
    /// The exposition text.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(2048);
        let gauge = |out: &mut String, name: &str, help: &str, g: &Gauge| {
            let _ = writeln!(out, "# HELP fake_{name}_open {help} (open now)");
            let _ = writeln!(out, "# TYPE fake_{name}_open gauge");
            let _ = writeln!(out, "fake_{name}_open {}", g.open());
            let _ = writeln!(out, "fake_{name}_peak {}", g.peak());
            let _ = writeln!(out, "# TYPE fake_{name}_total counter");
            let _ = writeln!(out, "fake_{name}_total {}", g.total());
        };
        gauge(
            &mut out,
            "connections",
            "TCP connections",
            &self.connections,
        );
        gauge(
            &mut out,
            "stt_streams",
            "Deepgram STT websockets",
            &self.stt_streams,
        );
        gauge(
            &mut out,
            "tts_streams",
            "Deepgram Aura websockets",
            &self.tts_streams,
        );
        gauge(
            &mut out,
            "llm_requests",
            "chat completion requests",
            &self.llm_requests,
        );
        let counter = |out: &mut String, name: &str, v: &AtomicU64| {
            let _ = writeln!(out, "# TYPE fake_{name} counter");
            let _ = writeln!(out, "fake_{name} {}", v.load(Ordering::Relaxed));
        };
        counter(&mut out, "stt_audio_bytes_total", &self.stt_audio_bytes);
        counter(&mut out, "tts_audio_bytes_total", &self.tts_audio_bytes);
        counter(&mut out, "stt_results_total", &self.stt_results);
        counter(&mut out, "tls_handshakes_total", &self.tls_handshakes);
        counter(&mut out, "tls_failures_total", &self.tls_failures);
        counter(&mut out, "refused_connections_total", &self.refused);
        counter(&mut out, "bad_requests_total", &self.bad_requests);
        process_stats(&mut out);
        out
    }
}

/// CPU time and resident memory of this process, read on every scrape.
fn process_stats(out: &mut String) {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    static PROBE: std::sync::Mutex<Option<System>> = std::sync::Mutex::new(None);
    let Ok(pid) = sysinfo::get_current_pid() else {
        return;
    };
    let mut guard = PROBE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let system = guard.get_or_insert_with(System::new);
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::nothing().with_memory().with_cpu(),
    );
    let Some(process) = system.process(pid) else {
        return;
    };
    // Milliseconds of CPU time to seconds: far below 2^52.
    #[allow(clippy::cast_precision_loss)]
    let cpu = process.accumulated_cpu_time() as f64 / 1000.0;
    let _ = writeln!(out, "# TYPE process_cpu_seconds_total counter");
    let _ = writeln!(out, "process_cpu_seconds_total {cpu:.2}");
    let _ = writeln!(out, "# TYPE process_resident_memory_bytes gauge");
    let _ = writeln!(out, "process_resident_memory_bytes {}", process.memory());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gauges_count_open_peak_and_total() {
        let m = Metrics::default();
        let a = m.stt_streams.enter();
        let b = m.stt_streams.enter();
        drop(a);
        assert_eq!(
            (
                m.stt_streams.open(),
                m.stt_streams.peak(),
                m.stt_streams.total()
            ),
            (1, 2, 2)
        );
        drop(b);
        let text = m.render();
        assert!(text.contains("fake_stt_streams_open 0"));
        assert!(text.contains("fake_stt_streams_peak 2"));
        assert!(text.contains("fake_stt_streams_total 2"));
    }
}
