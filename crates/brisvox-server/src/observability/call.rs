//! The observer of one call: the [`TimelineSink`] that turns each finished turn into metrics and
//! one trace, and folds the call's counters into the server's at the end.
//!
//! # Cost
//! A finished turn costs a handful of atomic increments, one trace built from the turn's
//! marks (about a dozen small allocations) and one `export` call that must not block. That is
//! once per turn, on the stage task that finished it; nothing runs per audio frame or per
//! token. `observability::tests::a_turn_costs_microseconds` measures it.
//!
//! # Interrupted turns
//! The output reports the end of a barge-in after the turn has finished (`AudioOutStopped`
//! patches the finished record). So that the `interruption` span is complete, an interrupted
//! turn that has not stopped yet is held back and exported when the patch arrives, when the
//! next turn finishes, or when the call ends, whichever is first. At most one turn is held.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use brisvox_core::{SharedClock, TimelineRecorder, TimelineSink, TurnEpoch, TurnEvent, TurnRecord};
use brisvox_engine::session::CallStats;
use brisvox_providers::resilience::ResilienceProbe;

use super::Observability;
use super::registry::{ProviderStage, Stage, resilience_events};
use super::trace::{Anchor, TurnIdentity, build_trace};

/// What the observer remembers between events.
#[derive(Default)]
struct State {
    /// An interrupted turn waiting for its `AudioOutStopped`.
    held: Option<TurnRecord>,
    /// The newest epoch whose barge-in is already counted.
    interruption_counted: Option<TurnEpoch>,
    /// The newest epoch whose time to silence is already observed.
    silence_observed: Option<TurnEpoch>,
    /// The call's resilience wrappers and what they had counted when last read.
    probes: Vec<Watched>,
}

struct Watched {
    stage: ProviderStage,
    probe: ResilienceProbe,
    seen: [u64; 11],
}

/// The [`TimelineSink`] of one call. Create it with [`Observability::call`].
pub struct CallObserver {
    obs: Arc<Observability>,
    who: TurnIdentity,
    clock: SharedClock,
    state: Mutex<State>,
    /// [`Self::call_ended`] already ran (it runs once).
    ended: AtomicBool,
}

impl std::fmt::Debug for CallObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallObserver")
            .field("session", &self.who.session)
            .field("agent", &self.who.agent)
            .finish_non_exhaustive()
    }
}

impl CallObserver {
    pub(super) fn new(obs: Arc<Observability>, who: TurnIdentity, clock: SharedClock) -> Arc<Self> {
        Arc::new(Self {
            obs,
            who,
            clock,
            state: Mutex::new(State::default()),
            ended: AtomicBool::new(false),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A timeline recorder that reports to this observer.
    pub fn recorder(self: &Arc<Self>) -> TimelineRecorder {
        TimelineRecorder::with_sink(Arc::clone(self) as Arc<dyn TimelineSink>)
    }

    /// Watches the call's resilience wrappers (`(stage, probe)` of each): their counters are
    /// folded into `brisvox_resilience_events_total` at every finished turn and at the end.
    pub fn watch_resilience(
        &self,
        probes: impl IntoIterator<Item = (ProviderStage, ResilienceProbe)>,
    ) {
        let mut state = self.lock();
        state
            .probes
            .extend(probes.into_iter().map(|(stage, probe)| Watched {
                stage,
                probe,
                seen: [0; 11],
            }));
    }

    /// Adds what the watched wrappers counted since the last read.
    fn sync_resilience(&self, state: &mut State) {
        for watched in &mut state.probes {
            let now = resilience_events(&watched.probe.stats());
            let mut delta = [0u64; 11];
            for ((d, n), seen) in delta.iter_mut().zip(now).zip(&watched.seen) {
                *d = n.saturating_sub(*seen);
            }
            watched.seen = now;
            self.obs.metrics().add_resilience(watched.stage, delta);
        }
    }

    /// Exports the held turn, if any.
    fn flush_held(&self, state: &mut State) {
        if let Some(record) = state.held.take() {
            self.export(&record);
        }
    }

    /// Builds and exports the trace of `record` when the server has an exporter.
    fn export(&self, record: &TurnRecord) {
        let Some(exporter) = self.obs.exporter() else {
            return;
        };
        let anchor = Anchor::new(self.clock.now(), self.clock.wall());
        if let Some(trace) = build_trace(record, &self.who, anchor, self.obs.ids()) {
            self.obs.metrics().traces.exported.inc();
            exporter.export(trace);
        }
    }

    /// Ends the call: exports the held turn, folds in the resilience counters and, with the
    /// engine's end-of-call statistics, the provider and engine counters. Call it once, after
    /// the call has stopped and the recorder was finished (`TimelineRecorder::finish`). Only the
    /// first call counts; later ones do nothing (so a [`CallGuard`] may back an explicit call).
    pub fn call_ended(&self, stats: Option<&CallStats>) {
        if self.ended.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut state = self.lock();
        self.flush_held(&mut state);
        self.sync_resilience(&mut state);
        drop(state);
        self.obs.metrics().observe_call(stats);
    }
}

impl TimelineSink for CallObserver {
    fn turn_finished(&self, record: &TurnRecord) {
        let metrics = self.obs.metrics();
        metrics.observe_turn(record);
        let mut state = self.lock();
        if record.at(TurnEvent::Interrupted).is_some() {
            state.interruption_counted = Some(record.epoch());
        }
        self.sync_resilience(&mut state);
        // The previous held turn ends here: no patch came, the next turn already finished.
        self.flush_held(&mut state);
        let waits_for_silence = record.at(TurnEvent::Interrupted).is_some()
            && record.at(TurnEvent::AudioOutStopped).is_none();
        if waits_for_silence && self.obs.exporter().is_some() {
            state.held = Some(*record);
        } else {
            drop(state);
            self.export(record);
        }
    }

    fn turn_patched(&self, record: &TurnRecord) {
        let metrics = self.obs.metrics();
        let mut state = self.lock();
        let epoch = record.epoch();
        if record.at(TurnEvent::Interrupted).is_some() && state.interruption_counted != Some(epoch)
        {
            state.interruption_counted = Some(epoch);
            metrics.observe_late_interruption();
        }
        if let Some(silence) = Stage::TimeToSilence.of(record)
            && state.silence_observed != Some(epoch)
        {
            state.silence_observed = Some(epoch);
            metrics.observe_silence(silence);
        }
        // The held turn is complete now (or it is a different one: then it stays held).
        if state.held.is_some_and(|held| held.epoch() == epoch)
            && record.at(TurnEvent::AudioOutStopped).is_some()
        {
            state.held = None;
            drop(state);
            self.export(record);
        }
    }
}

/// Ends a call's observation when it goes out of scope, however that happens: the call's
/// future returning, being dropped (aborted after the drain timeout), unwinding from a panic, or
/// leaving early through `?`. Without it those calls would never count in
/// `brisvox_calls_ended_total`, their provider errors would be lost and their held trace never
/// exported.
///
/// It owns the call object `C` (the engine's `Call`), so its counters can still be read when it
/// drops: fields drop after [`Drop::drop`] ran. Drop the guard explicitly at the normal end of
/// the call; the end then happens once either way.
pub struct CallGuard<C> {
    observer: Option<Arc<CallObserver>>,
    timeline: TimelineRecorder,
    stats: fn(&C) -> Option<CallStats>,
    call: Option<C>,
}

impl<C> CallGuard<C> {
    /// A guard for a call that has not started yet. `stats` reads the call's end-of-call
    /// statistics; without an `observer` the guard only finishes the `timeline`.
    pub fn new(
        observer: Option<Arc<CallObserver>>,
        timeline: TimelineRecorder,
        stats: fn(&C) -> Option<CallStats>,
    ) -> Self {
        Self {
            observer,
            timeline,
            stats,
            call: None,
        }
    }

    /// Hands the started call to the guard and returns it for use.
    pub fn attach(&mut self, call: C) -> &mut C {
        self.call.insert(call)
    }
}

impl<C> Drop for CallGuard<C> {
    fn drop(&mut self) {
        // The last turn is finished only now; then the call's counters join the server's.
        self.timeline.finish();
        if let Some(observer) = &self.observer {
            let stats = self.call.as_ref().and_then(self.stats);
            observer.call_ended(stats.as_ref());
        }
    }
}
