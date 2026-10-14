//! Timeline events reach tracing subscribers. Kept in its own test binary: tracing caches
//! callsite interest globally, so concurrent tests without a subscriber in the same process can
//! make a scoped subscriber miss events.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_core::{TimelineRecorder, TurnEpoch, TurnEvent};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("poisoned"))?
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
#[allow(clippy::disallowed_methods)] // a base instant for the marks
fn events_and_turn_summaries_are_emitted_as_tracing_events() {
    let out = Capture::default();
    let writer = out.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let rec = TimelineRecorder::new();
        let t0 = tokio::time::Instant::now();
        let epoch = TurnEpoch::new(3);
        rec.mark(epoch, TurnEvent::LlmRequest, t0);
        rec.mark(
            epoch,
            TurnEvent::LlmFirstToken,
            t0 + Duration::from_millis(300),
        );
        rec.finish();
    });
    let logs = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("event=\"llm_first_token\""), "{logs}");
    assert!(logs.contains("turn finished"), "{logs}");
    assert!(logs.contains("epoch=3"), "{logs}");
    assert!(logs.contains("llm_ttft_us=300000"), "{logs}");
}

#[test]
#[allow(clippy::disallowed_methods)] // a base instant for the marks
fn the_turn_summary_carries_the_span_the_recorder_was_made_in() {
    let out = Capture::default();
    let writer = out.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let rec = tracing::info_span!("session", id = "s7").in_scope(TimelineRecorder::new);
        let t0 = tokio::time::Instant::now();
        rec.mark(TurnEpoch::new(1), TurnEvent::LlmRequest, t0);
        // Finished from a context outside the session span (another task, the next turn).
        rec.mark(TurnEpoch::new(2), TurnEvent::LlmRequest, t0);
    });
    let logs = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
    let line = logs
        .lines()
        .find(|l| l.contains("turn finished"))
        .unwrap_or_default();
    assert!(line.contains("session{id=\"s7\"}"), "{logs}");
}
