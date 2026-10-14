//! The engine's per-stage times in sim mode: what the engine's own turn timeline
//! ([`TimelineRecorder`](brisvox_core::TimelineRecorder)) recorded for every turn of every
//! call the [`EngineServer`](super::engine_target::EngineServer) served, next to the V2V the
//! client measured, and the process's resident memory per call.
//!
//! Each [`Stage`] is the time between two [`TurnEvent`]s of one turn, stamped by the engine on
//! its own clock where the work happens (see `docs/LATENCY.md`, "Engine stage times"):
//!
//! | stage | from | to | what it is |
//! |---|---|---|---|
//! | `vad_stop_to_finalize` | `VadStop` | `SttFinalize` | the VAD worker's decision reaching the STT provider |
//! | `turn_end_to_llm_request` | `TurnEnd` | `LlmRequest` | the gated end of turn to the request handed to the LLM (budget) |
//! | `llm_request_to_first_token` | `LlmRequest` | `LlmFirstToken` | the mock LLM's first token (provider time: shows the timer's precision) |
//! | `first_token_to_tts_text` | `LlmFirstToken` | `TtsRequest` | the segmenter: first token to the first text handed to the TTS |
//! | `first_chunk_to_tts_text` | `FirstChunk` | `TtsFirstText` | the hop of the first chunk from the segmenter to the TTS stage |
//! | `tts_request_to_first_byte` | `TtsRequest` | `TtsFirstByte` | the mock TTS's first audio (provider time) |
//! | `tts_audio_to_transport` | `TtsFirstByte` | `FirstAudioOut` | the first TTS audio to the transport command (budget) |
//! | `interrupt_to_clear` | `Interrupted` | `AudioOutStopped` | a confirmed barge-in to the applied transport `Clear` (budget) |
//! | `turn_end_to_first_audio` | `TurnEnd` | `FirstAudioOut` | everything after the end of turn, providers included |
//!
//! Some spans contain provider time by construction; [`StageTimes::provider`] states how much,
//! so the engine's share is the rest. The two provider spans show how precisely the mocks' fixed
//! latencies are kept: a sleep overshoots by the platform's timer precision (Windows: up to one
//! ~15.6 ms tick), and that overshoot is not the engine's. `first_token_to_tts_text` holds no
//! provider time: the mock answers in one token whose first clause (`Okay,`) is a first chunk of
//! its own, decided inside the token.
//!
//! The engine-overhead budget of `docs/LATENCY.md` applies at p95 to the three marked stages:
//! [`BUDGET_INTO_LLM`], [`BUDGET_OUT_OF_TTS`] and [`BUDGET_CLEAR`].
//!
//! Turns of the latency calls give every stage but `interrupt_to_clear`; the barge-in calls
//! (where the caller talks over a long answer on purpose) give only `interrupt_to_clear`, so
//! their different answer shape cannot shift the other stages; the memory calls (several at
//! once) give none.
//!
//! Memory ([`RssReport`]) is measured on calls played at once: with one call at a time, a call
//! reuses what the previous one freed and the growth says nothing about a call's footprint.

use std::time::Duration;

use brisvox_core::{TurnEvent, TurnRecord};
use brisvox_engine::SpeculationStatsSnapshot;
use serde::{Deserialize, Serialize};

use super::engine_target::EngineTargetSettings;
use super::report::{CallKind, Dist};
use crate::stats::ms;

/// Budget at p95 of `turn_end_to_llm_request`: the engine's time into the LLM.
pub const BUDGET_INTO_LLM: Duration = Duration::from_millis(3);
/// Budget at p95 of `tts_audio_to_transport`: the engine's time out of the TTS.
pub const BUDGET_OUT_OF_TTS: Duration = Duration::from_millis(2);
/// Budget at p95 of `interrupt_to_clear`: a confirmed barge-in to the transport's `Clear`.
pub const BUDGET_CLEAR: Duration = Duration::from_millis(1);
/// Samples kept per stage; later ones are counted in [`StageReport::dropped`].
pub const MAX_SAMPLES: usize = 100_000;

/// One engine stage: the span between two timeline events of a turn (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// `VadStop` to `SttFinalize`.
    VadStopToFinalize,
    /// `TurnEnd` to `LlmRequest`.
    TurnEndToLlmRequest,
    /// `LlmRequest` to `LlmFirstToken` (provider time).
    LlmRequestToFirstToken,
    /// `LlmFirstToken` to `TtsRequest`.
    FirstTokenToTtsText,
    /// `FirstChunk` to `TtsFirstText`: the hop between the segmenter and the TTS stage.
    FirstChunkToTtsText,
    /// `TtsRequest` to `TtsFirstByte` (provider time).
    TtsRequestToFirstByte,
    /// `TtsFirstByte` to `FirstAudioOut`.
    TtsAudioToTransport,
    /// `Interrupted` to `AudioOutStopped`.
    InterruptToClear,
    /// `TurnEnd` to `FirstAudioOut`.
    TurnEndToFirstAudio,
}

impl Stage {
    /// Number of stages.
    pub const COUNT: usize = 9;

    /// Every stage, in report order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::VadStopToFinalize,
        Self::TurnEndToLlmRequest,
        Self::LlmRequestToFirstToken,
        Self::FirstTokenToTtsText,
        Self::FirstChunkToTtsText,
        Self::TtsRequestToFirstByte,
        Self::TtsAudioToTransport,
        Self::InterruptToClear,
        Self::TurnEndToFirstAudio,
    ];

    /// The stage's name in reports.
    pub const fn name(self) -> &'static str {
        match self {
            Self::VadStopToFinalize => "vad_stop_to_finalize",
            Self::TurnEndToLlmRequest => "turn_end_to_llm_request",
            Self::LlmRequestToFirstToken => "llm_request_to_first_token",
            Self::FirstTokenToTtsText => "first_token_to_tts_text",
            Self::FirstChunkToTtsText => "first_chunk_to_tts_text",
            Self::TtsRequestToFirstByte => "tts_request_to_first_byte",
            Self::TtsAudioToTransport => "tts_audio_to_transport",
            Self::InterruptToClear => "interrupt_to_clear",
            Self::TurnEndToFirstAudio => "turn_end_to_first_audio",
        }
    }

    /// The two events the stage spans.
    pub const fn span(self) -> (TurnEvent, TurnEvent) {
        use TurnEvent as E;
        match self {
            Self::VadStopToFinalize => (E::VadStop, E::SttFinalize),
            Self::TurnEndToLlmRequest => (E::TurnEnd, E::LlmRequest),
            Self::LlmRequestToFirstToken => (E::LlmRequest, E::LlmFirstToken),
            Self::FirstTokenToTtsText => (E::LlmFirstToken, E::TtsRequest),
            Self::FirstChunkToTtsText => (E::FirstChunk, E::TtsFirstText),
            Self::TtsRequestToFirstByte => (E::TtsRequest, E::TtsFirstByte),
            Self::TtsAudioToTransport => (E::TtsFirstByte, E::FirstAudioOut),
            Self::InterruptToClear => (E::Interrupted, E::AudioOutStopped),
            Self::TurnEndToFirstAudio => (E::TurnEnd, E::FirstAudioOut),
        }
    }

    /// The engine-overhead budget at p95, for the stages that have one.
    pub const fn budget(self) -> Option<Duration> {
        match self {
            Self::TurnEndToLlmRequest => Some(BUDGET_INTO_LLM),
            Self::TtsAudioToTransport => Some(BUDGET_OUT_OF_TTS),
            Self::InterruptToClear => Some(BUDGET_CLEAR),
            _ => None,
        }
    }

    /// Mock provider time inside the span under `settings` (see the module docs).
    pub fn provider(self, settings: &EngineTargetSettings) -> Duration {
        let ms = Duration::from_millis;
        match self {
            Self::LlmRequestToFirstToken => ms(settings.llm_first_token_ms),
            Self::TtsRequestToFirstByte => ms(settings.tts_first_byte_ms),
            Self::TurnEndToFirstAudio => ms(settings
                .llm_first_token_ms
                .saturating_add(settings.tts_first_byte_ms)),
            _ => Duration::ZERO,
        }
    }

    /// Whether turns of calls of `kind` feed this stage (see the module docs).
    pub const fn fed_by(self, kind: CallKind) -> bool {
        match kind {
            CallKind::Latency => !matches!(self, Self::InterruptToClear),
            CallKind::BargeIn => matches!(self, Self::InterruptToClear),
            CallKind::Memory => false,
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// The stage samples of the calls served so far (bounded by [`MAX_SAMPLES`] per stage).
#[derive(Debug, Clone, Default)]
pub struct StageCollector {
    samples: [Vec<Duration>; Stage::COUNT],
    turns: usize,
    interrupts: usize,
    dropped: u64,
    speculation: SpeculationReport,
}

/// Speculative generation over the latency calls of a benchmark: the engine's own
/// counters, summed. Barge-in and memory calls are left out (their long answers are abandoned by
/// design, which is no hit or miss of the speculation).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeculationReport {
    /// Latency calls summed.
    pub calls: u64,
    /// Speculative LLM runs started.
    pub started: u64,
    /// Speculations committed: the turn ended with the same text.
    pub hits: u64,
    /// Discarded because the turn ended with other text.
    pub misses: u64,
    /// Replaced by a newer candidate before the turn ended.
    pub superseded: u64,
    /// Discarded because the caller went on speaking.
    pub withdrawn: u64,
    /// Discarded because the turn went away (barge-in, new turn, stop).
    pub abandoned: u64,
    /// Discarded for any other reason (expired, context changed).
    pub other_discarded: u64,
    /// Candidates identical to the speculation already running.
    pub kept: u64,
    /// Candidates ignored because the runner was still answering.
    pub skipped_busy: u64,
    /// Speculative runs whose stream failed.
    pub failures: u64,
    /// Output tokens of discarded speculations (estimated from bytes when the provider reports
    /// none).
    pub wasted_output_tokens: u64,
    /// Input tokens of discarded speculations (only what the provider reported).
    pub wasted_input_tokens: u64,
    /// Milliseconds the committed speculations were ahead of the turn end, summed.
    pub head_start_ms: u64,
}

impl SpeculationReport {
    /// Speculations decided (committed or discarded).
    pub const fn decided(&self) -> u64 {
        self.hits
            .saturating_add(self.misses)
            .saturating_add(self.superseded)
            .saturating_add(self.withdrawn)
            .saturating_add(self.abandoned)
            .saturating_add(self.other_discarded)
    }

    /// Committed speculations over decided ones; `None` before the first decision.
    #[allow(clippy::cast_precision_loss)] // counters far below 2^52
    pub fn hit_rate(&self) -> Option<f64> {
        let decided = self.decided();
        (decided > 0).then(|| self.hits as f64 / decided as f64)
    }

    /// Mean head start of a committed speculation, in milliseconds.
    #[allow(clippy::cast_precision_loss)] // counters far below 2^52
    pub fn mean_head_start_ms(&self) -> Option<f64> {
        (self.hits > 0).then(|| self.head_start_ms as f64 / self.hits as f64)
    }

    /// Adds the counters of one call.
    pub fn add(&mut self, s: &SpeculationStatsSnapshot) {
        let sum = |a: u64, b: u64| a.saturating_add(b);
        self.calls = sum(self.calls, 1);
        self.started = sum(self.started, s.started);
        self.hits = sum(self.hits, s.hits);
        self.misses = sum(self.misses, s.misses);
        self.superseded = sum(self.superseded, s.superseded);
        self.withdrawn = sum(self.withdrawn, s.withdrawn);
        self.abandoned = sum(self.abandoned, s.abandoned);
        self.other_discarded = sum(self.other_discarded, sum(s.expired, s.context_changes));
        self.kept = sum(self.kept, s.kept);
        self.skipped_busy = sum(self.skipped_busy, s.skipped_busy);
        self.failures = sum(self.failures, s.failures);
        self.wasted_output_tokens = sum(self.wasted_output_tokens, s.wasted_output_tokens);
        self.wasted_input_tokens = sum(self.wasted_input_tokens, s.wasted_input_tokens);
        self.head_start_ms = sum(self.head_start_ms, s.head_start_ms);
    }

    /// The markdown lines.
    pub fn markdown(&self, out: &mut String) {
        use std::fmt::Write as _;
        let rate = self
            .hit_rate()
            .map_or_else(|| "-".to_owned(), |r| format!("{:.1}%", r * 100.0));
        let head = self
            .mean_head_start_ms()
            .map_or_else(|| "-".to_owned(), |h| format!("{h:.0} ms"));
        let _ = writeln!(
            out,
            "\n## Speculation\n\nSpeculative generation was on. Over {} latency calls: {} \
             speculative LLM runs started, {} committed (hit rate {rate}, mean head start {head}), \
             {} missed, {} superseded, {} withdrawn, {} abandoned, {} other; {} failed. Wasted: \
             {} output tokens (estimated from the streamed bytes), {} input tokens as reported by \
             the provider (a discarded run reports none: a provider bills its prompt anyway).",
            self.calls,
            self.started,
            self.hits,
            self.misses,
            self.superseded,
            self.withdrawn,
            self.abandoned,
            self.other_discarded,
            self.failures,
            self.wasted_output_tokens,
            self.wasted_input_tokens,
        );
    }
}

impl StageCollector {
    /// Adds the speculation counters of one finished call of `kind` (latency calls only).
    pub fn add_speculation(&mut self, stats: &SpeculationStatsSnapshot, kind: CallKind) {
        if kind == CallKind::Latency {
            self.speculation.add(stats);
        }
    }

    /// Adds the stages of one finished turn of a call of `kind` (nothing for memory calls).
    pub fn add(&mut self, record: &TurnRecord, kind: CallKind) {
        if kind == CallKind::Memory {
            return;
        }
        self.turns = self.turns.saturating_add(1);
        if record.at(TurnEvent::Interrupted).is_some() {
            self.interrupts = self.interrupts.saturating_add(1);
        }
        for stage in Stage::ALL {
            if !stage.fed_by(kind) {
                continue;
            }
            let (from, to) = stage.span();
            let Some(span) = record.between(from, to) else {
                continue;
            };
            let Some(samples) = self.samples.get_mut(stage.index()) else {
                continue;
            };
            if samples.len() < MAX_SAMPLES {
                samples.push(span);
            } else {
                self.dropped = self.dropped.saturating_add(1);
            }
        }
    }

    /// The report of what was collected; `settings` gives the provider time of each span.
    pub fn report(&self, settings: &EngineTargetSettings) -> StageReport {
        let stages = Stage::ALL
            .into_iter()
            .map(|stage| {
                let (from, to) = stage.span();
                let dist = Dist::of(self.samples.get(stage.index()).cloned().unwrap_or_default());
                let budget = stage.budget();
                StageTimes {
                    name: stage.name().to_owned(),
                    from: from.name().to_owned(),
                    to: to.name().to_owned(),
                    provider: stage.provider(settings),
                    budget,
                    within_budget: budget
                        .filter(|_| dist.count > 0)
                        .map(|budget| dist.p95 <= budget),
                    dist,
                }
            })
            .collect();
        StageReport {
            stages,
            turns: self.turns,
            interrupts: self.interrupts,
            dropped: self.dropped,
            rss: None,
            speculation: settings.speculation.then_some(self.speculation),
        }
    }
}

/// One stage's times over every turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageTimes {
    /// The [`Stage::name`].
    pub name: String,
    /// The timeline event the span starts at.
    pub from: String,
    /// The timeline event the span ends at.
    pub to: String,
    /// Mock provider time inside the span ([`Stage::provider`]): the engine's share is the rest.
    #[serde(rename = "provider_us", with = "super::measure::micros")]
    pub provider: Duration,
    /// The engine-overhead budget at p95, if the stage has one.
    #[serde(rename = "budget_p95_us", with = "super::measure::micros::opt")]
    pub budget: Option<Duration>,
    /// Whether p95 is within the budget (`None` without a budget or without samples).
    pub within_budget: Option<bool>,
    /// The distribution.
    #[serde(flatten)]
    pub dist: Dist,
}

/// Resident memory of the benchmark process (engine, transport and harness share it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RssReport {
    /// After the engine server started (inference pool, listener), before the first call.
    pub cold_bytes: u64,
    /// Idle again after the latency and barge-in calls, right before the memory calls: the
    /// server warm (lazily built tables, allocator pools), no call alive.
    pub warm_bytes: u64,
    /// The largest reading while the memory calls ran (sampled every 50 ms).
    pub peak_bytes: u64,
    /// The memory calls played at once.
    pub calls_at_once: usize,
}

impl RssReport {
    /// Peak growth over the warm idle server, per call alive at once.
    pub fn per_call_bytes(&self) -> u64 {
        let calls = u64::try_from(self.calls_at_once.max(1)).unwrap_or(1);
        self.peak_bytes.saturating_sub(self.warm_bytes) / calls
    }

    /// What the first calls left behind for good (warm idle over cold idle).
    pub fn warm_up_bytes(&self) -> u64 {
        self.warm_bytes.saturating_sub(self.cold_bytes)
    }
}

/// The engine's stage times and memory over a benchmark (sim mode against the engine).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageReport {
    /// Every [`Stage`], in order.
    pub stages: Vec<StageTimes>,
    /// Turns recorded by the engine (latency and barge-in calls).
    pub turns: usize,
    /// Of those, the turns a barge-in interrupted.
    pub interrupts: usize,
    /// Samples beyond [`MAX_SAMPLES`] of a stage, not kept.
    pub dropped: u64,
    /// Resident memory, if the platform reports it.
    pub rss: Option<RssReport>,
    /// Speculative generation, when it was on.
    #[serde(default)]
    pub speculation: Option<SpeculationReport>,
}

impl StageReport {
    /// The stages over their budget (name, p95, budget).
    pub fn over_budget(&self) -> Vec<(&str, Duration, Duration)> {
        self.stages
            .iter()
            .filter(|s| s.within_budget == Some(false))
            .filter_map(|s| Some((s.name.as_str(), s.dist.p95, s.budget?)))
            .collect()
    }

    /// The markdown section.
    pub fn markdown(&self, out: &mut String) {
        use std::fmt::Write as _;
        let _ = writeln!(
            out,
            "\n## Engine stages\n\nThe engine's own turn timeline (`TimelineRecorder`, stamped \
             where the work happens) over {} turns, {} of them interrupted by a barge-in. \
             `provider` is mock provider time inside the span; the engine's share is the rest. \
             The budget applies at p95 (docs/LATENCY.md).\n\n| stage | from -> to | n | p50 ms \
             | p95 ms | p99 ms | max ms | provider ms | budget p95 ms | |\n|---|---|---:|---:|\
             ---:|---:|---:|---:|---:|---|",
            self.turns, self.interrupts
        );
        for s in &self.stages {
            let d = &s.dist;
            let value = |v: Duration| if d.count == 0 { "-".to_owned() } else { ms(v) };
            let verdict = match s.within_budget {
                Some(true) => "met",
                Some(false) => "**missed**",
                None => "",
            };
            let _ = writeln!(
                out,
                "| {} | {} -> {} | {} | {} | {} | {} | {} | {} | {} | {verdict} |",
                s.name,
                s.from,
                s.to,
                d.count,
                value(d.p50),
                value(d.p95),
                value(d.p99),
                value(d.max),
                ms(s.provider),
                s.budget.map_or_else(|| "-".to_owned(), ms),
            );
        }
        if self.dropped > 0 {
            let _ = writeln!(
                out,
                "\n{} samples over the cap were not kept.",
                self.dropped
            );
        }
        if let Some(speculation) = &self.speculation {
            speculation.markdown(out);
        }
        let mib = |b: u64| format!("{:.1}", b as f64 / (1024.0 * 1024.0));
        match &self.rss {
            Some(rss) => {
                let _ = writeln!(
                    out,
                    "\n**Memory** (resident set of the whole process: engine, transport and \
                     harness): {} MiB with the server idle and cold, {} MiB idle and warm after \
                     the calls above (+{} MiB of one-time warm-up), peak {} MiB with {} memory \
                     calls alive at once. **RSS per call {} MiB** (peak over warm idle, per \
                     call).",
                    mib(rss.cold_bytes),
                    mib(rss.warm_bytes),
                    mib(rss.warm_up_bytes()),
                    mib(rss.peak_bytes),
                    rss.calls_at_once,
                    mib(rss.per_call_bytes()),
                );
            }
            None => {
                let _ = writeln!(
                    out,
                    "\nMemory: not measured (no memory calls, or not reported by this platform)."
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use brisvox_core::{Clock, SimClock, TimelineRecorder, TurnEpoch};

    use super::*;
    use crate::v2v::engine_target::EngineTargetConfig;

    const MS: Duration = Duration::from_millis(1);

    fn turn(rec: &TimelineRecorder, epoch: u32, interrupted: bool) {
        use TurnEvent as E;
        let t0 = SimClock::new().now();
        let e = TurnEpoch::new(epoch);
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        rec.mark(e, E::UserSpeechStart, at(0));
        rec.mark_open_turn(E::VadStop, at(1_300));
        rec.mark_open_turn(E::SttFinalize, at(1_301));
        rec.mark(e, E::TurnEnd, at(1_450));
        rec.mark(e, E::LlmRequest, at(1_452));
        rec.mark(e, E::LlmFirstToken, at(1_802));
        rec.mark(e, E::FirstChunk, at(1_832));
        rec.mark(e, E::TtsFirstText, at(1_833));
        rec.mark(e, E::TtsRequest, at(1_833));
        rec.mark(e, E::TtsFirstByte, at(1_983));
        rec.mark(e, E::FirstAudioOut, at(1_984));
        if interrupted {
            rec.interrupt(e, TurnEpoch::new(epoch + 1), at(2_500), Some(at(2_100)));
            rec.mark(e, E::AudioOutStopped, at(2_502));
        }
    }

    #[test]
    fn each_stage_is_its_span_and_the_budget_is_checked_at_p95() {
        let rec = TimelineRecorder::new();
        turn(&rec, 0, false);
        turn(&rec, 1, true);
        rec.finish();
        let mut collector = StageCollector::default();
        let records = rec.drain_finished();
        collector.add(&records[0], CallKind::Latency);
        // The interrupted turn of a barge-in call gives only the clear.
        collector.add(&records[1], CallKind::BargeIn);
        // A memory call gives nothing.
        collector.add(&records[0], CallKind::Memory);
        let settings = EngineTargetConfig::default().settings();
        let report = collector.report(&settings);
        assert_eq!((report.turns, report.interrupts), (2, 1));
        let by = |name: &str| report.stages.iter().find(|s| s.name == name).unwrap();
        assert_eq!(by("vad_stop_to_finalize").dist.p95, MS);
        assert_eq!(by("turn_end_to_llm_request").dist.p95, 2 * MS);
        assert_eq!(by("turn_end_to_llm_request").within_budget, Some(true));
        assert_eq!(by("first_token_to_tts_text").dist.p50, 31 * MS);
        assert_eq!(by("first_token_to_tts_text").provider, Duration::ZERO);
        assert_eq!(by("first_chunk_to_tts_text").dist.p50, MS);
        assert_eq!(by("first_chunk_to_tts_text").provider, Duration::ZERO);
        assert_eq!(by("llm_request_to_first_token").dist.p50, 350 * MS);
        assert_eq!(by("llm_request_to_first_token").provider, 350 * MS);
        assert_eq!(by("tts_request_to_first_byte").dist.p50, 150 * MS);
        assert_eq!(by("tts_request_to_first_byte").within_budget, None);
        assert_eq!(by("tts_audio_to_transport").within_budget, Some(true));
        let clear = by("interrupt_to_clear");
        assert_eq!((clear.dist.count, clear.dist.p95), (1, 2 * MS));
        assert_eq!(clear.within_budget, Some(false));
        assert_eq!(by("turn_end_to_first_audio").dist.count, 1);
        assert_eq!(by("turn_end_to_first_audio").provider, 500 * MS);
        assert_eq!(
            report.over_budget(),
            vec![("interrupt_to_clear", 2 * MS, BUDGET_CLEAR)]
        );
        let mut md = String::new();
        report.markdown(&mut md);
        assert!(
            md.contains("| interrupt_to_clear | interrupted -> audio_out_stopped | 1 |"),
            "{md}"
        );
        assert!(md.contains("**missed**"), "{md}");
    }

    #[test]
    fn rss_per_call_is_the_peak_over_the_warm_idle_server_per_call() {
        let rss = RssReport {
            cold_bytes: 10 << 20,
            warm_bytes: 25 << 20,
            peak_bytes: 33 << 20,
            calls_at_once: 8,
        };
        assert_eq!(rss.per_call_bytes(), 1 << 20);
        assert_eq!(rss.warm_up_bytes(), 15 << 20);
    }

    #[test]
    fn speculation_counts_latency_calls_only_and_is_reported_only_when_on() {
        let call = SpeculationStatsSnapshot {
            started: 3,
            hits: 2,
            misses: 1,
            superseded: 0,
            expired: 1,
            wasted_output_tokens: 7,
            head_start_ms: 300,
            ..SpeculationStatsSnapshot::default()
        };
        let mut collector = StageCollector::default();
        collector.add_speculation(&call, CallKind::Latency);
        collector.add_speculation(&call, CallKind::Latency);
        // Barge-in calls abandon their long answers by design; memory calls are not measured.
        collector.add_speculation(&call, CallKind::BargeIn);
        collector.add_speculation(&call, CallKind::Memory);

        let off = EngineTargetConfig::default().settings();
        assert_eq!(collector.report(&off).speculation, None);

        let on = EngineTargetConfig {
            speculation: true,
            ..EngineTargetConfig::default()
        }
        .settings();
        let report = collector.report(&on).speculation.unwrap();
        assert_eq!(report.calls, 2);
        assert_eq!((report.started, report.hits, report.misses), (6, 4, 2));
        assert_eq!(report.other_discarded, 2, "expired and context changes");
        assert_eq!(report.wasted_output_tokens, 14);
        // Decided: 4 committed + 2 missed + 2 expired.
        assert_eq!(report.decided(), 8);
        assert_eq!(report.hit_rate(), Some(0.5));
        assert_eq!(report.mean_head_start_ms(), Some(150.0));
        assert_eq!(SpeculationReport::default().hit_rate(), None);

        let mut md = String::new();
        collector.report(&on).markdown(&mut md);
        assert!(md.contains("## Speculation"), "{md}");
        assert!(
            md.contains("hit rate 50.0%, mean head start 150 ms"),
            "{md}"
        );
    }
}
