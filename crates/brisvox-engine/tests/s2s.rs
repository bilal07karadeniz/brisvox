//! The speech-to-speech stage in a whole call: a real `TransportSession` over the
//! loopback transport, the production assembly (`SessionBuilder::new_s2s`, `run_call`), a mock
//! S2S provider that the test scripts event by event.
//!
//! What these tests prove: the epoch and barge-in machinery of the stage (clear, interrupt, no
//! stale audio), the truncation of the model's history to what the caller heard (computed from
//! the playout book), the tool round trip through the `ToolEngine` (read-only and side-effect
//! semantics), the one end mark per turn, local turn detection, and the failure paths
//! (reconnect, connect failure, connect timeout, stale events).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::disallowed_methods
)]

mod common;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_agent::tools::{
    EngineConfig as ToolEngineConfig, SessionId, ToolEngine, ToolKind, ToolRegistry, ToolSpec,
};
use brisvox_core::frame::SampleRate;
use brisvox_core::{SharedClock, SimClock, TimelineRecorder};
use brisvox_engine::sim::{LoopbackCall, energy_vad_setup, open_loopback};
use brisvox_engine::{
    Call, EngineConfig, EngineEvent, EngineEvents, EventStream, InterruptionConfig, Offload,
    S2sOptions, S2sStatsSnapshot, S2sTurnMode, SessionBuilder, run_call,
};
use brisvox_providers::{
    Error as ProviderError, S2sConfig, S2sControl, S2sEvent, S2sHistoryItem, S2sProvider,
    S2sResponseStatus, S2sSession,
};
use brisvox_sim::input::{InputSink, InputTarget, SendFuture};
use brisvox_sim::scenario::{Scenario, play_input};
use brisvox_sim::session::SimSession;
use brisvox_transport::{ClientEvent, LoopbackListener};
use bytes::Bytes;
use common::llm::{SlowTool, ToolProbe, lookup_tools_returning};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MS: Duration = Duration::from_millis(1);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------------------------
// The mock provider
// ---------------------------------------------------------------------------------------------

/// What the next `connect` does.
#[derive(Debug, Clone, Copy)]
enum Plan {
    Open,
    Refuse,
    /// Never answers: the stage's connect timeout must end it.
    Hang,
    /// Opens after this long (a slow connect).
    Slow(Duration),
}

/// One session the stage opened.
struct MockSession {
    config: S2sConfig,
    events: Mutex<Option<mpsc::Sender<S2sEvent>>>,
    controls: Arc<Mutex<Vec<S2sControl>>>,
    audio_frames: Arc<AtomicUsize>,
    audio_bytes: Arc<AtomicUsize>,
    /// The engine dropped the session's control sender (the session was closed).
    control_closed: Arc<AtomicBool>,
}

impl MockSession {
    async fn send(&self, event: S2sEvent) {
        let sender = lock(&self.events).clone().expect("the session was closed");
        sender.send(event).await.unwrap();
    }

    /// Ends the session the way a lost connection does: the event stream closes.
    fn die(&self) {
        lock(&self.events).take();
    }

    fn controls(&self) -> Vec<S2sControl> {
        lock(&self.controls).clone()
    }

    fn truncations(&self) -> Vec<(Arc<str>, Duration)> {
        self.controls()
            .into_iter()
            .filter_map(|c| match c {
                S2sControl::Truncate { item, audio_end } => Some((item, audio_end)),
                _ => None,
            })
            .collect()
    }

    fn count(&self, pick: impl Fn(&S2sControl) -> bool) -> usize {
        self.controls().iter().filter(|c| pick(c)).count()
    }
}

#[derive(Clone, Default)]
struct Mock {
    plan: Arc<Mutex<VecDeque<Plan>>>,
    sessions: Arc<Mutex<Vec<Arc<MockSession>>>>,
    connects: Arc<AtomicUsize>,
    /// What `session_limit` reports.
    limit: Option<Duration>,
}

impl Mock {
    fn with_plan(plan: impl IntoIterator<Item = Plan>) -> Self {
        let mock = Self::default();
        lock(&mock.plan).extend(plan);
        mock
    }

    fn session(&self, index: usize) -> Arc<MockSession> {
        Arc::clone(&lock(&self.sessions)[index])
    }

    fn opened(&self) -> usize {
        lock(&self.sessions).len()
    }
}

impl S2sProvider for Mock {
    fn name(&self) -> &'static str {
        "mock-s2s"
    }

    fn session_limit(&self) -> Option<Duration> {
        self.limit
    }

    async fn connect(&self, config: S2sConfig) -> Result<S2sSession, ProviderError> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        let plan = lock(&self.plan).pop_front().unwrap_or(Plan::Open);
        match plan {
            Plan::Refuse => {
                return Err(ProviderError::Rejected {
                    provider: "mock-s2s",
                    status: 503,
                });
            }
            Plan::Hang => std::future::pending().await,
            Plan::Slow(delay) => tokio::time::sleep(delay).await,
            Plan::Open => {}
        }
        let (audio_tx, mut audio_rx) = mpsc::channel::<Bytes>(64);
        let (control_tx, mut control_rx) = mpsc::channel::<S2sControl>(32);
        let (event_tx, event_rx) = mpsc::channel::<S2sEvent>(1024);
        let session = Arc::new(MockSession {
            config,
            events: Mutex::new(Some(event_tx)),
            controls: Arc::default(),
            audio_frames: Arc::default(),
            audio_bytes: Arc::default(),
            control_closed: Arc::default(),
        });
        let (frames, bytes) = (
            Arc::clone(&session.audio_frames),
            Arc::clone(&session.audio_bytes),
        );
        tokio::spawn(async move {
            while let Some(chunk) = audio_rx.recv().await {
                frames.fetch_add(1, Ordering::SeqCst);
                bytes.fetch_add(chunk.len(), Ordering::SeqCst);
            }
        });
        let controls = Arc::clone(&session.controls);
        let closed = Arc::clone(&session.control_closed);
        tokio::spawn(async move {
            while let Some(control) = control_rx.recv().await {
                lock(&controls).push(control);
            }
            closed.store(true, Ordering::SeqCst);
        });
        lock(&self.sessions).push(session);
        Ok(S2sSession {
            audio: audio_tx,
            control: control_tx,
            events: event_rx,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The rig
// ---------------------------------------------------------------------------------------------

struct Caller {
    caller: tokio::sync::Mutex<brisvox_transport::LoopbackCaller>,
}

impl InputTarget for Caller {
    fn send(&self, frame: brisvox_core::frame::AudioFrame) -> SendFuture<'_> {
        Box::pin(async move {
            self.caller
                .lock()
                .await
                .send_audio(frame.data().clone(), frame.sample_rate())
                .await
                .is_ok()
        })
    }
}

struct Rig {
    mock: Mock,
    clock: SharedClock,
    caller: Arc<Caller>,
    cancel: CancellationToken,
    call: Call,
    seen: Arc<Mutex<Vec<ClientEvent>>>,
    events: EventStream,
    probe: Arc<ToolProbe>,
}

struct Opts {
    turn: S2sTurnMode,
    plan: Vec<Plan>,
    /// A tool named `lookup` of this kind and delay.
    tool: Option<(ToolKind, Duration)>,
    max_buffered_audio: Duration,
    config: EngineConfig,
    /// The provider's `session_limit`.
    session_limit: Option<Duration>,
    /// `S2sOptions::input_transcription`.
    input_transcription: bool,
    /// The `lookup` tool asks for the user's confirmation (`ToolSpec::confirm`).
    confirm: bool,
}

impl Default for Opts {
    fn default() -> Self {
        let mut config = EngineConfig::DEFAULT;
        config.lead_margin = Duration::ZERO;
        config.interruption = InterruptionConfig::OFF;
        Self {
            turn: S2sTurnMode::Provider,
            plan: Vec::new(),
            tool: None,
            max_buffered_audio: Duration::from_secs(60),
            config,
            session_limit: None,
            input_transcription: true,
            confirm: false,
        }
    }
}

impl Rig {
    async fn start(opts: Opts) -> Self {
        let sim_clock = SimClock::new();
        let clock: SharedClock = Arc::new(sim_clock);
        let sim = SimSession::new(clock.clone());
        let LoopbackCall {
            session,
            client,
            cancel,
        } = open_loopback(&clock, 65_536).await.unwrap();
        let offload = Offload::Inline(clock.clone());
        let mut mock = Mock::with_plan(opts.plan);
        mock.limit = opts.session_limit;
        let probe = Arc::new(ToolProbe::default());
        let events = EngineEvents::new(1024);
        let stream = events.subscribe();
        let mut builder = SessionBuilder::new_s2s(
            opts.config,
            clock.clone(),
            TimelineRecorder::new(),
            offload.clone(),
            Arc::new(mock.clone()),
            S2sOptions {
                turn: opts.turn,
                input_transcription: opts.input_transcription,
                max_buffered_audio: opts.max_buffered_audio,
            },
        )
        .unwrap()
        .with_system_prompt("You are a test agent.")
        .with_voice("marin")
        .with_events(events);
        if opts.turn == S2sTurnMode::Local {
            builder = builder.with_vad(energy_vad_setup(offload, &sim.vad, &opts.config));
        }
        if let Some((kind, delay)) = opts.tool {
            let (registry, engine) = if opts.confirm {
                gated_lookup_tools(&sim_clock, &probe, delay)
            } else {
                lookup_tools_returning(
                    &sim_clock,
                    &probe,
                    kind,
                    delay,
                    None,
                    json!({"answer": "sunny"}),
                )
            };
            builder = builder.with_tools(registry, engine);
        }
        let call = run_call(session, builder, &cancel).await.unwrap();
        let (caller, listener) = client.split();
        let seen = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(collect(listener, Arc::clone(&seen)));
        Self {
            mock,
            clock,
            caller: Arc::new(Caller {
                caller: tokio::sync::Mutex::new(caller),
            }),
            cancel,
            call,
            seen,
            events: stream,
            probe,
        }
    }

    /// Plays `scenario` to the call (its length passes in simulated time).
    async fn play(&self, scenario: &Scenario) {
        let origin = self.clock.now();
        play_input(
            scenario,
            self.clock.clone(),
            origin,
            SampleRate::Hz16000,
            InputSink::target(self.caller.clone()),
            &self.cancel,
        )
        .await
        .unwrap();
    }

    async fn pass(&self, time: Duration) {
        self.clock.sleep(time).await;
    }

    /// Waits (in simulated time) until `cond` holds.
    async fn until(&self, what: &str, cond: impl Fn(&Rig) -> bool) {
        for _ in 0..4_000 {
            if cond(self) {
                return;
            }
            self.clock.sleep(5 * MS).await;
        }
        panic!("timed out waiting for {what}");
    }

    fn client(&self) -> Vec<ClientEvent> {
        lock(&self.seen).clone()
    }

    fn stats(&self) -> S2sStatsSnapshot {
        self.call.handles().s2s.snapshot()
    }

    fn marks(&self) -> usize {
        self.client()
            .iter()
            .filter(|e| matches!(e, ClientEvent::Mark { .. }))
            .count()
    }

    fn drain_events(&mut self) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        while let Some(Some(event)) = futures_lite_now(self.events.recv()) {
            out.push(event);
        }
        out
    }
}

/// Polls `future` once: `Some(output)` when it is ready now.
fn futures_lite_now<F: std::future::Future>(future: F) -> Option<F::Output> {
    use std::task::{Context, Poll, Waker};
    let mut future = std::pin::pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(output) => Some(output),
        Poll::Pending => None,
    }
}

async fn collect(mut listener: LoopbackListener, seen: Arc<Mutex<Vec<ClientEvent>>>) {
    while let Some(event) = listener.recv().await {
        lock(&seen).push(event);
    }
}

fn speech() -> Scenario {
    Scenario::new()
        .silence(100 * MS)
        .speak("what time is it", 800 * MS)
        .with_tail(600 * MS)
}

/// `ms` of model audio at 24 kHz (a constant tone, never silence).
fn pcm(ms: u64) -> Bytes {
    let samples = usize::try_from(ms * 24).unwrap();
    let mut data = Vec::with_capacity(samples * 2);
    for i in 0..samples {
        let sample: i16 = if (i / 12) % 2 == 0 { 3000 } else { -3000 };
        data.extend_from_slice(&sample.to_le_bytes());
    }
    Bytes::from(data)
}

fn id(text: &str) -> Arc<str> {
    Arc::from(text)
}

async fn respond(session: &MockSession, response: &str, item: &str, audio_ms: u64) {
    session
        .send(S2sEvent::ResponseStarted {
            response: id(response),
        })
        .await;
    let mut left = audio_ms;
    while left > 0 {
        let chunk = left.min(100);
        session
            .send(S2sEvent::Audio {
                response: id(response),
                item: id(item),
                pcm: pcm(chunk),
            })
            .await;
        left -= chunk;
    }
    session
        .send(S2sEvent::AssistantTranscript {
            response: id(response),
            item: id(item),
            text: id("one two three four five six seven eight nine ten"),
            is_final: true,
        })
        .await;
    session
        .send(S2sEvent::AudioDone {
            response: id(response),
            item: id(item),
        })
        .await;
    session
        .send(S2sEvent::ResponseDone {
            response: id(response),
            status: S2sResponseStatus::Completed,
        })
        .await;
}

fn audio_epochs_after(events: &[ClientEvent], from: usize) -> Vec<u32> {
    events
        .iter()
        .skip(from)
        .filter_map(|e| match e {
            ClientEvent::Audio { frame, .. } => Some(frame.epoch().get()),
            _ => None,
        })
        .collect()
}

fn client_audio_ms(events: &[ClientEvent]) -> u128 {
    events
        .iter()
        .filter_map(|e| match e {
            ClientEvent::Audio { frame, .. } => Some(frame.duration().as_millis()),
            _ => None,
        })
        .sum()
}

// ---------------------------------------------------------------------------------------------
// Setup and the happy path
// ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_session_is_configured_from_the_builder_and_the_callers_audio_reaches_it() {
    let rig = Rig::start(Opts {
        tool: Some((ToolKind::ReadOnly, 10 * MS)),
        ..Opts::default()
    })
    .await;
    rig.play(&speech()).await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let session = rig.mock.session(0);
    assert_eq!(&*session.config.instructions, "You are a test agent.");
    assert_eq!(session.config.voice.as_deref(), Some("marin"));
    assert_eq!(session.config.tools.len(), 1);
    assert_eq!(&*session.config.tools[0].name, "lookup");
    assert_eq!(
        session.config.turn_detection,
        brisvox_providers::S2sTurnDetection::Provider
    );
    assert_eq!(session.config.input_rate, SampleRate::Hz24000);
    assert!(session.config.input_transcription);
    // The caller's 16 kHz audio arrives as 20 ms frames at the provider's 24 kHz (960 bytes).
    let frames = session.audio_frames.load(Ordering::SeqCst);
    assert!(frames > 40, "{frames} frames");
    assert_eq!(session.audio_bytes.load(Ordering::SeqCst), frames * 960);
    assert_eq!(rig.stats().sessions_opened, 1);
}

#[tokio::test(start_paused = true)]
async fn a_response_is_played_in_one_epoch_and_ended_with_one_mark() {
    let mut rig = Rig::start(Opts::default()).await;
    rig.play(&speech()).await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let session = rig.mock.session(0);
    session.send(S2sEvent::SpeechStarted).await;
    session.send(S2sEvent::SpeechStopped).await;
    session
        .send(S2sEvent::UserTranscript {
            text: id("what time is it"),
        })
        .await;
    respond(&session, "resp_1", "item_1", 600).await;
    rig.until("the end mark", |r| r.marks() == 1).await;
    rig.pass(100 * MS).await;

    let client = rig.client();
    let epochs = audio_epochs_after(&client, 0);
    assert!(!epochs.is_empty());
    assert!(epochs.iter().all(|&e| e == 1), "{epochs:?}");
    // 600 ms of speech, within a frame or two.
    let ms = client_audio_ms(&client);
    assert!((580..=620).contains(&ms), "{ms} ms");
    assert_eq!(rig.marks(), 1, "one end mark");
    let stats = rig.stats();
    assert_eq!(stats.barge_ins, 0);
    assert_eq!(stats.utterances, 1);
    assert_eq!(stats.truncations_sent, 0, "nothing was cut");
    // The conversation mirror: the caller's words, then the spoken text.
    {
        let context = rig.call.handles().context.lock().unwrap();
        let texts: Vec<String> = context
            .messages()
            .iter()
            .map(|m| m.text().to_owned())
            .collect();
        assert!(texts.iter().any(|t| t == "what time is it"), "{texts:?}");
        assert!(
            texts.iter().any(|t| t.starts_with("one two three")),
            "{texts:?}"
        );
    }
    let events = rig.drain_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::UserStartedSpeaking { .. })),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::BotStartedSpeaking { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::BotStoppedSpeaking { .. }))
    );
    assert_eq!(rig.call.handles().unnoted_clear_cuts(), 0);
}

// ---------------------------------------------------------------------------------------------
// Barge-in
// ---------------------------------------------------------------------------------------------

/// The core contract: the caller barges in while the model's audio plays. The transport is
/// cleared, no stale audio follows, and the model's item is cut to the duration the caller heard
/// (taken from the playout book), not to what the model generated (3 s, ahead of real time).
#[tokio::test(start_paused = true)]
async fn barge_in_clears_the_client_and_truncates_the_item_to_what_was_heard() {
    let rig = Rig::start(Opts::default()).await;
    rig.play(&speech()).await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let session = rig.mock.session(0);
    session.send(S2sEvent::SpeechStarted).await;
    session.send(S2sEvent::SpeechStopped).await;
    respond_without_end(&session, "resp_1", "item_1", 3_000).await;
    rig.pass(1_000 * MS).await;

    session.send(S2sEvent::SpeechStarted).await;
    rig.until("the truncation", |_| !session.truncations().is_empty())
        .await;

    let (item, heard) = session.truncations().remove(0);
    assert_eq!(&*item, "item_1");
    assert!(
        (900..=1_040).contains(&heard.as_millis()),
        "heard {heard:?}: about the 1 s that played, not the 3 s generated"
    );
    let stats = rig.stats();
    assert_eq!(stats.barge_ins, 1);
    assert_eq!(stats.truncations_sent, 1);

    // The old response ends cancelled; the new turn is answered.
    session
        .send(S2sEvent::ResponseDone {
            response: id("resp_1"),
            status: S2sResponseStatus::Cancelled,
        })
        .await;
    session.send(S2sEvent::SpeechStopped).await;
    respond(&session, "resp_2", "item_2", 200).await;
    rig.until("the second end mark", |r| r.marks() >= 1).await;
    rig.pass(100 * MS).await;

    let client = rig.client();
    let clear = client
        .iter()
        .position(|e| matches!(e, ClientEvent::Clear { epoch, .. } if epoch.get() == 2))
        .expect("the barge-in cleared the client to epoch 2");
    let before = audio_epochs_after(&client[..clear], 0);
    let after = audio_epochs_after(&client, clear);
    assert!(before.iter().all(|&e| e == 1), "{before:?}");
    assert!(!after.is_empty(), "the new answer plays");
    assert!(
        after.iter().all(|&e| e == 2),
        "no stale audio after the clear: {after:?}"
    );
    // About 1 s of the old answer played.
    let played_before: u128 = client[..clear]
        .iter()
        .filter_map(|e| match e {
            ClientEvent::Audio { frame, .. } => Some(frame.duration().as_millis()),
            _ => None,
        })
        .sum();
    assert!(
        (900..=1_100).contains(&played_before),
        "{played_before} ms reached the client before the clear"
    );
    // The book was told of the clear before the command (no settle wait ran out), the
    // transport stage handled the interrupt.
    let handles = rig.call.handles();
    assert_eq!(handles.book.stats().settle_timeouts, 0);
    assert_eq!(handles.out.snapshot().interrupts, 1);
    // The local mirror holds the cut text (not all ten words).
    let context = rig.call.handles().context.lock().unwrap();
    let spoken: Vec<&str> = context
        .messages()
        .iter()
        .map(brisvox_agent::Message::text)
        .filter(|t| t.starts_with("one two"))
        .collect();
    assert!(
        spoken.iter().any(|t| t.split_whitespace().count() < 10),
        "{spoken:?}"
    );
}

/// A response whose end the test does not send (the barge-in comes first).
async fn respond_without_end(session: &MockSession, response: &str, item: &str, audio_ms: u64) {
    session
        .send(S2sEvent::ResponseStarted {
            response: id(response),
        })
        .await;
    let mut left = audio_ms;
    while left > 0 {
        let chunk = left.min(100);
        session
            .send(S2sEvent::Audio {
                response: id(response),
                item: id(item),
                pcm: pcm(chunk),
            })
            .await;
        left -= chunk;
    }
    session
        .send(S2sEvent::AssistantTranscript {
            response: id(response),
            item: id(item),
            text: id("one two three four five six seven eight nine ten"),
            is_final: true,
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn events_of_a_superseded_response_are_dropped() {
    let rig = Rig::start(Opts::default()).await;
    rig.play(&speech()).await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let session = rig.mock.session(0);
    session.send(S2sEvent::SpeechStarted).await;
    respond_without_end(&session, "resp_1", "item_1", 1_000).await;
    rig.pass(300 * MS).await;
    session.send(S2sEvent::SpeechStarted).await;
    rig.until("the barge-in", |r| r.stats().barge_ins == 1)
        .await;
    let frames_at_barge_in = rig.stats().frames_out;
    // Late audio and a tool call of the cancelled response arrive after the barge-in.
    for _ in 0..5 {
        session
            .send(S2sEvent::Audio {
                response: id("resp_1"),
                item: id("item_1"),
                pcm: pcm(100),
            })
            .await;
    }
    session
        .send(S2sEvent::ToolCall {
            response: id("resp_1"),
            call_id: id("call_late"),
            name: id("lookup"),
            arguments: id("{}"),
        })
        .await;
    rig.pass(200 * MS).await;
    let stats = rig.stats();
    assert_eq!(
        stats.frames_out, frames_at_barge_in,
        "no frame of the old response"
    );
    assert!(stats.stale_events >= 5, "{stats:?}");
    assert_eq!(stats.tool_calls_stale, 1);
    // The model's history still gets an answer for the call, and nothing was run.
    rig.until("the interrupted answer", |_| {
        session.count(
            |c| matches!(c, S2sControl::ToolResult { call_id, .. } if &**call_id == "call_late"),
        ) == 1
    })
    .await;
    assert_eq!(
        session.count(|c| matches!(c, S2sControl::CreateResponse)),
        0
    );
}

/// Audio that was played to its end is not a barge-in when the caller speaks afterwards.
#[tokio::test(start_paused = true)]
async fn speech_after_a_finished_answer_is_a_new_turn_not_a_barge_in() {
    let rig = Rig::start(Opts::default()).await;
    rig.play(&speech()).await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let session = rig.mock.session(0);
    session.send(S2sEvent::SpeechStarted).await;
    session.send(S2sEvent::SpeechStopped).await;
    respond(&session, "resp_1", "item_1", 300).await;
    rig.until("the end mark", |r| r.marks() == 1).await;
    rig.pass(300 * MS).await;
    session.send(S2sEvent::SpeechStarted).await;
    rig.pass(100 * MS).await;
    let stats = rig.stats();
    assert_eq!(stats.barge_ins, 0);
    assert!(session.truncations().is_empty());
    assert_eq!(
        rig.client()
            .iter()
            .filter(|e| matches!(e, ClientEvent::Clear { epoch, .. } if epoch.get() == 2))
            .count(),
        0,
        "nothing to clear"
    );
}

// ---------------------------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------------------------

async fn call_tool(session: &MockSession, response: &str, call: &str, with_end: bool) {
    session
        .send(S2sEvent::ResponseStarted {
            response: id(response),
        })
        .await;
    session
        .send(S2sEvent::Audio {
            response: id(response),
            item: id("item_a"),
            pcm: pcm(100),
        })
        .await;
    session
        .send(S2sEvent::AudioDone {
            response: id(response),
            item: id("item_a"),
        })
        .await;
    session
        .send(S2sEvent::ToolCall {
            response: id(response),
            call_id: id(call),
            name: id("lookup"),
            arguments: id(r#"{"q":"weather"}"#),
        })
        .await;
    if with_end {
        session
            .send(S2sEvent::ResponseDone {
                response: id(response),
                status: S2sResponseStatus::Completed,
            })
            .await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_tool_call_runs_through_the_tool_engine_and_the_model_answers_the_result() {
    let rig = Rig::start(Opts {
        tool: Some((ToolKind::ReadOnly, 50 * MS)),
        ..Opts::default()
    })
    .await;
    rig.play(&speech()).await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let session = rig.mock.session(0);
    session.send(S2sEvent::SpeechStarted).await;
    session.send(S2sEvent::SpeechStopped).await;
    call_tool(&session, "resp_1", "call_1", true).await;
    rig.until("the tool result and a new response request", |_| {
        session.count(|c| matches!(c, S2sControl::CreateResponse)) == 1
    })
    .await;

    let controls = session.controls();
    let result_at = controls
        .iter()
        .position(|c| matches!(c, S2sControl::ToolResult { .. }))
        .unwrap();
    let create_at = controls
        .iter()
        .position(|c| matches!(c, S2sControl::CreateResponse))
        .unwrap();
    assert!(
        result_at < create_at,
        "output first, then response.create: {controls:?}"
    );
    match &controls[result_at] {
        S2sControl::ToolResult { call_id, output } => {
            assert_eq!(&**call_id, "call_1");
            assert!(output.contains("sunny"), "{output}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(rig.probe.started(), 1);
    // No end mark yet: the turn is not over until the model has answered the result.
    assert_eq!(rig.marks(), 0);
    respond(&session, "resp_2", "item_b", 300).await;
    rig.until("the end mark", |r| r.marks() == 1).await;
    rig.pass(200 * MS).await;
    assert_eq!(rig.marks(), 1, "one end mark for the whole turn");
    let client = rig.client();
    let epochs = audio_epochs_after(&client, 0);
    assert!(
        epochs.iter().all(|&e| e == 1),
        "both answers in the same turn: {epochs:?}"
    );
    // The mark ends the turn: it comes after the audio of BOTH answers (100 ms + 300 ms).
    let mark_at = client
        .iter()
        .position(|e| matches!(e, ClientEvent::Mark { .. }))
        .unwrap();
    let last_audio = client
        .iter()
        .rposition(|e| matches!(e, ClientEvent::Audio { .. }))
        .unwrap();
    assert!(last_audio < mark_at, "the end mark follows the last audio");
    let ms = client_audio_ms(&client);
    assert!((380..=420).contains(&ms), "both answers played: {ms} ms");
    let stats = rig.stats();
    assert_eq!(stats.tool_calls, 1);
    assert_eq!(stats.tool_rounds, 1);
    assert_eq!(stats.barge_ins, 0);
    // The mirror holds the call and its result.
    let context = rig.call.handles().context.lock().unwrap();
    assert!(context.messages().iter().any(
        |m| matches!(m, brisvox_agent::Message::Tool { content, .. } if content.contains("sunny"))
    ));
}

#[tokio::test(start_paused = true)]
async fn a_side_effect_call_the_provider_repeats_runs_once() {
    let rig = Rig::start(Opts {
        tool: Some((ToolKind::SideEffect, 20 * MS)),
        ..Opts::default()
    })
    .await;
    rig.play(&speech()).await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let session = rig.mock.session(0);
    session.send(S2sEvent::SpeechStarted).await;
    session.send(S2sEvent::SpeechStopped).await;
    // The provider reports the same call twice (its two events for one call).
    call_tool(&session, "resp_1", "call_book", false).await;
    session
        .send(S2sEvent::ToolCall {
            response: id("resp_1"),
            call_id: id("call_book"),
            name: id("lookup"),
            arguments: id(r#"{"q":"weather"}"#),
        })
        .await;
    session
        .send(S2sEvent::ResponseDone {
            response: id("resp_1"),
            status: S2sResponseStatus::Completed,
        })
        .await;
    rig.until("the result", |_| {
        session.count(|c| matches!(c, S2sControl::ToolResult { .. })) >= 1
    })
    .await;
    rig.pass(500 * MS).await;
    assert_eq!(rig.probe.started(), 1, "the booking ran once");
    assert_eq!(
        session.count(|c| matches!(c, S2sControl::ToolResult { .. })),
        1
    );
    assert_eq!(
        session.count(|c| matches!(c, S2sControl::CreateResponse)),
        1
    );
}

/// The caller barges in while a tool runs: the read-only tool is cancelled, its answer still goes
/// to the model's history (so it is well formed), and NO response is requested for the old turn.
#[tokio::test(start_paused = true)]
async fn a_tool_result_of_a_superseded_turn_is_added_but_never_answered() {
    let rig = Rig::start(Opts {
        tool: Some((ToolKind::ReadOnly, 2_000 * MS)),
        ..Opts::default()
    })
    .await;
    rig.play(&speech()).await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let session = rig.mock.session(0);
    session.send(S2sEvent::SpeechStarted).await;
    session.send(S2sEvent::SpeechStopped).await;
    call_tool(&session, "resp_1", "call_1", true).await;
    rig.until("the tool started", |r| r.probe.started() == 1)
        .await;
    rig.pass(100 * MS).await;
    // The caller speaks while the tool runs.
    session.send(S2sEvent::SpeechStarted).await;
    rig.until("the interrupted answer", |_| {
        session.count(|c| matches!(c, S2sControl::ToolResult { .. })) == 1
    })
    .await;
    rig.pass(3_000 * MS).await;
    let output = session
        .controls()
        .into_iter()
        .find_map(|c| match c {
            S2sControl::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .unwrap();
    assert!(output.contains("interrupted"), "{output}");
    assert_eq!(rig.probe.finished(), 0, "the read-only tool was dropped");
    assert_eq!(
        session.count(|c| matches!(c, S2sControl::CreateResponse)),
        0
    );
    assert_eq!(rig.stats().tool_results_stale, 1);
}

// ---------------------------------------------------------------------------------------------
// Local turn detection
// ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn local_turn_detection_commits_the_utterance_and_asks_for_a_response() {
    let rig = Rig::start(Opts {
        turn: S2sTurnMode::Local,
        ..Opts::default()
    })
    .await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let session = rig.mock.session(0);
    assert_eq!(
        session.config.turn_detection,
        brisvox_providers::S2sTurnDetection::Manual
    );
    // 1.1 s of audio with 0.8 s of speech in the middle.
    rig.play(&speech()).await;
    rig.until("the commit", |_| {
        session.count(|c| matches!(c, S2sControl::Commit)) == 1
    })
    .await;
    assert_eq!(
        session.count(|c| matches!(c, S2sControl::CreateResponse)),
        1
    );
    // Only the speech and its pre-roll were sent, not the silence around it.
    let sent_ms = session.audio_frames.load(Ordering::SeqCst) * 20;
    assert!(
        (700..=1_300).contains(&sent_ms),
        "{sent_ms} ms sent of a 1.1 s scenario with a 0.8 s utterance"
    );
    // The response belongs to the turn the local VAD ended.
    respond(&session, "resp_1", "item_1", 300).await;
    rig.until("the end mark", |r| r.marks() == 1).await;
    assert_eq!(rig.stats().barge_ins, 0);
}

#[tokio::test(start_paused = true)]
async fn local_barge_in_cancels_the_running_response() {
    let rig = Rig::start(Opts {
        turn: S2sTurnMode::Local,
        ..Opts::default()
    })
    .await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let session = rig.mock.session(0);
    rig.play(&speech()).await;
    rig.until("the commit", |_| {
        session.count(|c| matches!(c, S2sControl::Commit)) == 1
    })
    .await;
    respond_without_end(&session, "resp_1", "item_1", 3_000).await;
    rig.pass(800 * MS).await;
    // The caller speaks again over the answer (the local VAD hears it).
    rig.play(&speech()).await;
    rig.until("the cancel", |_| {
        session.count(|c| matches!(c, S2sControl::CancelResponse)) >= 1
    })
    .await;
    rig.until("the truncation", |_| !session.truncations().is_empty())
        .await;
    assert!(rig.stats().barge_ins >= 1);
    let (item, heard) = session.truncations().remove(0);
    assert_eq!(&*item, "item_1");
    assert!(heard < Duration::from_millis(3_000), "{heard:?}");
}

// ---------------------------------------------------------------------------------------------
// Failure
// ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_lost_session_is_replaced_after_the_backoff() {
    let mut rig = Rig::start(Opts::default()).await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    rig.mock.session(0).die();
    rig.until("a second session", |r| r.mock.opened() == 2)
        .await;
    let stats = rig.stats();
    assert_eq!(stats.session_failures, 1);
    assert_eq!(stats.sessions_opened, 2);
    // Audio flows into the new session.
    rig.play(&speech()).await;
    let second = rig.mock.session(1);
    assert!(second.audio_frames.load(Ordering::SeqCst) > 20);
    let events = rig.drain_events();
    assert!(
        events.iter().any(|e| matches!(
            e,
            EngineEvent::Error {
                stage: "s2s",
                fatal: false,
                ..
            }
        )),
        "{events:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_refused_connect_is_retried() {
    let rig = Rig::start(Opts {
        plan: vec![Plan::Refuse, Plan::Open],
        ..Opts::default()
    })
    .await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let stats = rig.stats();
    assert_eq!(stats.connect_failures, 1);
    assert_eq!(rig.mock.connects.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn a_connect_that_never_answers_times_out_and_is_retried() {
    let started = tokio::time::Instant::now();
    let rig = Rig::start(Opts {
        plan: vec![Plan::Hang, Plan::Open],
        ..Opts::default()
    })
    .await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let timeouts = EngineConfig::DEFAULT.timeouts;
    let waited = started.elapsed();
    assert!(waited >= timeouts.connect, "{waited:?}");
    assert!(
        waited < timeouts.connect + 3 * timeouts.reconnect_backoff,
        "{waited:?}"
    );
    assert_eq!(rig.stats().connect_failures, 1);
}

#[tokio::test(start_paused = true)]
async fn when_every_attempt_fails_the_stage_reports_one_fatal_error_and_stops_trying() {
    let mut rig = Rig::start(Opts {
        plan: vec![Plan::Refuse; 10],
        ..Opts::default()
    })
    .await;
    let max = EngineConfig::DEFAULT.timeouts.max_reconnects;
    rig.pass(60_000 * MS).await;
    let connects = rig.mock.connects.load(Ordering::SeqCst);
    assert_eq!(
        connects,
        usize::try_from(max).unwrap() + 1,
        "one try plus the budget"
    );
    assert_eq!(rig.mock.opened(), 0);
    // Audio is dropped and counted, not queued for ever.
    rig.play(&speech()).await;
    assert!(rig.stats().audio_dropped_no_session > 0);
    let events = rig.drain_events();
    let fatal = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                EngineEvent::Error {
                    stage: "s2s",
                    fatal: true,
                    ..
                }
            )
        })
        .count();
    assert_eq!(fatal, 1, "{events:?}");
}

#[tokio::test(start_paused = true)]
async fn the_output_queue_is_bounded() {
    let rig = Rig::start(Opts {
        max_buffered_audio: Duration::from_secs(1),
        ..Opts::default()
    })
    .await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let session = rig.mock.session(0);
    session.send(S2sEvent::SpeechStarted).await;
    respond(&session, "resp_1", "item_1", 6_000).await;
    rig.pass(100 * MS).await;
    let stats = rig.stats();
    assert!(stats.output_overflow > 0, "{stats:?}");
}

/// An answer that played to its end stays whole in the model's history when a later answer is
/// interrupted: only the interrupted item is cut.
#[tokio::test(start_paused = true)]
async fn a_barge_in_cuts_only_the_interrupted_answer_not_earlier_whole_ones() {
    let rig = Rig::start(Opts::default()).await;
    rig.play(&speech()).await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let session = rig.mock.session(0);
    session.send(S2sEvent::SpeechStarted).await;
    session.send(S2sEvent::SpeechStopped).await;
    respond(&session, "resp_1", "item_1", 300).await;
    rig.until("the first end mark", |r| r.marks() == 1).await;
    rig.pass(300 * MS).await;
    // A second turn, interrupted half-way.
    session.send(S2sEvent::SpeechStarted).await;
    session.send(S2sEvent::SpeechStopped).await;
    respond_without_end(&session, "resp_2", "item_2", 3_000).await;
    rig.pass(700 * MS).await;
    session.send(S2sEvent::SpeechStarted).await;
    rig.until("the truncation", |_| !session.truncations().is_empty())
        .await;
    rig.pass(300 * MS).await;
    let cut: Vec<String> = session
        .truncations()
        .iter()
        .map(|(item, _)| item.to_string())
        .collect();
    assert_eq!(cut, ["item_2"], "item_1 was heard whole");
    // Not even looked at again: it is settled once its end was played.
    assert_eq!(rig.stats().truncations_skipped, 0);
}

/// The model generates far ahead of real time. What the transport cannot take yet waits in the
/// stage's output queue, and a barge-in throws it away instead of sending it to be dropped.
#[tokio::test(start_paused = true)]
async fn a_barge_in_discards_the_audio_still_queued_in_the_stage() {
    let rig = Rig::start(Opts::default()).await;
    rig.play(&speech()).await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let session = rig.mock.session(0);
    session.send(S2sEvent::SpeechStarted).await;
    respond_without_end(&session, "resp_1", "item_1", 30_000).await;
    rig.pass(500 * MS).await;
    let before = rig.stats();
    assert!(before.frames_out >= 1_000, "{before:?}");
    session.send(S2sEvent::SpeechStarted).await;
    rig.until("the barge-in", |r| r.stats().barge_ins == 1)
        .await;
    rig.pass(100 * MS).await;
    let after = rig.stats();
    assert!(
        after.queued_discarded >= 500,
        "most of the 30 s answer was still queued in the stage: {after:?}"
    );
    // Nothing of the discarded audio was sent afterwards.
    assert_eq!(after.frames_out, before.frames_out);
}

// ---------------------------------------------------------------------------------------------
// Tool limits, failed responses, a session that dies mid-response (review fixes)
// ---------------------------------------------------------------------------------------------

/// A response that only asks for tools (no audio).
async fn call_only(session: &MockSession, response: &str, calls: &[&str]) {
    session
        .send(S2sEvent::ResponseStarted {
            response: id(response),
        })
        .await;
    for call in calls {
        session
            .send(S2sEvent::ToolCall {
                response: id(response),
                call_id: id(call),
                name: id("lookup"),
                arguments: id(r#"{"q":"weather"}"#),
            })
            .await;
    }
    session
        .send(S2sEvent::ResponseDone {
            response: id(response),
            status: S2sResponseStatus::Completed,
        })
        .await;
}

/// A registry with one SideEffect tool, `lookup`, held for the user's confirmation.
fn gated_lookup_tools(
    rig_clock: &SimClock,
    probe: &Arc<ToolProbe>,
    delay: Duration,
) -> (Arc<ToolRegistry>, Arc<ToolEngine>) {
    let clock: SharedClock = Arc::new(*rig_clock);
    let mut registry = ToolRegistry::new();
    registry
        .register(
            ToolSpec::new(
                "lookup",
                "Does something that needs confirmation.",
                json!({"type": "object", "properties": {"q": {"type": "string"}}}),
                ToolKind::SideEffect,
            )
            .with_confirm(true),
            SlowTool {
                probe: Arc::clone(probe),
                clock: clock.clone(),
                delay,
                result: json!({"done": true}),
            },
        )
        .unwrap();
    let registry = Arc::new(registry);
    let engine = Arc::new(ToolEngine::new(
        Arc::clone(&registry),
        SessionId::new(1),
        clock,
        ToolEngineConfig::default(),
    ));
    (registry, engine)
}

fn tool_results(session: &MockSession) -> Vec<(String, String)> {
    session
        .controls()
        .into_iter()
        .filter_map(|c| match c {
            S2sControl::ToolResult { call_id, output } => {
                Some((call_id.to_string(), output.to_string()))
            }
            _ => None,
        })
        .collect()
}

/// The model answers the user's turn and calls the gated tool before the provider's "speech
/// stopped" event arrives; the model then calls `confirm_pending` right after that event. The
/// stop event closes the turn the request was made in, so it is no reply to the request: the
/// action must not run. A genuine later turn confirms it.
#[tokio::test(start_paused = true)]
async fn a_speech_stopped_event_after_the_tool_call_is_not_the_reply_to_the_request() {
    let rig = Rig::start(Opts {
        tool: Some((ToolKind::SideEffect, 50 * MS)),
        confirm: true,
        ..Opts::default()
    })
    .await;
    rig.play(&speech()).await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let session = rig.mock.session(0);
    session.send(S2sEvent::SpeechStarted).await;
    call_tool(&session, "resp_1", "call_1", true).await;
    rig.until("the request is answered", |_| {
        !tool_results(&session).is_empty()
    })
    .await;
    let request: serde_json::Value = serde_json::from_str(&tool_results(&session)[0].1).unwrap();
    assert_eq!(request["status"], "confirmation_required", "{request}");
    let held = request["call_id"].as_str().unwrap().to_owned();

    // The provider's stop event for that same speech arrives only now; the model, answering the
    // tool result, calls confirm_pending at once.
    session.send(S2sEvent::SpeechStopped).await;
    session
        .send(S2sEvent::ResponseStarted {
            response: id("resp_2"),
        })
        .await;
    session
        .send(S2sEvent::ToolCall {
            response: id("resp_2"),
            call_id: id("call_2"),
            name: id("confirm_pending"),
            arguments: Arc::from(format!(r#"{{"call_id":"{held}"}}"#)),
        })
        .await;
    session
        .send(S2sEvent::ResponseDone {
            response: id("resp_2"),
            status: S2sResponseStatus::Completed,
        })
        .await;
    rig.until("the confirmation is answered", |_| {
        tool_results(&session).len() >= 2
    })
    .await;
    let refused = &tool_results(&session)[1].1;
    assert!(refused.contains("not replied since"), "{refused}");
    rig.pass(500 * MS).await;
    assert_eq!(rig.probe.started(), 0, "the action must not run");

    // The model asks the user and finishes speaking; the user really answers (a new speech,
    // after the bot is done): the confirmation works.
    respond(&session, "resp_3", "item_b", 300).await;
    rig.until("the end mark", |r| r.marks() == 1).await;
    rig.pass(Duration::from_secs(1)).await;
    session.send(S2sEvent::SpeechStarted).await;
    session.send(S2sEvent::SpeechStopped).await;
    session
        .send(S2sEvent::ResponseStarted {
            response: id("resp_4"),
        })
        .await;
    session
        .send(S2sEvent::ToolCall {
            response: id("resp_4"),
            call_id: id("call_3"),
            name: id("confirm_pending"),
            arguments: Arc::from(format!(r#"{{"call_id":"{held}"}}"#)),
        })
        .await;
    session
        .send(S2sEvent::ResponseDone {
            response: id("resp_4"),
            status: S2sResponseStatus::Completed,
        })
        .await;
    rig.until("the action runs", |r| r.probe.started() == 1)
        .await;
    rig.until("the result", |_| tool_results(&session).len() >= 3)
        .await;
    assert!(tool_results(&session)[2].1.contains("done"));
}

#[tokio::test(start_paused = true)]
async fn after_the_tool_round_limit_the_model_is_told_and_asked_to_answer_once() {
    let mut config = Opts::default().config;
    config.llm.max_tool_rounds = 1;
    let rig = Rig::start(Opts {
        tool: Some((ToolKind::ReadOnly, 20 * MS)),
        config,
        ..Opts::default()
    })
    .await;
    rig.play(&speech()).await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let session = rig.mock.session(0);
    session.send(S2sEvent::SpeechStarted).await;
    session.send(S2sEvent::SpeechStopped).await;

    // Round 1 runs.
    call_only(&session, "resp_1", &["call_1"]).await;
    rig.until("the first answer request", |_| {
        session.count(|c| matches!(c, S2sControl::CreateResponse)) == 1
    })
    .await;
    assert_eq!(rig.probe.started(), 1);

    // Round 2 is over the limit: answered with an error without running, and the model is asked
    // to speak (it would otherwise leave the caller in silence).
    call_only(&session, "resp_2", &["call_2"]).await;
    rig.until("the final answer request", |_| {
        session.count(|c| matches!(c, S2sControl::CreateResponse)) == 2
    })
    .await;
    assert_eq!(rig.probe.started(), 1, "the tool of round 2 did not run");
    let results = tool_results(&session);
    assert!(
        results
            .iter()
            .any(|(call, out)| call == "call_2" && out.contains("tool_round_limit")),
        "{results:?}"
    );
    assert_eq!(
        session.count(|c| matches!(c, S2sControl::Note { .. })),
        1,
        "the model is told why"
    );
    let controls = session.controls();
    let note_at = controls
        .iter()
        .position(|c| matches!(c, S2sControl::Note { .. }))
        .unwrap();
    let last_create = controls
        .iter()
        .rposition(|c| matches!(c, S2sControl::CreateResponse))
        .unwrap();
    assert!(note_at < last_create, "the note precedes the request");

    // A model that still calls tools does not loop the stage: no third request.
    call_only(&session, "resp_3", &["call_3"]).await;
    rig.pass(300 * MS).await;
    assert_eq!(
        session.count(|c| matches!(c, S2sControl::CreateResponse)),
        2
    );
    assert!(
        tool_results(&session)
            .iter()
            .any(|(call, out)| call == "call_3" && out.contains("tool_round_limit"))
    );
    assert_eq!(rig.stats().tool_round_limit, 2);

    // When the model does answer, the caller hears it.
    respond(&session, "resp_4", "item_z", 300).await;
    rig.until("the end mark", |r| r.marks() == 1).await;
    assert!(client_audio_ms(&rig.client()) >= 280);
}

#[tokio::test(start_paused = true)]
async fn calls_beyond_the_per_response_limit_are_refused_and_the_rest_run() {
    let rig = Rig::start(Opts {
        tool: Some((ToolKind::ReadOnly, 10 * MS)),
        ..Opts::default()
    })
    .await;
    rig.play(&speech()).await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let session = rig.mock.session(0);
    session.send(S2sEvent::SpeechStarted).await;
    session.send(S2sEvent::SpeechStopped).await;
    let names: Vec<String> = (0..18).map(|i| format!("call_{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    call_only(&session, "resp_1", &refs).await;
    rig.until("the answer request", |_| {
        session.count(|c| matches!(c, S2sControl::CreateResponse)) == 1
    })
    .await;
    let refused: Vec<_> = tool_results(&session)
        .into_iter()
        .filter(|(_, out)| out.contains("too_many_calls"))
        .map(|(call, _)| call)
        .collect();
    assert_eq!(refused, ["call_16", "call_17"]);
    assert_eq!(rig.probe.started(), 16);
    assert_eq!(rig.stats().tool_rounds, 1);
}

#[tokio::test(start_paused = true)]
async fn a_failed_or_cancelled_response_answers_its_calls_without_running_them() {
    for status in [
        S2sResponseStatus::Failed(Arc::from("model overloaded")),
        S2sResponseStatus::Cancelled,
        S2sResponseStatus::Incomplete,
    ] {
        let failed = matches!(status, S2sResponseStatus::Failed(_));
        let mut rig = Rig::start(Opts {
            tool: Some((ToolKind::ReadOnly, 10 * MS)),
            ..Opts::default()
        })
        .await;
        rig.play(&speech()).await;
        rig.until("a session", |r| r.mock.opened() == 1).await;
        let session = rig.mock.session(0);
        session.send(S2sEvent::SpeechStarted).await;
        session.send(S2sEvent::SpeechStopped).await;
        session
            .send(S2sEvent::ResponseStarted {
                response: id("resp_1"),
            })
            .await;
        session
            .send(S2sEvent::ToolCall {
                response: id("resp_1"),
                call_id: id("call_1"),
                name: id("lookup"),
                arguments: id(r#"{"q":"x"}"#),
            })
            .await;
        session
            .send(S2sEvent::ResponseDone {
                response: id("resp_1"),
                status,
            })
            .await;
        rig.until("the call answered", |_| tool_results(&session).len() == 1)
            .await;
        rig.pass(300 * MS).await;
        // The history is consistent (the call has an output), the tool never ran, and the stage
        // does not loop by asking for another response of a failed one.
        assert_eq!(rig.probe.started(), 0);
        assert_eq!(
            session.count(|c| matches!(c, S2sControl::CreateResponse)),
            0
        );
        let errors = rig
            .drain_events()
            .into_iter()
            .filter(|e| matches!(e, EngineEvent::Error { stage: "s2s", .. }))
            .count();
        assert_eq!(errors, usize::from(failed), "only a failure is reported");
    }
}

#[tokio::test(start_paused = true)]
async fn a_session_that_fails_mid_response_is_settled_and_replaced() {
    // What the adapter's liveness timeout produces: an Error event while an answer is playing.
    let rig = Rig::start(Opts::default()).await;
    rig.play(&speech()).await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let session = rig.mock.session(0);
    session.send(S2sEvent::SpeechStarted).await;
    session.send(S2sEvent::SpeechStopped).await;
    respond_without_end(&session, "resp_1", "item_1", 400).await;
    rig.pass(100 * MS).await;
    session
        .send(S2sEvent::Error(ProviderError::Core(
            brisvox_core::Error::Timeout {
                what: "openai-realtime liveness (no pong)",
                after: Duration::from_secs(10),
            },
        )))
        .await;
    rig.until("the replacement", |r| r.mock.opened() == 2).await;
    let stats = rig.stats();
    assert_eq!(stats.session_failures, 1);
    assert_eq!(stats.sessions_opened, 2);
    // The answer that will never be finished is ended for the client (one mark), and the new
    // session serves the next turn.
    rig.until("the end mark", |r| r.marks() == 1).await;
    let second = rig.mock.session(1);
    second.send(S2sEvent::SpeechStarted).await;
    second.send(S2sEvent::SpeechStopped).await;
    respond(&second, "resp_9", "item_9", 200).await;
    rig.until("the second end mark", |r| r.marks() == 2).await;
}

#[tokio::test(start_paused = true)]
async fn the_heard_text_reaches_the_context_when_the_session_is_gone_at_barge_in() {
    let rig = Rig::start(Opts {
        turn: S2sTurnMode::Local,
        ..Opts::default()
    })
    .await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let session = rig.mock.session(0);
    rig.play(&speech()).await;
    rig.until("the commit", |_| {
        session.count(|c| matches!(c, S2sControl::Commit)) == 1
    })
    .await;
    respond_without_end(&session, "resp_1", "item_1", 3_000).await;
    rig.pass(600 * MS).await;
    // The connection drops while the caller hears the answer; the caller talks over it during
    // the reconnect backoff (the local VAD works without a session).
    session.die();
    rig.until("the session loss", |r| r.stats().session_failures == 1)
        .await;
    assert_eq!(rig.mock.opened(), 1, "still in the backoff");
    rig.play(&speech()).await;
    rig.until("the barge-in", |r| r.stats().barge_ins >= 1)
        .await;
    rig.until("the cut text in the context", |r| {
        r.call
            .handles()
            .context
            .lock()
            .unwrap()
            .messages()
            .iter()
            .any(|m| m.text().starts_with("one two"))
    })
    .await;
    let context = rig.call.handles().context.lock().unwrap();
    let spoken: Vec<&str> = context
        .messages()
        .iter()
        .map(brisvox_agent::Message::text)
        .filter(|t| t.starts_with("one two"))
        .collect();
    assert!(
        spoken.iter().all(|t| t.split_whitespace().count() < 10),
        "only the heard share is kept: {spoken:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Re-seeding and rotation
// ---------------------------------------------------------------------------------------------

/// A finished exchange: the caller speaks, the model answers (played whole, end mark echoed).
async fn exchange(rig: &Rig, session: &MockSession, response: &str, item: &str) {
    let before = rig.marks();
    rig.play(&speech()).await;
    session.send(S2sEvent::SpeechStarted).await;
    session.send(S2sEvent::SpeechStopped).await;
    session
        .send(S2sEvent::UserTranscript {
            text: id("what time is it"),
        })
        .await;
    respond(session, response, item, 600).await;
    rig.until("the end mark", |r| r.marks() == before + 1).await;
    rig.pass(100 * MS).await;
}

fn history_of(session: &MockSession) -> Vec<S2sHistoryItem> {
    session.config.history.clone()
}

#[tokio::test(start_paused = true)]
async fn a_replacement_session_is_seeded_from_the_local_mirror() {
    let rig = Rig::start(Opts::default()).await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let first = rig.mock.session(0);
    assert!(
        history_of(&first).is_empty(),
        "the first session has no history"
    );
    exchange(&rig, &first, "resp_1", "item_1").await;
    first.die();
    rig.until("a second session", |r| r.mock.opened() == 2)
        .await;
    let history = history_of(&rig.mock.session(1));
    assert_eq!(
        history,
        vec![
            S2sHistoryItem::User(id("what time is it")),
            S2sHistoryItem::Assistant(id("one two three four five six seven eight nine ten")),
        ]
    );
}

/// After an interruption the seed holds only what was played: the cut text, not the whole answer.
#[tokio::test(start_paused = true)]
async fn the_seed_holds_the_played_share_of_an_interrupted_answer() {
    let rig = Rig::start(Opts::default()).await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let first = rig.mock.session(0);
    rig.play(&speech()).await;
    first.send(S2sEvent::SpeechStarted).await;
    first.send(S2sEvent::SpeechStopped).await;
    respond_without_end(&first, "resp_1", "item_1", 3_000).await;
    first
        .send(S2sEvent::ResponseDone {
            response: id("resp_1"),
            status: S2sResponseStatus::Completed,
        })
        .await;
    rig.pass(600 * MS).await;
    // The caller talks over the answer; then the session dies.
    first.send(S2sEvent::SpeechStarted).await;
    rig.until("the barge-in", |r| r.stats().barge_ins == 1)
        .await;
    rig.until("the truncation", |r| r.stats().truncations_sent == 1)
        .await;
    first.die();
    rig.until("a second session", |r| r.mock.opened() == 2)
        .await;
    let history = history_of(&rig.mock.session(1));
    let assistant: Vec<&Arc<str>> = history
        .iter()
        .filter_map(|i| match i {
            S2sHistoryItem::Assistant(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(assistant.len(), 1, "{history:?}");
    assert!(
        assistant[0].split_whitespace().count() < 10 && assistant[0].starts_with("one two"),
        "{assistant:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_session_is_rotated_before_the_providers_limit_without_a_gap() {
    let mut rig = Rig::start(Opts {
        session_limit: Some(Duration::from_secs(600)),
        ..Opts::default()
    })
    .await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let first = rig.mock.session(0);
    exchange(&rig, &first, "resp_1", "item_1").await;
    assert_eq!(rig.mock.opened(), 1, "far from the limit");
    // The margin is 120 s: the replacement opens at 480 s, while the bot is idle.
    rig.pass(Duration::from_secs(470)).await;
    rig.until("the rotation", |r| r.stats().rotations == 1)
        .await;
    assert_eq!(rig.mock.opened(), 2);
    let second = rig.mock.session(1);
    assert_eq!(
        history_of(&second),
        vec![
            S2sHistoryItem::User(id("what time is it")),
            S2sHistoryItem::Assistant(id("one two three four five six seven eight nine ten")),
        ]
    );
    let stats = rig.stats();
    assert_eq!(
        stats.session_failures, 0,
        "the old session was not lost, it was replaced"
    );
    assert_eq!(stats.sessions_opened, 2);
    // The caller's audio goes to the new session from now on.
    let (old_frames, new_frames) = (
        first.audio_frames.load(Ordering::SeqCst),
        second.audio_frames.load(Ordering::SeqCst),
    );
    rig.play(&speech()).await;
    assert_eq!(first.audio_frames.load(Ordering::SeqCst), old_frames);
    assert!(second.audio_frames.load(Ordering::SeqCst) > new_frames + 20);
    // The new session answers like the old one did.
    second.send(S2sEvent::SpeechStarted).await;
    second.send(S2sEvent::SpeechStopped).await;
    respond(&second, "resp_2", "item_2", 400).await;
    rig.until("the second end mark", |r| r.marks() == 2).await;
    // The call never saw an error.
    let errors = rig
        .drain_events()
        .into_iter()
        .filter(|e| matches!(e, EngineEvent::Error { .. }))
        .count();
    assert_eq!(errors, 0);
    // The old session is closed (its control channel was dropped), so it cannot answer any more.
    rig.pass(Duration::from_secs(1)).await;
    assert!(
        first.control_closed.load(Ordering::SeqCst),
        "the old session was closed"
    );
    assert!(!second.control_closed.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn a_rotation_waits_for_the_bot_to_finish_speaking() {
    let rig = Rig::start(Opts {
        session_limit: Some(Duration::from_secs(600)),
        ..Opts::default()
    })
    .await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let first = rig.mock.session(0);
    // A long answer that is still being delivered when the rotation falls due (480 s).
    rig.pass(Duration::from_secs(470)).await;
    rig.play(&speech()).await;
    first.send(S2sEvent::SpeechStarted).await;
    first.send(S2sEvent::SpeechStopped).await;
    respond_without_end(&first, "resp_1", "item_1", 600).await;
    // The response never completes: 60 s pass, far beyond the due time.
    rig.pass(Duration::from_secs(60)).await;
    assert_eq!(rig.mock.opened(), 1, "no replacement while the bot is busy");
    assert_eq!(rig.stats().rotations, 0);
    first
        .send(S2sEvent::ResponseDone {
            response: id("resp_1"),
            status: S2sResponseStatus::Completed,
        })
        .await;
    rig.until("the rotation", |r| r.stats().rotations == 1)
        .await;
    assert_eq!(rig.mock.opened(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_replacement_that_arrives_after_the_conversation_moved_on_is_discarded() {
    let rig = Rig::start(Opts {
        session_limit: Some(Duration::from_secs(600)),
        plan: vec![Plan::Open, Plan::Slow(Duration::from_secs(3)), Plan::Open],
        ..Opts::default()
    })
    .await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let first = rig.mock.session(0);
    rig.pass(Duration::from_secs(479)).await;
    // The rotation falls due; its connect takes 3 s. The caller speaks meanwhile.
    rig.until("the standby connect", |r| {
        r.mock.connects.load(Ordering::SeqCst) == 2
    })
    .await;
    let frames = first.audio_frames.load(Ordering::SeqCst);
    rig.play(&speech()).await;
    first.send(S2sEvent::SpeechStarted).await;
    first.send(S2sEvent::SpeechStopped).await;
    first
        .send(S2sEvent::UserTranscript {
            text: id("what time is it"),
        })
        .await;
    respond(&first, "resp_1", "item_1", 400).await;
    // The old session kept serving the caller the whole time.
    assert!(first.audio_frames.load(Ordering::SeqCst) > frames + 20);
    rig.until("the stale replacement to be dropped", |r| {
        r.stats().rotations_deferred >= 1
    })
    .await;
    // At the next quiet moment a fresh replacement, seeded with the new turn, takes over.
    rig.until("the rotation", |r| r.stats().rotations == 1)
        .await;
    let second = rig.mock.session(rig.mock.opened() - 1);
    assert!(
        history_of(&second).contains(&S2sHistoryItem::User(id("what time is it"))),
        "{:?}",
        history_of(&second)
    );
    assert_eq!(rig.stats().session_failures, 0);
}

#[tokio::test(start_paused = true)]
async fn a_failed_rotation_leaves_the_live_session_alone_and_is_retried() {
    let rig = Rig::start(Opts {
        session_limit: Some(Duration::from_secs(600)),
        plan: vec![Plan::Open, Plan::Refuse, Plan::Open],
        ..Opts::default()
    })
    .await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let first = rig.mock.session(0);
    rig.pass(Duration::from_secs(470)).await;
    rig.until("the failed rotation", |r| r.stats().rotation_failures == 1)
        .await;
    // The live session is untouched and still gets the audio.
    let frames = first.audio_frames.load(Ordering::SeqCst);
    rig.play(&speech()).await;
    assert!(first.audio_frames.load(Ordering::SeqCst) > frames + 20);
    assert_eq!(rig.stats().session_failures, 0);
    rig.until("the retried rotation", |r| r.stats().rotations == 1)
        .await;
    assert_eq!(rig.mock.opened(), 2);
}

#[tokio::test(start_paused = true)]
async fn the_providers_announcement_that_the_session_ends_starts_the_rotation_at_once() {
    let rig = Rig::start(Opts::default()).await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let first = rig.mock.session(0);
    exchange(&rig, &first, "resp_1", "item_1").await;
    assert_eq!(rig.mock.opened(), 1, "no limit is known");
    first
        .send(S2sEvent::SessionEnding {
            time_left: Duration::from_secs(30),
        })
        .await;
    rig.until("the rotation", |r| r.stats().rotations == 1)
        .await;
    assert_eq!(rig.mock.opened(), 2);
    assert_eq!(rig.stats().session_failures, 0);
    assert!(!history_of(&rig.mock.session(1)).is_empty());
}

/// A provider that rotates sessions needs the caller's transcripts for the seed, so the stage
/// asks for them whatever the options say.
#[tokio::test(start_paused = true)]
async fn a_rotating_provider_is_asked_for_transcripts_even_when_the_options_do_not() {
    let rig = Rig::start(Opts {
        session_limit: Some(Duration::from_secs(600)),
        input_transcription: false,
        ..Opts::default()
    })
    .await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    assert!(rig.mock.session(0).config.input_transcription);
    // Without a session limit the option is respected.
    let plain = Rig::start(Opts {
        input_transcription: false,
        ..Opts::default()
    })
    .await;
    plain.until("a session", |r| r.mock.opened() == 1).await;
    assert!(!plain.mock.session(0).config.input_transcription);
}

/// A provider whose server closes a session that was given a history it dislikes: the call must
/// not die at every rotation. The stage retries without a seed and stops seeding.
#[tokio::test(start_paused = true)]
async fn a_seeded_session_that_dies_at_once_is_replaced_by_an_unseeded_one() {
    let rig = Rig::start(Opts {
        session_limit: Some(Duration::from_secs(600)),
        ..Opts::default()
    })
    .await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    let first = rig.mock.session(0);
    exchange(&rig, &first, "resp_1", "item_1").await;
    rig.pass(Duration::from_secs(470)).await;
    rig.until("the rotation", |r| r.stats().rotations == 1)
        .await;
    let seeded = rig.mock.session(1);
    assert!(!history_of(&seeded).is_empty());
    // The server refuses the seed and closes the socket.
    rig.pass(Duration::from_secs(1)).await;
    seeded.die();
    rig.until("a third session", |r| r.mock.opened() == 3).await;
    assert!(
        history_of(&rig.mock.session(2)).is_empty(),
        "the retry carries no history"
    );
    // The unseeded session survives to its own rotation; from then on no seed is sent.
    rig.pass(Duration::from_secs(480)).await;
    rig.until("the next rotation", |r| r.stats().rotations == 2)
        .await;
    assert!(history_of(&rig.mock.session(rig.mock.opened() - 1)).is_empty());
    assert_eq!(rig.stats().session_failures, 1, "only the refused seed");
}

/// The provider sends no speech-start events: loud caller audio alone keeps a rotation from
/// swapping mid-utterance.
#[tokio::test(start_paused = true)]
async fn a_rotation_does_not_swap_while_the_caller_speaks_unannounced() {
    let rig = Rig::start(Opts {
        session_limit: Some(Duration::from_secs(600)),
        plan: vec![Plan::Open, Plan::Slow(Duration::from_secs(1)), Plan::Open],
        ..Opts::default()
    })
    .await;
    rig.until("a session", |r| r.mock.opened() == 1).await;
    rig.pass(Duration::from_secs(479)).await;
    rig.until("the standby connect", |r| {
        r.mock.connects.load(Ordering::SeqCst) == 2
    })
    .await;
    // The caller speaks while the replacement opens; the provider announces nothing.
    rig.play(&speech()).await;
    rig.until("the early replacement to be dropped", |r| {
        r.stats().rotations_deferred >= 1
    })
    .await;
    assert_eq!(rig.stats().rotations, 0);
    rig.until("the rotation", |r| r.stats().rotations == 1)
        .await;
}
