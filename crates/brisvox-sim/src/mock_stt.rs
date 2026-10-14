//! Scripted mock STT with latency profiles and stream faults.
//!
//! Each session plays queued [`SttUtterance`]s in order. The first audio of an utterance
//! schedules its interim transcripts (one every interim latency sample); `Finalize` drops the
//! interims still pending and, after the final latency, emits the final segments flagged
//! `from_finalize`, the last one `speech_final` (an utterance without finals yields one empty
//! `speech_final` transcript). Then the next utterance is current. A `Finalize` without audio
//! since the previous one is ignored. There is no provider-side endpointing: `speech_final`
//! only follows a `Finalize`.
//!
//! With [`MockStt::with_timeout`] the mock behaves like an adapter with a deadline: a connect
//! slower than the timeout fails with `Error::Core(Timeout)`, and so does a session that owes
//! an event for the timeout without delivering it. When the caller drops the input, the interims
//! still owed are discarded; finals requested by a `Finalize` are still delivered.
//!
//! Determinism assumes a current-thread runtime with paused time: samplers and queues are
//! shared by all sessions of one mock.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_core::SharedClock;
use brisvox_core::clock::deadline_after;
use brisvox_providers::stt::STT_CHANNEL_CAPACITY;
use brisvox_providers::{Error, Result, SttConfig, SttEvent, SttInput, SttProvider, SttSession};
use tokio::sync::mpsc;

use crate::latency::{LatencyProfile, LatencySampler};
use crate::mock_common::{FaultPlan, Outbox, StreamFault};

const NAME: &str = "mock-stt";

/// What the mock "hears" for one utterance.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SttUtterance {
    /// Interim hypotheses, in order.
    pub interims: Vec<String>,
    /// Final segments, in order (the last ends the utterance).
    pub finals: Vec<String>,
}

impl SttUtterance {
    /// One final segment, with growing prefixes of it as interims (word by word).
    pub fn words(text: &str) -> Self {
        let words: Vec<&str> = text.split_whitespace().collect();
        let interims = (1..words.len())
            .map(|n| words.get(..n).map(|w| w.join(" ")).unwrap_or_default())
            .collect();
        Self {
            interims,
            finals: vec![text.to_owned()],
        }
    }
}

struct Timing {
    connect: LatencySampler,
    interim: LatencySampler,
    final_: LatencySampler,
}

struct State {
    utterances: VecDeque<SttUtterance>,
    faults: VecDeque<Vec<StreamFault>>,
    timing: Timing,
    /// Adapter-side deadline (connect, and every owed event); `None` waits as long as the
    /// latency and faults say.
    timeout: Option<Duration>,
    audio_bytes: usize,
    finalizes: usize,
    connects: usize,
    /// Whether the final text of an utterance is also its last interim.
    converge: bool,
    /// Interims are timed from the first audio frame whose RMS reaches this (PCM16 units), not
    /// from the first frame after the last `Finalize`. `None`: the first frame of any level.
    speech_rms: Option<f64>,
}

/// A mock [`SttProvider`].
#[derive(Clone)]
pub struct MockStt {
    pub(crate) clock: SharedClock,
    state: Arc<Mutex<State>>,
}

impl MockStt {
    /// A mock with zero latency and nothing scripted.
    pub fn new(clock: SharedClock) -> Self {
        Self {
            clock,
            state: Arc::new(Mutex::new(State {
                utterances: VecDeque::new(),
                faults: VecDeque::new(),
                timing: Timing {
                    connect: LatencySampler::new(LatencyProfile::ZERO, 0),
                    interim: LatencySampler::new(LatencyProfile::ZERO, 0),
                    final_: LatencySampler::new(LatencyProfile::ZERO, 0),
                },
                timeout: None,
                audio_bytes: 0,
                finalizes: 0,
                connects: 0,
                converge: false,
                speech_rms: None,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Connect latency, interim spacing and finalize-to-final latency.
    #[must_use]
    pub fn with_latency(
        self,
        connect: LatencyProfile,
        interim: LatencyProfile,
        final_: LatencyProfile,
        seed: u64,
    ) -> Self {
        self.lock().timing = Timing {
            connect: LatencySampler::new(connect, seed),
            interim: LatencySampler::new(interim, seed.wrapping_add(1)),
            final_: LatencySampler::new(final_, seed.wrapping_add(2)),
        };
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

    /// Makes the final text of every utterance also its last interim, spaced like the others (a
    /// real STT that has converged by the time the caller pauses). By default the interims are
    /// the utterance's own (`SttUtterance::words`: word prefixes, the last one a word short).
    pub fn set_converged_interims(&self, on: bool) {
        self.lock().converge = on;
    }

    /// Times the interims of an utterance from the first audio frame whose RMS is at least `rms`
    /// (PCM16 units), as a real STT's interims start with the speech. By default they start with
    /// the first frame after the previous `Finalize`, silence included, so the interims of an
    /// utterance that follows a long silence arrive long before it is spoken.
    pub fn set_interims_from_speech(&self, rms: f64) {
        self.lock().speech_rms = Some(rms);
    }

    /// Queues an utterance (shared by all sessions, consumed in order).
    pub fn push(&self, utterance: SttUtterance) {
        self.lock().utterances.push_back(utterance);
    }

    /// Faults for the next session that connects (one entry per `connect` call).
    pub fn push_session_faults(&self, faults: Vec<StreamFault>) {
        self.lock().faults.push_back(faults);
    }

    /// Drops queued utterances and session faults that nothing has consumed yet.
    pub fn clear_script(&self) {
        let mut state = self.lock();
        state.utterances.clear();
        state.faults.clear();
    }

    /// Total audio bytes received across sessions.
    pub fn audio_bytes(&self) -> usize {
        self.lock().audio_bytes
    }

    /// `Finalize` requests received across sessions.
    pub fn finalizes(&self) -> usize {
        self.lock().finalizes
    }

    /// `connect` calls so far (reconnects included).
    pub fn connects(&self) -> usize {
        self.lock().connects
    }
}

impl std::fmt::Debug for MockStt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockStt").finish_non_exhaustive()
    }
}

impl SttProvider for MockStt {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn connect(&self, _config: SttConfig) -> Result<SttSession> {
        let (connect, faults, timeout) = {
            let mut state = self.lock();
            state.connects += 1;
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
                what: "stt connect",
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
        let (input_tx, input_rx) = mpsc::channel(STT_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel(STT_CHANNEL_CAPACITY);
        // Ends when the engine drops the input sender or the event receiver.
        tokio::spawn(run_session(
            self.clone(),
            faults,
            timeout,
            input_rx,
            event_tx,
        ));
        Ok(SttSession {
            input: input_tx,
            events: event_rx,
        })
    }
}

/// RMS of a PCM16 LE frame (0 for an empty one).
fn frame_rms(audio: &[u8]) -> f64 {
    let mut sum = 0.0f64;
    let mut count = 0u32;
    let (pairs, _) = audio.as_chunks::<2>();
    for pair in pairs {
        let sample = f64::from(i16::from_le_bytes(*pair));
        sum += sample * sample;
        count = count.saturating_add(1);
    }
    if count == 0 {
        0.0
    } else {
        (sum / f64::from(count)).sqrt()
    }
}

async fn run_session(
    mock: MockStt,
    mut faults: FaultPlan,
    timeout: Option<Duration>,
    mut input: mpsc::Receiver<SttInput>,
    events: mpsc::Sender<SttEvent>,
) {
    let clock = mock.clock.clone();
    let mut outbox: Outbox<SttEvent> = Outbox::default();
    // Whether the current utterance has started (interims scheduled).
    let mut started = false;
    // Audio received since the last Finalize.
    let mut heard = false;
    let mut input_open = true;
    let mut stall_until: Option<tokio::time::Instant> = None;
    // When the session started owing events, or last delivered one: the watchdog's base.
    let mut owed_since = clock.now();
    loop {
        let deadline = outbox
            .next_deadline()
            .map(|d| stall_until.map_or(d, |s| s.max(d)));
        if !input_open && deadline.is_none() {
            return;
        }
        let watchdog = timeout.and_then(|t| deadline.map(|_| deadline_after(owed_since, t)));
        let wake = match (deadline, watchdog) {
            (Some(d), Some(w)) => Some(d.min(w)),
            (d, _) => d,
        };
        tokio::select! {
            biased;
            () = events.closed() => return,
            message = input.recv(), if input_open => match message {
                None => {
                    // Hang-up: the interims still owed are discarded (they were never final).
                    // Finals requested by a Finalize are still delivered.
                    input_open = false;
                    outbox.remove_where(|e| matches!(e, SttEvent::Transcript { is_final: false, .. }));
                }
                Some(SttInput::Audio(audio)) => {
                    let mut state = mock.lock();
                    state.audio_bytes += audio.len();
                    heard = true;
                    let loud = state.speech_rms.is_none_or(|t| frame_rms(&audio) >= t);
                    if !started
                        && loud
                        && let Some(utterance) = state.utterances.front().cloned()
                    {
                        started = true;
                        if outbox.is_empty() {
                            owed_since = clock.now();
                        }
                        let mut at = clock.now();
                        let full = utterance.finals.join(" ");
                        let mut interims = utterance.interims;
                        if state.converge
                            && !full.is_empty()
                            && interims.last() != Some(&full)
                        {
                            interims.push(full);
                        }
                        for text in interims {
                            at += state.timing.interim.sample();
                            outbox.push(at, transcript(&text, false, false, false));
                        }
                    }
                }
                Some(SttInput::Finalize) => {
                    let mut state = mock.lock();
                    state.finalizes += 1;
                    if !heard {
                        continue;
                    }
                    heard = false;
                    outbox.remove_where(|e| matches!(e, SttEvent::Transcript { is_final: false, .. }));
                    started = false;
                    if let Some(utterance) = state.utterances.pop_front() {
                        let at = clock.now() + state.timing.final_.sample();
                        if outbox.is_empty() {
                            owed_since = clock.now();
                        }
                        if utterance.finals.is_empty() {
                            outbox.push(at, transcript("", true, true, true));
                        }
                        let last = utterance.finals.len().saturating_sub(1);
                        for (i, text) in utterance.finals.iter().enumerate() {
                            outbox.push(at, transcript(text, true, i == last, true));
                        }
                    }
                }
            },
            () = sleep_until(&clock, wake), if wake.is_some() => {
                // The watchdog: an owed event that is not due within the timeout fails the session.
                if let (Some(timeout), Some(d), Some(w)) = (timeout, deadline, watchdog)
                    && w <= clock.now()
                    && w < d
                {
                    let _ = events
                        .send(SttEvent::Error(Error::Core(brisvox_core::Error::Timeout {
                            what: "stt event",
                            after: timeout,
                        })))
                        .await;
                    return;
                }
                // A stall is loop state, so input, cancel and drop stay live while it lasts.
                if stall_until.is_none() {
                    let stall = faults.stall();
                    if !stall.is_zero() {
                        stall_until = Some(clock.now() + stall);
                        continue;
                    }
                }
                stall_until = None;
                if let Some(error) = faults.cut(NAME) {
                    let _ = events.send(SttEvent::Error(error)).await;
                    return;
                }
                if let Some(event) = outbox.pop() {
                    if events.send(event).await.is_err() {
                        return;
                    }
                    faults.delivered();
                    owed_since = clock.now();
                    if let Some(error) = faults.cut(NAME) {
                        let _ = events.send(SttEvent::Error(error)).await;
                        return;
                    }
                }
            }
        }
    }
}

async fn sleep_until(clock: &SharedClock, deadline: Option<tokio::time::Instant>) {
    if let Some(deadline) = deadline {
        brisvox_core::clock::wait_until(&**clock, deadline).await;
    }
}

fn transcript(text: &str, is_final: bool, speech_final: bool, from_finalize: bool) -> SttEvent {
    SttEvent::Transcript {
        text: Arc::from(text),
        is_final,
        speech_final,
        from_finalize,
    }
}
