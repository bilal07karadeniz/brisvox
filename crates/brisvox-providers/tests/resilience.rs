//! The resilience layer over the simulator's mocks, on a paused clock: failover at
//! connect and mid-call, the process-wide breaker (open, half-open, closed), the retry budgets,
//! what carries over to the new session (STT replay, LLM re-issue, TTS re-speak) and the idle
//! refresh of TTS sessions.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{SharedClock, SimClock};
use brisvox_providers::resilience::{
    BreakerConfig, BreakerState, BudgetConfig, IdleRefresh, Leg, Resilience, ResilienceConfig,
    ResilientLlm, ResilientStt, ResilientTts,
};
use brisvox_providers::stt::STT_CHANNEL_CAPACITY;
use brisvox_providers::{
    ContextId, Error, LlmEvent, LlmProvider, LlmRequest, Message, Role, SttConfig, SttEvent,
    SttInput, SttProvider, SttSession, TtsConfig, TtsEvent, TtsInput, TtsProvider, TtsSession,
};
use brisvox_sim::latency::LatencyProfile;
use brisvox_sim::mock_common::StreamFault;
use brisvox_sim::mock_llm::{LlmFault, LlmScript, MockLlm};
use brisvox_sim::mock_stt::{MockStt, SttUtterance};
use brisvox_sim::mock_tts::MockTts;
use brisvox_sim::outage::{Outage, OutageSwitch};
use bytes::Bytes;

const MS: Duration = Duration::from_millis(1);
const SEC: Duration = Duration::from_secs(1);

fn config() -> ResilienceConfig {
    ResilienceConfig {
        breaker: BreakerConfig {
            failure_threshold: 2,
            open_for: 30 * SEC,
        },
        ..ResilienceConfig::DEFAULT
    }
}

fn request() -> LlmRequest {
    LlmRequest {
        messages: vec![Message::new(Role::User, "hi")],
        tools: Vec::new(),
    }
}

/// Text deltas until `Done`, or the error that ended the stream.
async fn collect(llm: &impl LlmProvider) -> Result<String, Error> {
    let mut stream = llm.stream(request()).await?;
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            LlmEvent::TextDelta(delta) => text.push_str(&delta),
            LlmEvent::Done { .. } => return Ok(text),
            LlmEvent::Error(error) => return Err(error),
            LlmEvent::ToolCallDelta { .. } => {}
        }
    }
    Err(Error::Disconnected { provider: "cut" })
}

struct LlmRig {
    resilience: Resilience,
    mock: MockLlm,
    primary: Leg<Outage<MockLlm>>,
    secondary: Leg<Outage<MockLlm>>,
    down: OutageSwitch,
    backup_down: OutageSwitch,
}

impl LlmRig {
    fn new(clock: &SharedClock, config: ResilienceConfig) -> Self {
        let resilience = Resilience::new(config, clock.clone());
        let mock = MockLlm::new(clock.clone());
        let (down, backup_down) = (OutageSwitch::new(), OutageSwitch::new());
        let primary = resilience.leg("primary", Arc::new(Outage::new(mock.clone(), down.clone())));
        let secondary = resilience.leg(
            "secondary",
            Arc::new(Outage::new(mock.clone(), backup_down.clone())),
        );
        Self {
            resilience,
            mock,
            primary,
            secondary,
            down,
            backup_down,
        }
    }

    /// One call's LLM.
    fn call(&self) -> ResilientLlm<Outage<MockLlm>> {
        ResilientLlm::new(self.primary.clone(), &self.resilience)
            .with_secondary(self.secondary.clone())
    }
}

#[tokio::test(start_paused = true)]
async fn llm_primary_down_secondary_answers_and_the_shared_breaker_opens_then_half_opens() {
    let clock = SimClock::shared();
    let rig = LlmRig::new(&clock, config());
    rig.mock.set_fallback(LlmScript::text(["answer"]));
    rig.down.set_down(true);

    // Two calls: each tries the primary (refused), then the secondary answers (one retry each).
    for _ in 0..2 {
        let llm = rig.call();
        assert_eq!(collect(&llm).await.unwrap(), "answer");
        let stats = llm.stats();
        assert_eq!((stats.served_secondary, stats.retries), (1, 1), "{stats:?}");
    }
    let breaker = rig.primary.breaker();
    assert_eq!(
        breaker.state(),
        BreakerState::Open,
        "two failures across calls"
    );

    // Open: the third call skips the primary for free.
    let llm = rig.call();
    assert_eq!(collect(&llm).await.unwrap(), "answer");
    let stats = llm.stats();
    assert_eq!(
        (stats.breaker_skips, stats.retries, stats.served_secondary),
        (1, 0, 1)
    );
    assert_eq!(rig.down.refused(), 2, "no request reached the open primary");

    // The primary comes back; after the open period one probe goes to it and closes the breaker.
    rig.down.set_down(false);
    clock.sleep(30 * SEC).await;
    assert_eq!(breaker.state(), BreakerState::HalfOpen);
    let llm = rig.call();
    assert_eq!(collect(&llm).await.unwrap(), "answer");
    assert_eq!(llm.stats().served_primary, 1);
    assert_eq!(breaker.state(), BreakerState::Closed);
    assert_eq!(breaker.stats().probes, 1);
}

#[tokio::test(start_paused = true)]
async fn llm_failure_before_content_is_reissued_but_never_after_content() {
    let clock = SimClock::shared();
    let rig = LlmRig::new(&clock, config());
    rig.mock
        .push(LlmScript::text(["lost"]).with_fault(LlmFault::Disconnect { after_events: 0 }));
    rig.mock.push(LlmScript::text(["second ", "try"]));
    let llm = rig.call();
    assert_eq!(collect(&llm).await.unwrap(), "second try");
    assert_eq!(llm.stats().recoveries, 1);
    assert_eq!(rig.mock.requests().len(), 2);

    // Cut after the first delta: the error reaches the caller, nothing is re-issued.
    rig.mock.push(
        LlmScript::text(["heard ", "rest"]).with_fault(LlmFault::Disconnect { after_events: 1 }),
    );
    rig.mock.push(LlmScript::text(["must not be used"]));
    let llm = rig.call();
    assert!(matches!(
        collect(&llm).await,
        Err(Error::Disconnected { .. })
    ));
    assert_eq!(rig.mock.requests().len(), 3);
    assert_eq!(llm.stats().recoveries, 0);
}

#[tokio::test(start_paused = true)]
async fn llm_silent_primary_is_abandoned_at_the_first_event_deadline() {
    let clock = SimClock::shared();
    let rig = LlmRig::new(&clock, config());
    rig.mock
        .push(LlmScript::text(["late"]).with_fault(LlmFault::Stall {
            before_event: 0,
            duration: 20 * SEC,
        }));
    rig.mock.push(LlmScript::text(["on time"]));
    let start = clock.now();
    let llm = rig.call();
    assert_eq!(collect(&llm).await.unwrap(), "on time");
    assert_eq!(clock.now() - start, config().llm_first_event);
    assert_eq!(rig.primary.breaker().stats().failures, 1);
}

#[tokio::test(start_paused = true)]
async fn llm_request_errors_do_not_fail_over_or_count() {
    let clock = SimClock::shared();
    let rig = LlmRig::new(&clock, config());
    rig.mock
        .push(LlmScript::text(["x"]).with_fault(LlmFault::ErrorResponse { status: 400 }));
    let llm = rig.call();
    assert!(matches!(
        collect(&llm).await,
        Err(Error::Rejected { status: 400, .. })
    ));
    assert_eq!(rig.mock.requests().len(), 1);
    assert_eq!(rig.primary.breaker().stats().failures, 0);
}

#[tokio::test(start_paused = true)]
async fn exhausted_budgets_fail_cleanly_without_touching_the_providers() {
    let clock = SimClock::shared();
    let config = ResilienceConfig {
        breaker: BreakerConfig {
            failure_threshold: 100,
            open_for: 30 * SEC,
        },
        session_budget: BudgetConfig {
            capacity: 1,
            refill_every: 60 * SEC,
        },
        process_budget: BudgetConfig {
            capacity: 2,
            refill_every: 60 * SEC,
        },
        ..ResilienceConfig::DEFAULT
    };
    let rig = LlmRig::new(&clock, config);
    rig.down.set_down(true);
    rig.backup_down.set_down(true);

    // Call A: its one retry goes to the secondary (down too); the next request has no budget.
    let a = rig.call();
    assert!(matches!(
        collect(&a).await,
        Err(Error::Rejected { status: 503, .. })
    ));
    assert!(matches!(
        collect(&a).await,
        Err(Error::Rejected { status: 503, .. })
    ));
    assert_eq!(a.stats().budget_denied, 1);
    assert_eq!(a.budget_available(), 0);
    assert_eq!(
        rig.backup_down.refused(),
        1,
        "the denied retry never reached it"
    );

    // Calls B and C share the process budget (2 tokens): A took one, B the other, C none.
    let b = rig.call();
    let _ = collect(&b).await;
    let c = rig.call();
    let _ = collect(&c).await;
    assert_eq!((b.stats().retries, c.stats().retries), (1, 0));
    assert_eq!(c.stats().budget_denied, 1);
    assert_eq!(rig.resilience.process_budget().available(), 0);
    assert_eq!(rig.backup_down.refused(), 2);
}

#[tokio::test(start_paused = true)]
async fn stt_connect_fails_over_and_a_mid_utterance_death_replays_the_open_utterance() {
    let clock = SimClock::shared();
    let resilience = Resilience::new(config(), clock.clone());
    let mock = MockStt::new(clock.clone());
    mock.push(SttUtterance::words("one two three four"));
    let down = OutageSwitch::down();
    let primary = resilience.leg("primary", Arc::new(Outage::new(mock.clone(), down.clone())));
    let secondary = resilience.leg(
        "secondary",
        Arc::new(Outage::new(mock.clone(), OutageSwitch::new())),
    );
    let stt = ResilientStt::new(primary, &resilience).with_secondary(secondary);
    // The secondary's first session drops right after its first interim.
    mock.push_session_faults(vec![StreamFault::Disconnect { after_events: 1 }]);
    let mut session = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz16000,
            language: None,
        })
        .await
        .unwrap();
    assert_eq!(stt.stats().served_secondary, 1);

    let frame = Bytes::from(vec![0u8; 640]);
    for _ in 0..10 {
        session
            .input
            .send(SttInput::Audio(frame.clone()))
            .await
            .unwrap();
    }
    clock.sleep(50 * MS).await;
    session.input.send(SttInput::Finalize).await.unwrap();
    let mut finals = Vec::new();
    loop {
        match session.events.recv().await.unwrap() {
            SttEvent::Transcript {
                text,
                is_final: true,
                speech_final,
                ..
            } => {
                finals.push(text.to_string());
                if speech_final {
                    break;
                }
            }
            SttEvent::Transcript { .. } => {}
            SttEvent::Error(error) => panic!("the error reached the engine: {error}"),
        }
    }
    assert_eq!(finals, ["one two three four"]);
    let stats = stt.stats();
    assert_eq!(stats.recoveries, 1, "{stats:?}");
    // The new session heard the whole utterance: 10 frames in the dead session, 10 replayed.
    assert_eq!(mock.audio_bytes(), 2 * 10 * 640);
    // The session died before the Finalize was sent: only the audio was replayed.
    assert_eq!(stats.replayed, 10);
}

fn tts_config() -> TtsConfig {
    TtsConfig {
        sample_rate: SampleRate::Hz24000,
        voice: Arc::from("v"),
        language: None,
        model: None,
    }
}

/// Events until `Flushed` of `context`, or the first error.
async fn speak(session: &mut TtsSession, context: u64, text: &str) -> Result<usize, Error> {
    let context = ContextId(context);
    session
        .input
        .send(TtsInput::Text {
            context,
            text: Arc::from(text),
        })
        .await
        .unwrap();
    session
        .input
        .send(TtsInput::Flush { context })
        .await
        .unwrap();
    let mut audio = 0;
    loop {
        match session.events.recv().await {
            Some(TtsEvent::Audio { audio: bytes, .. }) => audio += bytes.len(),
            Some(TtsEvent::Flushed { context: c }) if c == context => return Ok(audio),
            Some(TtsEvent::Error(error)) => return Err(error),
            Some(_) => {}
            None => return Err(Error::Disconnected { provider: "closed" }),
        }
    }
}

#[tokio::test(start_paused = true)]
async fn tts_death_before_audio_respeaks_the_utterance_and_after_audio_fails_as_before() {
    let clock = SimClock::shared();
    let resilience = Resilience::new(config(), clock.clone());
    let mock = MockTts::new(clock.clone());
    let tts = ResilientTts::new(
        resilience.leg(
            "tts",
            Arc::new(Outage::new(mock.clone(), OutageSwitch::new())),
        ),
        &resilience,
    );
    mock.push_session_faults(vec![StreamFault::Disconnect { after_events: 0 }]);
    let mut session = tts.connect(tts_config()).await.unwrap();
    let whole = speak(&mut session, 1, "hello").await.unwrap();
    assert!(whole > 0);
    assert_eq!(tts.stats().recoveries, 1);
    assert_eq!(mock.connects(), 2);
    let texts: Vec<_> = mock.texts().into_iter().map(|(_, t)| t).collect();
    assert_eq!(
        texts,
        ["hello", "hello"],
        "re-spoken whole on the new session"
    );

    // A session that dies after audio of an open utterance: the engine handles it as before.
    let tts = ResilientTts::new(
        resilience.leg(
            "tts2",
            Arc::new(Outage::new(mock.clone(), OutageSwitch::new())),
        ),
        &resilience,
    );
    mock.push_session_faults(vec![StreamFault::Disconnect { after_events: 2 }]);
    let mut session = tts.connect(tts_config()).await.unwrap();
    assert!(matches!(
        speak(&mut session, 1, "a much longer sentence").await,
        Err(Error::Disconnected { .. })
    ));
    assert_eq!(tts.stats().recovery_failures, 1);
}

/// A TTS that documents an idle limit (as Cartesia's 5 minutes).
#[derive(Debug, Clone)]
struct Limited(MockTts);

impl TtsProvider for Limited {
    fn name(&self) -> &'static str {
        "limited"
    }

    async fn connect(&self, config: TtsConfig) -> brisvox_providers::Result<TtsSession> {
        self.0.connect(config).await
    }

    fn idle_limit(&self) -> Option<Duration> {
        Some(100 * SEC)
    }
}

#[tokio::test(start_paused = true)]
async fn an_idle_tts_session_is_replaced_before_the_provider_idle_limit() {
    let clock = SimClock::shared();
    let resilience = Resilience::new(config(), clock.clone());
    let mock = MockTts::new(clock.clone());
    let tts = ResilientTts::new(
        resilience.leg("limited", Arc::new(Limited(mock.clone()))),
        &resilience,
    );
    let mut session = tts.connect(tts_config()).await.unwrap();
    assert_eq!(mock.connects(), 1);
    clock.sleep(79 * SEC).await;
    assert_eq!(mock.connects(), 1);
    clock.sleep(2 * SEC).await;
    assert_eq!(mock.connects(), 2, "refreshed at 80 % of the limit");
    assert_eq!(tts.stats().refreshes, 1);

    // The next utterance is spoken on the fresh session without another connect.
    assert!(speak(&mut session, 1, "hi").await.unwrap() > 0);
    assert_eq!(mock.connects(), 2);

    // A quiet call is refreshed once per period, counted from the last traffic.
    let tts = ResilientTts::new(
        resilience.leg("limited2", Arc::new(Limited(mock.clone()))),
        &resilience,
    );
    let mut session = tts.connect(tts_config()).await.unwrap();
    clock.sleep(81 * SEC).await;
    assert_eq!(tts.stats().refreshes, 1);
    // Idle again for 80 s after the last traffic: one more refresh, not before.
    clock.sleep(79 * SEC).await;
    assert_eq!(tts.stats().refreshes, 1);
    clock.sleep(2 * SEC).await;
    assert_eq!(tts.stats().refreshes, 2);
    assert!(speak(&mut session, 7, "still fine").await.unwrap() > 0);
    drop(session);
}

#[tokio::test(start_paused = true)]
async fn idle_refresh_off_and_no_limit_mean_no_refresh() {
    let clock = SimClock::shared();
    let off = ResilienceConfig {
        idle_refresh: IdleRefresh::Off,
        ..config()
    };
    for (config, provider_limited) in [(off, true), (config(), false)] {
        let resilience = Resilience::new(config, clock.clone());
        let mock = MockTts::new(clock.clone());
        let session = if provider_limited {
            ResilientTts::new(
                resilience.leg("l", Arc::new(Limited(mock.clone()))),
                &resilience,
            )
            .connect(tts_config())
            .await
            .unwrap()
        } else {
            ResilientTts::new(resilience.leg("m", Arc::new(mock.clone())), &resilience)
                .connect(tts_config())
                .await
                .unwrap()
        };
        clock.sleep(600 * SEC).await;
        assert_eq!(mock.connects(), 1);
        drop(session);
    }
}

#[tokio::test(start_paused = true)]
async fn stt_death_after_a_finalize_replays_the_audio_and_the_finalize_in_place() {
    let clock = SimClock::shared();
    let resilience = Resilience::new(config(), clock.clone());
    let mock = MockStt::new(clock.clone()).with_latency(
        LatencyProfile::ZERO,
        LatencyProfile::Fixed(10 * SEC),
        LatencyProfile::Fixed(100 * MS),
        1,
    );
    // The dead session consumes the first copy at its Finalize; the replay is answered by the
    // second (the mock pops one utterance per Finalize).
    mock.push(SttUtterance::words("lost answer"));
    mock.push(SttUtterance::words("complete answer"));
    // The first session drops when its final is due.
    mock.push_session_faults(vec![StreamFault::Disconnect { after_events: 0 }]);
    let stt = ResilientStt::new(resilience.leg("stt", Arc::new(mock.clone())), &resilience);
    let mut session = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz16000,
            language: None,
        })
        .await
        .unwrap();
    let frame = Bytes::from(vec![0u8; 640]);
    for _ in 0..5 {
        session
            .input
            .send(SttInput::Audio(frame.clone()))
            .await
            .unwrap();
    }
    session.input.send(SttInput::Finalize).await.unwrap();
    let text = loop {
        match session.events.recv().await.unwrap() {
            SttEvent::Transcript {
                text,
                speech_final: true,
                ..
            } => break text,
            SttEvent::Transcript { .. } => {}
            SttEvent::Error(error) => panic!("the error reached the engine: {error}"),
        }
    };
    assert_eq!(&*text, "complete answer");
    assert_eq!(mock.finalizes(), 2);
    assert_eq!(mock.connects(), 2);
    assert_eq!(stt.stats().replayed, 5 + 1);
}

/// Every event until `Flushed` of `context` (or an error, or the end).
async fn events_until_flushed(session: &mut TtsSession, context: ContextId) -> Vec<TtsEvent> {
    let mut seen = Vec::new();
    while let Some(event) = session.events.recv().await {
        let done = matches!(&event, TtsEvent::Flushed { context: c } if *c == context)
            || matches!(&event, TtsEvent::Error(_));
        seen.push(event);
        if done {
            break;
        }
    }
    seen
}

#[tokio::test(start_paused = true)]
async fn text_read_after_its_cancel_is_dropped_so_refresh_and_recovery_never_see_it() {
    let clock = SimClock::shared();
    let resilience = Resilience::new(config(), clock.clone());
    let (cancelled, open) = (ContextId(1), ContextId(2));
    // Connects take 500 ms, so a recovery is still connecting when the engine barges in.
    let mock = MockTts::new(clock.clone()).with_latency(
        LatencyProfile::Fixed(500 * MS),
        LatencyProfile::ZERO,
        LatencyProfile::ZERO,
        1,
    );
    let tts = ResilientTts::new(
        resilience.leg("limited", Arc::new(Limited(mock.clone()))),
        &resilience,
    );
    // The first session drops before its first audio.
    mock.push_session_faults(vec![StreamFault::Disconnect { after_events: 0 }]);
    let mut session = tts.connect(tts_config()).await.unwrap();
    session
        .input
        .send(TtsInput::Text {
            context: open,
            text: Arc::from("hello"),
        })
        .await
        .unwrap();
    session
        .input
        .send(TtsInput::Flush { context: open })
        .await
        .unwrap();
    clock.sleep(100 * MS).await;
    // The session died, the recovery is connecting. A barge-in: the engine had queued text of
    // the next utterance (not flushed yet) and cancels it. The cancel channel is read first, so
    // the relay reads the text after the cancel.
    session
        .input
        .try_send(TtsInput::Text {
            context: cancelled,
            text: Arc::from("never mind"),
        })
        .unwrap();
    session.cancel.try_send(cancelled).unwrap();
    let mut events = events_until_flushed(&mut session, open).await;
    while let Ok(Some(event)) = tokio::time::timeout(2 * SEC, session.events.recv()).await {
        events.push(event);
    }
    assert!(
        matches!(events.first(), Some(TtsEvent::Cancelled { context }) if *context == cancelled),
        "{events:?}"
    );
    // Nothing of the cancelled utterance follows its `Cancelled`: the recovery re-spoke only
    // the open one.
    let of_cancelled = events
        .iter()
        .filter(|e| match e {
            TtsEvent::Audio { context, .. }
            | TtsEvent::Word { context, .. }
            | TtsEvent::Flushed { context }
            | TtsEvent::Cancelled { context } => *context == cancelled,
            TtsEvent::Error(_) => false,
        })
        .count();
    assert_eq!(of_cancelled, 1, "{events:?}");
    assert_eq!(tts.stats().recoveries, 1);
    let texts: Vec<_> = mock.texts().into_iter().map(|(_, t)| t).collect();
    assert_eq!(
        texts,
        ["hello", "hello"],
        "the cancelled text was never sent"
    );
    // Nothing is left open in the layer: the idle refresh comes at 80 % of the limit (a leaked
    // utterance would keep the session "busy" for the rest of the call).
    clock.sleep(81 * SEC).await;
    assert_eq!(tts.stats().refreshes, 1, "{:?}", tts.stats());
    assert!(speak(&mut session, 3, "hi").await.unwrap() > 0);
}

fn stt_config() -> SttConfig {
    SttConfig {
        sample_rate: SampleRate::Hz16000,
        language: None,
    }
}

#[tokio::test(start_paused = true)]
async fn a_half_open_probe_closes_on_probation_when_it_connects() {
    let clock = SimClock::shared();
    // Threshold 2, no secondary (the conservative single-provider deployment).
    let resilience = Resilience::new(config(), clock.clone());
    let mock = MockStt::new(clock.clone());
    let switch = OutageSwitch::down();
    let leg = resilience.leg("stt", Arc::new(Outage::new(mock.clone(), switch.clone())));
    let call = || ResilientStt::new(leg.clone(), &resilience);
    for _ in 0..2 {
        assert!(call().connect(stt_config()).await.is_err());
    }
    assert_eq!(leg.breaker().state(), BreakerState::Open);
    switch.set_down(false);
    clock.sleep(30 * SEC).await;
    assert_eq!(leg.breaker().state(), BreakerState::HalfOpen);

    // The probe connects: the breaker closes at once, and a second call is admitted while the
    // probe session has not proved healthy yet (no transcript, `healthy_after` not reached).
    let probe = call().connect(stt_config()).await.unwrap();
    assert_eq!(leg.breaker().state(), BreakerState::Closed);
    let other = call().connect(stt_config()).await;
    assert!(other.is_ok(), "{:?}", other.err());
    assert_eq!(leg.breaker().stats().probes, 1);
    drop((probe, other));

    // A probe that connects but dies before it proved healthy opens the breaker again at once
    // (on probation one failure is enough, whatever the threshold).
    switch.set_down(true);
    for _ in 0..2 {
        assert!(call().connect(stt_config()).await.is_err());
    }
    switch.set_down(false);
    clock.sleep(30 * SEC).await;
    mock.push(SttUtterance::words("hello there"));
    mock.push_session_faults(vec![StreamFault::Disconnect { after_events: 0 }]);
    let mut probe = call().connect(stt_config()).await.unwrap();
    assert_eq!(leg.breaker().state(), BreakerState::Closed);
    probe
        .input
        .send(SttInput::Audio(Bytes::from(vec![0u8; 640])))
        .await
        .unwrap();
    // The death is not recovered (the reopened breaker refuses the recovery): the error ends
    // the session.
    assert!(matches!(
        probe.events.recv().await,
        Some(SttEvent::Error(Error::Disconnected { .. }))
    ));
    assert_eq!(leg.breaker().state(), BreakerState::Open);
    assert_eq!(leg.breaker().stats().probes, 2);
}

#[tokio::test(start_paused = true)]
async fn a_hung_primary_connect_fails_over_at_the_attempt_timeout() {
    let clock = SimClock::shared();
    let resilience = Resilience::new(config(), clock.clone());
    let attempt = config().attempt_timeout;
    let hung = OutageSwitch::new();
    hung.set_hanging(true);

    // STT.
    let mock = MockStt::new(clock.clone());
    let stt = ResilientStt::new(
        resilience.leg("stt-a", Arc::new(Outage::new(mock.clone(), hung.clone()))),
        &resilience,
    )
    .with_secondary(resilience.leg(
        "stt-b",
        Arc::new(Outage::new(mock.clone(), OutageSwitch::new())),
    ));
    let start = clock.now();
    let session = tokio::time::timeout(30 * SEC, stt.connect(stt_config()))
        .await
        .expect("a hung primary must not hang the connect")
        .unwrap();
    assert_eq!(
        clock.now() - start,
        attempt,
        "failover at the attempt timeout"
    );
    assert_eq!(
        (stt.stats().served_primary, stt.stats().served_secondary),
        (0, 1)
    );
    drop(session);

    // TTS.
    let mock = MockTts::new(clock.clone());
    let tts = ResilientTts::new(
        resilience.leg("tts-a", Arc::new(Outage::new(mock.clone(), hung.clone()))),
        &resilience,
    )
    .with_secondary(resilience.leg(
        "tts-b",
        Arc::new(Outage::new(mock.clone(), OutageSwitch::new())),
    ));
    let start = clock.now();
    let mut session = tokio::time::timeout(30 * SEC, tts.connect(tts_config()))
        .await
        .expect("a hung primary must not hang the connect")
        .unwrap();
    assert_eq!(
        clock.now() - start,
        attempt,
        "failover at the attempt timeout"
    );
    assert_eq!(
        (tts.stats().served_primary, tts.stats().served_secondary),
        (0, 1)
    );
    assert!(speak(&mut session, 1, "hi").await.unwrap() > 0);
    assert_eq!(hung.hung(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_hung_recovery_ends_with_the_error_at_the_attempt_timeout() {
    let clock = SimClock::shared();
    let resilience = Resilience::new(config(), clock.clone());
    let attempt = config().attempt_timeout;

    // STT: the session dies, its provider no longer answers connects.
    let mock = MockStt::new(clock.clone());
    mock.push(SttUtterance::words("one two three"));
    mock.push_session_faults(vec![StreamFault::Disconnect { after_events: 0 }]);
    let switch = OutageSwitch::new();
    let stt = ResilientStt::new(
        resilience.leg("stt", Arc::new(Outage::new(mock.clone(), switch.clone()))),
        &resilience,
    );
    let mut session = stt.connect(stt_config()).await.unwrap();
    switch.set_hanging(true);
    let start = clock.now();
    session
        .input
        .send(SttInput::Audio(Bytes::from(vec![0u8; 640])))
        .await
        .unwrap();
    let end = tokio::time::timeout(30 * SEC, session.events.recv())
        .await
        .expect("a hung recovery must end");
    assert!(
        matches!(end, Some(SttEvent::Error(Error::Disconnected { .. }))),
        "{end:?}"
    );
    assert_eq!(clock.now() - start, attempt);
    assert_eq!(stt.stats().recovery_failures, 1);
    assert_eq!(switch.hung(), 1);

    // TTS: the same.
    let mock = MockTts::new(clock.clone());
    mock.push_session_faults(vec![StreamFault::Disconnect { after_events: 0 }]);
    let switch = OutageSwitch::new();
    let tts = ResilientTts::new(
        resilience.leg("tts", Arc::new(Outage::new(mock.clone(), switch.clone()))),
        &resilience,
    );
    let mut session = tts.connect(tts_config()).await.unwrap();
    switch.set_hanging(true);
    let start = clock.now();
    let end = tokio::time::timeout(30 * SEC, speak(&mut session, 1, "hello"))
        .await
        .expect("a hung recovery must end");
    assert!(matches!(end, Err(Error::Disconnected { .. })), "{end:?}");
    assert_eq!(clock.now() - start, attempt);
    assert_eq!(tts.stats().recovery_failures, 1);
    assert_eq!(switch.hung(), 1);
}

/// 20 ms of 16 kHz PCM16.
const FRAME: usize = 640;
/// Frames the lagging final covers.
const COVERED: usize = 100;
/// Frames the final arrives behind them.
const LAG: usize = 15;
/// Frames after the final until the session drops.
const DIE_AFTER: usize = 10;

/// An STT whose first session sends a final segment (not ending the utterance) for the first
/// [`COVERED`] frames only [`LAG`] frames later, as real providers do while the audio streams
/// on, and drops [`DIE_AFTER`] frames after that; later sessions count the frames they hear.
#[derive(Debug, Clone, Default)]
struct LaggingStt {
    connects: Arc<std::sync::atomic::AtomicUsize>,
    heard: Arc<std::sync::atomic::AtomicUsize>,
}

impl SttProvider for LaggingStt {
    fn name(&self) -> &'static str {
        "lagging"
    }

    async fn connect(&self, _config: SttConfig) -> brisvox_providers::Result<SttSession> {
        use std::sync::atomic::Ordering::SeqCst;
        let first = self.connects.fetch_add(1, SeqCst) == 0;
        let heard = Arc::clone(&self.heard);
        let (input, mut input_rx) = tokio::sync::mpsc::channel(STT_CHANNEL_CAPACITY);
        let (events_tx, events) = tokio::sync::mpsc::channel(STT_CHANNEL_CAPACITY);
        tokio::spawn(async move {
            let mut frames = 0;
            while let Some(input) = input_rx.recv().await {
                let SttInput::Audio(_) = input else { continue };
                frames += 1;
                if !first {
                    heard.fetch_add(1, SeqCst);
                    continue;
                }
                if frames == COVERED + LAG {
                    let event = SttEvent::Transcript {
                        text: Arc::from("I want to book"),
                        is_final: true,
                        speech_final: false,
                        from_finalize: false,
                        language: None,
                    };
                    if events_tx.send(event).await.is_err() {
                        return;
                    }
                }
                if frames == COVERED + LAG + DIE_AFTER {
                    // The connection drops: the events end without a final for the rest.
                    return;
                }
            }
        });
        Ok(SttSession { input, events })
    }
}

#[tokio::test(start_paused = true)]
async fn stt_recovery_replays_the_audio_a_lagging_final_did_not_cover() {
    let clock = SimClock::shared();
    let resilience = Resilience::new(config(), clock.clone());
    let provider = LaggingStt::default();
    let stt = ResilientStt::new(
        resilience.leg("lagging", Arc::new(provider.clone())),
        &resilience,
    );
    let mut session = stt.connect(stt_config()).await.unwrap();
    let frame = Bytes::from(vec![0u8; FRAME]);
    for _ in 0..COVERED + LAG {
        session
            .input
            .send(SttInput::Audio(frame.clone()))
            .await
            .unwrap();
    }
    assert!(matches!(
        session.events.recv().await,
        Some(SttEvent::Transcript { is_final: true, .. })
    ));
    for _ in 0..DIE_AFTER {
        session
            .input
            .send(SttInput::Audio(frame.clone()))
            .await
            .unwrap();
    }
    clock.sleep(100 * MS).await;
    assert_eq!(stt.stats().recoveries, 1, "{:?}", stt.stats());
    // The new session heard the lag the final had not covered and what followed: the tail
    // kept at the final (1 s = 50 frames), then the frames after it.
    let heard = provider.heard.load(std::sync::atomic::Ordering::SeqCst);
    assert!(heard >= LAG + DIE_AFTER, "heard {heard} frames");
    assert_eq!(heard, 50 + DIE_AFTER);
    assert_eq!(stt.stats().replayed, 60);
    drop(session);
}
