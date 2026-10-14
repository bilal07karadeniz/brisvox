//! Provider contract suite run against the three sim mocks, and against deliberately broken
//! doubles: every check must be able to fail, or the suite proves nothing.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
// test helpers

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use brisvox_core::clock::wait_for;
use brisvox_core::{SharedClock, SimClock};
use brisvox_providers::llm::LLM_EVENT_CAPACITY;
use brisvox_providers::stt::STT_CHANNEL_CAPACITY;
use brisvox_providers::tts::{TTS_CANCEL_CAPACITY, TTS_CHANNEL_CAPACITY};
use brisvox_providers::{
    ContextId, Error as ProviderError, LlmEvent, LlmProvider, LlmRequest, LlmStream, Result,
    SttConfig, SttEvent, SttInput, SttProvider, SttSession, TtsConfig, TtsEvent, TtsProvider,
    TtsSession,
};
use brisvox_sim::contract::{
    CANCEL_SLACK_EVENTS, CONTRACT_TIMEOUT, ContractReport, Fault, LlmHarness, Response, SttHarness,
    TtsHarness, Utterance, llm_contract, stt_contract, tts_contract,
};
use brisvox_sim::latency::LatencyProfile;
use brisvox_sim::mock_llm::MockLlm;
use brisvox_sim::mock_stt::MockStt;
use brisvox_sim::mock_tts::MockTts;
use bytes::Bytes;
use tokio::sync::mpsc;

const MS: Duration = Duration::from_millis(1);

/// How much slower a connect after `warm()` is made by the slow-warm doubles.
const SLOW_WARM: Duration = Duration::from_millis(200);

/// Gap between the ghost transcripts of [`Broken::EmitsAfterHangUp`], and the delay of
/// [`Broken::DelayedCancel`].
const GAP: Duration = Duration::from_secs(1);

/// How late the last audio of a context arrives in [`Broken::LateAudio`].
const LATE_AUDIO: Duration = Duration::from_millis(100);

/// Offset the [`Broken::LeadingSilence`] double adds to every word (alignment after silence).
const LEADING_SILENCE: Duration = Duration::from_millis(80);

/// How long [`Broken::KeepAlive`] keeps its task after the response is over. Longer than the
/// whole suite: the lingering tasks of earlier checks must not exit inside a later leak window
/// and offset the one the leak check is looking for (the alive-task count is runtime-wide).
const KEEP_ALIVE: Duration = Duration::from_secs(3_600);

fn stt(clock: &SharedClock) -> MockStt {
    MockStt::new(clock.clone()).with_timeout(CONTRACT_TIMEOUT)
}

fn llm(clock: &SharedClock) -> MockLlm {
    // Slow enough (50 ms per event) that a cancelled 200-event response is observable.
    MockLlm::new(clock.clone())
        .with_latency(LatencyProfile::ZERO, LatencyProfile::Fixed(50 * MS), 0)
        .with_timeout(CONTRACT_TIMEOUT)
}

/// Audio is paced like a real-time synthesizer (one 40 ms chunk per 40 ms), so a barge-in finds
/// only in-flight audio in the buffers.
fn tts_paced(clock: &SharedClock) -> MockTts {
    MockTts::new(clock.clone()).with_latency(
        LatencyProfile::ZERO,
        LatencyProfile::ZERO,
        LatencyProfile::Fixed(40 * MS),
        0,
    )
}

fn tts(clock: &SharedClock) -> MockTts {
    tts_paced(clock).with_timeout(CONTRACT_TIMEOUT)
}

fn failed(report: &ContractReport) -> Vec<&'static str> {
    report.failures().map(|c| c.name).collect()
}

#[tokio::test(start_paused = true)]
async fn stt_mock_passes_every_check() {
    let clock = SimClock::shared();
    let report = stt_contract(&stt(&clock)).await;
    assert_eq!(report.checks.len(), 7, "{report:#?}");
    assert!(report.passed(), "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn llm_mock_passes_every_check() {
    let clock = SimClock::shared();
    let report = llm_contract(&llm(&clock)).await;
    assert_eq!(report.checks.len(), 7, "{report:#?}");
    assert!(report.passed(), "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn tts_mock_passes_every_check() {
    let clock = SimClock::shared();
    let report = tts_contract(&tts(&clock)).await;
    assert_eq!(report.checks.len(), 8, "{report:#?}");
    assert!(report.passed(), "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn mock_connect_latency_above_the_deadline_is_a_timeout() {
    let clock = SimClock::shared();
    let start = clock.now();
    let stt = MockStt::new(clock.clone())
        .with_latency(
            LatencyProfile::Fixed(3_000 * MS),
            LatencyProfile::ZERO,
            LatencyProfile::ZERO,
            0,
        )
        .with_timeout(CONTRACT_TIMEOUT);
    let err = stt.connect(stt_config()).await.err().unwrap();
    assert!(
        matches!(
            err,
            ProviderError::Core(brisvox_core::Error::Timeout { .. })
        ),
        "{err:?}"
    );
    assert_eq!(
        clock.now().saturating_duration_since(start),
        CONTRACT_TIMEOUT
    );

    let start = clock.now();
    let llm = MockLlm::new(clock.clone())
        .with_connect_latency(LatencyProfile::Fixed(3_000 * MS), 0)
        .with_timeout(CONTRACT_TIMEOUT);
    let err = llm.stream(LlmRequest::default()).await.err().unwrap();
    assert!(
        matches!(
            err,
            ProviderError::Core(brisvox_core::Error::Timeout { .. })
        ),
        "{err:?}"
    );
    assert_eq!(
        clock.now().saturating_duration_since(start),
        CONTRACT_TIMEOUT
    );

    let start = clock.now();
    let tts = MockTts::new(clock.clone())
        .with_latency(
            LatencyProfile::Fixed(3_000 * MS),
            LatencyProfile::ZERO,
            LatencyProfile::ZERO,
            0,
        )
        .with_timeout(CONTRACT_TIMEOUT);
    let err = tts.connect(tts_config()).await.err().unwrap();
    assert!(
        matches!(
            err,
            ProviderError::Core(brisvox_core::Error::Timeout { .. })
        ),
        "{err:?}"
    );
    assert_eq!(
        clock.now().saturating_duration_since(start),
        CONTRACT_TIMEOUT
    );
}

fn stt_config() -> SttConfig {
    SttConfig {
        sample_rate: brisvox_core::frame::SampleRate::Hz16000,
        language: None,
    }
}

fn tts_config() -> TtsConfig {
    TtsConfig {
        sample_rate: brisvox_core::frame::SampleRate::Hz16000,
        voice: Arc::from("contract"),
    }
}

// --- Mocks without a deadline: only the timeout check may fail -------------------------------

#[tokio::test(start_paused = true)]
async fn stt_without_a_deadline_fails_only_the_timeout_check() {
    let clock = SimClock::shared();
    // The same mock, but its adapter never enforces the deadline.
    let report = stt_contract(&MockStt::new(clock.clone())).await;
    assert_eq!(failed(&report), vec!["timeout"], "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn llm_without_a_deadline_fails_only_the_timeout_check() {
    let clock = SimClock::shared();
    let report = llm_contract(&MockLlm::new(clock.clone())).await;
    assert_eq!(failed(&report), vec!["timeout"], "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn tts_without_a_deadline_fails_only_the_timeout_check() {
    let clock = SimClock::shared();
    let report = tts_contract(&tts_paced(&clock)).await;
    assert_eq!(failed(&report), vec!["timeout"], "{report:#?}");
}

// --- Broken doubles -------------------------------------------------------------------------

/// How a double breaks the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Broken {
    /// Ignores the caller's hang-up: the provider session outlives the caller and its output
    /// keeps flowing into a dead channel.
    Leaky,
    /// Maps errors to the wrong variants: refusals lose their status, drops become protocol errors.
    MisMapped,
    /// `warm()` fails.
    WarmFails,
    /// `warm()` never returns.
    WarmHangs,
    /// After `warm()`, every connect takes [`SLOW_WARM`] longer.
    WarmSlows,
    /// `connect` and `stream` never return.
    ConnectHangs,
    /// After a mid-session drop, the next connect hands out a session that is already closed.
    StaleSession,
    /// Interim transcripts are never forwarded (STT streaming).
    NoInterims,
    /// The hang-up does not stop the provider: it keeps emitting interims once a second for five
    /// seconds (STT cancel).
    EmitsAfterHangUp,
    /// The cancel channel is dead: a barge-in fails to send (TTS).
    DeadCancel,
    /// The cancel channel is open but never serviced: a barge-in is accepted and lost (TTS).
    IgnoresCancel,
    /// Cancels reach the provider one second late (TTS).
    DelayedCancel,
    /// Only the first audio chunk of each context is forwarded (TTS).
    Truncated,
    /// The last audio of a context arrives [`LATE_AUDIO`] after its `Flushed` (TTS).
    LateAudio,
    /// The first text delta is lost (LLM streaming).
    LosesFirstDelta,
    /// The response task lingers [`KEEP_ALIVE`] after `Done`, although the stream has closed.
    KeepAlive,
    /// The deadline covers the first event only: once an event has been delivered, a stalled
    /// provider is waited for forever (the `Timeout` error is swallowed).
    NoIdleTimeout,
    /// The harness counter `events_sent` never advances (LLM cancel).
    FrozenCounter,
    /// The cancel is acknowledged at once but never reaches the synthesizer, which keeps
    /// streaming audio of the cancelled context (TTS).
    AckThenKeepsStreaming,
    /// The session dies with the acknowledgement of a cancel: the events close and further
    /// input is refused (TTS).
    DiesAfterCancel,
    /// This many extra audio events of the cancelled context arrive after the cancel, before its
    /// acknowledgement (TTS).
    Stragglers(usize),
    /// Every word is reported [`LEADING_SILENCE`] late, as a provider with leading silence does
    /// (TTS). This is conformant.
    LeadingSilence,
    /// The harness declares `emits_words() == false` but the provider still emits word times
    /// (TTS): the suite must notice that the declaration is false.
    DeclaresNoWords,
}

/// State shared by a double's clones, set by the calls the checks make.
#[derive(Default)]
struct Flags {
    /// `warm()` has returned at least once since the last reset.
    warmed: AtomicBool,
    /// The last session dropped mid-stream: the next connect hands out a dead session.
    stale: AtomicBool,
}

fn mangle(mode: Broken, error: ProviderError) -> ProviderError {
    match (mode, error) {
        (Broken::MisMapped, ProviderError::Rejected { provider, .. }) => ProviderError::Rejected {
            provider,
            status: 500,
        },
        (Broken::MisMapped, ProviderError::Disconnected { provider }) => ProviderError::Protocol {
            provider,
            detail: "mangled".to_owned(),
        },
        (_, other) => other,
    }
}

// STT ----------------------------------------------------------------------------------------

#[derive(Clone)]
struct SttDouble {
    inner: MockStt,
    mode: Broken,
    time: SharedClock,
    flags: Arc<Flags>,
}

fn stt_double(clock: &SharedClock, mode: Broken) -> SttDouble {
    SttDouble {
        inner: stt(clock),
        mode,
        time: Arc::clone(clock),
        flags: Arc::default(),
    }
}

fn dead_stt_session() -> SttSession {
    let (input, input_rx) = mpsc::channel(1);
    drop(input_rx);
    let (events_tx, events) = mpsc::channel(1);
    drop(events_tx);
    SttSession { input, events }
}

/// Forwards `events`, applying the transcript and error faults of `mode`.
fn forward_stt(
    mode: Broken,
    flags: Arc<Flags>,
    mut events: mpsc::Receiver<SttEvent>,
) -> mpsc::Receiver<SttEvent> {
    let (tx, rx) = mpsc::channel(STT_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        let mut forwarded = 0_usize;
        while let Some(event) = events.recv().await {
            let event = match event {
                SttEvent::Transcript {
                    is_final: false, ..
                } if mode == Broken::NoInterims => {
                    continue;
                }
                SttEvent::Error(ProviderError::Core(_))
                    if mode == Broken::NoIdleTimeout && forwarded > 0 =>
                {
                    // No idle timer: the stall is waited out for good, the stream stays open.
                    std::future::pending::<()>().await;
                    return;
                }
                SttEvent::Error(error) => {
                    if mode == Broken::StaleSession
                        && matches!(error, ProviderError::Disconnected { .. })
                    {
                        flags.stale.store(true, Ordering::SeqCst);
                    }
                    SttEvent::Error(mangle(mode, error))
                }
                other => other,
            };
            if tx.send(event).await.is_err() {
                return;
            }
            forwarded += 1;
        }
    });
    rx
}

/// A session whose provider keeps talking after the caller hangs up: five ghost interims, one
/// [`GAP`] apart. The relay is otherwise transparent until the caller hangs up.
fn emits_after_hang_up(
    time: SharedClock,
    input: mpsc::Sender<SttInput>,
    mut events: mpsc::Receiver<SttEvent>,
) -> SttSession {
    let (caller_tx, mut caller_rx) = mpsc::channel(STT_CHANNEL_CAPACITY);
    let (tx, rx) = mpsc::channel(STT_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        let inner_input = input;
        let (mut caller_open, mut events_open) = (true, true);
        while caller_open {
            tokio::select! {
                message = caller_rx.recv() => match message {
                    Some(message) => {
                        if inner_input.send(message).await.is_err() {
                            return;
                        }
                    }
                    None => caller_open = false,
                },
                event = events.recv(), if events_open => match event {
                    Some(event) => {
                        if tx.send(event).await.is_err() {
                            return;
                        }
                    }
                    None => events_open = false,
                },
            }
        }
        for n in 0..5 {
            wait_for(&*time, GAP).await;
            let ghost = SttEvent::Transcript {
                text: Arc::from(format!("ghost {n}")),
                is_final: false,
                speech_final: false,
                from_finalize: false,
            };
            if tx.send(ghost).await.is_err() {
                return;
            }
        }
    });
    SttSession {
        input: caller_tx,
        events: rx,
    }
}

impl SttProvider for SttDouble {
    fn name(&self) -> &'static str {
        "double-stt"
    }

    async fn connect(&self, config: SttConfig) -> Result<SttSession> {
        if self.mode == Broken::ConnectHangs {
            return std::future::pending().await;
        }
        if self.mode == Broken::WarmSlows && self.flags.warmed.load(Ordering::SeqCst) {
            wait_for(&*self.time, SLOW_WARM).await;
        }
        if self.mode == Broken::StaleSession && self.flags.stale.swap(false, Ordering::SeqCst) {
            return Ok(dead_stt_session());
        }
        let SttSession { input, events } = self
            .inner
            .connect(config)
            .await
            .map_err(|e| mangle(self.mode, e))?;
        match self.mode {
            Broken::Leaky => {
                let (tx, rx) = mpsc::channel(STT_CHANNEL_CAPACITY);
                // Holds a sender of the provider's input for good: dropping the caller's input
                // never ends the provider session, and its events are forwarded forever.
                let keep = input.clone();
                let mut events = events;
                tokio::spawn(async move {
                    let _keep = keep;
                    while let Some(event) = events.recv().await {
                        let _ = tx.send(event).await;
                    }
                });
                Ok(SttSession { input, events: rx })
            }
            Broken::EmitsAfterHangUp => {
                Ok(emits_after_hang_up(Arc::clone(&self.time), input, events))
            }
            Broken::MisMapped
            | Broken::NoInterims
            | Broken::StaleSession
            | Broken::NoIdleTimeout => Ok(SttSession {
                input,
                events: forward_stt(self.mode, Arc::clone(&self.flags), events),
            }),
            _ => Ok(SttSession { input, events }),
        }
    }

    async fn warm(&self) -> Result<()> {
        match self.mode {
            Broken::WarmFails => Err(ProviderError::Rejected {
                provider: "double-stt",
                status: 503,
            }),
            Broken::WarmHangs => std::future::pending().await,
            _ => {
                self.flags.warmed.store(true, Ordering::SeqCst);
                self.inner.warm().await
            }
        }
    }
}

impl SttHarness for SttDouble {
    type P = SttDouble;

    fn provider(&self) -> Arc<SttDouble> {
        Arc::new(self.clone())
    }

    fn clock(&self) -> SharedClock {
        SttHarness::clock(&self.inner)
    }

    fn reset(&self) {
        SttHarness::reset(&self.inner);
        self.flags.warmed.store(false, Ordering::SeqCst);
    }

    fn script(&self, utterance: Utterance) {
        SttHarness::script(&self.inner, utterance);
    }

    fn inject(&self, fault: Fault) {
        SttHarness::inject(&self.inner, fault);
    }
}

// LLM ----------------------------------------------------------------------------------------

#[derive(Clone)]
struct LlmDouble {
    inner: MockLlm,
    mode: Broken,
    time: SharedClock,
    flags: Arc<Flags>,
}

fn llm_double(clock: &SharedClock, mode: Broken) -> LlmDouble {
    LlmDouble {
        inner: llm(clock),
        mode,
        time: Arc::clone(clock),
        flags: Arc::default(),
    }
}

fn closed_llm_stream() -> LlmStream {
    let (tx, rx) = mpsc::channel(1);
    drop(tx);
    LlmStream::new(rx)
}

/// Forwards `stream`, applying the faults of `mode`. The forwarder ends with the response.
fn forward_llm(
    mode: Broken,
    flags: Arc<Flags>,
    time: SharedClock,
    mut stream: LlmStream,
) -> LlmStream {
    let (tx, rx) = mpsc::channel(LLM_EVENT_CAPACITY);
    tokio::spawn(async move {
        let mut first_text = true;
        let mut forwarded = 0_usize;
        while let Some(event) = stream.next().await {
            let done = matches!(event, LlmEvent::Done { .. });
            if mode == Broken::NoIdleTimeout
                && forwarded > 0
                && matches!(event, LlmEvent::Error(ProviderError::Core(_)))
            {
                // No idle timer: the stall is waited out for good, the stream stays open.
                std::future::pending::<()>().await;
                return;
            }
            if mode == Broken::LosesFirstDelta
                && first_text
                && matches!(event, LlmEvent::TextDelta(_))
            {
                first_text = false;
                continue;
            }
            let event = match event {
                LlmEvent::Error(error) => {
                    if mode == Broken::StaleSession
                        && matches!(error, ProviderError::Disconnected { .. })
                    {
                        flags.stale.store(true, Ordering::SeqCst);
                    }
                    LlmEvent::Error(mangle(mode, error))
                }
                other => other,
            };
            if tx.send(event).await.is_err() {
                return;
            }
            forwarded += 1;
            if done {
                break;
            }
        }
        if mode == Broken::KeepAlive {
            // The stream is closed for the caller, but this task lingers.
            drop(tx);
            wait_for(&*time, KEEP_ALIVE).await;
        }
    });
    LlmStream::new(rx)
}

impl LlmProvider for LlmDouble {
    fn name(&self) -> &'static str {
        "double-llm"
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmStream> {
        if self.mode == Broken::ConnectHangs {
            return std::future::pending().await;
        }
        if self.mode == Broken::WarmSlows && self.flags.warmed.load(Ordering::SeqCst) {
            wait_for(&*self.time, SLOW_WARM).await;
        }
        if self.mode == Broken::StaleSession && self.flags.stale.swap(false, Ordering::SeqCst) {
            return Ok(closed_llm_stream());
        }
        let mut stream = self
            .inner
            .stream(request)
            .await
            .map_err(|e| mangle(self.mode, e))?;
        match self.mode {
            Broken::Leaky => {
                // Keeps pulling the response after the caller dropped it.
                let (tx, rx) = mpsc::channel(LLM_EVENT_CAPACITY);
                tokio::spawn(async move {
                    while let Some(event) = stream.next().await {
                        let _ = tx.send(event).await;
                    }
                });
                Ok(LlmStream::new(rx))
            }
            Broken::MisMapped
            | Broken::StaleSession
            | Broken::LosesFirstDelta
            | Broken::KeepAlive
            | Broken::NoIdleTimeout => Ok(forward_llm(
                self.mode,
                Arc::clone(&self.flags),
                Arc::clone(&self.time),
                stream,
            )),
            _ => Ok(stream),
        }
    }

    async fn warm(&self) -> Result<()> {
        match self.mode {
            Broken::WarmFails => Err(ProviderError::Rejected {
                provider: "double-llm",
                status: 503,
            }),
            Broken::WarmHangs => std::future::pending().await,
            _ => {
                self.flags.warmed.store(true, Ordering::SeqCst);
                self.inner.warm().await
            }
        }
    }
}

impl LlmHarness for LlmDouble {
    type P = LlmDouble;

    fn provider(&self) -> Arc<LlmDouble> {
        Arc::new(self.clone())
    }

    fn clock(&self) -> SharedClock {
        LlmHarness::clock(&self.inner)
    }

    fn reset(&self) {
        LlmHarness::reset(&self.inner);
        self.flags.warmed.store(false, Ordering::SeqCst);
    }

    fn script(&self, response: Response) {
        LlmHarness::script(&self.inner, response);
    }

    fn events_sent(&self) -> usize {
        if self.mode == Broken::FrozenCounter {
            return 0;
        }
        LlmHarness::events_sent(&self.inner)
    }
}

// TTS ----------------------------------------------------------------------------------------

#[derive(Clone)]
struct TtsDouble {
    inner: MockTts,
    mode: Broken,
    time: SharedClock,
    flags: Arc<Flags>,
}

fn tts_double(clock: &SharedClock, mode: Broken) -> TtsDouble {
    TtsDouble {
        inner: tts(clock),
        mode,
        time: Arc::clone(clock),
        flags: Arc::default(),
    }
}

fn dead_tts_session() -> TtsSession {
    let (input, input_rx) = mpsc::channel(1);
    drop(input_rx);
    let (cancel, cancel_rx) = mpsc::channel(1);
    drop(cancel_rx);
    let (events_tx, events) = mpsc::channel(1);
    drop(events_tx);
    TtsSession {
        input,
        cancel,
        events,
    }
}

/// Forwards `events`, applying the audio, error and flush faults of `mode`.
fn forward_tts(
    mode: Broken,
    flags: Arc<Flags>,
    time: SharedClock,
    mut events: mpsc::Receiver<TtsEvent>,
) -> mpsc::Receiver<TtsEvent> {
    let (tx, rx) = mpsc::channel(TTS_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        let mut audio_chunks = 0_usize;
        let mut forwarded = 0_usize;
        let mut held: Option<TtsEvent> = None;
        while let Some(event) = events.recv().await {
            let event = match event {
                TtsEvent::Error(ProviderError::Core(_))
                    if mode == Broken::NoIdleTimeout && forwarded > 0 =>
                {
                    // No idle timer: the stall is waited out for good, the stream stays open.
                    std::future::pending::<()>().await;
                    return;
                }
                TtsEvent::Word {
                    context,
                    word,
                    start,
                } if mode == Broken::LeadingSilence => TtsEvent::Word {
                    context,
                    word,
                    start: start + LEADING_SILENCE,
                },
                TtsEvent::Audio { .. } if mode == Broken::Truncated => {
                    audio_chunks += 1;
                    if audio_chunks > 1 {
                        continue;
                    }
                    event
                }
                TtsEvent::Audio { .. } if mode == Broken::LateAudio => {
                    // Hold each audio event back until the next one: the last of a context is
                    // then sent after its Flushed.
                    if let Some(previous) = held.replace(event)
                        && tx.send(previous).await.is_err()
                    {
                        return;
                    }
                    continue;
                }
                TtsEvent::Flushed { .. } if mode == Broken::LateAudio => {
                    if tx.send(event).await.is_err() {
                        return;
                    }
                    wait_for(&*time, LATE_AUDIO).await;
                    if let Some(late) = held.take()
                        && tx.send(late).await.is_err()
                    {
                        return;
                    }
                    continue;
                }
                TtsEvent::Error(error) => {
                    if mode == Broken::StaleSession
                        && matches!(error, ProviderError::Disconnected { .. })
                    {
                        flags.stale.store(true, Ordering::SeqCst);
                    }
                    TtsEvent::Error(mangle(mode, error))
                }
                other => other,
            };
            if tx.send(event).await.is_err() {
                return;
            }
            forwarded += 1;
        }
        if let Some(late) = held.take() {
            let _ = tx.send(late).await;
        }
    });
    rx
}

/// Accepts cancels into a channel that nothing reads: the barge-in is lost.
fn ignore_cancels(
    mut events: mpsc::Receiver<TtsEvent>,
) -> (mpsc::Sender<ContextId>, mpsc::Receiver<TtsEvent>) {
    let (cancel_tx, cancel_rx) = mpsc::channel(TTS_CANCEL_CAPACITY);
    let (tx, rx) = mpsc::channel(TTS_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        // Holds the cancel receiver open and never reads it.
        let _cancel_rx = cancel_rx;
        while let Some(event) = events.recv().await {
            if tx.send(event).await.is_err() {
                return;
            }
        }
    });
    (cancel_tx, rx)
}

/// Passes cancels on to `inner` one [`GAP`] late.
fn delay_cancels(time: SharedClock, inner: mpsc::Sender<ContextId>) -> mpsc::Sender<ContextId> {
    let (cancel_tx, mut cancel_rx) = mpsc::channel(TTS_CANCEL_CAPACITY);
    tokio::spawn(async move {
        while let Some(context) = cancel_rx.recv().await {
            wait_for(&*time, GAP).await;
            if inner.send(context).await.is_err() {
                return;
            }
        }
    });
    cancel_tx
}

/// Wraps a session so that its cancels misbehave the way `mode` says. Everything else passes
/// through unchanged.
fn misbehaving_cancels(mode: Broken, session: TtsSession) -> TtsSession {
    let TtsSession {
        input,
        cancel,
        mut events,
    } = session;
    let (caller_input, mut caller_input_rx) = mpsc::channel(TTS_CHANNEL_CAPACITY);
    let (caller_cancel, mut caller_cancel_rx) = mpsc::channel(TTS_CANCEL_CAPACITY);
    let (tx, rx) = mpsc::channel(TTS_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        let mut cancel_open = true;
        loop {
            tokio::select! {
                biased;
                context = caller_cancel_rx.recv(), if cancel_open => {
                    let Some(context) = context else {
                        cancel_open = false;
                        continue;
                    };
                    match mode {
                        Broken::AckThenKeepsStreaming => {
                            // Acknowledged, but the synthesizer never hears of it.
                            if tx.send(TtsEvent::Cancelled { context }).await.is_err() {
                                return;
                            }
                        }
                        Broken::Stragglers(extra) => {
                            for _ in 0..extra {
                                let audio = TtsEvent::Audio {
                                    context,
                                    audio: Bytes::from_static(&[0_u8; 64]),
                                };
                                if tx.send(audio).await.is_err() {
                                    return;
                                }
                            }
                            if cancel.send(context).await.is_err() {
                                return;
                            }
                        }
                        _ => {
                            if cancel.send(context).await.is_err() {
                                return;
                            }
                        }
                    }
                }
                message = caller_input_rx.recv() => match message {
                    Some(message) => {
                        if input.send(message).await.is_err() {
                            return;
                        }
                    }
                    None => return,
                },
                event = events.recv() => match event {
                    Some(event) => {
                        let acknowledged = matches!(event, TtsEvent::Cancelled { .. });
                        if tx.send(event).await.is_err() {
                            return;
                        }
                        if acknowledged && mode == Broken::DiesAfterCancel {
                            return;
                        }
                    }
                    None => return,
                },
            }
        }
    });
    TtsSession {
        input: caller_input,
        cancel: caller_cancel,
        events: rx,
    }
}

impl TtsProvider for TtsDouble {
    fn name(&self) -> &'static str {
        "double-tts"
    }

    async fn connect(&self, config: TtsConfig) -> Result<TtsSession> {
        if self.mode == Broken::ConnectHangs {
            return std::future::pending().await;
        }
        if self.mode == Broken::WarmSlows && self.flags.warmed.load(Ordering::SeqCst) {
            wait_for(&*self.time, SLOW_WARM).await;
        }
        if self.mode == Broken::StaleSession && self.flags.stale.swap(false, Ordering::SeqCst) {
            return Ok(dead_tts_session());
        }
        let TtsSession {
            input,
            cancel,
            events,
        } = self
            .inner
            .connect(config)
            .await
            .map_err(|e| mangle(self.mode, e))?;
        match self.mode {
            Broken::Leaky => {
                let (tx, rx) = mpsc::channel(TTS_CHANNEL_CAPACITY);
                let keep = input.clone();
                let mut events = events;
                tokio::spawn(async move {
                    let _keep = keep;
                    while let Some(event) = events.recv().await {
                        let _ = tx.send(event).await;
                    }
                });
                Ok(TtsSession {
                    input,
                    cancel,
                    events: rx,
                })
            }
            Broken::MisMapped
            | Broken::StaleSession
            | Broken::Truncated
            | Broken::LateAudio
            | Broken::NoIdleTimeout
            | Broken::LeadingSilence => Ok(TtsSession {
                input,
                cancel,
                events: forward_tts(
                    self.mode,
                    Arc::clone(&self.flags),
                    Arc::clone(&self.time),
                    events,
                ),
            }),
            Broken::DeadCancel => {
                // The receiver is dropped at once: every barge-in fails to send.
                let (dead, _) = mpsc::channel(TTS_CANCEL_CAPACITY);
                Ok(TtsSession {
                    input,
                    cancel: dead,
                    events,
                })
            }
            Broken::IgnoresCancel => {
                let (cancel, events) = ignore_cancels(events);
                Ok(TtsSession {
                    input,
                    cancel,
                    events,
                })
            }
            Broken::AckThenKeepsStreaming | Broken::DiesAfterCancel | Broken::Stragglers(_) => {
                Ok(misbehaving_cancels(
                    self.mode,
                    TtsSession {
                        input,
                        cancel,
                        events,
                    },
                ))
            }
            Broken::DelayedCancel => Ok(TtsSession {
                input,
                cancel: delay_cancels(Arc::clone(&self.time), cancel),
                events,
            }),
            _ => Ok(TtsSession {
                input,
                cancel,
                events,
            }),
        }
    }

    async fn warm(&self) -> Result<()> {
        match self.mode {
            Broken::WarmFails => Err(ProviderError::Rejected {
                provider: "double-tts",
                status: 503,
            }),
            Broken::WarmHangs => std::future::pending().await,
            _ => {
                self.flags.warmed.store(true, Ordering::SeqCst);
                self.inner.warm().await
            }
        }
    }
}

impl TtsHarness for TtsDouble {
    type P = TtsDouble;

    fn provider(&self) -> Arc<TtsDouble> {
        Arc::new(self.clone())
    }

    fn clock(&self) -> SharedClock {
        TtsHarness::clock(&self.inner)
    }

    fn emits_words(&self) -> bool {
        self.mode != Broken::DeclaresNoWords
    }

    fn reset(&self) {
        TtsHarness::reset(&self.inner);
        self.flags.warmed.store(false, Ordering::SeqCst);
    }

    fn inject(&self, fault: Fault) {
        TtsHarness::inject(&self.inner, fault);
    }
}

// --- Checks against the doubles ---------------------------------------------------------------

async fn run_stt(mode: Broken) -> ContractReport {
    let clock = SimClock::shared();
    stt_contract(&stt_double(&clock, mode)).await
}

async fn run_llm(mode: Broken) -> ContractReport {
    let clock = SimClock::shared();
    llm_contract(&llm_double(&clock, mode)).await
}

async fn run_tts(mode: Broken) -> ContractReport {
    let clock = SimClock::shared();
    tts_contract(&tts_double(&clock, mode)).await
}

#[tokio::test(start_paused = true)]
async fn leaky_stt_fails_cancel_and_leak_checks() {
    let report = run_stt(Broken::Leaky).await;
    let names = failed(&report);
    assert!(names.contains(&"cancel"), "{report:#?}");
    assert!(names.contains(&"no_task_leak"), "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn leaky_llm_fails_the_cancel_and_leak_checks() {
    // The leaked task keeps pulling the long hang-up response, so it outlives the leak bound.
    let report = run_llm(Broken::Leaky).await;
    assert_eq!(
        failed(&report),
        vec!["cancel", "no_task_leak"],
        "{report:#?}"
    );
}

#[tokio::test(start_paused = true)]
async fn leaky_tts_fails_the_leak_check() {
    let report = run_tts(Broken::Leaky).await;
    assert_eq!(failed(&report), vec!["no_task_leak"], "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn mis_mapped_errors_fail_the_error_mapping_check() {
    // A mis-mapped drop (Disconnected -> Protocol) also fails the reconnect check, so the
    // assertion is that error_mapping is among the failures, not that it is the only one.
    assert!(failed(&run_stt(Broken::MisMapped).await).contains(&"error_mapping"));
    assert!(failed(&run_llm(Broken::MisMapped).await).contains(&"error_mapping"));
    assert!(failed(&run_tts(Broken::MisMapped).await).contains(&"error_mapping"));
}

#[tokio::test(start_paused = true)]
async fn failing_warm_up_fails_the_warm_check() {
    assert_eq!(
        failed(&run_stt(Broken::WarmFails).await),
        vec!["warm"],
        "stt"
    );
    assert_eq!(
        failed(&run_llm(Broken::WarmFails).await),
        vec!["warm"],
        "llm"
    );
    assert_eq!(
        failed(&run_tts(Broken::WarmFails).await),
        vec!["warm"],
        "tts"
    );
}

#[tokio::test(start_paused = true)]
async fn dead_cancel_channel_fails_the_tts_cancel_check() {
    let report = run_tts(Broken::DeadCancel).await;
    assert_eq!(failed(&report), vec!["cancel"], "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn ignored_cancel_fails_the_tts_cancel_check() {
    // The cancel is accepted into a channel that nothing reads: the audio never stops.
    let report = run_tts(Broken::IgnoresCancel).await;
    assert_eq!(failed(&report), vec!["cancel"], "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn late_cancel_fails_the_tts_cancel_check() {
    let report = run_tts(Broken::DelayedCancel).await;
    assert_eq!(failed(&report), vec!["cancel"], "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn transcripts_after_a_hang_up_fail_the_stt_cancel_check() {
    let report = run_stt(Broken::EmitsAfterHangUp).await;
    assert!(failed(&report).contains(&"cancel"), "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn truncated_audio_fails_audio_and_words() {
    let report = run_tts(Broken::Truncated).await;
    assert!(failed(&report).contains(&"audio_and_words"), "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn audio_after_flushed_fails_audio_and_words_and_flush() {
    let report = run_tts(Broken::LateAudio).await;
    let names = failed(&report);
    assert!(names.contains(&"audio_and_words"), "{report:#?}");
    assert!(names.contains(&"flush"), "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn missing_interims_fail_stt_streaming() {
    let report = run_stt(Broken::NoInterims).await;
    assert!(failed(&report).contains(&"streaming"), "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn a_lost_text_delta_fails_llm_streaming() {
    let report = run_llm(Broken::LosesFirstDelta).await;
    assert!(failed(&report).contains(&"streaming"), "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn a_task_lingering_after_done_fails_llm_no_task_leak() {
    let report = run_llm(Broken::KeepAlive).await;
    assert!(failed(&report).contains(&"no_task_leak"), "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn a_stale_session_after_a_drop_fails_reconnect() {
    assert!(failed(&run_stt(Broken::StaleSession).await).contains(&"reconnect"));
    assert!(failed(&run_llm(Broken::StaleSession).await).contains(&"reconnect"));
    assert!(failed(&run_tts(Broken::StaleSession).await).contains(&"reconnect"));
}

#[tokio::test(start_paused = true)]
async fn a_warm_that_never_returns_fails_only_warm() {
    assert_eq!(
        failed(&run_stt(Broken::WarmHangs).await),
        vec!["warm"],
        "stt"
    );
    assert_eq!(
        failed(&run_llm(Broken::WarmHangs).await),
        vec!["warm"],
        "llm"
    );
    assert_eq!(
        failed(&run_tts(Broken::WarmHangs).await),
        vec!["warm"],
        "tts"
    );
}

#[tokio::test(start_paused = true)]
async fn a_connect_that_never_returns_is_reported_not_hung() {
    // Every check connects, so every check fails; the point is that all of them report, and
    // that the stalled connect is among them as timeout and warm.
    let stt = run_stt(Broken::ConnectHangs).await;
    assert_eq!(stt.checks.len(), 7, "{stt:#?}");
    assert!(failed(&stt).contains(&"timeout"), "{stt:#?}");
    assert!(failed(&stt).contains(&"warm"), "{stt:#?}");
    let llm = run_llm(Broken::ConnectHangs).await;
    assert_eq!(llm.checks.len(), 7, "{llm:#?}");
    assert!(failed(&llm).contains(&"timeout"), "{llm:#?}");
    assert!(failed(&llm).contains(&"warm"), "{llm:#?}");
    let tts = run_tts(Broken::ConnectHangs).await;
    assert_eq!(tts.checks.len(), 8, "{tts:#?}");
    assert!(failed(&tts).contains(&"timeout"), "{tts:#?}");
    assert!(failed(&tts).contains(&"warm"), "{tts:#?}");
}

#[tokio::test(start_paused = true)]
async fn a_warm_that_slows_the_next_connect_fails_only_warm() {
    assert_eq!(
        failed(&run_stt(Broken::WarmSlows).await),
        vec!["warm"],
        "stt"
    );
    assert_eq!(
        failed(&run_llm(Broken::WarmSlows).await),
        vec!["warm"],
        "llm"
    );
    assert_eq!(
        failed(&run_tts(Broken::WarmSlows).await),
        vec!["warm"],
        "tts"
    );
}

#[tokio::test(start_paused = true)]
async fn a_shorter_own_deadline_fails_only_the_timeout_check() {
    // The adapter gives up before the 2 s contract deadline: at 500 ms it cuts off a slow but
    // healthy provider (and fails a stalled call early); at 1.5 s it tolerates the slow provider
    // and is caught by the lower bound on the stalled call alone.
    for early in [CONTRACT_TIMEOUT / 4, CONTRACT_TIMEOUT * 3 / 4] {
        let clock = SimClock::shared();
        let report = stt_contract(&MockStt::new(clock.clone()).with_timeout(early)).await;
        assert_eq!(
            failed(&report),
            vec!["timeout"],
            "stt {early:?} {report:#?}"
        );
        let report = llm_contract(
            &MockLlm::new(clock.clone())
                .with_latency(LatencyProfile::ZERO, LatencyProfile::Fixed(50 * MS), 0)
                .with_timeout(early),
        )
        .await;
        assert_eq!(
            failed(&report),
            vec!["timeout"],
            "llm {early:?} {report:#?}"
        );
        let report = tts_contract(&tts_paced(&clock).with_timeout(early)).await;
        assert_eq!(
            failed(&report),
            vec!["timeout"],
            "tts {early:?} {report:#?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_deadline_that_covers_only_the_first_event_fails_only_the_timeout_check() {
    assert_eq!(
        failed(&run_stt(Broken::NoIdleTimeout).await),
        vec!["timeout"],
        "stt"
    );
    assert_eq!(
        failed(&run_llm(Broken::NoIdleTimeout).await),
        vec!["timeout"],
        "llm"
    );
    assert_eq!(
        failed(&run_tts(Broken::NoIdleTimeout).await),
        vec!["timeout"],
        "tts"
    );
}

#[tokio::test(start_paused = true)]
async fn a_stuck_events_sent_counter_fails_the_llm_cancel_check() {
    let report = run_llm(Broken::FrozenCounter).await;
    assert_eq!(failed(&report), vec!["cancel"], "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn audio_after_the_cancel_ack_fails_the_tts_cancel_check() {
    let report = run_tts(Broken::AckThenKeepsStreaming).await;
    assert_eq!(failed(&report), vec!["cancel"], "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn a_session_that_dies_after_a_cancel_fails_the_tts_cancel_check() {
    let report = run_tts(Broken::DiesAfterCancel).await;
    assert_eq!(failed(&report), vec!["cancel"], "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn the_tts_cancel_slack_is_two_events() {
    // Two stragglers before the acknowledgement are tolerated, three are not.
    let report = run_tts(Broken::Stragglers(CANCEL_SLACK_EVENTS)).await;
    assert!(report.passed(), "{report:#?}");
    let report = run_tts(Broken::Stragglers(CANCEL_SLACK_EVENTS + 1)).await;
    assert_eq!(failed(&report), vec!["cancel"], "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn a_synthesizer_faster_than_real_time_passes_the_tts_suite() {
    // Audio is not paced: the whole utterance is queued in the event channel when the first
    // chunk is read. Output already buffered must not count against the cancel.
    let clock = SimClock::shared();
    let burst = MockTts::new(clock.clone()).with_timeout(CONTRACT_TIMEOUT);
    let report = tts_contract(&burst).await;
    assert!(report.passed(), "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn words_after_leading_silence_pass_the_tts_suite() {
    // Alignment that starts 80 ms into the audio is conformant: the first word need not be at 0.
    let report = run_tts(Broken::LeadingSilence).await;
    assert!(report.passed(), "{report:#?}");
}

#[tokio::test(start_paused = true)]
async fn word_events_from_a_provider_that_declares_none_fail_the_word_checks() {
    // The declaration `emits_words() == false` is checked, not trusted: a provider that
    // reports word times anyway fails both checks that look at them.
    let report = run_tts(Broken::DeclaresNoWords).await;
    let names = failed(&report);
    assert!(names.contains(&"audio_and_words"), "{report:#?}");
    assert!(names.contains(&"flush"), "{report:#?}");
}
