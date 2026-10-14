//! The server's metric families and their Prometheus rendering.
//!
//! | family | kind | labels | fed by |
//! |---|---|---|---|
//! | `brisvox_stage_latency_seconds` | histogram | `stage` | every finished turn |
//! | `brisvox_turns_total` | counter | `outcome` | every finished turn |
//! | `brisvox_sessions_*`, `brisvox_session_admissions_rejected_total` | gauge/counter | `reason` | the session manager, read at scrape |
//! | `brisvox_pool_*`, `brisvox_provider_*` | gauge/counter | `provider` | admission control, read at scrape |
//! | `brisvox_provider_errors_total` | counter | `stage`, `kind` | the engine's stage counters, at call end |
//! | `brisvox_engine_events_total` | counter | `event` | the engine's call stats, at call end |
//! | `brisvox_breaker_*` | gauge/counter | `stage`, `role`, `provider` | the circuit breakers, read at scrape |
//! | `brisvox_retry_budget_*` | gauge/counter | none | the process retry budget, read at scrape |
//! | `brisvox_smart_turn_queue_depth`, `brisvox_smart_turn_rejected_total`, `brisvox_smart_turn_timed_out_total` | gauge/counter | none | the Smart Turn executor, read at scrape; absent without one |
//! | `brisvox_resilience_events_total` | counter | `stage`, `event` | each call's resilience counters (failovers, recoveries, budget refusals) |
//! | `brisvox_traces_*_total` | counter | none | the trace exporter |
//!
//! Counters fed per call are folded in at every finished turn and at the end of the call, so a
//! failover during a long call is visible within one turn. Nothing is touched per audio frame.

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{TurnEvent, TurnRecord};
use brisvox_engine::session::StageCounters;
use brisvox_inference::PoolMetrics;
use brisvox_providers::resilience::{BreakerState, BreakerStats, ResilienceStats};

use super::metrics::{Counter, Histogram, Kind, Prom};
use crate::session::{SessionManager, SessionStats};

/// The latency stages of a turn (definitions in `docs/LATENCY.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    /// `TurnEnd - UserSpeechEnd`: the end-of-turn policy's wait.
    EndOfTurn,
    /// `SttFinal - UserSpeechEnd`.
    SttFinal,
    /// `LlmFirstToken - LlmRequest`.
    LlmFirstToken,
    /// `LlmFirstSentence - LlmRequest`.
    LlmFirstSentence,
    /// `TtsFirstByte - TtsRequest`.
    TtsFirstByte,
    /// `LlmRequest - TurnEnd`: the engine's hop into the LLM (budget 3 ms).
    IntoLlm,
    /// `FirstAudioOut - TtsFirstByte`: the engine's hop out of the TTS (budget 2 ms).
    OutOfTts,
    /// `FirstAudioOut - TurnEnd`: everything after the end-of-turn decision.
    PostEndpoint,
    /// `FirstAudioOut - UserSpeechEnd`: server-side voice-to-voice.
    Response,
    /// `AudioOutStopped - Interrupted`: how fast the bot went silent after a barge-in.
    TimeToSilence,
    /// Duration of one LLM request (every round of a tool loop).
    LlmRound,
}

impl Stage {
    /// Number of stages.
    pub const COUNT: usize = 11;

    /// Every stage, in exposition order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::EndOfTurn,
        Self::SttFinal,
        Self::LlmFirstToken,
        Self::LlmFirstSentence,
        Self::TtsFirstByte,
        Self::IntoLlm,
        Self::OutOfTts,
        Self::PostEndpoint,
        Self::Response,
        Self::TimeToSilence,
        Self::LlmRound,
    ];

    /// The `stage` label value.
    pub const fn name(self) -> &'static str {
        match self {
            Self::EndOfTurn => "end_of_turn",
            Self::SttFinal => "stt_final",
            Self::LlmFirstToken => "llm_first_token",
            Self::LlmFirstSentence => "llm_first_sentence",
            Self::TtsFirstByte => "tts_first_byte",
            Self::IntoLlm => "into_llm",
            Self::OutOfTts => "out_of_tts",
            Self::PostEndpoint => "post_endpoint",
            Self::Response => "response",
            Self::TimeToSilence => "time_to_silence",
            Self::LlmRound => "llm_round",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }

    /// This stage's duration in `turn`; `None` where an endpoint is missing or reversed.
    /// ([`Stage::LlmRound`] is per round: see [`Metrics::observe_turn`].)
    pub fn of(self, turn: &TurnRecord) -> Option<Duration> {
        use TurnEvent as E;
        let m = turn.metrics();
        match self {
            Self::EndOfTurn => m.end_of_turn_delay,
            Self::SttFinal => m.stt_final_delay,
            Self::LlmFirstToken => m.llm_time_to_first_token,
            Self::LlmFirstSentence => m.llm_time_to_first_sentence,
            Self::TtsFirstByte => m.tts_time_to_first_byte,
            Self::IntoLlm => turn.between(E::TurnEnd, E::LlmRequest),
            Self::OutOfTts => turn.between(E::TtsFirstByte, E::FirstAudioOut),
            Self::PostEndpoint => m.post_endpoint_latency,
            Self::Response => m.response_latency,
            Self::TimeToSilence => m.time_to_silence,
            Self::LlmRound => None,
        }
    }
}

/// The provider stages the resilience layer and the error counters distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderStage {
    /// Speech to text.
    Stt,
    /// The language model.
    Llm,
    /// Text to speech.
    Tts,
}

impl ProviderStage {
    /// The three stages.
    pub const ALL: [Self; 3] = [Self::Stt, Self::Llm, Self::Tts];

    /// The `stage` label value.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Stt => "stt",
            Self::Llm => "llm",
            Self::Tts => "tts",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// `(stage, kind)` of every provider error counter, in the order [`provider_errors`] fills them.
const ERROR_KINDS: [(ProviderStage, &str); 12] = [
    (ProviderStage::Stt, "connect_failure"),
    (ProviderStage::Stt, "session_death"),
    (ProviderStage::Llm, "request_error"),
    (ProviderStage::Llm, "stream_error"),
    (ProviderStage::Llm, "stream_cut_off"),
    (ProviderStage::Llm, "first_token_timeout"),
    (ProviderStage::Llm, "idle_timeout"),
    (ProviderStage::Tts, "connect_failure"),
    (ProviderStage::Tts, "session_death"),
    (ProviderStage::Tts, "send_failure"),
    (ProviderStage::Tts, "stall"),
    (ProviderStage::Tts, "normalize_failure"),
];

/// The provider-side failures a call's stages counted, in [`ERROR_KINDS`] order.
fn provider_errors(s: &StageCounters) -> [u64; 12] {
    [
        s.stt.connect_failures,
        s.stt.session_deaths,
        s.llm.request_errors,
        s.llm.stream_errors,
        s.llm.streams_cut_off,
        s.llm.first_token_timeouts,
        s.llm.idle_timeouts,
        s.tts.connect_failures,
        s.tts.session_deaths,
        s.tts.send_failures,
        s.tts.stalls,
        s.tts.normalize_failures,
    ]
}

/// Names of the engine event counters, in the order [`engine_events`] fills them.
const ENGINE_EVENTS: [&str; 9] = [
    "queue_dropped",
    "stale_dropped",
    "pool_trouble",
    "vad_failed_open",
    "finals_gate_timeouts",
    "settle_timeouts",
    "mark_timeouts",
    "truncations",
    "clear_failures",
];

/// What a call's stages counted for [`ENGINE_EVENTS`].
fn engine_events(stats: &brisvox_engine::session::CallStats) -> [u64; 9] {
    let s = &stats.stages;
    [
        stats.pipeline.total_dropped(),
        stats.stale_dropped(),
        s.pool_trouble(),
        u64::from(s.vad.failed_open),
        s.driver.gate_timeouts,
        s.book.settle_timeouts,
        s.out.mark_timeouts,
        s.truncations.truncations,
        s.out.clear_failures,
    ]
}

/// Names of the per-call resilience counters, in the order [`resilience_events`] fills them.
const RESILIENCE_EVENTS: [&str; 11] = [
    "attempts",
    "retries",
    "failovers",
    "breaker_skips",
    "budget_refusals",
    "unavailable",
    "recoveries",
    "recovery_failures",
    "replayed",
    "idle_refreshes",
    "idle_refresh_failures",
];

/// A call's resilience counters in [`RESILIENCE_EVENTS`] order (`failovers` are the attempts the
/// secondary served).
pub(crate) fn resilience_events(s: &ResilienceStats) -> [u64; 11] {
    [
        s.attempts,
        s.retries,
        s.served_secondary,
        s.breaker_skips,
        s.budget_denied,
        s.unavailable,
        s.recoveries,
        s.recovery_failures,
        s.replayed,
        s.refreshes,
        s.refresh_failures,
    ]
}

/// One circuit breaker, as read at a scrape.
#[derive(Debug, Clone)]
pub struct BreakerSnapshot {
    /// The stage the leg serves.
    pub stage: ProviderStage,
    /// `primary` or `secondary`.
    pub role: &'static str,
    /// The provider's name.
    pub provider: &'static str,
    /// Where the breaker stands.
    pub state: BreakerState,
    /// Its counters.
    pub stats: BreakerStats,
}

/// The process retry budget, as read at a scrape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BudgetSnapshot {
    /// Tokens left now.
    pub available: u32,
    /// Retries granted so far.
    pub granted: u64,
    /// Retries refused so far.
    pub denied: u64,
}

/// The resilience layer's process-wide state.
#[derive(Debug, Clone, Default)]
pub struct ResilienceSnapshot {
    /// Every leg's breaker.
    pub breakers: Vec<BreakerSnapshot>,
    /// The process retry budget.
    pub budget: BudgetSnapshot,
}

/// Where a scrape reads the resilience layer from (implemented by
/// [`ResilientProviders`](crate::resilient::ResilientProviders)).
pub trait ResilienceSource: Send + Sync {
    /// The breakers and the process budget now.
    fn resilience_snapshot(&self) -> ResilienceSnapshot;
}

/// Counters of the trace pipeline.
#[derive(Debug, Default)]
pub struct TraceCounters {
    /// Traces built and handed to the exporter.
    pub exported: Counter,
    /// Traces an exporter dropped (queue full) or failed to send.
    pub dropped: Counter,
    /// Batches an exporter sent.
    pub batches_sent: Counter,
    /// Batches an exporter failed to send.
    pub batches_failed: Counter,
}

/// All the counters and histograms the server feeds.
#[derive(Debug)]
pub struct Metrics {
    stage: [Histogram; Stage::COUNT],
    turns: Counter,
    turns_completed: Counter,
    turns_interrupted: Counter,
    calls_ended: Counter,
    provider_errors: [Counter; ERROR_KINDS.len()],
    engine: [Counter; ENGINE_EVENTS.len()],
    resilience: [[Counter; RESILIENCE_EVENTS.len()]; 3],
    /// The trace pipeline's counters.
    pub traces: Arc<TraceCounters>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// All zero.
    pub fn new() -> Self {
        Self {
            stage: std::array::from_fn(|_| Histogram::new()),
            turns: Counter::new(),
            turns_completed: Counter::new(),
            turns_interrupted: Counter::new(),
            calls_ended: Counter::new(),
            provider_errors: std::array::from_fn(|_| Counter::new()),
            engine: std::array::from_fn(|_| Counter::new()),
            resilience: std::array::from_fn(|_| std::array::from_fn(|_| Counter::new())),
            traces: Arc::new(TraceCounters::default()),
        }
    }

    /// The histogram of `stage`.
    pub fn stage(&self, stage: Stage) -> Option<&Histogram> {
        self.stage.get(stage.index())
    }

    /// Records the stage latencies of a finished turn (not `TimeToSilence`, which arrives after
    /// the turn: [`Self::observe_silence`]) and counts the turn.
    pub fn observe_turn(&self, turn: &TurnRecord) {
        if TurnEvent::ALL.iter().all(|e| turn.at(*e).is_none()) {
            // A turn nothing was marked on (the epoch of a barge-in that never spoke).
            return;
        }
        for stage in Stage::ALL {
            if matches!(stage, Stage::TimeToSilence | Stage::LlmRound) {
                continue;
            }
            if let (Some(d), Some(h)) = (stage.of(turn), self.stage(stage)) {
                h.observe(d);
            }
        }
        if let Some(h) = self.stage(Stage::LlmRound) {
            for round in turn.llm_rounds() {
                if let Some(d) = round.duration {
                    h.observe(d);
                }
            }
        }
        self.turns.inc();
        if turn.at(TurnEvent::Interrupted).is_some() {
            self.turns_interrupted.inc();
        } else {
            self.turns_completed.inc();
        }
    }

    /// Counts a barge-in that was recorded after its turn finished. That turn was already
    /// counted as completed and stays so: both series only ever go up (a Prometheus counter
    /// that went down would read as a reset), so a late barge-in makes the two outcomes sum to
    /// more than [`Self::turns`].
    pub fn observe_late_interruption(&self) {
        self.turns_interrupted.inc();
    }

    /// Records how long the bot took to go silent after a barge-in.
    pub fn observe_silence(&self, time_to_silence: Duration) {
        if let Some(h) = self.stage(Stage::TimeToSilence) {
            h.observe(time_to_silence);
        }
    }

    /// Turns finished so far.
    pub fn turns(&self) -> u64 {
        self.turns.get()
    }

    /// Turns that finished without a barge-in so far (a monotonic series; see
    /// [`Self::observe_late_interruption`]).
    pub fn turns_completed(&self) -> u64 {
        self.turns_completed.get()
    }

    /// Turns that were interrupted so far.
    pub fn turns_interrupted(&self) -> u64 {
        self.turns_interrupted.get()
    }

    /// Calls that ended so far.
    pub fn calls_ended(&self) -> u64 {
        self.calls_ended.get()
    }

    /// Counts an ended call (`calls_ended`) and, when its statistics could be read, folds its
    /// counters in. A call that never got as far as having statistics still counts.
    pub fn observe_call(&self, stats: Option<&brisvox_engine::session::CallStats>) {
        self.calls_ended.inc();
        let Some(stats) = stats else {
            return;
        };
        for (counter, n) in self
            .provider_errors
            .iter()
            .zip(provider_errors(&stats.stages))
        {
            counter.add(n);
        }
        for (counter, n) in self.engine.iter().zip(engine_events(stats)) {
            counter.add(n);
        }
    }

    /// Adds what a call's resilience wrapper counted since the last time (the caller passes the
    /// difference).
    pub(crate) fn add_resilience(&self, stage: ProviderStage, delta: [u64; 11]) {
        if let Some(row) = self.resilience.get(stage.index()) {
            for (counter, n) in row.iter().zip(delta) {
                counter.add(n);
            }
        }
    }

    /// The resilience counter `event` of `stage`.
    pub fn resilience_event(&self, stage: ProviderStage, event: &str) -> u64 {
        let Some(i) = RESILIENCE_EVENTS.iter().position(|e| *e == event) else {
            return 0;
        };
        self.resilience
            .get(stage.index())
            .and_then(|row| row.get(i))
            .map_or(0, Counter::get)
    }

    /// The provider error counter for `stage` and `kind`.
    pub fn provider_error(&self, stage: ProviderStage, kind: &str) -> u64 {
        ERROR_KINDS
            .iter()
            .position(|(s, k)| *s == stage && *k == kind)
            .and_then(|i| self.provider_errors.get(i))
            .map_or(0, Counter::get)
    }

    /// Renders the whole exposition: these metrics plus the manager's live state and, when
    /// there are any, the resilience layer's and the Smart Turn executor's (`smart_turn`: its
    /// pool's counters now; `None` when the process runs no such executor).
    pub fn render(
        &self,
        manager: &SessionManager,
        resilience: Option<&Arc<dyn ResilienceSource>>,
        smart_turn: Option<&PoolMetrics>,
    ) -> String {
        let mut p = Prom::new();
        self.render_stages(&mut p);
        render_sessions(&mut p, &manager.stats());
        render_admission(&mut p, manager);
        self.render_errors(&mut p);
        if let Some(source) = resilience {
            self.render_resilience(&mut p, &source.resilience_snapshot());
        }
        if let Some(pool) = smart_turn {
            render_smart_turn(&mut p, pool);
        }
        self.render_traces(&mut p);
        p.finish()
    }

    fn render_stages(&self, p: &mut Prom) {
        p.family(
            "brisvox_stage_latency_seconds",
            "Latency of one turn stage (docs/LATENCY.md): one observation per finished turn (per LLM request for llm_round).",
            Kind::Histogram,
        );
        for stage in Stage::ALL {
            if let Some(h) = self.stage(stage) {
                p.histogram(
                    "brisvox_stage_latency_seconds",
                    &[("stage", stage.name())],
                    h,
                );
            }
        }
        p.family(
            "brisvox_turns_total",
            "Turns finished, by outcome.",
            Kind::Counter,
        );
        p.sample(
            "brisvox_turns_total",
            &[("outcome", "completed")],
            self.turns_completed.get(),
        );
        p.sample(
            "brisvox_turns_total",
            &[("outcome", "interrupted")],
            self.turns_interrupted.get(),
        );
    }

    fn render_errors(&self, p: &mut Prom) {
        p.family(
            "brisvox_provider_errors_total",
            "Provider failures the engine's stages counted, by stage and kind (folded in when a call ends).",
            Kind::Counter,
        );
        for ((stage, kind), counter) in ERROR_KINDS.iter().zip(&self.provider_errors) {
            p.sample(
                "brisvox_provider_errors_total",
                &[("stage", stage.name()), ("kind", kind)],
                counter.get(),
            );
        }
        p.family(
            "brisvox_engine_events_total",
            "Engine trouble counters of finished calls: drops, pool trouble, timeouts.",
            Kind::Counter,
        );
        for (event, counter) in ENGINE_EVENTS.iter().zip(&self.engine) {
            p.sample(
                "brisvox_engine_events_total",
                &[("event", event)],
                counter.get(),
            );
        }
        p.family(
            "brisvox_calls_ended_total",
            "Calls whose end-of-call statistics were recorded.",
            Kind::Counter,
        );
        p.sample("brisvox_calls_ended_total", &[], self.calls_ended.get());
    }

    fn render_resilience(&self, p: &mut Prom, snap: &ResilienceSnapshot) {
        p.family(
            "brisvox_breaker_state",
            "Circuit breaker of a provider: 0 closed, 1 half-open, 2 open.",
            Kind::Gauge,
        );
        for b in &snap.breakers {
            let state = match b.state {
                BreakerState::Closed => 0,
                BreakerState::HalfOpen => 1,
                BreakerState::Open => 2,
            };
            p.sample(
                "brisvox_breaker_state",
                &[
                    ("stage", b.stage.name()),
                    ("role", b.role),
                    ("provider", b.provider),
                ],
                state,
            );
        }
        p.family(
            "brisvox_breaker_events_total",
            "Circuit breaker counters: attempts admitted, refused, failed, probes, openings, closings.",
            Kind::Counter,
        );
        for b in &snap.breakers {
            let s = &b.stats;
            for (event, n) in [
                ("admitted", s.admitted),
                ("refused", s.refused),
                ("failures", s.failures),
                ("opened", s.opened),
                ("probes", s.probes),
                ("closed", s.closed),
            ] {
                p.sample(
                    "brisvox_breaker_events_total",
                    &[
                        ("stage", b.stage.name()),
                        ("role", b.role),
                        ("provider", b.provider),
                        ("event", event),
                    ],
                    n,
                );
            }
        }
        p.family(
            "brisvox_retry_budget_available",
            "Tokens left in the process retry budget.",
            Kind::Gauge,
        );
        p.sample("brisvox_retry_budget_available", &[], snap.budget.available);
        p.family(
            "brisvox_retry_budget_total",
            "Retries the process budget granted or refused.",
            Kind::Counter,
        );
        p.sample(
            "brisvox_retry_budget_total",
            &[("outcome", "granted")],
            snap.budget.granted,
        );
        p.sample(
            "brisvox_retry_budget_total",
            &[("outcome", "refused")],
            snap.budget.denied,
        );
        p.family(
            "brisvox_resilience_events_total",
            "Resilience counters of calls: retries, failovers to the secondary provider, budget refusals, recoveries.",
            Kind::Counter,
        );
        for stage in ProviderStage::ALL {
            let Some(row) = self.resilience.get(stage.index()) else {
                continue;
            };
            for (event, counter) in RESILIENCE_EVENTS.iter().zip(row) {
                p.sample(
                    "brisvox_resilience_events_total",
                    &[("stage", stage.name()), ("event", event)],
                    counter.get(),
                );
            }
        }
    }

    fn render_traces(&self, p: &mut Prom) {
        let t = &self.traces;
        p.family(
            "brisvox_traces_total",
            "Turn traces built, dropped by the exporter, or sent in batches.",
            Kind::Counter,
        );
        p.sample(
            "brisvox_traces_total",
            &[("outcome", "exported")],
            t.exported.get(),
        );
        p.sample(
            "brisvox_traces_total",
            &[("outcome", "dropped")],
            t.dropped.get(),
        );
        p.family(
            "brisvox_trace_batches_total",
            "Batches the OTLP exporter sent, by outcome.",
            Kind::Counter,
        );
        p.sample(
            "brisvox_trace_batches_total",
            &[("outcome", "sent")],
            t.batches_sent.get(),
        );
        p.sample(
            "brisvox_trace_batches_total",
            &[("outcome", "failed")],
            t.batches_failed.get(),
        );
    }
}

fn render_sessions(p: &mut Prom, s: &SessionStats) {
    p.family(
        "brisvox_sessions_active",
        "Sessions running now.",
        Kind::Gauge,
    );
    p.sample("brisvox_sessions_active", &[], s.active);
    p.family(
        "brisvox_sessions_reserved",
        "Admission slots reserved and not yet used.",
        Kind::Gauge,
    );
    p.sample("brisvox_sessions_reserved", &[], s.reserved);
    p.family("brisvox_sessions_max", "The admission limit.", Kind::Gauge);
    p.sample("brisvox_sessions_max", &[], s.max_sessions);
    p.family(
        "brisvox_sessions_peak",
        "Most sessions running at once since start.",
        Kind::Gauge,
    );
    p.sample("brisvox_sessions_peak", &[], s.peak_active);
    p.family(
        "brisvox_sessions_started_total",
        "Sessions started since start.",
        Kind::Counter,
    );
    p.sample("brisvox_sessions_started_total", &[], s.started);
    p.family(
        "brisvox_sessions_ended_total",
        "Sessions ended, by reason.",
        Kind::Counter,
    );
    for (reason, n) in [
        ("completed", s.ended.completed),
        ("failed", s.ended.failed),
        ("panicked", s.ended.panicked),
        ("cancelled", s.ended.cancelled),
        ("aborted", s.ended.aborted),
    ] {
        p.sample("brisvox_sessions_ended_total", &[("reason", reason)], n);
    }
    p.family(
        "brisvox_session_admissions_rejected_total",
        "Admissions refused, by reason (budget refusals included).",
        Kind::Counter,
    );
    for (reason, n) in [
        ("full", s.rejected_at_capacity),
        ("overloaded", s.rejected_overloaded),
        ("provider_budget", s.rejected_provider_budget),
        ("shutting_down", s.rejected_shutting_down),
    ] {
        p.sample(
            "brisvox_session_admissions_rejected_total",
            &[("reason", reason)],
            n,
        );
    }
}

/// The Smart Turn executor's own pool. Admission reads only the VAD's pool, so an
/// overloaded executor (analyses refused or late: every affected turn ends at its fallback
/// deadline) is visible only here: alert when the counters rise.
fn render_smart_turn(p: &mut Prom, pool: &PoolMetrics) {
    p.family(
        "brisvox_smart_turn_queue_depth",
        "Smart Turn analyses waiting for an executor thread now.",
        Kind::Gauge,
    );
    p.sample("brisvox_smart_turn_queue_depth", &[], pool.queue_depth);
    p.family(
        "brisvox_smart_turn_rejected_total",
        "Smart Turn analyses refused because the executor's queue was full (the turn ends at its fallback deadline).",
        Kind::Counter,
    );
    p.sample("brisvox_smart_turn_rejected_total", &[], pool.rejected);
    p.family(
        "brisvox_smart_turn_timed_out_total",
        "Smart Turn analyses whose verdict came too late (the turn ends at its fallback deadline).",
        Kind::Counter,
    );
    p.sample("brisvox_smart_turn_timed_out_total", &[], pool.timed_out);
}

fn render_admission(p: &mut Prom, manager: &SessionManager) {
    let snap = manager.admission_snapshot();
    if let Some(pressure) = &snap.pressure {
        p.family(
            "brisvox_pool_overloaded",
            "1 while admission refuses new calls for inference pool pressure.",
            Kind::Gauge,
        );
        p.sample(
            "brisvox_pool_overloaded",
            &[],
            u8::from(pressure.overloaded),
        );
        p.family(
            "brisvox_pool_queue_depth",
            "Jobs queued on the inference pool at the last sample.",
            Kind::Gauge,
        );
        p.sample("brisvox_pool_queue_depth", &[], pressure.queue_depth);
        p.family(
            "brisvox_pool_mean_wait_seconds",
            "Mean queue wait of the last full window of pool jobs.",
            Kind::Gauge,
        );
        p.sample(
            "brisvox_pool_mean_wait_seconds",
            &[],
            pressure.mean_wait.as_secs_f64(),
        );
        p.family(
            "brisvox_pool_overload_entered_total",
            "Times the server turned overloaded.",
            Kind::Counter,
        );
        p.sample("brisvox_pool_overload_entered_total", &[], pressure.entered);
    }
    if !snap.providers.is_empty() {
        p.family(
            "brisvox_provider_concurrency_limit",
            "Concurrent calls a provider may serve.",
            Kind::Gauge,
        );
        for provider in &snap.providers {
            p.sample(
                "brisvox_provider_concurrency_limit",
                &[("provider", &provider.name)],
                provider.limit,
            );
        }
        p.family(
            "brisvox_provider_concurrency_in_use",
            "Calls holding a permit of the provider now.",
            Kind::Gauge,
        );
        for provider in &snap.providers {
            p.sample(
                "brisvox_provider_concurrency_in_use",
                &[("provider", &provider.name)],
                provider.in_use,
            );
        }
        p.family(
            "brisvox_provider_budget_refusals_total",
            "Admissions refused because the provider was at its concurrency limit.",
            Kind::Counter,
        );
        for provider in &snap.providers {
            p.sample(
                "brisvox_provider_budget_refusals_total",
                &[("provider", &provider.name)],
                provider.rejected,
            );
        }
    }
}
