//! Mock TTS with latency profiles and stream faults.
//!
//! Speech is synthetic: every character lasts `ms_per_char` of audio (a low square wave, so
//! it is not silence). For each context, the first audio follows `Text` after the first-byte
//! latency; audio arrives in chunks of `chunk` (faster than real time, one chunk per
//! chunk-interval sample). `Word` events (offset of each word into the context's audio) are
//! delivered together with the text's first audio, not when the word is played. `Flush`
//! reports `Flushed` after the context's queued audio. Text for an idle context waits the
//! first-byte latency again. A cancel (priority channel) drops the context's
//! pending events at once and reports `Cancelled`; input still queued for a cancelled context
//! (it was sent before the cancel) is ignored.
//!
//! The engine must keep draining `events`: a session blocked on a full event channel serves
//! neither input nor cancels. Determinism assumes a current-thread runtime with paused time.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_core::SharedClock;
use brisvox_core::clock::deadline_after;
use brisvox_providers::tts::{TTS_CANCEL_CAPACITY, TTS_CHANNEL_CAPACITY};
use brisvox_providers::{
    ContextId, Error, Result, TtsConfig, TtsEvent, TtsInput, TtsProvider, TtsSession,
};
use bytes::Bytes;
use std::collections::{HashMap, VecDeque};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::latency::{LatencyProfile, LatencySampler};
use crate::mock_common::{FaultPlan, Outbox, StreamFault};

const NAME: &str = "mock-tts";

struct Timing {
    connect: LatencySampler,
    first_byte: LatencySampler,
    chunk_interval: LatencySampler,
}

struct State {
    timing: Timing,
    faults: VecDeque<Vec<StreamFault>>,
    /// Adapter-side deadline (connect, and every owed event); `None` waits as long as the
    /// latency and faults say.
    timeout: Option<Duration>,
    ms_per_char: u64,
    chunk: Duration,
    texts: VecDeque<(ContextId, String)>,
    connects: usize,
    /// The configuration of every session opened (the last [`CONFIG_LOG_CAPACITY`]).
    configs: VecDeque<TtsConfig>,
    /// Whether word timestamps are reported (a provider without them when `false`).
    word_timings: bool,
}

/// Texts remembered by [`MockTts::texts`] (oldest dropped).
const TEXT_LOG_CAPACITY: usize = 1_024;

/// Session configurations remembered by [`MockTts::configs`] (oldest dropped).
const CONFIG_LOG_CAPACITY: usize = 64;

/// A mock [`TtsProvider`].
#[derive(Clone)]
pub struct MockTts {
    pub(crate) clock: SharedClock,
    state: Arc<Mutex<State>>,
}

impl MockTts {
    /// Zero latency, 60 ms of audio per character, 40 ms chunks.
    pub fn new(clock: SharedClock) -> Self {
        Self {
            clock,
            state: Arc::new(Mutex::new(State {
                timing: Timing {
                    connect: LatencySampler::new(LatencyProfile::ZERO, 0),
                    first_byte: LatencySampler::new(LatencyProfile::ZERO, 0),
                    chunk_interval: LatencySampler::new(LatencyProfile::ZERO, 0),
                },
                faults: VecDeque::new(),
                timeout: None,
                ms_per_char: 60,
                chunk: Duration::from_millis(40),
                texts: VecDeque::new(),
                connects: 0,
                configs: VecDeque::new(),
                word_timings: true,
            })),
        }
    }

    /// A provider that reports no word timestamps (only audio), as some TTS engines do: the
    /// played-text truncation then falls back to the speaking-rate estimate.
    #[must_use]
    pub fn without_word_timings(self) -> Self {
        self.lock().word_timings = false;
        self
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Connect latency, first-byte latency (per context) and the gap between audio chunks.
    #[must_use]
    pub fn with_latency(
        self,
        connect: LatencyProfile,
        first_byte: LatencyProfile,
        chunk_interval: LatencyProfile,
        seed: u64,
    ) -> Self {
        self.lock().timing = Timing {
            connect: LatencySampler::new(connect, seed),
            first_byte: LatencySampler::new(first_byte, seed.wrapping_add(1)),
            chunk_interval: LatencySampler::new(chunk_interval, seed.wrapping_add(2)),
        };
        self
    }

    /// Audio length per character and chunk size.
    #[must_use]
    pub fn with_audio(self, ms_per_char: u64, chunk: Duration) -> Self {
        {
            let mut state = self.lock();
            state.ms_per_char = ms_per_char.max(1);
            state.chunk = chunk.max(Duration::from_millis(1));
        }
        self
    }

    /// Adapter-side deadline: a connect slower than `timeout` fails with
    /// `Error::Core(Timeout)`, and so does a session that owes an event for `timeout` without
    /// delivering it.
    #[must_use]
    pub fn with_timeout(self, timeout: Duration) -> Self {
        self.lock().timeout = Some(timeout);
        self
    }

    /// Faults for the next session that connects (one entry per `connect` call).
    pub fn push_session_faults(&self, faults: Vec<StreamFault>) {
        self.lock().faults.push_back(faults);
    }

    /// Drops session faults that no session has consumed yet.
    pub fn clear_script(&self) {
        self.lock().faults.clear();
    }

    /// Text received, with its context, in order (the last 1024 pieces).
    pub fn texts(&self) -> Vec<(ContextId, String)> {
        self.lock().texts.iter().cloned().collect()
    }

    /// `connect` calls so far (reconnects included).
    pub fn connects(&self) -> usize {
        self.lock().connects
    }

    /// The [`TtsConfig`] of each session opened, in order (the last 64): the voice, language
    /// and model the engine asked for.
    pub fn configs(&self) -> Vec<TtsConfig> {
        self.lock().configs.iter().cloned().collect()
    }
}

impl std::fmt::Debug for MockTts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockTts").finish_non_exhaustive()
    }
}

impl TtsProvider for MockTts {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn connect(&self, config: TtsConfig) -> Result<TtsSession> {
        let (connect, faults, timeout) = {
            let mut state = self.lock();
            state.connects += 1;
            if state.configs.len() >= CONFIG_LOG_CAPACITY {
                state.configs.pop_front();
            }
            state.configs.push_back(config.clone());
            let faults = FaultPlan::new(state.faults.pop_front().unwrap_or_default());
            (
                state
                    .timing
                    .connect
                    .sample()
                    .saturating_add(faults.connect_stall()),
                faults,
                state.timeout,
            )
        };
        if let Some(timeout) = timeout
            && connect > timeout
        {
            brisvox_core::clock::wait_for(&*self.clock, timeout).await;
            return Err(Error::Core(brisvox_core::Error::Timeout {
                what: "tts connect",
                after: timeout,
            }));
        }
        brisvox_core::clock::wait_for(&*self.clock, connect).await;
        if let Some(status) = faults.connect_error() {
            return Err(Error::Rejected {
                provider: NAME,
                status,
            });
        }
        let (input_tx, input_rx) = mpsc::channel(TTS_CHANNEL_CAPACITY);
        let (cancel_tx, cancel_rx) = mpsc::channel(TTS_CANCEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel(TTS_CHANNEL_CAPACITY);
        let session = Session {
            mock: self.clone(),
            faults,
            bytes_per_ms: usize::try_from(config.sample_rate.hz() / 1_000)
                .unwrap_or(16)
                .saturating_mul(2),
            contexts: HashMap::new(),
            outbox: Outbox::default(),
            cancelled: VecDeque::with_capacity(CANCELLED_MEMORY),
            tone_cache: Bytes::new(),
            timeout,
            owed_since: self.clock.now(),
        };
        // Ends when the engine drops the input sender or the event receiver.
        tokio::spawn(session.run(input_rx, cancel_rx, event_tx));
        Ok(TtsSession {
            input: input_tx,
            cancel: cancel_tx,
            events: event_rx,
        })
    }
}

/// Synthesis progress of one context.
#[derive(Default)]
struct Context {
    /// Audio scheduled so far (offset of the next word).
    audio: Duration,
    /// When its last scheduled event is delivered.
    busy_until: Option<Instant>,
}

struct Session {
    mock: MockTts,
    faults: FaultPlan,
    bytes_per_ms: usize,
    contexts: HashMap<ContextId, Context>,
    outbox: Outbox<TtsEvent>,
    /// Recently cancelled contexts (bounded): their late input is ignored.
    cancelled: VecDeque<ContextId>,
    tone_cache: Bytes,
    /// Adapter-side deadline (see [`MockTts::with_timeout`]).
    timeout: Option<Duration>,
    /// When the session started owing events, or last delivered one: the watchdog's base.
    owed_since: Instant,
}

/// Cancelled contexts remembered per session.
const CANCELLED_MEMORY: usize = 64;

impl Session {
    fn is_cancelled(&self, context: ContextId) -> bool {
        self.cancelled.contains(&context)
    }

    async fn run(
        mut self,
        mut input: mpsc::Receiver<TtsInput>,
        mut cancel: mpsc::Receiver<ContextId>,
        events: mpsc::Sender<TtsEvent>,
    ) {
        let clock = self.mock.clock.clone();
        let (mut input_open, mut cancel_open) = (true, true);
        let mut stall_until: Option<Instant> = None;
        loop {
            let deadline = self
                .outbox
                .next_deadline()
                .map(|d| stall_until.map_or(d, |s| s.max(d)));
            if !input_open && deadline.is_none() {
                return;
            }
            let watchdog = self
                .timeout
                .and_then(|t| deadline.map(|_| deadline_after(self.owed_since, t)));
            let wake = match (deadline, watchdog) {
                (Some(d), Some(w)) => Some(d.min(w)),
                (d, _) => d,
            };
            tokio::select! {
                biased;
                () = events.closed() => return,
                context = cancel.recv(), if cancel_open => match context {
                    None => cancel_open = false,
                    Some(context) => {
                        self.outbox.remove_where(|e| context_of(e) == Some(context));
                        self.contexts.remove(&context);
                        if self.cancelled.len() >= CANCELLED_MEMORY {
                            self.cancelled.pop_front();
                        }
                        self.cancelled.push_back(context);
                        if events.send(TtsEvent::Cancelled { context }).await.is_err() {
                            return;
                        }
                    }
                },
                message = input.recv(), if input_open => match message {
                    None => input_open = false,
                    Some(TtsInput::Text { context, .. } | TtsInput::Flush { context })
                        if self.is_cancelled(context) => {}
                    Some(TtsInput::Text { context, text }) => self.schedule_text(&clock, context, &text),
                    Some(TtsInput::Flush { context }) => {
                        let now = clock.now();
                        if self.outbox.is_empty() {
                            self.owed_since = now;
                        }
                        let at = self
                            .contexts
                            .get(&context)
                            .and_then(|c| c.busy_until)
                            .map_or(now, |busy| busy.max(now));
                        self.outbox.push(at, TtsEvent::Flushed { context });
                    }
                },
                () = sleep_until(&clock, wake), if wake.is_some() => {
                    // The watchdog: an owed event that is not due within the timeout fails the session.
                    if let (Some(timeout), Some(d), Some(w)) = (self.timeout, deadline, watchdog)
                        && w <= clock.now()
                        && w < d
                    {
                        let _ = events
                            .send(TtsEvent::Error(Error::Core(brisvox_core::Error::Timeout {
                                what: "tts event",
                                after: timeout,
                            })))
                            .await;
                        return;
                    }
                    // A stall is loop state, so input, cancel and drop stay live while it lasts.
                    if stall_until.is_none() {
                        let stall = self.faults.stall();
                        if !stall.is_zero() {
                            stall_until = Some(clock.now() + stall);
                            continue;
                        }
                    }
                    stall_until = None;
                    if let Some(error) = self.faults.cut(NAME) {
                        let _ = events.send(TtsEvent::Error(error)).await;
                        return;
                    }
                    if let Some(event) = self.outbox.pop() {
                        if events.send(event).await.is_err() {
                            return;
                        }
                        self.faults.delivered();
                        self.owed_since = clock.now();
                        if let Some(error) = self.faults.cut(NAME) {
                            let _ = events.send(TtsEvent::Error(error)).await;
                            return;
                        }
                    }
                }
            }
        }
    }

    fn schedule_text(&mut self, clock: &SharedClock, context: ContextId, text: &str) {
        if self.outbox.is_empty() {
            self.owed_since = clock.now();
        }
        let (ms_per_char, chunk, first_byte, word_timings, mut intervals) = {
            let mut state = self.mock.lock();
            if state.texts.len() >= TEXT_LOG_CAPACITY {
                state.texts.pop_front();
            }
            state.texts.push_back((context, text.to_owned()));
            let first_byte = state.timing.first_byte.sample();
            // One interval sample per audio chunk.
            let audio_ms = u128::try_from(text.chars().count())
                .unwrap_or(u128::MAX)
                .saturating_mul(u128::from(state.ms_per_char));
            let chunk_ms = state.chunk.as_millis().max(1);
            let chunks = usize::try_from(audio_ms.div_ceil(chunk_ms)).unwrap_or(usize::MAX);
            let intervals: Vec<Duration> = (0..chunks)
                .map(|_| state.timing.chunk_interval.sample())
                .collect();
            (
                state.ms_per_char,
                state.chunk,
                first_byte,
                state.word_timings,
                intervals.into_iter(),
            )
        };
        let now = clock.now();
        // Forget other contexts that are idle (bounded map over a long session).
        self.contexts
            .retain(|id, c| *id == context || c.busy_until.is_some_and(|busy| busy > now));
        let ctx = self.contexts.entry(context).or_default();
        let mut at = match ctx.busy_until {
            Some(busy) if busy > now => busy,
            _ => now + first_byte,
        };
        let char_audio = Duration::from_millis(ms_per_char);
        // Word events at their audio offsets.
        let mut offset = ctx.audio;
        let mut after_space = true;
        for (i, c) in text.char_indices() {
            if word_timings && !c.is_whitespace() && after_space {
                let word = text
                    .get(i..)
                    .and_then(|rest| rest.split_whitespace().next())
                    .unwrap_or_default();
                self.outbox.push(
                    at,
                    TtsEvent::Word {
                        context,
                        word: Arc::from(word),
                        start: offset,
                    },
                );
            }
            after_space = c.is_whitespace();
            offset += char_audio;
        }
        // Audio chunks covering the text's duration.
        let total = offset.saturating_sub(ctx.audio);
        let mut produced = Duration::ZERO;
        while produced < total {
            let piece = chunk.min(total - produced);
            let bytes = self
                .bytes_per_ms
                .saturating_mul(usize::try_from(piece.as_millis()).unwrap_or(usize::MAX));
            self.outbox.push(
                at,
                TtsEvent::Audio {
                    context,
                    audio: cached_tone(&mut self.tone_cache, bytes),
                },
            );
            produced += piece;
            at += intervals.next().unwrap_or_default();
        }
        ctx.audio = offset;
        ctx.busy_until = Some(at);
    }
}

fn context_of(event: &TtsEvent) -> Option<ContextId> {
    match event {
        TtsEvent::Audio { context, .. }
        | TtsEvent::Word { context, .. }
        | TtsEvent::Flushed { context }
        | TtsEvent::Cancelled { context } => Some(*context),
        TtsEvent::Error(_) => None,
    }
}

/// `len` bytes of tone, sliced from a per-session buffer (no allocation per chunk).
fn cached_tone(cache: &mut Bytes, len: usize) -> Bytes {
    if cache.len() < len {
        *cache = tone(len.max(4_096));
    }
    cache.slice(..len.min(cache.len()))
}

/// `len` bytes of a quiet PCM16 square wave (not silence, so audio detectors see it).
fn tone(len: usize) -> Bytes {
    let samples = len / 2;
    let mut out = Vec::with_capacity(samples * 2);
    for i in 0..samples {
        let value: i16 = if (i / 20) % 2 == 0 { 1_000 } else { -1_000 };
        out.extend_from_slice(&value.to_le_bytes());
    }
    Bytes::from(out)
}

async fn sleep_until(clock: &SharedClock, deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        brisvox_core::clock::wait_until(&**clock, deadline).await;
    }
}
