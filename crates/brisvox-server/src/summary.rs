//! The per-call summary: how long the call ran, its turns and latencies, and the provider units it
//! used (LLM tokens, STT audio, TTS characters). One function builds it ([`build_summary`]); the
//! same [`CallSummary`] is then delivered four ways: the agent link's `session.usage`, the
//! `summary` of the transcript's `end` line, one `call summary` log line and the usage metrics
//! (`docs/OPERATIONS.md`, "Usage and cost").
//!
//! The latency numbers are the per-turn stage latencies the server already computes for
//! `turn.metrics`, the turn traces and the stage histograms ([`Stage::of`]); the units are the
//! engine's per-call counters ([`CallStats`]). Provider and model are the agent definition's
//! configured primaries ([`ProviderModels`]).
//!
//! # Cost
//! A finished turn costs one lock and a few additions on the stage task that finished it, once
//! per turn; the summary itself is built once per call, at its end.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use brisvox_agent_link::protocol::StageLatency;
use brisvox_agent_link::protocol::{
    CallSummary, LatencySummary, LlmUsage, SttUsage, TtsUsage, UsageSummary,
};
use brisvox_core::{SharedClock, TimelineRecorder, TimelineSink, TurnEpoch, TurnRecord};
use brisvox_engine::session::CallStats;
use brisvox_engine::{LlmStatsSnapshot, SttStatsSnapshot, TtsStatsSnapshot};
use tokio::time::Instant;

use crate::config::ProviderModels;
use crate::observability::Stage;
use crate::observability::registry::turn_has_marks;

/// The stages of the summary's `latency`, in the order of [`TurnTotals::stages`].
const SUMMARY_STAGES: [Stage; 5] = [
    Stage::EndOfTurn,
    Stage::SttFinal,
    Stage::LlmFirstToken,
    Stage::TtsFirstByte,
    Stage::Response,
];

/// Count, sum and worst of one stage over a call's turns, in microseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct StageTotal {
    count: u64,
    sum_us: u64,
    max_us: u64,
}

impl StageTotal {
    fn add(&mut self, value: Duration) {
        let us = u64::try_from(value.as_micros()).unwrap_or(u64::MAX);
        self.count = self.count.saturating_add(1);
        self.sum_us = self.sum_us.saturating_add(us);
        self.max_us = self.max_us.max(us);
    }

    /// Average and worst in milliseconds, rounded to 0.1 ms; `None` without a sample.
    fn latency(&self) -> Option<StageLatency> {
        if self.count == 0 {
            return None;
        }
        let tenths_ms = |us: u64| {
            let tenths = us.saturating_add(50) / 100;
            f64::from(u32::try_from(tenths).unwrap_or(u32::MAX)) / 10.0
        };
        Some(StageLatency {
            avg_ms: tenths_ms(self.sum_us / self.count),
            max_ms: tenths_ms(self.max_us),
        })
    }
}

/// What a call's finished turns add up to: the turn counts and the latency totals of the
/// summary's stages. Fixed size, whatever the length of the call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnTotals {
    turns: u64,
    interrupted: u64,
    /// [`SUMMARY_STAGES`], in that order.
    stages: [StageTotal; SUMMARY_STAGES.len()],
    /// The newest epoch whose barge-in is already counted (a barge-in recorded after the turn
    /// finished counts once, whichever way it arrives).
    interruption_counted: Option<TurnEpoch>,
}

impl TurnTotals {
    /// Adds a turn that just finished, by the rules of `Metrics::observe_turn` (a turn nothing
    /// was marked on does not count).
    pub fn turn_finished(&mut self, turn: &TurnRecord) {
        if !turn_has_marks(turn) {
            return;
        }
        for (total, stage) in self.stages.iter_mut().zip(SUMMARY_STAGES) {
            if let Some(d) = stage.of(turn) {
                total.add(d);
            }
        }
        self.turns = self.turns.saturating_add(1);
        if turn.at(brisvox_core::TurnEvent::Interrupted).is_some() {
            self.interrupted = self.interrupted.saturating_add(1);
            self.interruption_counted = Some(turn.epoch());
        }
    }

    /// A finished turn gained a barge-in afterwards: it counts as interrupted (once).
    pub fn turn_patched(&mut self, turn: &TurnRecord) {
        let epoch = turn.epoch();
        if turn.at(brisvox_core::TurnEvent::Interrupted).is_some()
            && self.interruption_counted != Some(epoch)
        {
            self.interruption_counted = Some(epoch);
            self.interrupted = self.interrupted.saturating_add(1);
        }
    }

    /// Turns that finished.
    pub const fn turns(&self) -> u64 {
        self.turns
    }

    /// Turns that were interrupted.
    pub const fn interrupted(&self) -> u64 {
        self.interrupted
    }
}

/// Builds the summary of a call that ran for `duration`: the turn totals, and the billing
/// counters of the three provider stages named by `names` (a stage `names` leaves out, as for a
/// speech-to-speech call, is left out of the usage).
pub fn build_summary(
    duration: Duration,
    totals: &TurnTotals,
    names: &ProviderModels,
    llm: &LlmStatsSnapshot,
    stt: &SttStatsSnapshot,
    tts: &TtsStatsSnapshot,
) -> CallSummary {
    let latency_of = |stage: Stage| {
        SUMMARY_STAGES
            .iter()
            .position(|s| *s == stage)
            .and_then(|i| totals.stages.get(i))
            .and_then(StageTotal::latency)
    };
    CallSummary {
        duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        turns: totals.turns,
        interrupted_turns: totals.interrupted,
        latency: LatencySummary {
            end_of_turn: latency_of(Stage::EndOfTurn),
            stt_final: latency_of(Stage::SttFinal),
            llm_ttft: latency_of(Stage::LlmFirstToken),
            tts_ttfb: latency_of(Stage::TtsFirstByte),
            response: latency_of(Stage::Response),
        },
        usage: UsageSummary {
            llm: names.llm.as_ref().map(|name| LlmUsage {
                provider: name.provider.to_owned(),
                model: name.model.clone(),
                input_tokens: llm.input_tokens,
                cached_input_tokens: llm.cached_input_tokens,
                output_tokens: llm.output_tokens,
                responses_without_usage: llm.responses_without_usage,
            }),
            stt: names.stt.as_ref().map(|name| SttUsage {
                provider: name.provider.to_owned(),
                model: name.model.clone(),
                audio_seconds: Duration::from_millis(stt.audio_ms_sent).as_secs_f64(),
            }),
            tts: names.tts.as_ref().map(|name| TtsUsage {
                provider: name.provider.to_owned(),
                model: name.model.clone(),
                characters: tts.chars_sent,
            }),
        },
    }
}

/// Collects a call's turn totals and builds its summary once, at the end.
///
/// It is the [`TimelineSink`] of the call's recorder ([`Self::recorder`]) and passes every turn
/// on to the call's observer (`inner`), so it works with or without metrics.
pub struct CallSummarizer {
    clock: SharedClock,
    started: Instant,
    session: u64,
    agent: Arc<str>,
    names: ProviderModels,
    totals: Mutex<TurnTotals>,
    inner: Option<Arc<dyn TimelineSink>>,
    summary: OnceLock<Arc<CallSummary>>,
}

impl std::fmt::Debug for CallSummarizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallSummarizer")
            .field("session", &self.session)
            .field("agent", &self.agent)
            .finish_non_exhaustive()
    }
}

impl CallSummarizer {
    /// A summarizer for call `session` of `agent`, whose clock `clock` marks the start now.
    /// Turns are passed on to `inner` (the call's observer), if any.
    pub fn new(
        clock: SharedClock,
        session: u64,
        agent: &str,
        names: ProviderModels,
        inner: Option<Arc<dyn TimelineSink>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            started: clock.now(),
            clock,
            session,
            agent: Arc::from(agent),
            names,
            totals: Mutex::new(TurnTotals::default()),
            inner,
            summary: OnceLock::new(),
        })
    }

    /// A timeline recorder that reports to this summarizer (and through it to the observer).
    pub fn recorder(self: &Arc<Self>) -> TimelineRecorder {
        TimelineRecorder::with_sink(Arc::clone(self) as Arc<dyn TimelineSink>)
    }

    /// The totals of the turns finished so far.
    pub fn totals(&self) -> TurnTotals {
        *self.lock()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TurnTotals> {
        self.totals.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The summary of the call, built from `stats` when this is first asked (call it after the
    /// call stopped and its recorder was finished) and logged once as `call summary`. Later
    /// calls return the same summary, so the delivery paths agree to the digit.
    pub fn summary(&self, stats: &CallStats) -> Arc<CallSummary> {
        Arc::clone(self.summary.get_or_init(|| {
            let totals = self.totals();
            let duration = self.clock.now().saturating_duration_since(self.started);
            let s = &stats.stages;
            let summary = build_summary(duration, &totals, &self.names, &s.llm, &s.stt, &s.tts);
            log_summary(self.session, &self.agent, &summary);
            Arc::new(summary)
        }))
    }
}

impl TimelineSink for CallSummarizer {
    fn turn_finished(&self, record: &TurnRecord) {
        self.lock().turn_finished(record);
        if let Some(inner) = &self.inner {
            inner.turn_finished(record);
        }
    }

    fn turn_patched(&self, record: &TurnRecord) {
        self.lock().turn_patched(record);
        if let Some(inner) = &self.inner {
            inner.turn_patched(record);
        }
    }
}

/// Writes the numbers of `summary` as one `info` event on `brisvox::server`: counts, latencies
/// and units only, no text of the conversation and nothing about the caller.
pub(crate) fn log_summary(session: u64, agent: &str, summary: &CallSummary) {
    let l = &summary.latency;
    let avg = |s: &Option<StageLatency>| s.map(|s| s.avg_ms);
    let max = |s: &Option<StageLatency>| s.map(|s| s.max_ms);
    let u = &summary.usage;
    tracing::info!(
        target: "brisvox::server",
        session,
        agent,
        duration_ms = summary.duration_ms,
        turns = summary.turns,
        interrupted_turns = summary.interrupted_turns,
        end_of_turn_avg_ms = avg(&l.end_of_turn),
        end_of_turn_max_ms = max(&l.end_of_turn),
        stt_final_avg_ms = avg(&l.stt_final),
        stt_final_max_ms = max(&l.stt_final),
        llm_ttft_avg_ms = avg(&l.llm_ttft),
        llm_ttft_max_ms = max(&l.llm_ttft),
        tts_ttfb_avg_ms = avg(&l.tts_ttfb),
        tts_ttfb_max_ms = max(&l.tts_ttfb),
        response_avg_ms = avg(&l.response),
        response_max_ms = max(&l.response),
        llm_provider = u.llm.as_ref().map(|u| u.provider.as_str()),
        llm_model = u.llm.as_ref().map(|u| u.model.as_str()),
        llm_input_tokens = u.llm.as_ref().map(|u| u.input_tokens),
        llm_cached_input_tokens = u.llm.as_ref().map(|u| u.cached_input_tokens),
        llm_output_tokens = u.llm.as_ref().map(|u| u.output_tokens),
        llm_responses_without_usage = u.llm.as_ref().map(|u| u.responses_without_usage),
        stt_provider = u.stt.as_ref().map(|u| u.provider.as_str()),
        stt_model = u.stt.as_ref().map(|u| u.model.as_str()),
        stt_audio_seconds = u.stt.as_ref().map(|u| u.audio_seconds),
        tts_provider = u.tts.as_ref().map(|u| u.provider.as_str()),
        tts_model = u.tts.as_ref().map(|u| u.model.as_str()),
        tts_characters = u.tts.as_ref().map(|u| u.characters),
        "call summary"
    );
}

#[cfg(test)]
mod tests {
    use brisvox_core::{SimClock, TurnEvent};

    use super::*;
    use crate::config::ProviderModel;

    fn names() -> ProviderModels {
        ProviderModels {
            stt: Some(ProviderModel {
                provider: "deepgram",
                model: "nova-3".to_owned(),
            }),
            llm: Some(ProviderModel {
                provider: "openai",
                model: "gpt-test".to_owned(),
            }),
            tts: Some(ProviderModel {
                provider: "cartesia",
                model: "sonic".to_owned(),
            }),
        }
    }

    fn epoch(n: u32) -> TurnEpoch {
        TurnEpoch::new(n)
    }

    /// A summarizer on a simulated clock and the recorder that feeds it.
    fn rig() -> (Arc<CallSummarizer>, TimelineRecorder, Instant) {
        let clock = SimClock::shared();
        let t0 = clock.now();
        let summarizer = CallSummarizer::new(clock, 7, "test-agent", names(), None);
        let rec = summarizer.recorder();
        (summarizer, rec, t0)
    }

    /// A turn whose caller stopped speaking at `t0`; `offsets` in ms from there: turn end, STT
    /// final, LLM request, LLM first token, TTS request, TTS first byte, first audio out.
    fn turn(rec: &TimelineRecorder, e: u32, t0: Instant, offsets: [u64; 7]) {
        use TurnEvent as E;
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let [
            turn_end,
            stt_final,
            llm_req,
            llm_first,
            tts_req,
            tts_first,
            audio,
        ] = offsets;
        rec.mark(epoch(e), E::UserSpeechEnd, at(0));
        rec.mark(epoch(e), E::SttFinal, at(stt_final));
        rec.mark(epoch(e), E::TurnEnd, at(turn_end));
        let round = rec.llm_request(epoch(e), at(llm_req)).unwrap();
        rec.llm_first_token(epoch(e), round, at(llm_first));
        rec.mark(epoch(e), E::TtsRequest, at(tts_req));
        rec.mark(epoch(e), E::TtsFirstByte, at(tts_first));
        rec.mark(epoch(e), E::FirstAudioOut, at(audio));
    }

    fn llm_stats() -> LlmStatsSnapshot {
        LlmStatsSnapshot {
            input_tokens: 1_200,
            cached_input_tokens: 800,
            output_tokens: 90,
            responses_without_usage: 2,
            ..LlmStatsSnapshot::default()
        }
    }

    fn summary_of(
        totals: &TurnTotals,
        stt: &SttStatsSnapshot,
        tts: &TtsStatsSnapshot,
    ) -> CallSummary {
        build_summary(
            Duration::from_secs(95),
            totals,
            &names(),
            &llm_stats(),
            stt,
            tts,
        )
    }

    #[test]
    fn an_empty_call_has_no_latency_and_zero_units() {
        let summary = build_summary(
            Duration::from_millis(1_234),
            &TurnTotals::default(),
            &names(),
            &LlmStatsSnapshot::default(),
            &SttStatsSnapshot::default(),
            &TtsStatsSnapshot::default(),
        );
        assert_eq!(summary.duration_ms, 1_234);
        assert_eq!((summary.turns, summary.interrupted_turns), (0, 0));
        assert_eq!(summary.latency, LatencySummary::default());
        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(value["latency"], serde_json::json!({}));
        assert_eq!(value["usage"]["stt"]["audio_seconds"], 0.0);
        assert_eq!(value["usage"]["tts"]["characters"], 0);
        assert_eq!(value["usage"]["llm"]["input_tokens"], 0);
    }

    #[test]
    fn turn_latencies_are_averaged_and_maxed_per_stage() {
        let (summarizer, rec, t0) = rig();
        // end of turn 300/500, stt final 120/180, llm ttft 250/450, tts ttfb 100/300,
        // response 900/1400 ms.
        turn(&rec, 1, t0, [300, 120, 310, 560, 580, 680, 900]);
        turn(&rec, 2, t0, [500, 180, 520, 970, 1_000, 1_300, 1_400]);
        rec.finish();
        let totals = summarizer.totals();
        let stt = SttStatsSnapshot {
            audio_ms_sent: 93_125,
            ..SttStatsSnapshot::default()
        };
        let tts = TtsStatsSnapshot {
            chars_sent: 812,
            ..TtsStatsSnapshot::default()
        };
        let summary = summary_of(&totals, &stt, &tts);
        assert_eq!(summary.turns, 2);
        assert_eq!(summary.interrupted_turns, 0);
        let l = &summary.latency;
        let pair = |s: Option<StageLatency>| s.map(|s| (s.avg_ms, s.max_ms));
        assert_eq!(pair(l.end_of_turn), Some((400.0, 500.0)));
        assert_eq!(pair(l.stt_final), Some((150.0, 180.0)));
        assert_eq!(pair(l.llm_ttft), Some((350.0, 450.0)));
        assert_eq!(pair(l.tts_ttfb), Some((200.0, 300.0)));
        assert_eq!(pair(l.response), Some((1_150.0, 1_400.0)));
        let llm = summary.usage.llm.as_ref().unwrap();
        assert_eq!(
            (llm.provider.as_str(), llm.model.as_str()),
            ("openai", "gpt-test")
        );
        assert_eq!(
            (
                llm.input_tokens,
                llm.cached_input_tokens,
                llm.output_tokens,
                llm.responses_without_usage
            ),
            (1_200, 800, 90, 2)
        );
        let stt = summary.usage.stt.as_ref().unwrap();
        assert_eq!(
            (stt.provider.as_str(), stt.model.as_str()),
            ("deepgram", "nova-3")
        );
        assert!((stt.audio_seconds - 93.125).abs() < 1e-9);
        let tts = summary.usage.tts.as_ref().unwrap();
        assert_eq!(
            (tts.provider.as_str(), tts.model.as_str()),
            ("cartesia", "sonic")
        );
        assert_eq!(tts.characters, 812);
    }

    #[test]
    fn a_stage_without_samples_is_absent_and_averages_round_to_a_tenth() {
        let (summarizer, rec, t0) = rig();
        // A turn that never reached the TTS: only the end of turn and the STT final exist.
        rec.mark(epoch(1), TurnEvent::UserSpeechEnd, t0);
        rec.mark(
            epoch(1),
            TurnEvent::SttFinal,
            t0 + Duration::from_micros(100_000),
        );
        rec.mark(
            epoch(1),
            TurnEvent::TurnEnd,
            t0 + Duration::from_micros(250_049),
        );
        rec.finish();
        let summary = summary_of(
            &summarizer.totals(),
            &SttStatsSnapshot::default(),
            &TtsStatsSnapshot::default(),
        );
        assert_eq!(summary.turns, 1);
        assert_eq!(
            summary.latency.end_of_turn,
            Some(StageLatency {
                avg_ms: 250.0,
                max_ms: 250.0
            })
        );
        assert_eq!(summary.latency.llm_ttft, None);
        assert_eq!(summary.latency.tts_ttfb, None);
        assert_eq!(summary.latency.response, None);
        let value = serde_json::to_value(&summary).unwrap();
        assert!(value["latency"].get("llm_ttft").is_none());
    }

    #[test]
    fn interrupted_turns_count_once_at_the_end_or_by_a_late_patch() {
        let (summarizer, rec, t0) = rig();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        // Turn 1 is interrupted while it is current: counted when it finishes.
        turn(&rec, 1, t0, [300, 120, 310, 560, 580, 680, 900]);
        rec.interrupt(epoch(1), epoch(2), at(950), Some(at(940)));
        // Turn 2 finishes by the next turn's first mark, then a barge-in is recorded on it late.
        turn(&rec, 2, t0, [300, 120, 310, 560, 580, 680, 900]);
        rec.mark(epoch(3), TurnEvent::UserSpeechStart, at(2_000));
        rec.interrupt(epoch(2), epoch(4), at(2_100), None);
        rec.finish();
        let totals = summarizer.totals();
        assert_eq!(totals.interrupted(), 2, "{totals:?}");
        assert!(totals.turns() >= 3, "{totals:?}");
    }

    #[test]
    fn a_turn_nothing_was_marked_on_does_not_count() {
        let (summarizer, rec, _) = rig();
        rec.finish();
        assert_eq!(summarizer.totals().turns(), 0);
    }

    #[test]
    fn a_speech_to_speech_call_omits_the_stages_it_did_not_run() {
        let names = ProviderModels {
            stt: None,
            llm: None,
            tts: None,
        };
        let summary = build_summary(
            Duration::from_secs(3),
            &TurnTotals::default(),
            &names,
            &llm_stats(),
            &SttStatsSnapshot::default(),
            &TtsStatsSnapshot::default(),
        );
        assert_eq!(summary.usage, UsageSummary::default());
        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(value["usage"], serde_json::json!({}));
        assert_eq!(value["duration_ms"], 3_000);
    }
}
