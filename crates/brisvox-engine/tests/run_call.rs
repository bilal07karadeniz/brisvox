//! `run_call` and `build_session_pipeline`: one call over a real `TransportSession`
//! (loopback), started by the library assembly, driven by the scripted caller of the simulator.
//!
//! What these tests prove that the suite on `EngineFactory` does not: the library entry point
//! itself (stop with a timeout, cancel, hang-up, no task left behind), the clear hook wiring
//! (`note_clear` before `Clear`), the early-analysis switch reaching the VAD worker, and the
//! start-of-call warm-up of every stage.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{SharedClock, SimClock, TurnEpoch};
use brisvox_engine::pump::PumpEnd;
use brisvox_engine::sim::{FixedTurnModel, LoopbackCall, energy_vad_setup, open_loopback};
use brisvox_engine::{
    Call, EarlyAnalysisConfig, EngineConfig, FrameTap, InterruptionConfig, Offload, SessionBuilder,
    run_call,
};
use brisvox_sim::input::{InputSink, InputTarget, SendFuture};
use brisvox_sim::leak::TaskCounter;
use brisvox_sim::mock_llm::LlmScript;
use brisvox_sim::scenario::{Scenario, play_input};
use brisvox_sim::session::SimSession;
use brisvox_transport::ClientEvent;
use tokio_util::sync::CancellationToken;

const MS: Duration = Duration::from_millis(1);

/// The scripted caller's side of the loopback call.
struct Caller {
    client: tokio::sync::Mutex<brisvox_transport::LoopbackClient>,
}

impl InputTarget for Caller {
    fn send(&self, frame: brisvox_core::frame::AudioFrame) -> SendFuture<'_> {
        Box::pin(async move {
            self.client
                .lock()
                .await
                .send_audio(frame.data().clone(), frame.sample_rate())
                .await
                .is_ok()
        })
    }
}

struct Rig {
    sim: SimSession,
    clock: SharedClock,
    caller: Arc<Caller>,
    cancel: CancellationToken,
    call: Call,
    timeline: brisvox_core::TimelineRecorder,
}

fn config() -> EngineConfig {
    let mut config = EngineConfig::DEFAULT;
    config.lead_margin = Duration::ZERO;
    config.llm.retries = 0;
    config.interruption = InterruptionConfig::OFF;
    config.turn = brisvox_agent::TurnPolicy::fixed(450 * MS);
    config
}

/// A running call: mocks from the simulator (scripted by the caller of this function through the
/// returned `sim`), the energy VAD of the simulator, the loopback transport.
async fn start(
    config: EngineConfig,
    smart_turn: Option<f32>,
    scenario: &Scenario,
    scripts: Vec<LlmScript>,
) -> Rig {
    start_with(config, smart_turn, scenario, scripts, None, 65_536).await
}

/// [`start`], with `tap` installed before `TransportOut` and room for `client_queue` events on
/// the client's side of the wire.
async fn start_with(
    config: EngineConfig,
    smart_turn: Option<f32>,
    scenario: &Scenario,
    scripts: Vec<LlmScript>,
    tap: Option<PanicTap>,
    client_queue: usize,
) -> Rig {
    let clock: SharedClock = Arc::new(SimClock::new());
    let sim = SimSession::new(clock.clone());
    for utterance in scenario.utterances() {
        sim.stt.push(utterance);
    }
    for script in scripts {
        sim.llm.push(script);
    }
    let LoopbackCall {
        session,
        client,
        cancel,
    } = open_loopback(&clock, client_queue).await.unwrap();
    let offload = Offload::Inline(clock.clone());
    let timeline = brisvox_core::TimelineRecorder::new();
    let mut builder = SessionBuilder::new(
        config,
        clock.clone(),
        timeline.clone(),
        offload.clone(),
        Arc::new(sim.stt.clone()),
        Arc::new(sim.llm.clone()),
        Arc::new(sim.tts.clone()),
    )
    .unwrap()
    .with_vad(energy_vad_setup(offload, &sim.vad, &config));
    if let Some(probability) = smart_turn {
        builder = builder.with_smart_turn(Arc::new(FixedTurnModel(probability)));
    }
    if let Some(tap) = tap {
        builder = builder.with_tap(tap);
    }
    let call = run_call(session, builder, &cancel).await.unwrap();
    Rig {
        sim,
        clock,
        caller: Arc::new(Caller {
            client: tokio::sync::Mutex::new(client),
        }),
        cancel,
        call,
        timeline,
    }
}

impl Rig {
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

    /// Every event the client got until it was quiet for `quiet`.
    async fn events(&self, quiet: Duration) -> Vec<ClientEvent> {
        let mut events = Vec::new();
        let mut client = self.caller.client.lock().await;
        loop {
            tokio::select! {
                event = client.recv() => match event {
                    Some(event) => events.push(event),
                    None => break,
                },
                () = self.clock.sleep(quiet) => break,
            }
        }
        events
    }

    /// The client sees the transport side go (its event stream ends) within `within`.
    async fn session_ends_within(&self, within: Duration) -> bool {
        let mut client = self.caller.client.lock().await;
        tokio::time::timeout(within, async { while client.recv().await.is_some() {} })
            .await
            .is_ok()
    }
}

/// A tap that panics on the first bot audio frame: a stage failure in the middle of a call.
struct PanicTap;

impl FrameTap for PanicTap {
    fn observe(&mut self, frame: &brisvox_core::frame::Frame) {
        assert!(
            !matches!(
                frame,
                brisvox_core::frame::Frame::Data(brisvox_core::frame::DataFrame::Audio(_))
            ),
            "injected stage failure"
        );
    }
}

fn answer() -> LlmScript {
    LlmScript::text(["Sure thing."])
}

fn question() -> Scenario {
    Scenario::new()
        .silence(100 * MS)
        .speak("what time is it", 800 * MS)
        .with_tail(600 * MS)
}

fn audio_epochs(events: &[ClientEvent]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|e| match e {
            ClientEvent::Audio { frame, .. } => Some(frame.epoch().get()),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_call_runs_end_to_end_and_stops_gracefully_within_its_timeout() {
    let scenario = question();
    let mut rig = start(config(), None, &scenario, vec![answer()]).await;
    rig.play(&scenario).await;
    rig.clock.sleep(2_000 * MS).await;
    let handles = rig.call.handles().clone();

    // The graceful stop drains every stage: Ok, well inside the configured timeout.
    let before = rig.clock.now();
    tokio::time::timeout(config().timeouts.stop, rig.call.stop())
        .await
        .expect("stop is bounded")
        .unwrap();
    assert!(rig.clock.now() - before < config().timeouts.stop);

    let events = rig.events(50 * MS).await;
    let epochs = audio_epochs(&events);
    assert!(!epochs.is_empty(), "the bot answered");
    assert!(epochs.iter().all(|&e| e == 0), "one turn: {epochs:?}");
    assert!(
        events.iter().any(|e| matches!(e, ClientEvent::Mark { .. })),
        "the end of the response was marked"
    );
    // The conversation: the user's words and the scripted answer.
    {
        let context = handles.context.lock().unwrap();
        assert_eq!(context.len(), 2, "the user's words and the answer");
    }
    assert_eq!(handles.unnoted_clear_cuts(), 0);
    assert_eq!(rig.sim.llm.requests().len(), 1, "one turn, one request");

    // The stop ended the pump too (`Call::stop` cancels the call's token), which dropped the
    // transport session: the client's line is closed although the owner still holds the call.
    let end = tokio::time::timeout(Duration::from_secs(1), rig.call.pump().wait_end())
        .await
        .expect("the pump ends with the stop");
    assert_eq!(end, PumpEnd::Cancelled);
    assert!(
        rig.session_ends_within(Duration::from_secs(1)).await,
        "the client saw the session end"
    );
}

/// A story long enough that the bot is still speaking when the test stops the call.
fn story_call() -> (Scenario, Vec<LlmScript>) {
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("tell me a long story", 900 * MS)
        .with_tail(600 * MS);
    let story = LlmScript::text([
        "Once upon a time there was a voice agent that talked and talked ",
        "and never stopped talking until somebody interrupted it, ",
        "and then it talked a little more about the time it was interrupted.",
    ]);
    (scenario, vec![story])
}

#[tokio::test(start_paused = true)]
async fn a_stop_that_cannot_drain_in_time_times_out_cancels_everything_and_leaves_no_task() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let counter = TaskCounter::start().unwrap();
    let mut cfg = config();
    cfg.timeouts.stop = 500 * MS;
    let (scenario, scripts) = story_call();
    // The client stops reading and its side of the wire holds 8 events: `TransportOut` waits
    // `timeouts.send` for every frame it cannot hand over, so draining the queued answer takes
    // far longer than `timeouts.stop`.
    {
        let mut rig = start_with(cfg, None, &scenario, scripts, None, 8).await;
        rig.play(&scenario).await;
        // The bot is speaking (it started about 0.5 s after the end of the question).
        rig.clock.sleep(500 * MS).await;
        let before = rig.clock.now();
        let result = tokio::time::timeout(Duration::from_secs(5), rig.call.stop())
            .await
            .expect("stop is bounded by its own timeout");
        assert!(
            matches!(result, Err(brisvox_core::Error::Timeout { .. })),
            "{result:?}"
        );
        assert!(rig.clock.now() - before >= cfg.timeouts.stop);
        // The pump ended under the cancelled call token, and the client's line closed.
        let end = tokio::time::timeout(Duration::from_secs(1), rig.call.pump().wait_end())
            .await
            .expect("the pump ends after a timed-out stop");
        assert_eq!(end, PumpEnd::Cancelled);
        assert!(rig.session_ends_within(Duration::from_secs(1)).await);
        drop(rig);
    }
    counter
        .wait_for_baseline(&*clock, Duration::from_secs(2))
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_failing_stage_ends_the_pump_once_the_owner_waits_on_the_call() {
    let (scenario, scripts) = story_call();
    let mut rig = start_with(config(), None, &scenario, scripts, Some(PanicTap), 65_536).await;
    rig.play(&scenario).await;
    // The first audio frame failed the tap stage: `wait` reports it...
    let result = tokio::time::timeout(Duration::from_secs(5), rig.call.wait())
        .await
        .expect("the failed pipeline ends");
    assert!(
        matches!(result, Err(brisvox_core::Error::ProcessorPanicked(_))),
        "{result:?}"
    );
    // ...and ends the pump, although the owner keeps the call (to read its stats, say): the
    // caller is not left on an open line of dead air.
    let _ = rig.call.stats();
    let end = tokio::time::timeout(Duration::from_secs(1), rig.call.pump().wait_end())
        .await
        .expect("the pump ends after a stage failure");
    assert_eq!(end, PumpEnd::Cancelled);
    assert!(rig.session_ends_within(Duration::from_secs(1)).await);
}

#[tokio::test(start_paused = true)]
async fn the_callers_hangup_ends_the_pipeline_by_itself() {
    let scenario = question();
    let mut rig = start(config(), None, &scenario, vec![answer()]).await;
    rig.play(&scenario).await;
    rig.clock.sleep(2_000 * MS).await;
    rig.caller.client.lock().await.hangup().await.unwrap();

    // No stop() from the owner: the pump saw `Stopped`, sent `Stop`, every stage drained.
    tokio::time::timeout(config().timeouts.stop, rig.call.wait())
        .await
        .expect("a hang-up ends the pipeline")
        .unwrap();
    assert!(rig.call.pump().end().is_some(), "the pump reported the end");
}

#[tokio::test(start_paused = true)]
async fn cancel_ends_every_task_at_once_and_dropping_the_call_leaves_none_behind() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let counter = TaskCounter::start().unwrap();
    let scenario = question();
    {
        let mut rig = start(config(), None, &scenario, vec![answer()]).await;
        // A call in the middle of a turn: the caller has been speaking for 600 ms.
        let origin = rig.clock.now();
        rig.play_for(&scenario, 600 * MS, origin).await;
        rig.call.cancel();
        tokio::time::timeout(Duration::from_secs(1), rig.call.wait())
            .await
            .expect("a cancelled pipeline ends at once")
            .ok();
        // The pump runs under the call's token too: it ends without the call being dropped.
        let end = tokio::time::timeout(Duration::from_secs(1), rig.call.pump().wait_end())
            .await
            .expect("a cancelled call's pump ends at once");
        assert_eq!(end, brisvox_engine::pump::PumpEnd::Cancelled);
        drop(rig);
    }
    // Everything the call spawned (pump, stages, helper tasks) is gone.
    counter
        .wait_for_baseline(&*clock, Duration::from_secs(2))
        .await
        .unwrap();
}

impl Rig {
    async fn play_for(
        &self,
        scenario: &Scenario,
        how_long: Duration,
        origin: tokio::time::Instant,
    ) {
        let cancel = CancellationToken::new();
        let sink = InputSink::target(self.caller.clone());
        let clock = self.clock.clone();
        let scenario = scenario.clone();
        let stopper = cancel.clone();
        let stop_clock = self.clock.clone();
        tokio::join!(
            async move {
                play_input(&scenario, clock, origin, SampleRate::Hz16000, sink, &cancel)
                    .await
                    .unwrap();
            },
            async move {
                stop_clock.sleep(how_long).await;
                stopper.cancel();
            }
        );
    }
}

/// The fields of one event: name and debug-formatted value.
type EventFields = Vec<(String, String)>;

/// A tracing subscriber that keeps the fields of `call stats` events (no extra crate).
#[derive(Default, Clone)]
struct StatsEvents(Arc<std::sync::Mutex<Vec<EventFields>>>);

struct Fields(EventFields, bool);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let value = format!("{value:?}");
        if field.name() == "message" && value == "call stats" {
            self.1 = true;
        }
        self.0.push((field.name().to_owned(), value));
    }
}

impl tracing::Subscriber for StatsEvents {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.target() == "brisvox::engine"
    }
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = Fields(Vec::new(), false);
        event.record(&mut fields);
        if fields.1 {
            self.0.lock().unwrap().push(fields.0);
        }
    }
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

#[tokio::test(start_paused = true)]
async fn the_calls_counters_are_visible_through_stats_and_tracing() {
    // Queue depths, drops, stale drops, pool trouble, truncations, finals-gate and
    // settle timeouts through `Call::stats` and one `brisvox::engine` event.
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("tell me a long story", 900 * MS)
        .silence(1_800 * MS)
        .speak("okay stop", 600 * MS)
        .with_tail(900 * MS);
    let story = LlmScript::text([
        "Once upon a time there was a voice agent that talked and talked ",
        "and never stopped talking until somebody interrupted it.",
    ]);
    let mut rig = start(config(), None, &scenario, vec![story, answer()]).await;
    rig.play(&scenario).await;
    rig.clock.sleep(3_000 * MS).await;
    let stats = rig.call.stats();
    let s = &stats.stages;
    assert!(s.audio_in.frames_in > 0, "{:?}", s.audio_in);
    assert!(s.vad.jobs_ok > 0 && s.vad.starts >= 2, "{:?}", s.vad);
    assert_eq!(s.driver.interrupts, 1, "{:?}", s.driver);
    assert!(s.driver.turns_ended >= 2, "{:?}", s.driver);
    assert_eq!(s.out.interrupts, 1, "{:?}", s.out);
    assert!(
        s.truncations.truncations >= 1,
        "the cut story: {:?}",
        s.truncations
    );
    assert!(s.book.clears_noted >= 1);
    assert_eq!(s.unnoted_clear_cuts, 0);
    assert_eq!(s.pool_trouble(), 0, "{s:?}");
    assert_eq!(stats.pipeline.total_dropped(), 0);
    assert!(stats.pipeline.stages.len() >= 7, "{:?}", stats.pipeline);
    // The same numbers, as one event on the engine's tracing target.
    let events = StatsEvents::default();
    tracing::subscriber::with_default(events.clone(), || stats.trace());
    let recorded = events.0.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1, "one `call stats` event");
    let fields: std::collections::BTreeMap<_, _> = recorded[0].iter().cloned().collect();
    for name in [
        "max_queued",
        "dropped",
        "stale_dropped",
        "vad_saturated",
        "pool_trouble",
        "truncations",
        "finals_gate_timeouts",
        "settle_timeouts",
    ] {
        assert!(fields.contains_key(name), "{name} missing: {fields:?}");
    }
    assert_eq!(fields["truncations"], s.truncations.truncations.to_string());
    rig.call.stop().await.unwrap();
}

/// The end-of-call numbers reach `brisvox::engine` without the owner asking: the call writes
/// `call stats` once when it ends, and a later cancel or drop does not repeat it.
#[tokio::test(start_paused = true)]
async fn the_call_traces_its_stats_once_when_it_ends() {
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("hello there", 600 * MS)
        .with_tail(900 * MS);
    let mut rig = start(config(), None, &scenario, vec![answer()]).await;
    rig.play(&scenario).await;
    rig.clock.sleep(1_000 * MS).await;
    let events = StatsEvents::default();
    let guard = tracing::subscriber::set_default(events.clone());
    rig.call.stop().await.unwrap();
    assert_eq!(events.0.lock().unwrap().len(), 1, "one event at the stop");
    rig.call.cancel();
    drop(rig);
    drop(guard);
    assert_eq!(events.0.lock().unwrap().len(), 1, "written once");
}

#[tokio::test(start_paused = true)]
async fn the_timeline_carries_the_vad_stop_the_finalize_and_the_applied_clear() {
    use brisvox_core::TurnEvent as E;
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("tell me a long story", 900 * MS)
        .silence(1_800 * MS)
        .speak("okay stop", 600 * MS)
        .with_tail(900 * MS);
    let story = LlmScript::text([
        "Once upon a time there was a voice agent that talked and talked ",
        "and never stopped talking until somebody interrupted it.",
    ]);
    let mut rig = start(config(), None, &scenario, vec![story, answer()]).await;
    rig.play(&scenario).await;
    rig.clock.sleep(3_000 * MS).await;
    rig.call.stop().await.unwrap();
    rig.timeline.finish();
    let turns = rig.timeline.history();
    assert_eq!(turns.len(), 2, "{turns:?}");
    for turn in &turns {
        // The VAD decides the stop its minimum silence after the (backdated) speech end; the
        // `Finalize` follows it; both before the end of the turn, which waits for the final.
        let (end, stop, finalize, turn_end) = (
            turn.at(E::UserSpeechEnd).unwrap(),
            turn.at(E::VadStop).unwrap(),
            turn.at(E::SttFinalize).unwrap(),
            turn.at(E::TurnEnd).unwrap(),
        );
        assert!(end < stop, "{turn:?}");
        assert!(stop <= finalize && finalize <= turn_end, "{turn:?}");
        // The segmenter's first chunk, then the text reaching the TTS stage, once per
        // response, between the first LLM token and the TTS request.
        let (first_token, chunk, text, request) = (
            turn.at(E::LlmFirstToken).unwrap(),
            turn.at(E::FirstChunk).unwrap(),
            turn.at(E::TtsFirstText).unwrap(),
            turn.at(E::TtsRequest).unwrap(),
        );
        assert!(
            first_token <= chunk && chunk <= text && text <= request,
            "{turn:?}"
        );
    }
    let interrupted = &turns[0];
    assert!(interrupted.at(E::Interrupted).is_some());
    // End to end only: on this paused clock `TransportOut`'s own mark (when the `Interrupt`
    // reaches it) is at the same instant, so this does not tell the two marks apart. The
    // driver's mark and its gates are pinned in `tests/turn_driver.rs`
    // (`the_driver_marks_the_audio_stopped_when_its_clear_is_applied` and the two after it).
    assert_eq!(
        interrupted.between(E::Interrupted, E::AudioOutStopped),
        Some(Duration::ZERO),
        "the bot's audio stopped when the driver's clear was applied: {interrupted:?}"
    );
    assert!(turns[1].at(E::AudioOutStopped).is_none());
}

#[tokio::test(start_paused = true)]
async fn a_barge_in_notes_the_clear_in_the_book_before_the_command() {
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("tell me a long story", 900 * MS)
        .silence(1_800 * MS)
        .speak("okay stop", 600 * MS)
        .with_tail(900 * MS);
    let story = LlmScript::text([
        "Once upon a time there was a voice agent that talked and talked ",
        "and never stopped talking until somebody interrupted it.",
    ]);
    let mut rig = start(config(), None, &scenario, vec![story, answer()]).await;
    rig.play(&scenario).await;
    rig.clock.sleep(3_000 * MS).await;
    let handles = rig.call.handles().clone();
    rig.call.stop().await.unwrap();

    let book = handles.book.stats();
    assert!(
        book.clears_noted >= 1,
        "the driver's hook noted the clear: {book:?}"
    );
    assert_eq!(
        handles.unnoted_clear_cuts(),
        0,
        "no interrupted response was cut without the clear being noted"
    );
    let stats = handles.driver.snapshot();
    assert_eq!(stats.interrupts, 1, "{stats:?}");
    let events = rig.events(50 * MS).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ClientEvent::Clear { epoch, .. } if *epoch >= TurnEpoch::new(1))),
        "the client was cleared on the barge-in"
    );
}

#[tokio::test(start_paused = true)]
async fn the_early_analysis_switch_reaches_the_vad_worker_and_the_driver() {
    let scenario = question();
    let mut on = config();
    on.turn = brisvox_agent::TurnPolicy::Adaptive {
        fallback_silence: Duration::from_secs(3),
        require_final: true,
        final_grace: 800 * MS,
    };
    on.early_analysis = EarlyAnalysisConfig::ENABLED;
    let mut off = on;
    off.early_analysis = EarlyAnalysisConfig::OFF;

    let mut hints = Vec::new();
    for config in [on, off] {
        let mut rig = start(config, Some(0.9), &scenario, vec![answer()]).await;
        rig.play(&scenario).await;
        rig.clock.sleep(2_000 * MS).await;
        let handles = rig.call.handles().clone();
        rig.call.stop().await.unwrap();
        hints.push((
            handles.vad.snapshot().falls,
            handles.driver.snapshot().early_hints,
        ));
    }
    let (on_falls, on_hints) = hints[0];
    let (off_falls, off_hints) = hints[1];
    assert!(
        on_falls >= 1,
        "the VAD worker sent a falling hint: {hints:?}"
    );
    assert!(on_hints >= 1, "the driver received it: {hints:?}");
    assert_eq!((off_falls, off_hints), (0, 0), "off sends none: {hints:?}");
}

#[tokio::test(start_paused = true)]
async fn every_stage_warms_when_the_call_starts() {
    let scenario = question();
    let mut cfg = config();
    cfg.turn = brisvox_agent::TurnPolicy::Adaptive {
        fallback_silence: Duration::from_secs(3),
        require_final: true,
        final_grace: 800 * MS,
    };
    let mut rig = start(cfg, Some(0.9), &scenario, vec![answer()]).await;
    // Nothing has been said yet: the warm-ups ran at Start.
    rig.clock.sleep(50 * MS).await;
    let h = rig.call.handles().clone();
    assert_eq!(h.vad.snapshot().warms, 1, "vad model (inline: one thread)");
    assert_eq!(h.driver.snapshot().warms, 1, "smart turn model");
    assert_eq!(h.stt.snapshot().warms, 1, "stt provider");
    assert_eq!(h.tts.snapshot().warms, 1, "tts provider");
    assert_eq!(h.llm.snapshot().warms, 1, "llm provider");
    let failures = h.vad.snapshot().warm_failures
        + h.driver.snapshot().warm_failures
        + h.stt.snapshot().warm_failures
        + h.tts.snapshot().warm_failures
        + h.llm.snapshot().warm_failures;
    assert_eq!(failures, 0);
    rig.call.stop().await.unwrap();
}
