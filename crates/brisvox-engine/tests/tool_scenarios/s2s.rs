//! The speech-to-speech stage (`S2sStage`, `SessionBuilder::new_s2s`) with parallel and chained
//! tool calls: a whole call over the loopback transport, the stage's `ToolEngine` and the tools of
//! `world.rs`.
//!
//! * Scripted (normal suite): a provider that the test drives event by event, as the Gemini Live
//!   server sends them: a response with three `ToolCall`s, then `ResponseDone`.
//! * Live (`BRISVOX_LIVE_TESTS=1`, `GEMINI_API_KEY`): the real `GeminiLive` adapter with a
//!   recorded sentence of the FLEURS fixtures as the caller's speech (`fixtures/en_tr_mixed.wav`:
//!   "The East African Islands are in the Indian Ocean off the eastern coast of Africa."). The
//!   prompts turn that sentence into three independent lookups (scenario 1) or into a chain of
//!   three dependent ones (scenario 2).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{Clock, SharedClock, SimClock, TimelineRecorder};
use brisvox_engine::sim::{LoopbackCall, open_loopback};
use brisvox_engine::{
    Call, EngineConfig, InterruptionConfig, Offload, S2sOptions, S2sTurnMode, SessionBuilder,
    run_call,
};
use brisvox_providers::{
    Error as ProviderError, S2sConfig, S2sControl, S2sEvent, S2sProvider, S2sResponseStatus,
    S2sSession,
};
use brisvox_sim::input::{InputSink, InputTarget, SendFuture};
use brisvox_sim::scenario::{Scenario, play_input};
use brisvox_transport::{ClientEvent, LoopbackListener};
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::harness::MS;
use crate::world::{Delays, Which, World, tools};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn id(text: &str) -> Arc<str> {
    Arc::from(text)
}

// ---- the scripted provider -------------------------------------------------------------------------

/// One session the stage opened.
pub struct MockSession {
    pub config: S2sConfig,
    events: Mutex<Option<mpsc::Sender<S2sEvent>>>,
    controls: Arc<Mutex<Vec<S2sControl>>>,
}

impl MockSession {
    pub async fn send(&self, event: S2sEvent) {
        let sender = lock(&self.events).clone().expect("the session was closed");
        sender.send(event).await.unwrap();
    }

    pub fn controls(&self) -> Vec<S2sControl> {
        lock(&self.controls).clone()
    }

    pub fn count(&self, pick: impl Fn(&S2sControl) -> bool) -> usize {
        self.controls().iter().filter(|c| pick(c)).count()
    }

    /// The tool results sent so far: `(call id, output)`.
    pub fn results(&self) -> Vec<(String, String)> {
        self.controls()
            .into_iter()
            .filter_map(|c| match c {
                S2sControl::ToolResult { call_id, output } => {
                    Some((call_id.to_string(), output.to_string()))
                }
                _ => None,
            })
            .collect()
    }
}

#[derive(Clone, Default)]
pub struct Mock {
    sessions: Arc<Mutex<VecDeque<Arc<MockSession>>>>,
    opened: Arc<AtomicUsize>,
}

impl Mock {
    fn session(&self) -> Option<Arc<MockSession>> {
        lock(&self.sessions).front().cloned()
    }
}

impl S2sProvider for Mock {
    fn name(&self) -> &'static str {
        "scripted-s2s"
    }

    async fn connect(&self, config: S2sConfig) -> Result<S2sSession, ProviderError> {
        let (audio_tx, mut audio_rx) = mpsc::channel::<Bytes>(64);
        let (control_tx, mut control_rx) = mpsc::channel::<S2sControl>(32);
        let (event_tx, event_rx) = mpsc::channel::<S2sEvent>(1024);
        let session = Arc::new(MockSession {
            config,
            events: Mutex::new(Some(event_tx)),
            controls: Arc::default(),
        });
        tokio::spawn(async move { while audio_rx.recv().await.is_some() {} });
        let controls = Arc::clone(&session.controls);
        tokio::spawn(async move {
            while let Some(control) = control_rx.recv().await {
                lock(&controls).push(control);
            }
        });
        lock(&self.sessions).push_back(session);
        self.opened.fetch_add(1, Ordering::SeqCst);
        Ok(S2sSession {
            audio: audio_tx,
            control: control_tx,
            events: event_rx,
        })
    }
}

// ---- the rig -------------------------------------------------------------------------------------------

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

pub struct S2sRig {
    pub clock: SimClock,
    pub world: Arc<World>,
    pub call: Call,
    caller: Arc<Caller>,
    cancel: CancellationToken,
    seen: Arc<Mutex<Vec<ClientEvent>>>,
}

async fn collect(mut listener: LoopbackListener, seen: Arc<Mutex<Vec<ClientEvent>>>) {
    while let Some(event) = listener.recv().await {
        lock(&seen).push(event);
    }
}

impl S2sRig {
    pub async fn start<P: S2sProvider>(
        provider: P,
        which: Which,
        delays: Delays,
        system: &'static str,
    ) -> Self {
        let clock = SimClock::new();
        let shared: SharedClock = Arc::new(clock);
        let world = World::new(clock, delays);
        let (registry, engine) = tools(&world, which, None);
        let LoopbackCall {
            session,
            client,
            cancel,
        } = open_loopback(&shared, 65_536).await.unwrap();
        let mut config = EngineConfig::DEFAULT;
        config.lead_margin = Duration::ZERO;
        config.interruption = InterruptionConfig::OFF;
        let builder = SessionBuilder::new_s2s(
            config,
            shared.clone(),
            TimelineRecorder::new(),
            Offload::Inline(shared.clone()),
            Arc::new(provider),
            S2sOptions {
                turn: S2sTurnMode::Provider,
                input_transcription: true,
                max_buffered_audio: Duration::from_secs(60),
            },
        )
        .unwrap()
        .with_system_prompt(system)
        .with_tools(registry, engine);
        let call = run_call(session, builder, &cancel).await.unwrap();
        let (caller, listener) = client.split();
        let seen = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(collect(listener, Arc::clone(&seen)));
        Self {
            clock,
            world,
            call,
            caller: Arc::new(Caller {
                caller: tokio::sync::Mutex::new(caller),
            }),
            cancel,
            seen,
        }
    }

    /// Plays `scenario` to the call (its length passes on the clock).
    pub async fn play(&self, scenario: &Scenario) {
        let shared: SharedClock = Arc::new(self.clock);
        play_input(
            scenario,
            shared.clone(),
            shared.now(),
            SampleRate::Hz16000,
            InputSink::target(self.caller.clone()),
            &self.cancel,
        )
        .await
        .unwrap();
    }

    /// Sends PCM16 mono 16 kHz `samples` as 20 ms frames in real time, then `tail` of silence.
    pub async fn say(&self, samples: &[i16], tail: Duration) {
        let frame = |chunk: &[i16]| {
            let mut data = Vec::with_capacity(chunk.len() * 2);
            for s in chunk {
                data.extend_from_slice(&s.to_le_bytes());
            }
            Bytes::from(data)
        };
        for chunk in samples.chunks(320) {
            self.send(frame(chunk)).await;
        }
        let silence = frame(&[0i16; 320]);
        let frames = tail.as_millis() / 20;
        for _ in 0..frames {
            self.send(silence.clone()).await;
        }
    }

    async fn send(&self, pcm: Bytes) {
        self.caller
            .caller
            .lock()
            .await
            .send_audio(pcm, SampleRate::Hz16000)
            .await
            .unwrap();
        self.clock.sleep(20 * MS).await;
    }

    pub async fn until(&self, what: &str, limit: Duration, cond: impl Fn(&Self) -> bool) {
        let deadline = self.clock.elapsed() + limit;
        loop {
            if cond(self) {
                return;
            }
            assert!(
                self.clock.elapsed() < deadline,
                "timed out ({limit:?}) waiting for {what}; tools: {:?}; stats {:?}",
                self.world.events(),
                self.call.handles().s2s.snapshot()
            );
            self.clock.sleep(5 * MS).await;
        }
    }

    pub fn client(&self) -> Vec<ClientEvent> {
        lock(&self.seen).clone()
    }

    pub fn marks(&self) -> usize {
        self.client()
            .iter()
            .filter(|e| matches!(e, ClientEvent::Mark { .. }))
            .count()
    }

    /// The tool calls of each assistant message of the engine's mirror of the conversation, by
    /// name: one group per model response that called tools.
    pub fn call_groups(&self) -> Vec<Vec<String>> {
        let context = self.call.handles().context.lock().unwrap();
        context
            .messages()
            .iter()
            .filter_map(|m| match m {
                brisvox_agent::Message::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                    Some(tool_calls.iter().map(|c| c.name.to_string()).collect())
                }
                _ => None,
            })
            .collect()
    }

    pub fn answer(&self) -> String {
        let context = self.call.handles().context.lock().unwrap();
        context
            .messages()
            .iter()
            .rev()
            .find_map(|m| match m {
                brisvox_agent::Message::Assistant { text, .. } if !text.is_empty() => {
                    Some(text.to_string())
                }
                _ => None,
            })
            .unwrap_or_default()
    }
}

fn speech() -> Scenario {
    Scenario::new()
        .silence(100 * MS)
        .speak("what is the weather in paris tokyo and cairo", 800 * MS)
        .with_tail(600 * MS)
}

/// The model's response with `calls` (`(call id, tool, JSON arguments)`).
async fn call_response(session: &MockSession, response: &str, calls: &[(&str, &str, &str)]) {
    session
        .send(S2sEvent::ResponseStarted {
            response: id(response),
        })
        .await;
    for (call_id, name, arguments) in calls {
        session
            .send(S2sEvent::ToolCall {
                response: id(response),
                call_id: id(call_id),
                name: id(name),
                arguments: id(arguments),
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

/// The model's spoken answer (`ms` of audio).
async fn answer_response(session: &MockSession, response: &str, ms: u64) {
    let samples = usize::try_from(ms * 24).unwrap();
    let mut data = Vec::with_capacity(samples * 2);
    for i in 0..samples {
        let sample: i16 = if (i / 12) % 2 == 0 { 3000 } else { -3000 };
        data.extend_from_slice(&sample.to_le_bytes());
    }
    session
        .send(S2sEvent::ResponseStarted {
            response: id(response),
        })
        .await;
    session
        .send(S2sEvent::Audio {
            response: id(response),
            item: id("item_answer"),
            pcm: Bytes::from(data),
        })
        .await;
    session
        .send(S2sEvent::AudioDone {
            response: id(response),
            item: id("item_answer"),
        })
        .await;
    session
        .send(S2sEvent::ResponseDone {
            response: id(response),
            status: S2sResponseStatus::Completed,
        })
        .await;
}

/// A started call with a scripted session whose caller has spoken and whose turn the provider
/// has detected.
async fn scripted(which: Which, delays: Delays) -> (S2sRig, Arc<MockSession>) {
    let mock = Mock::default();
    let rig = S2sRig::start(mock.clone(), which, delays, "You are a test agent.").await;
    rig.play(&speech()).await;
    rig.until("a session", Duration::from_secs(30), |_| {
        mock.session().is_some()
    })
    .await;
    let session = mock.session().unwrap();
    session.send(S2sEvent::SpeechStarted).await;
    session.send(S2sEvent::SpeechStopped).await;
    (rig, session)
}

fn paris_tokyo_cairo() -> Delays {
    Delays {
        weather: vec![
            ("Paris", 600 * MS),
            ("Tokyo", 400 * MS),
            ("Cairo", 200 * MS),
        ],
        default: 300 * MS,
        lookup: 300 * MS,
        booking: 300 * MS,
    }
}

// ---- scenarios, scripted ------------------------------------------------------------------------------

/// S1 on the S2S stage: three calls of one response run at the same time; their results go to the
/// provider in issue order, and the model is asked once for its answer.
pub async fn s1_scripted() {
    let (rig, session) = scripted(Which::Weather, paris_tokyo_cairo()).await;
    assert!(
        session
            .config
            .tools
            .iter()
            .any(|t| &*t.name == "get_weather")
    );
    call_response(
        &session,
        "resp_1",
        &[
            ("c1", "get_weather", r#"{"city":"Paris"}"#),
            ("c2", "get_weather", r#"{"city":"Tokyo"}"#),
            ("c3", "get_weather", r#"{"city":"Cairo"}"#),
        ],
    )
    .await;
    rig.until("the answer request", Duration::from_secs(30), |_| {
        session.count(|c| matches!(c, S2sControl::CreateResponse)) == 1
    })
    .await;

    let events = rig.world.events_of("get_weather");
    assert_eq!(events.len(), 3, "{events:?}");
    let last_start = events.iter().map(|e| e.started).max().unwrap();
    let first_end = events.iter().filter_map(|e| e.finished).min().unwrap();
    assert!(
        last_start < first_end,
        "the lookups did not overlap: {events:?}"
    );
    let wall = crate::harness::span(&events);
    assert!(wall >= 600 * MS && wall < 650 * MS, "{wall:?}");

    // The three results, in the order of the calls, then one request for the answer.
    let results = session.results();
    let order: Vec<&str> = results.iter().map(|(call, _)| call.as_str()).collect();
    assert_eq!(order, ["c1", "c2", "c3"], "{results:?}");
    for ((_, output), want) in results.iter().zip(["Paris", "Tokyo", "Cairo"]) {
        assert!(output.contains(want), "{results:?}");
    }
    let controls = session.controls();
    let last_result = controls
        .iter()
        .rposition(|c| matches!(c, S2sControl::ToolResult { .. }))
        .unwrap();
    let create = controls
        .iter()
        .position(|c| matches!(c, S2sControl::CreateResponse))
        .unwrap();
    assert!(last_result < create, "{controls:?}");
    let stats = rig.call.handles().s2s.snapshot();
    assert_eq!((stats.tool_rounds, stats.tool_calls), (1, 3));
    assert_eq!(rig.call_groups(), [vec!["get_weather"; 3]]);

    answer_response(&session, "resp_2", 300).await;
    rig.until("the end mark", Duration::from_secs(30), |r| r.marks() == 1)
        .await;
    rig.clock.sleep(200 * MS).await;
    assert_eq!(rig.marks(), 1, "one end mark for the whole turn");
    assert_eq!(
        session.count(|c| matches!(c, S2sControl::CreateResponse)),
        1,
        "the model was asked once"
    );
}

/// S2 on the S2S stage: three rounds, each a response with one call that needs the result before.
pub async fn s2_scripted() {
    let (rig, session) = scripted(Which::GeoChain, Delays::flat(200 * MS)).await;
    let rounds: [(&str, &str, &str, &str); 3] = [
        (
            "c1",
            "get_largest_country",
            r#"{"continent":"Africa"}"#,
            "Algeria",
        ),
        ("c2", "get_capital", r#"{"country":"Algeria"}"#, "Algiers"),
        ("c3", "get_weather", r#"{"city":"Algiers"}"#, "25"),
    ];
    for (round, (call_id, tool, arguments, wanted)) in rounds.iter().enumerate() {
        call_response(
            &session,
            &format!("resp_{}", round + 1),
            &[(call_id, tool, arguments)],
        )
        .await;
        rig.until("the tool result", Duration::from_secs(30), |_| {
            session.count(|c| matches!(c, S2sControl::CreateResponse)) == round + 1
        })
        .await;
        let results = session.results();
        assert_eq!(results.len(), round + 1, "{results:?}");
        let (last_id, output) = results.last().unwrap();
        assert_eq!(last_id, call_id);
        assert!(output.contains(wanted), "{output}");
    }
    let events = rig.world.events();
    let order: Vec<&str> = events.iter().map(|e| e.tool.as_str()).collect();
    assert_eq!(order, ["get_largest_country", "get_capital", "get_weather"]);
    for pair in events.windows(2) {
        assert!(pair[1].started >= pair[0].finished.unwrap(), "{events:?}");
    }
    let stats = rig.call.handles().s2s.snapshot();
    assert_eq!((stats.tool_rounds, stats.tool_calls), (3, 3));
    answer_response(&session, "resp_4", 300).await;
    rig.until("the end mark", Duration::from_secs(30), |r| r.marks() == 1)
        .await;
}

/// S3 on the S2S stage: the model repeats a booking in a later response; it runs once.
pub async fn s3_scripted() {
    let delays = Delays {
        weather: Vec::new(),
        default: 200 * MS,
        lookup: 200 * MS,
        booking: 400 * MS,
    };
    let (rig, session) = scripted(Which::Booking, delays).await;
    let booking = r#"{"name":"Kaya","party_size":2,"time":"19:30"}"#;
    call_response(
        &session,
        "resp_1",
        &[
            ("c1", "book_table", booking),
            ("c2", "get_weather", r#"{"city":"Paris"}"#),
        ],
    )
    .await;
    rig.until("the first results", Duration::from_secs(30), |_| {
        session.count(|c| matches!(c, S2sControl::CreateResponse)) == 1
    })
    .await;
    call_response(&session, "resp_2", &[("c3", "book_table", booking)]).await;
    rig.until("the second results", Duration::from_secs(30), |_| {
        session.count(|c| matches!(c, S2sControl::CreateResponse)) == 2
    })
    .await;
    assert_eq!(rig.world.bookings.load(Ordering::SeqCst), 1, "booked twice");
    let results = session.results();
    assert_eq!(results.len(), 3, "{results:?}");
    assert!(results[0].1.contains("BK-1001"), "{results:?}");
    assert!(results[2].1.contains("already_done"), "{results:?}");
    answer_response(&session, "resp_3", 300).await;
    rig.until("the end mark", Duration::from_secs(30), |r| r.marks() == 1)
        .await;
}

// ---- live: Gemini Live ---------------------------------------------------------------------------------------

/// The PCM16 samples `start..end` of a fixture WAV (16 kHz mono).
fn fixture(name: &str, start: usize, end: usize) -> Vec<i16> {
    let path = format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let bytes = std::fs::read(&path).expect("fixture");
    let mut at = 12;
    let data = loop {
        let id = &bytes[at..at + 4];
        let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        if id == b"data" {
            break &bytes[at + 8..at + 8 + len];
        }
        at += 8 + len + (len & 1);
    };
    data.chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .skip(start)
        .take(end - start)
        .collect()
}

const GEO_SYSTEM: &str = "You are a voice assistant on a phone call. The caller says one sentence that names a region, an ocean and a continent. Immediately call lookup_region, lookup_ocean and lookup_continent, one call for each name, all three in the same step because they do not depend on each other. Then answer in two short spoken sentences using what the tools returned.";

const CHAIN_SYSTEM: &str = "You are a voice assistant on a phone call. The caller says one sentence that names a continent. Find its largest country with get_largest_country, then that country's capital with get_capital, then the weather in that capital with get_weather. Each step needs the result of the one before, so call one tool at a time. Then answer in one short spoken sentence naming the capital and the weather.";

/// Gemini Live with the real tool engine: `which` 1 is the parallel scenario, 2 the chain.
/// Loose assertions; prints what happened.
pub async fn live_gemini_live(scenario: u8) {
    use brisvox_core::{Secret, SystemClock};
    use brisvox_providers::gemini_live::{GeminiLive, GeminiLiveConfig};

    let Some(key) = ["GEMINI_API_KEY", "GOOGLE_API_KEY"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|k| !k.is_empty()))
    else {
        eprintln!("skipped: GEMINI_API_KEY is not set");
        return;
    };
    let mut config = GeminiLiveConfig::new(Secret::new(key));
    config.model = std::env::var("GEMINI_LIVE_MODEL")
        .ok()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "gemini-3.8-live".to_owned());
    config.io_timeout = Duration::from_secs(20);
    eprintln!(
        "--- S{scenario} on the S2S stage [gemini-live {}] ---",
        config.model
    );
    let provider = GeminiLive::new(config, SystemClock::shared());
    let (which, system) = match scenario {
        1 => (Which::Geo, GEO_SYSTEM),
        _ => (Which::GeoChain, CHAIN_SYSTEM),
    };
    let rig = S2sRig::start(provider, which, Delays::flat(300 * MS), system).await;
    // "The East African Islands are in the Indian Ocean off the eastern coast of Africa."
    let sentence = fixture("en_tr_mixed.wav", 6_400, 71_200);
    rig.clock.sleep(1_500 * MS).await;
    rig.say(&sentence, 3_000 * MS).await;
    rig.until("the end of the turn", Duration::from_secs(90), |r| {
        r.marks() >= 1
    })
    .await;
    rig.clock.sleep(500 * MS).await;

    let events = rig.world.events();
    let stats = rig.call.handles().s2s.snapshot();
    for e in &events {
        eprintln!(
            "  tool {} {} @{} ms .. {}",
            e.tool,
            e.args,
            e.started.as_millis(),
            e.finished
                .map_or("-".to_owned(), |d| format!("{} ms", d.as_millis()))
        );
    }
    eprintln!(
        "  stage: tool rounds {}, tool calls {}, groups {:?}",
        stats.tool_rounds,
        stats.tool_calls,
        rig.call_groups()
    );
    eprintln!("  answer (transcript): {:?}", rig.answer());
    assert!(rig.marks() >= 1, "the model never finished its answer");
    match scenario {
        1 => {
            let names: std::collections::BTreeSet<&str> =
                events.iter().map(|e| e.tool.as_str()).collect();
            assert!(names.len() >= 2, "the model used {names:?}");
            let biggest = rig.call_groups().iter().map(Vec::len).max().unwrap_or(0);
            assert!(
                biggest >= 2,
                "the model never asked for lookups together: {:?}",
                rig.call_groups()
            );
            assert!(
                events
                    .iter()
                    .enumerate()
                    .any(|(i, a)| events.iter().skip(i + 1).any(|b| {
                        a.started < b.finished.unwrap_or(Duration::MAX)
                            && b.started < a.finished.unwrap_or(Duration::MAX)
                    })),
                "no two lookups overlapped: {events:?}"
            );
        }
        _ => {
            let order: Vec<&str> = events.iter().map(|e| e.tool.as_str()).collect();
            assert_eq!(
                order.first().copied(),
                Some("get_largest_country"),
                "{events:?}"
            );
            assert!(stats.tool_rounds >= 3, "{stats:?}");
            let capital = events.iter().find(|e| e.tool == "get_capital");
            assert!(
                capital.is_some_and(|e| e.args["country"] == "Algeria"),
                "{events:?}"
            );
        }
    }
}
