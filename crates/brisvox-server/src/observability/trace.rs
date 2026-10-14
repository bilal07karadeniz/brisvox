//! One OpenTelemetry trace per turn.
//!
//! A finished turn's [`TurnRecord`] (the per-stage marks of `brisvox_core::timeline`) is
//! turned into a [`TurnTrace`]: a root span `turn` and a child span per stage that has both of
//! its marks. The spans carry the real timing of the call's clock, mapped to unix time with one
//! `(Clock::now, Clock::wall)` anchor, so the trace is deterministic under a simulated clock and
//! costs nothing while the turn runs: it is built once, when the turn is over.
//!
//! | span | from | to |
//! |---|---|---|
//! | `turn` (root) | first mark | last mark |
//! | `user_speech` | `UserSpeechStart` | `UserSpeechEnd` |
//! | `end_of_turn` | `UserSpeechEnd` | `TurnEnd` |
//! | `stt_final` | `UserSpeechEnd` | `SttFinal` |
//! | `llm` (one per round) | the round's request | its completion, else its first token |
//! | `tts` | `TtsRequest` | `TtsFirstByte` |
//! | `audio_out` | `TtsFirstByte` | `FirstAudioOut` |
//! | `interruption` | `Interrupted` | `AudioOutStopped` (else zero length) |
//!
//! Exporters implement [`TraceExporter`]: `export` runs on a pipeline task and must not block.
//! [`MemoryExporter`] keeps the last traces for tests and embedders; the OTLP exporter behind the
//! `otlp` feature ships them to a collector.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brisvox_core::{TurnEvent, TurnRecord};
use tokio::time::Instant;

/// A 16-byte trace id (never all zero).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceId(pub [u8; 16]);

/// An 8-byte span id (never all zero).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanId(pub [u8; 8]);

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

impl TraceId {
    /// Lower-case hex, 32 characters.
    pub fn to_hex(self) -> String {
        hex(&self.0)
    }
}

impl SpanId {
    /// Lower-case hex, 16 characters.
    pub fn to_hex(self) -> String {
        hex(&self.0)
    }
}

/// An attribute value.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrValue {
    /// Text.
    Str(Arc<str>),
    /// An integer.
    Int(i64),
    /// A flag.
    Bool(bool),
}

/// One finished span.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    /// The trace it belongs to.
    pub trace_id: TraceId,
    /// Its id.
    pub span_id: SpanId,
    /// The parent span; `None` for the root.
    pub parent: Option<SpanId>,
    /// The span name (see the table in the [module docs](self)).
    pub name: &'static str,
    /// Start, nanoseconds since the unix epoch.
    pub start_unix_nanos: u64,
    /// End, nanoseconds since the unix epoch (never before the start).
    pub end_unix_nanos: u64,
    /// The attributes.
    pub attributes: Vec<(&'static str, AttrValue)>,
}

impl Span {
    /// The attribute called `key`.
    pub fn attribute(&self, key: &str) -> Option<&AttrValue> {
        self.attributes
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }

    /// The span's length.
    pub fn duration(&self) -> Duration {
        Duration::from_nanos(self.end_unix_nanos.saturating_sub(self.start_unix_nanos))
    }
}

/// The spans of one turn: the root first, then the stages in start order.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnTrace {
    /// The trace id every span carries.
    pub trace_id: TraceId,
    /// The spans.
    pub spans: Vec<Span>,
}

impl TurnTrace {
    /// The root `turn` span.
    pub fn root(&self) -> Option<&Span> {
        self.spans.iter().find(|s| s.parent.is_none())
    }

    /// The first span called `name`.
    pub fn span(&self, name: &str) -> Option<&Span> {
        self.spans.iter().find(|s| s.name == name)
    }
}

/// Where finished traces go. Called on a pipeline task, once per turn: implementations must not
/// block or do heavy work (queue the trace, drop it when the queue is full).
pub trait TraceExporter: Send + Sync {
    /// Takes one finished trace.
    fn export(&self, trace: TurnTrace);
}

/// Keeps the newest traces in memory (an in-memory exporter for tests and embedders). Bounded:
/// when full, the oldest trace is dropped and counted.
#[derive(Debug)]
pub struct MemoryExporter {
    capacity: usize,
    traces: Mutex<VecDeque<TurnTrace>>,
    dropped: AtomicU64,
}

impl MemoryExporter {
    /// An exporter keeping at most `capacity` traces (at least one).
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity: capacity.max(1),
            traces: Mutex::new(VecDeque::new()),
            dropped: AtomicU64::new(0),
        })
    }

    /// The traces now held, oldest first.
    pub fn traces(&self) -> Vec<TurnTrace> {
        self.traces
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    /// Removes and returns the traces held.
    pub fn take(&self) -> Vec<TurnTrace> {
        self.traces
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
            .collect()
    }

    /// Traces dropped to make room.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl TraceExporter for MemoryExporter {
    fn export(&self, trace: TurnTrace) {
        let mut traces = self.traces.lock().unwrap_or_else(PoisonError::into_inner);
        if traces.len() >= self.capacity {
            traces.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        traces.push_back(trace);
    }
}

/// Makes trace and span ids: a SplitMix64 stream over a per-process seed and a counter. Unique
/// within the process by construction (the counter), unpredictable across processes (the seed).
#[derive(Debug)]
pub struct IdGen {
    seed: u64,
    counter: AtomicU64,
}

fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl IdGen {
    /// Ids from `seed` (tests: a fixed one; production: [`Self::random`]).
    pub const fn with_seed(seed: u64) -> Self {
        Self {
            seed,
            counter: AtomicU64::new(0),
        }
    }

    /// A generator seeded from the process's hasher randomness.
    pub fn random() -> Self {
        use std::hash::{BuildHasher, Hasher};
        Self::with_seed(
            std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish(),
        )
    }

    fn next_word(&self) -> u64 {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        splitmix64(self.seed ^ splitmix64(n))
    }

    /// A new trace id.
    pub fn trace_id(&self) -> TraceId {
        let hi = u128::from(self.next_word());
        let lo = u128::from(self.next_word() | 1);
        TraceId(((hi << 64) | lo).to_be_bytes())
    }

    /// A new span id.
    pub fn span_id(&self) -> SpanId {
        SpanId((self.next_word() | 1).to_be_bytes())
    }
}

/// Maps the call clock's instants to unix time with one anchor taken when the trace is built.
#[derive(Debug, Clone, Copy)]
pub struct Anchor {
    instant: Instant,
    unix_nanos: u64,
}

impl Anchor {
    /// An anchor where `instant` (from `Clock::now`) is `wall` (from `Clock::wall`).
    pub fn new(instant: Instant, wall: SystemTime) -> Self {
        let unix_nanos = wall
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        Self {
            instant,
            unix_nanos,
        }
    }

    /// `at` as nanoseconds since the unix epoch.
    pub fn unix_nanos(&self, at: Instant) -> u64 {
        if let Some(before) = self.instant.checked_duration_since(at) {
            let before = u64::try_from(before.as_nanos()).unwrap_or(u64::MAX);
            self.unix_nanos.saturating_sub(before)
        } else {
            let after = u64::try_from(at.saturating_duration_since(self.instant).as_nanos())
                .unwrap_or(u64::MAX);
            self.unix_nanos.saturating_add(after)
        }
    }
}

/// What identifies a turn's trace beyond its timing.
#[derive(Debug, Clone)]
pub struct TurnIdentity {
    /// The session id (`SessionId::get`).
    pub session: u64,
    /// The agent definition's name.
    pub agent: Arc<str>,
}

/// Builds the trace of `record`. `None` when the turn has no marks at all.
pub fn build_trace(
    record: &TurnRecord,
    who: &TurnIdentity,
    anchor: Anchor,
    ids: &IdGen,
) -> Option<TurnTrace> {
    use TurnEvent as E;
    let first = E::ALL.iter().filter_map(|e| record.at(*e)).min()?;
    let mut last = E::ALL.iter().filter_map(|e| record.at(*e)).max()?;
    for round in 0..record.llm_round_count() {
        if let (Some(start), Some(m)) =
            (record.llm_round_request_at(round), record.llm_round(round))
        {
            let length = m.duration.or(m.time_to_first_token).unwrap_or_default();
            if let Some(end) = start.checked_add(length) {
                last = last.max(end);
            }
        }
    }
    let trace_id = ids.trace_id();
    let root_id = ids.span_id();
    let metrics = record.metrics();
    let mut root_attrs = vec![
        (
            "brisvox.session.id",
            AttrValue::Int(i64::try_from(who.session).unwrap_or(i64::MAX)),
        ),
        ("brisvox.agent", AttrValue::Str(Arc::clone(&who.agent))),
        (
            "brisvox.turn.epoch",
            AttrValue::Int(i64::from(record.epoch().get())),
        ),
        (
            "brisvox.turn.interrupted",
            AttrValue::Bool(record.at(E::Interrupted).is_some()),
        ),
        (
            "brisvox.turn.llm_rounds",
            AttrValue::Int(i64::try_from(record.llm_round_count()).unwrap_or(i64::MAX)),
        ),
    ];
    if let Some(d) = metrics.response_latency {
        root_attrs.push(("brisvox.turn.response_latency_us", micros(d)));
    }
    if let Some(d) = metrics.post_endpoint_latency {
        root_attrs.push(("brisvox.turn.post_endpoint_latency_us", micros(d)));
    }
    let mut spans = vec![Span {
        trace_id,
        span_id: root_id,
        parent: None,
        name: "turn",
        start_unix_nanos: anchor.unix_nanos(first),
        end_unix_nanos: anchor.unix_nanos(last),
        attributes: root_attrs,
    }];
    let mut stage = |name: &'static str,
                     from: Option<Instant>,
                     to: Option<Instant>,
                     attributes: Vec<(&'static str, AttrValue)>| {
        let (Some(from), Some(to)) = (from, to) else {
            return;
        };
        if to < from {
            return;
        }
        spans.push(Span {
            trace_id,
            span_id: ids.span_id(),
            parent: Some(root_id),
            name,
            start_unix_nanos: anchor.unix_nanos(from),
            end_unix_nanos: anchor.unix_nanos(to),
            attributes,
        });
    };
    stage(
        "user_speech",
        record.at(E::UserSpeechStart),
        record.at(E::UserSpeechEnd),
        Vec::new(),
    );
    stage(
        "end_of_turn",
        record.at(E::UserSpeechEnd),
        record.at(E::TurnEnd),
        Vec::new(),
    );
    stage(
        "stt_final",
        record.at(E::UserSpeechEnd),
        record.at(E::SttFinal),
        Vec::new(),
    );
    for round in record.llm_rounds() {
        let Some(start) = record.llm_round_request_at(round.round) else {
            continue;
        };
        let mut attrs = vec![(
            "brisvox.llm.round",
            AttrValue::Int(i64::try_from(round.round).unwrap_or(i64::MAX)),
        )];
        if let Some(ttft) = round.time_to_first_token {
            attrs.push(("brisvox.llm.time_to_first_token_us", micros(ttft)));
        }
        let length = round
            .duration
            .or(round.time_to_first_token)
            .unwrap_or_default();
        stage("llm", Some(start), start.checked_add(length), attrs);
    }
    stage(
        "tts",
        record.at(E::TtsRequest),
        record.at(E::TtsFirstByte),
        Vec::new(),
    );
    stage(
        "audio_out",
        record.at(E::TtsFirstByte),
        record.at(E::FirstAudioOut),
        Vec::new(),
    );
    if let Some(interrupted) = record.at(E::Interrupted) {
        let end = record.at(E::AudioOutStopped).unwrap_or(interrupted);
        stage("interruption", Some(interrupted), Some(end), Vec::new());
    }
    // Stage spans in start order after the root.
    if let Some((_, stages)) = spans.split_first_mut() {
        stages.sort_by_key(|s| s.start_unix_nanos);
    }
    Some(TurnTrace { trace_id, spans })
}

fn micros(d: Duration) -> AttrValue {
    AttrValue::Int(i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_never_zero() {
        let ids = IdGen::with_seed(0);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let t = ids.trace_id();
            let s = ids.span_id();
            assert_ne!(t.0, [0; 16]);
            assert_ne!(s.0, [0; 8]);
            assert!(seen.insert(t.0.to_vec()) && seen.insert(s.0.to_vec()));
        }
        assert_eq!(
            IdGen::with_seed(7).trace_id(),
            IdGen::with_seed(7).trace_id()
        );
    }

    #[test]
    fn hex_is_fixed_width() {
        assert_eq!(
            SpanId([0, 1, 2, 3, 4, 5, 6, 255]).to_hex(),
            "00010203040506ff"
        );
        assert_eq!(TraceId([0xab; 16]).to_hex().len(), 32);
    }

    #[test]
    fn memory_exporter_drops_the_oldest() {
        let exporter = MemoryExporter::new(2);
        let ids = IdGen::with_seed(1);
        for _ in 0..3 {
            exporter.export(TurnTrace {
                trace_id: ids.trace_id(),
                spans: Vec::new(),
            });
        }
        assert_eq!(exporter.traces().len(), 2);
        assert_eq!(exporter.dropped(), 1);
    }
}
