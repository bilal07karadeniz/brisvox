//! Connection pre-warming on the turn-1 critical path (`docs/PIPELINE.md` section 8).
//!
//! The whole production chain (pump, `AudioIn`, VAD worker, `SttStage`, `TurnDriver`,
//! `LlmRunner`, `SegmenterStage`, `TtsStage`, `TransportOut` over the loopback transport) runs a
//! scripted call on a paused clock. The providers are the simulator's mocks wrapped in a probe
//! that gives every `warm()` and every `connect()` a simulated handshake time (as a real
//! network would) and writes each one, with its start and finish on the injected clock, into a
//! ledger. The assertions are on that ledger and on the stages' counters:
//!
//! - every STT, TTS and LLM setup (warm-up or connect) FINISHED before the caller started to
//!   speak and so before the first VAD stop (`TurnEvent::UserSpeechEnd` of the timeline, read
//!   back from the run), so none of them is on the critical path of turn 1 (nor of any later
//!   turn); the same holds for a second call in the same process (own sockets, own warm-ups;
//!   the shared HTTP pool itself is proved on the real adapters in `brisvox-providers`);
//! - exactly one connect per socket stage and one warm-up per provider stage for the whole call,
//!   also over several turns (no reconnect, no handshake per turn);
//! - a failing or hanging warm-up is counted in `warm_failures`, never fails the call, and the
//!   connect still happens right after the short warm-up budget, also with the default 5 s
//!   connect timeout;
//! - a TTS socket that died while the call was quiet is replaced when the next response starts,
//!   overlapped with the LLM's time to first token, so it costs that turn nothing;
//! - an STT session that died while the call was quiet is replaced after the reconnect backoff
//!   alone: the reconnect does not warm the provider again (one warm-up for the whole call);
//! - the checker itself can fail: a handshake slower than the head start is reported.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_agent::Language;
use brisvox_core::{Pipeline, SharedClock, SimClock, TurnEvent};
use brisvox_engine::{
    AudioIn, CallRatesCell, EnergyModel, EngineConfig, InterruptionConfig, LlmRunner, LlmStats,
    Offload, SegmenterStage, SegmenterStageConfig, SttStage, SttStageConfig, SttStats,
    TransportOut, TransportOutConfig, TtsStage, TtsStageConfig, TtsStats, TurnDriver,
    TurnDriverConfig, VadSetup, VadWorkerConfig,
};
use brisvox_inference::VadParams;
use brisvox_providers::{
    LlmProvider, LlmRequest, LlmStream, SttConfig, SttProvider, SttSession, TtsConfig, TtsProvider,
    TtsSession,
};
use brisvox_sim::factory::{BuiltSession, SessionFactory, SessionParts};
use brisvox_sim::input::{InputSink, InputTarget};
use brisvox_sim::mock_llm::{LlmScript, MockLlm};
use brisvox_sim::mock_stt::MockStt;
use brisvox_sim::mock_tts::MockTts;
use brisvox_sim::output::{OutputLog, WireKind};
use brisvox_sim::reference::{PlayLog, SessionProbes, WordLog};
use brisvox_sim::scenario::Scenario;
use brisvox_sim::session::SimSession;
use brisvox_transport::ClientEvent;
use common::MS;
use common::loopback::{self, LoopbackOptions};
use tokio::sync::mpsc;
use tokio::time::Instant;

/// What a probe's `warm()` does.
#[derive(Clone, Copy, Debug)]
enum Warm {
    /// Succeeds after this much simulated time.
    After(Duration),
    /// Fails after this much simulated time.
    Fails(Duration),
    /// Never answers.
    Hangs,
}

/// One finished handshake or warm-up.
#[derive(Clone, Copy, Debug)]
struct Setup {
    stage: &'static str,
    what: &'static str,
    started: Instant,
    finished: Instant,
}

type Ledger = Arc<Mutex<Vec<Setup>>>;

/// A mock provider with simulated handshake times and a ledger of what happened when.
#[derive(Clone)]
struct Probe<P> {
    inner: P,
    stage: &'static str,
    clock: SharedClock,
    ledger: Ledger,
    warm: Warm,
    connect_time: Duration,
    /// The first TTS session dies this long after its connect (a provider closing an idle
    /// socket); later ones live.
    tts_idle_death: Option<Duration>,
    tts_connects: Arc<AtomicUsize>,
    /// The first STT session dies this long after its connect; later ones live.
    stt_idle_death: Option<Duration>,
    stt_connects: Arc<AtomicUsize>,
    /// Latency of an LLM request to its first event.
    llm_latency: Duration,
}

impl<P> Probe<P> {
    fn new(
        inner: P,
        stage: &'static str,
        clock: &SharedClock,
        ledger: &Ledger,
        knobs: &Knobs,
        shared: &Shared,
    ) -> Self {
        Self {
            inner,
            stage,
            clock: clock.clone(),
            ledger: Arc::clone(ledger),
            warm: knobs.warm,
            connect_time: knobs.connect_time,
            tts_idle_death: knobs.tts_idle_death,
            tts_connects: Arc::clone(&shared.tts_connects),
            stt_idle_death: knobs.stt_idle_death,
            stt_connects: Arc::clone(&shared.stt_connects),
            llm_latency: knobs.llm_latency,
        }
    }

    async fn warm_up(&self) -> brisvox_providers::Result<()> {
        let started = self.clock.now();
        let result = match self.warm {
            Warm::After(time) => {
                self.clock.sleep(time).await;
                Ok(())
            }
            Warm::Fails(time) => {
                self.clock.sleep(time).await;
                Err(brisvox_providers::Error::Disconnected { provider: "probe" })
            }
            Warm::Hangs => std::future::pending().await,
        };
        self.record("warm", started);
        result
    }

    async fn handshake(&self) {
        let started = self.clock.now();
        self.clock.sleep(self.connect_time).await;
        self.record("connect", started);
    }

    fn record(&self, what: &'static str, started: Instant) {
        self.ledger
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Setup {
                stage: self.stage,
                what,
                started,
                finished: self.clock.now(),
            });
    }
}

impl Probe<MockTts> {
    /// Relays `session` for `lifetime` and then drops it: the event channel closes, as when the
    /// provider closes an idle socket.
    fn dying(&self, session: TtsSession, lifetime: Duration) -> TtsSession {
        let TtsSession {
            input,
            cancel,
            mut events,
        } = session;
        let (input_tx, mut input_rx) = mpsc::channel(256);
        let (cancel_tx, mut cancel_rx) = mpsc::channel(64);
        let (events_tx, events_rx) = mpsc::channel(256);
        let clock = self.clock.clone();
        tokio::spawn(async move {
            let death = clock.sleep(lifetime);
            tokio::pin!(death);
            loop {
                tokio::select! {
                    () = &mut death => break,
                    Some(item) = input_rx.recv() => {
                        if input.send(item).await.is_err() {
                            break;
                        }
                    }
                    Some(id) = cancel_rx.recv() => {
                        let _ = cancel.send(id).await;
                    }
                    Some(event) = events.recv() => {
                        if events_tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    else => break,
                }
            }
        });
        TtsSession {
            input: input_tx,
            cancel: cancel_tx,
            events: events_rx,
        }
    }
}

impl Probe<MockStt> {
    /// Relays `session` for `lifetime` and then drops it: the event channel closes, as when the
    /// provider closes the connection.
    fn dying(&self, session: SttSession, lifetime: Duration) -> SttSession {
        let SttSession { input, mut events } = session;
        let (input_tx, mut input_rx) = mpsc::channel(256);
        let (events_tx, events_rx) = mpsc::channel(256);
        let clock = self.clock.clone();
        tokio::spawn(async move {
            let death = clock.sleep(lifetime);
            tokio::pin!(death);
            loop {
                tokio::select! {
                    () = &mut death => break,
                    Some(item) = input_rx.recv() => {
                        if input.send(item).await.is_err() {
                            break;
                        }
                    }
                    Some(event) = events.recv() => {
                        if events_tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    else => break,
                }
            }
        });
        SttSession {
            input: input_tx,
            events: events_rx,
        }
    }
}

impl SttProvider for Probe<MockStt> {
    fn name(&self) -> &'static str {
        "probe-stt"
    }

    async fn connect(&self, config: SttConfig) -> brisvox_providers::Result<SttSession> {
        self.handshake().await;
        let session = self.inner.connect(config).await?;
        let first = self.stt_connects.fetch_add(1, Ordering::SeqCst) == 0;
        match self.stt_idle_death {
            Some(lifetime) if first => Ok(self.dying(session, lifetime)),
            _ => Ok(session),
        }
    }

    async fn warm(&self) -> brisvox_providers::Result<()> {
        self.warm_up().await
    }
}

impl TtsProvider for Probe<MockTts> {
    fn name(&self) -> &'static str {
        "probe-tts"
    }

    async fn connect(&self, config: TtsConfig) -> brisvox_providers::Result<TtsSession> {
        self.handshake().await;
        let session = self.inner.connect(config).await?;
        let first = self.tts_connects.fetch_add(1, Ordering::SeqCst) == 0;
        match self.tts_idle_death {
            Some(lifetime) if first => Ok(self.dying(session, lifetime)),
            _ => Ok(session),
        }
    }

    async fn warm(&self) -> brisvox_providers::Result<()> {
        self.warm_up().await
    }
}

impl LlmProvider for Probe<MockLlm> {
    fn name(&self) -> &'static str {
        "probe-llm"
    }

    async fn stream(&self, request: LlmRequest) -> brisvox_providers::Result<LlmStream> {
        self.clock.sleep(self.llm_latency).await;
        self.inner.stream(request).await
    }

    async fn warm(&self) -> brisvox_providers::Result<()> {
        self.warm_up().await
    }
}

/// How the simulated network behaves.
#[derive(Clone, Copy, Debug)]
struct Knobs {
    warm: Warm,
    connect_time: Duration,
    /// `Timeouts::connect` of the engine (the warm-up deadline is the shorter of it and 400 ms).
    connect_timeout: Duration,
    /// The first TTS session dies this long after its connect.
    tts_idle_death: Option<Duration>,
    /// The first STT session dies this long after its connect.
    stt_idle_death: Option<Duration>,
    /// Latency of an LLM request to its first event.
    llm_latency: Duration,
}

/// State shared by the calls of one factory (one "process").
#[derive(Default)]
struct Shared {
    tts_connects: Arc<AtomicUsize>,
    stt_connects: Arc<AtomicUsize>,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            warm: Warm::After(60 * MS),
            connect_time: 150 * MS,
            connect_timeout: Duration::from_secs(5),
            tts_idle_death: None,
            stt_idle_death: None,
            llm_latency: Duration::ZERO,
        }
    }
}

/// Handles of one built call.
#[derive(Clone)]
struct CallHandles {
    ledger: Ledger,
    stt: SttStats,
    tts: TtsStats,
    llm: LlmStats,
}

#[derive(Clone)]
struct PrewarmFactory {
    knobs: Knobs,
    shared: Arc<Shared>,
    calls: Arc<Mutex<Vec<CallHandles>>>,
}

impl PrewarmFactory {
    fn new(knobs: Knobs) -> Self {
        Self {
            knobs,
            shared: Arc::default(),
            calls: Arc::default(),
        }
    }

    fn handles(&self) -> Vec<CallHandles> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl SessionFactory for PrewarmFactory {
    async fn build(&self, parts: SessionParts) -> brisvox_sim::Result<BuiltSession> {
        let SessionParts {
            clock,
            stt,
            llm,
            tts,
            vad,
            timeline,
            origin,
            ..
        } = parts;
        let mut config = EngineConfig::DEFAULT;
        config.interruption = InterruptionConfig::OFF;
        config.interruption.vad_hangover = Duration::ZERO;
        config.turn = brisvox_agent::TurnPolicy::fixed(450 * MS);
        config.lead_margin = Duration::ZERO;
        config.timeouts.connect = self.knobs.connect_timeout;

        let ledger = Ledger::default();
        let stt = Probe::new(stt, "stt", &clock, &ledger, &self.knobs, &self.shared);
        let tts = Probe::new(tts, "tts", &clock, &ledger, &self.knobs, &self.shared);
        // The LLM has no handshake of its own in the mock: only the warm-up is probed.
        let llm = Probe::new(llm, "llm", &clock, &ledger, &self.knobs, &self.shared);

        let rates = CallRatesCell::new(config.rates.fallback_call);
        let (input, commands) = loopback::open_with(
            &clock,
            &config,
            LoopbackOptions {
                client_queue: Some(65_536),
                output: Some(rates.clone()),
            },
        )
        .await
        .map_err(|e| brisvox_sim::Error::Assertion(format!("prewarm setup: {e}")))?;
        let book = Arc::clone(&input.book);

        let vad_setup = VadSetup::new(
            Offload::Inline(clock.clone()),
            VadWorkerConfig {
                params: VadParams {
                    threshold: 0.5,
                    neg_threshold: 0.35,
                    min_speech: vad.start_after,
                    min_silence: vad.stop_after,
                    speech_pad: Duration::ZERO,
                },
                ..VadWorkerConfig::from_engine(&config)
            },
            move || {
                #[allow(clippy::cast_possible_truncation)] // a threshold of a few hundred
                Ok(EnergyModel::from_pcm_rms(vad.threshold as f32))
            },
        );
        let audio_in = AudioIn::new(&config).with_vad(vad_setup);
        let stt_stage = SttStage::new(
            Arc::new(stt),
            SttStageConfig::from_engine(&config),
            clock.clone(),
        );
        let driver = TurnDriver::new(
            TurnDriverConfig::from_engine(&config),
            clock.clone(),
            timeline.clone(),
        )
        .with_commands(commands.clone());
        let llm_stage = LlmRunner::builder(Arc::new(llm), &config, clock.clone(), timeline.clone())
            .with_book(Arc::clone(&book))
            .build();
        let segmenter = SegmenterStage::new(SegmenterStageConfig::new(Language::En));
        let tts_stage = TtsStage::new(
            Arc::new(tts),
            TtsStageConfig::from_engine(&config),
            clock.clone(),
            timeline.clone(),
        )
        .with_book(Arc::clone(&book))
        .with_rates(rates)
        .with_offload(Offload::Inline(clock.clone()));
        let transport_out = TransportOut::new(
            commands,
            Arc::clone(&book),
            TransportOutConfig::from_engine(&config),
            clock.clone(),
            timeline.clone(),
        );

        let handles = CallHandles {
            ledger,
            stt: stt_stage.stats(),
            tts: tts_stage.stats(),
            llm: llm_stage.stats(),
        };
        let probes = SessionProbes::new(
            llm_stage.context(),
            WordLog::default(),
            PlayLog::default(),
            llm_stage.truncations(),
        );
        let output = OutputLog::new(origin, None);

        let queues = config.queues;
        let pipeline = Pipeline::new("hybrid-prewarm")
            .then_with_queue(audio_in, queues.input)
            .then_with_queue(stt_stage, queues.user)
            .then_with_queue(driver, queues.user)
            .then_with_queue(llm_stage, queues.bot)
            .then_with_queue(segmenter, queues.bot)
            .then_with_queue(tts_stage, queues.bot)
            .then_with_queue(transport_out, queues.sink);
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(handles);

        let drain_input = Arc::clone(&input);
        let drain_output = output.clone();
        Ok(BuiltSession::new(
            pipeline,
            probes,
            InputSink::target(Arc::clone(&input) as Arc<dyn InputTarget>),
            output,
        )
        .with_drain(move || async move {
            while let Some(event) = drain_input.recv_client(MS).await {
                if let ClientEvent::Audio { frame, received_at } = &event {
                    drain_output.record_wire(
                        WireKind::Audio,
                        *received_at,
                        Some(frame.epoch()),
                        Some(frame.duration()),
                    );
                }
            }
        })
        .with_guard(input))
    }
}

/// The setups of one stage and kind.
fn setups(ledger: &Ledger, stage: &str, what: &str) -> Vec<Setup> {
    ledger
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|s| s.stage == stage && s.what == what)
        .copied()
        .collect()
}

/// The turn-1 critical-path check: every recorded handshake or warm-up must have FINISHED by
/// `deadline` (the instant the caller starts to speak, which is before the first VAD stop).
/// Returns the offenders.
fn setups_after(ledger: &Ledger, deadline: Instant) -> Vec<Setup> {
    ledger
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|s| s.finished > deadline)
        .copied()
        .collect()
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// The caller waits `lead` and then says something for 900 ms; a second utterance follows when
/// `turns` is 2.
fn scenario(lead: Duration, turns: usize) -> Scenario {
    let mut scenario = Scenario::new()
        .silence(lead)
        .speak("what time is it in Tokyo", ms(900));
    for _ in 1..turns {
        scenario = scenario
            .silence(ms(2_500))
            .speak("and in Paris please", ms(900));
    }
    scenario.with_tail(ms(600))
}

/// The timeline marks of one turn that the assertions compare the ledger with.
#[derive(Clone, Copy, Debug)]
struct TurnMarks {
    /// The VAD stop (`TurnEvent::UserSpeechEnd`).
    vad_stop: Option<Instant>,
    llm_request: Option<Instant>,
    llm_first_token: Option<Instant>,
}

/// What a run returns.
struct Ran {
    origin: Instant,
    /// A spoken answer reached the wire and the pipeline ended well.
    answered: bool,
    turns: Vec<TurnMarks>,
}

async fn run(factory: &PrewarmFactory, scenario: &Scenario, turns: usize) -> Ran {
    let session = SimSession::new(Arc::new(SimClock::new()));
    for _ in 0..turns {
        session
            .llm
            .push(LlmScript::text(["It is nine in the morning."]));
    }
    let run = session
        .run_with(factory, scenario, ms(2_500))
        .await
        .unwrap();
    let heard = run.wire.iter().any(|r| r.kind == WireKind::Audio);
    Ran {
        origin: run.origin,
        answered: heard && run.outcome.is_ok(),
        turns: run
            .turns
            .iter()
            .map(|turn| TurnMarks {
                vad_stop: turn.at(TurnEvent::UserSpeechEnd),
                llm_request: turn.at(TurnEvent::LlmRequest),
                llm_first_token: turn.at(TurnEvent::LlmFirstToken),
            })
            .collect(),
    }
}

/// The checks of the turn-1 critical path on one call's ledger: every provider stage was set
/// up once, in the right order, long before the caller spoke, and nothing finished after the
/// caller started to speak nor after the first VAD stop of the timeline.
fn assert_turn_one_is_clear(call: &CallHandles, ran: &Ran, speech_start: Duration) {
    let origin = ran.origin;
    for (stage, what) in [
        ("stt", "warm"),
        ("stt", "connect"),
        ("tts", "warm"),
        ("tts", "connect"),
        ("llm", "warm"),
    ] {
        let found = setups(&call.ledger, stage, what);
        assert_eq!(found.len(), 1, "{stage} {what}: {found:?}");
        // They started with the pipeline (the Start frame).
        if what == "warm" {
            assert!(
                found[0].started <= origin + ms(5),
                "{stage} {what} started late: {:?}",
                found[0]
            );
        }
    }
    assert_eq!(setups(&call.ledger, "llm", "connect").len(), 0);
    // The socket stages warm first and connect immediately after (no gap: the connect is not
    // held back by anything else).
    for stage in ["stt", "tts"] {
        let warm = setups(&call.ledger, stage, "warm")[0];
        let connect = setups(&call.ledger, stage, "connect")[0];
        assert!(
            connect.started >= warm.finished && connect.started <= warm.finished + ms(1),
            "{stage}: {warm:?} {connect:?}"
        );
    }

    // The critical-path claim, against what the timeline observed: the first VAD stop is
    // after the caller started to speak, and no setup finished after either.
    let vad_stop = ran.turns[0].vad_stop.expect("a VAD stop in turn 1");
    assert!(vad_stop > origin + speech_start, "{vad_stop:?}");
    let offenders = setups_after(&call.ledger, origin + speech_start);
    assert!(
        offenders.is_empty(),
        "setup after speech start: {offenders:?}"
    );
    let offenders = setups_after(&call.ledger, vad_stop);
    assert!(
        offenders.is_empty(),
        "setup after the first VAD stop: {offenders:?}"
    );

    // The stages' own counters agree.
    let stt = call.stt.snapshot();
    assert_eq!((stt.connects, stt.connect_failures), (1, 0), "{stt:?}");
    assert_eq!((stt.warms, stt.warm_failures), (1, 0), "{stt:?}");
    assert_eq!(stt.finals_synthetic, 0, "{stt:?}");
    let tts = call.tts.snapshot();
    assert_eq!((tts.connects, tts.connect_failures), (1, 0), "{tts:?}");
    assert_eq!((tts.warms, tts.warm_failures), (1, 0), "{tts:?}");
    let llm = call.llm.snapshot();
    assert_eq!((llm.warms, llm.warm_failures), (1, 0), "{llm:?}");
}

#[tokio::test(start_paused = true)]
async fn no_handshake_or_warm_up_is_on_the_critical_path_of_turn_one() {
    let factory = PrewarmFactory::new(Knobs::default());
    let scenario = scenario(ms(500), 1);
    let ran = run(&factory, &scenario, 1).await;
    assert!(ran.answered, "the call must produce a spoken answer");

    let (speech_start, speech_end) = scenario.speech_segments()[0];
    assert_eq!(speech_start, ms(500));
    let calls = factory.handles();
    assert_eq!(calls.len(), 1);
    assert_turn_one_is_clear(&calls[0], &ran, speech_start);
    assert!(
        setups_after(&calls[0].ledger, ran.origin + speech_end).is_empty(),
        "setup after the first speech ended"
    );
}

#[tokio::test(start_paused = true)]
async fn a_second_call_in_the_same_process_has_the_same_clean_turn_one() {
    // Two calls, one factory: the sockets and the warm-ups are per call (each connects and warms
    // once, at its own Start). The sharing of the HTTP pool is not modelled here: it is proved
    // on the real adapters (`brisvox-providers`, `pool_stats` deltas).
    let factory = PrewarmFactory::new(Knobs::default());
    let scenario = scenario(ms(500), 1);
    let first = run(&factory, &scenario, 1).await;
    let second = run(&factory, &scenario, 1).await;
    assert!(first.answered && second.answered);
    let calls = factory.handles();
    assert_eq!(calls.len(), 2);
    for (call, ran) in calls.iter().zip([&first, &second]) {
        assert_turn_one_is_clear(call, ran, ms(500));
    }
    // Each call opened its own sockets, once.
    for call in &calls {
        assert_eq!(setups(&call.ledger, "stt", "connect").len(), 1);
        assert_eq!(setups(&call.ledger, "tts", "connect").len(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn later_turns_open_no_connection_either() {
    let factory = PrewarmFactory::new(Knobs::default());
    let scenario = scenario(ms(500), 2);
    let ran = run(&factory, &scenario, 2).await;
    assert!(ran.answered);
    let call = &factory.handles()[0];
    // Two full turns later there is still exactly one connect per socket and one warm-up each.
    assert_eq!(setups(&call.ledger, "stt", "connect").len(), 1);
    assert_eq!(setups(&call.ledger, "tts", "connect").len(), 1);
    assert_eq!(call.ledger.lock().unwrap().len(), 5);
    assert!(setups_after(&call.ledger, ran.origin + ms(500)).is_empty());
    assert_eq!(call.stt.snapshot().connects, 1);
    assert_eq!(call.tts.snapshot().connects, 1);
    assert_eq!(call.tts.snapshot().utterance_starts, 2);
}

#[tokio::test(start_paused = true)]
async fn the_checker_reports_a_handshake_that_is_still_open_when_the_caller_speaks() {
    // A control for the assertions above: with a handshake slower than the head start the same
    // check must fail, or it would prove nothing.
    let factory = PrewarmFactory::new(Knobs {
        connect_time: ms(1_500),
        ..Knobs::default()
    });
    let scenario = scenario(ms(500), 1);
    let ran = run(&factory, &scenario, 1).await;
    let call = &factory.handles()[0];
    let offenders = setups_after(&call.ledger, ran.origin + ms(500));
    assert!(
        offenders
            .iter()
            .any(|s| s.what == "connect" && (s.stage == "stt" || s.stage == "tts")),
        "{offenders:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failing_warm_up_is_counted_and_the_call_goes_on() {
    let factory = PrewarmFactory::new(Knobs {
        warm: Warm::Fails(ms(30)),
        ..Knobs::default()
    });
    let scenario = scenario(ms(500), 1);
    let ran = run(&factory, &scenario, 1).await;
    assert!(ran.answered, "a failed warm-up must not fail the call");
    let call = &factory.handles()[0];
    let stt = call.stt.snapshot();
    assert_eq!(
        (stt.warms, stt.warm_failures, stt.connects),
        (0, 1, 1),
        "{stt:?}"
    );
    let tts = call.tts.snapshot();
    assert_eq!(
        (tts.warms, tts.warm_failures, tts.connects),
        (0, 1, 1),
        "{tts:?}"
    );
    let llm = call.llm.snapshot();
    assert_eq!((llm.warms, llm.warm_failures), (0, 1), "{llm:?}");
    assert!(setups_after(&call.ledger, ran.origin + ms(500)).is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_hanging_warm_up_is_cut_at_the_deadline_and_the_connect_still_happens() {
    let factory = PrewarmFactory::new(Knobs {
        warm: Warm::Hangs,
        connect_timeout: ms(300),
        ..Knobs::default()
    });
    // 300 ms warm-up deadline (the connect timeout is shorter than the warm-up budget) + 150 ms
    // connect: ready at 450 ms, the caller speaks at 700 ms.
    let scenario = scenario(ms(700), 1);
    let ran = run(&factory, &scenario, 1).await;
    let origin = ran.origin;
    assert!(ran.answered);
    let call = &factory.handles()[0];
    let stt = call.stt.snapshot();
    assert_eq!(
        (stt.warms, stt.warm_failures, stt.connects),
        (0, 1, 1),
        "{stt:?}"
    );
    let tts = call.tts.snapshot();
    assert_eq!(
        (tts.warms, tts.warm_failures, tts.connects),
        (0, 1, 1),
        "{tts:?}"
    );
    let llm = call.llm.snapshot();
    assert_eq!((llm.warms, llm.warm_failures), (0, 1), "{llm:?}");
    let connect = setups(&call.ledger, "stt", "connect")[0];
    assert!(
        connect.started >= origin + ms(300) && connect.finished <= origin + ms(700),
        "{connect:?}"
    );
}

/// The default engine configuration (5 s connect timeout) with a warm-up that never answers: the
/// warm-up gets a short budget of its own, so the sockets are ready well before a caller who
/// speaks one second after the start, and the TTS stage is not stuck for 2 x 5 s.
#[tokio::test(start_paused = true)]
async fn a_hanging_warm_up_with_the_default_timeout_delays_the_connect_by_the_warm_budget_only() {
    let factory = PrewarmFactory::new(Knobs {
        warm: Warm::Hangs,
        ..Knobs::default()
    });
    assert_eq!(Knobs::default().connect_timeout, Duration::from_secs(5));
    let scenario = scenario(ms(1_000), 1);
    let ran = run(&factory, &scenario, 1).await;
    let origin = ran.origin;
    assert!(ran.answered, "the call must still be answered");
    let call = &factory.handles()[0];
    for stage in ["stt", "tts"] {
        let connect = setups(&call.ledger, stage, "connect");
        assert_eq!(connect.len(), 1, "{stage}: {connect:?}");
        // The warm-up budget is 400 ms: the connect starts then, takes 150 ms, and is done 450
        // ms before the caller speaks.
        assert!(
            connect[0].started >= origin + ms(400) && connect[0].started <= origin + ms(401),
            "{stage}: {:?}",
            connect[0]
        );
        assert!(
            connect[0].finished <= origin + ms(1_000),
            "{stage}: {:?}",
            connect[0]
        );
    }
    let stt = call.stt.snapshot();
    assert_eq!(
        (stt.warms, stt.warm_failures, stt.connects),
        (0, 1, 1),
        "{stt:?}"
    );
    let tts = call.tts.snapshot();
    assert_eq!(
        (tts.warms, tts.warm_failures, tts.connects),
        (0, 1, 1),
        "{tts:?}"
    );
    let vad_stop = ran.turns[0].vad_stop.expect("a VAD stop");
    assert!(setups_after(&call.ledger, vad_stop).is_empty());
}

/// A TTS socket that the provider closed while the call was quiet is replaced when the next
/// response STARTS (not when its first text arrives), so the reconnect overlaps the LLM's time to
/// first token and costs the turn nothing. (The sockets of the other stages are untouched.)
#[tokio::test(start_paused = true)]
async fn a_tts_socket_that_died_while_idle_is_replaced_off_the_critical_path_of_the_next_turn() {
    let factory = PrewarmFactory::new(Knobs {
        tts_idle_death: Some(ms(6_000)),
        llm_latency: ms(300),
        ..Knobs::default()
    });
    // Turn 1 is spoken and answered by about 4 s; the socket dies at about 6.2 s; the caller
    // speaks again at 9.4 s.
    let scenario = Scenario::new()
        .silence(ms(500))
        .speak("what time is it in Tokyo", ms(900))
        .silence(ms(8_000))
        .speak("and in Paris please", ms(900))
        .with_tail(ms(600));
    let ran = run(&factory, &scenario, 2).await;
    assert!(ran.answered);
    let call = &factory.handles()[0];

    let connects = setups(&call.ledger, "tts", "connect");
    assert_eq!(connects.len(), 2, "{connects:?}");
    let (first, second) = (connects[0], connects[1]);
    assert!(
        second.started >= first.finished + ms(6_000),
        "reconnected before the socket died: {first:?} {second:?}"
    );
    assert_eq!(ran.turns.len(), 2);
    let turn2 = ran.turns[1];
    let (llm_request, first_token) = (turn2.llm_request.unwrap(), turn2.llm_first_token.unwrap());
    assert!(
        second.started >= turn2.vad_stop.unwrap(),
        "the reconnect belongs to turn 2: {second:?}"
    );
    assert!(
        second.started <= llm_request,
        "reconnect must start with the response, not with its first text: {second:?} vs {llm_request:?}"
    );
    assert!(
        second.finished <= first_token,
        "reconnect finished after the LLM's first token: {second:?} vs {first_token:?}"
    );
    let tts = call.tts.snapshot();
    assert_eq!(tts.connects, 2, "{tts:?}");
    assert_eq!(
        tts.utterance_starts, 2,
        "both responses were spoken: {tts:?}"
    );
    // The STT socket and every warm-up are still the one from Start.
    assert_eq!(call.stt.snapshot().connects, 1);
    assert_eq!(setups(&call.ledger, "tts", "warm").len(), 1);
}

/// An STT session that the provider closed while the call was quiet is replaced after the
/// reconnect backoff ALONE: the reconnect does not warm the provider again (the endpoint was just
/// in use), so it neither adds a warm-up to the ledger and the counters nor a warm-up gap to the
/// time the call is deaf. A regression that warms before every reconnect would add up to the
/// 400 ms warm-up budget to every STT recovery.
#[tokio::test(start_paused = true)]
async fn an_stt_reconnect_does_not_warm_the_provider_again() {
    let factory = PrewarmFactory::new(Knobs {
        stt_idle_death: Some(ms(6_000)),
        ..Knobs::default()
    });
    // Turn 1 is answered; the STT session dies at about 6.2 s; the caller speaks again at 9.4 s.
    let scenario = Scenario::new()
        .silence(ms(500))
        .speak("what time is it in Tokyo", ms(900))
        .silence(ms(8_000))
        .speak("and in Paris please", ms(900))
        .with_tail(ms(600));
    let ran = run(&factory, &scenario, 2).await;
    assert!(ran.answered);
    let call = &factory.handles()[0];

    let connects = setups(&call.ledger, "stt", "connect");
    assert_eq!(connects.len(), 2, "{connects:?}");
    let (first, second) = (connects[0], connects[1]);
    // The first session lived exactly its lifetime; the second connect starts one backoff after
    // that, with no warm-up (60 ms in this probe, up to 400 ms in the budget) in front of it.
    let death = first.finished + ms(6_000);
    let backoff = Duration::from_secs(1);
    assert!(
        second.started >= death + backoff && second.started <= death + backoff + ms(1),
        "the reconnect must start one backoff after the death, with no warm gap: death {death:?}, {second:?}"
    );
    assert_eq!(
        setups(&call.ledger, "stt", "warm").len(),
        1,
        "only the first connect warms"
    );
    let stt = call.stt.snapshot();
    assert_eq!((stt.warms, stt.warm_failures), (1, 0), "{stt:?}");
    assert_eq!((stt.connects, stt.connect_failures), (2, 0), "{stt:?}");
    assert_eq!(stt.session_deaths, 1, "{stt:?}");
    // The new session is up before the caller speaks again and carries turn 2.
    let turn2 = ran.turns[1].vad_stop.expect("a VAD stop in turn 2");
    assert!(second.finished <= turn2 - ms(900), "{second:?} {turn2:?}");
    assert_eq!(stt.finals_synthetic, 0, "{stt:?}");
    assert_eq!(call.tts.snapshot().utterance_starts, 2);
    // The other stages are untouched.
    assert_eq!(call.tts.snapshot().connects, 1);
    assert_eq!(setups(&call.ledger, "tts", "warm").len(), 1);
    assert_eq!(setups(&call.ledger, "llm", "warm").len(), 1);
}
