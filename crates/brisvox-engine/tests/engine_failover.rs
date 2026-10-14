//! The resilience layer on the all-production pipeline (`EngineFactory`, loopback
//! transport, paused clock), with provider faults from `brisvox_sim`: every stage's primary
//! down (the secondaries serve, the process-wide breakers open, skip the primary for free, then
//! half-open and close when it is back), mid-call deaths of each stage (the turn survives whole:
//! STT replays the utterance, the LLM request is re-issued, the TTS re-speaks the unheard
//! answer), and both providers down with the budgets spent (the call ends cleanly, the providers
//! were contacted a bounded number of times, no task is left behind); a barge-in through the
//! layer (nothing stale, nothing left open), blackholed primaries (failover at the attempt
//! timeout) and the half-open probe of a deployment without secondaries.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_core::{SharedClock, SimClock};
use brisvox_engine::sim::{EngineFactory, SimProviders, SimResilient};
use brisvox_providers::Role;
use brisvox_providers::resilience::{
    BreakerConfig, BreakerState, BudgetConfig, CircuitBreaker, IdleRefresh, Leg, Resilience,
    ResilienceConfig, ResilienceProbe, ResilientLlm, ResilientStt, ResilientTts,
};
use brisvox_sim::leak::TaskCounter;
use brisvox_sim::mock_common::StreamFault;
use brisvox_sim::mock_llm::{LlmFault, LlmScript};
use brisvox_sim::outage::{Outage, OutageSwitch};
use brisvox_sim::scenario::Scenario;
use brisvox_sim::session::{SessionRun, SimSession};

const MS: Duration = Duration::from_millis(1);
const SEC: Duration = Duration::from_secs(1);
const ANSWER: &str = "Sure, here is the first answer.";
const SECOND: &str = "Second answer.";

fn scenario() -> Scenario {
    Scenario::new()
        .silence(100 * MS)
        .speak("first question please", 800 * MS)
        .silence(3_000 * MS)
        .speak("second question please", 800 * MS)
        .with_tail(500 * MS)
}

/// The process: one resilience context, a breaker per provider (STT, LLM, TTS; primary and
/// secondary), one outage switch for the primaries and one for the secondaries, and the probes
/// of every call's wrappers.
#[derive(Clone)]
struct Fleet {
    resilience: Resilience,
    /// `[stt, llm, tts]` primaries.
    primary: [Arc<CircuitBreaker>; 3],
    /// `[stt, llm, tts]` secondaries.
    secondary: [Arc<CircuitBreaker>; 3],
    primary_switch: OutageSwitch,
    secondary_switch: OutageSwitch,
    probes: Arc<Mutex<Vec<[ResilienceProbe; 3]>>>,
}

impl Fleet {
    fn new(clock: &SharedClock, config: ResilienceConfig) -> Self {
        let resilience = Resilience::new(config, clock.clone());
        let breaker = |name| Arc::new(CircuitBreaker::new(name, config.breaker, clock.clone()));
        Self {
            primary: [breaker("stt-a"), breaker("llm-a"), breaker("tts-a")],
            secondary: [breaker("stt-b"), breaker("llm-b"), breaker("tts-b")],
            resilience,
            primary_switch: OutageSwitch::new(),
            secondary_switch: OutageSwitch::new(),
            probes: Arc::default(),
        }
    }

    /// One call's wrappers: each stage's two legs are the call's mock behind the two switches
    /// (the secondary shares the mock's script, as a second vendor hearing the same caller).
    fn wrap(&self, parts: SimProviders) -> SimResilient {
        let (a, b) = (&self.primary_switch, &self.secondary_switch);
        let stt = ResilientStt::new(
            leg(Outage::new(parts.stt.clone(), a.clone()), &self.primary[0]),
            &self.resilience,
        )
        .with_secondary(leg(Outage::new(parts.stt, b.clone()), &self.secondary[0]));
        let llm = ResilientLlm::new(
            leg(Outage::new(parts.llm.clone(), a.clone()), &self.primary[1]),
            &self.resilience,
        )
        .with_secondary(leg(Outage::new(parts.llm, b.clone()), &self.secondary[1]));
        let tts = ResilientTts::new(
            leg(Outage::new(parts.tts.clone(), a.clone()), &self.primary[2]),
            &self.resilience,
        )
        .with_secondary(leg(Outage::new(parts.tts, b.clone()), &self.secondary[2]));
        self.probes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push([stt.probe(), llm.probe(), tts.probe()]);
        SimResilient { stt, llm, tts }
    }

    fn factory(&self) -> EngineFactory {
        let fleet = self.clone();
        base_factory().with_resilience(Arc::new(move |parts| fleet.wrap(parts)))
    }

    /// One call's wrappers over the primaries alone (no secondary: the conservative
    /// single-provider deployment).
    fn wrap_single(&self, parts: SimProviders) -> SimResilient {
        let a = &self.primary_switch;
        let stt = ResilientStt::new(
            leg(Outage::new(parts.stt, a.clone()), &self.primary[0]),
            &self.resilience,
        );
        let llm = ResilientLlm::new(
            leg(Outage::new(parts.llm, a.clone()), &self.primary[1]),
            &self.resilience,
        );
        let tts = ResilientTts::new(
            leg(Outage::new(parts.tts, a.clone()), &self.primary[2]),
            &self.resilience,
        );
        self.probes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push([stt.probe(), llm.probe(), tts.probe()]);
        SimResilient { stt, llm, tts }
    }

    fn single_factory(&self) -> EngineFactory {
        let fleet = self.clone();
        base_factory().with_resilience(Arc::new(move |parts| fleet.wrap_single(parts)))
    }

    /// The probes of the last call built.
    fn last(&self) -> [ResilienceProbe; 3] {
        self.probes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last()
            .cloned()
            .expect("a call was built")
    }

    fn states(breakers: &[Arc<CircuitBreaker>; 3]) -> [BreakerState; 3] {
        [
            breakers[0].state(),
            breakers[1].state(),
            breakers[2].state(),
        ]
    }
}

fn leg<P>(provider: P, breaker: &Arc<CircuitBreaker>) -> Leg<P> {
    Leg::new(Arc::new(provider), Arc::clone(breaker))
}

/// The chaos suite's engine factory (adaptive turn end, the default first chunk).
fn base_factory() -> EngineFactory {
    EngineFactory::new().with_smart_turn(0.9)
}

fn config() -> ResilienceConfig {
    ResilienceConfig {
        breaker: BreakerConfig {
            failure_threshold: 2,
            open_for: 60 * SEC,
        },
        ..ResilienceConfig::DEFAULT
    }
}

/// A call answered with [`ANSWER`] then [`SECOND`]; `script` adds faults.
fn call(clock: &SharedClock, script: impl FnOnce(&SimSession) -> LlmScript) -> SimSession {
    let s = SimSession::new(clock.clone());
    let first = script(&s);
    s.llm.push(first);
    s.llm.push(LlmScript::text([SECOND]));
    s
}

fn plain(_: &SimSession) -> LlmScript {
    LlmScript::text(ANSWER.split_inclusive(' '))
}

/// The conversation as (role, text) pairs.
fn conversation(run: &SessionRun) -> Vec<(Role, String)> {
    run.probes
        .history()
        .into_iter()
        .filter(|m| matches!(m.role, Role::User | Role::Assistant))
        .map(|m| (m.role, m.content.to_string()))
        .collect()
}

fn answered_whole(run: &SessionRun) -> bool {
    conversation(run)
        == [
            (Role::User, "first question please".to_owned()),
            (Role::Assistant, ANSWER.to_owned()),
            (Role::User, "second question please".to_owned()),
            (Role::Assistant, SECOND.to_owned()),
        ]
}

#[tokio::test(start_paused = true)]
async fn primary_outage_the_secondaries_serve_and_the_breakers_open_half_open_and_close() {
    let clock = SimClock::shared();
    let fleet = Fleet::new(&clock, config());
    let factory = fleet.factory();
    fleet.primary_switch.set_down(true);

    // Calls 1 and 2: every stage tries its primary (refused), the secondary serves.
    for n in 1..=2 {
        let s = call(&clock, plain);
        let run = s.run_with(&factory, &scenario(), 4 * SEC).await.unwrap();
        run.outcome.as_ref().unwrap();
        assert!(answered_whole(&run), "call {n}: {:?}", conversation(&run));
        for (stage, probe) in fleet.last().iter().enumerate() {
            let stats = probe.stats();
            assert!(
                stats.served_secondary >= 1,
                "call {n} stage {stage}: {stats:?}"
            );
            assert_eq!(stats.served_primary, 0, "call {n} stage {stage}: {stats:?}");
        }
    }
    // Two failures per provider across the two calls (the LLM had two in the first): open.
    assert_eq!(Fleet::states(&fleet.primary), [BreakerState::Open; 3]);
    let refused = fleet.primary_switch.refused();

    // Call 3: the open breakers skip the primaries without a request and without a retry.
    let s = call(&clock, plain);
    let run = s.run_with(&factory, &scenario(), 4 * SEC).await.unwrap();
    assert!(answered_whole(&run), "{:?}", conversation(&run));
    assert_eq!(
        fleet.primary_switch.refused(),
        refused,
        "no request reached them"
    );
    for probe in fleet.last() {
        let stats = probe.stats();
        assert_eq!(stats.retries, 0, "{stats:?}");
        assert!(stats.breaker_skips >= 1, "{stats:?}");
    }

    // The primaries are back. After the open period each breaker lets one probe through; the
    // probe succeeds and closes it, and the call is served by the primaries.
    fleet.primary_switch.set_down(false);
    clock.sleep(60 * SEC).await;
    assert_eq!(Fleet::states(&fleet.primary), [BreakerState::HalfOpen; 3]);
    let s = call(&clock, plain);
    let run = s.run_with(&factory, &scenario(), 4 * SEC).await.unwrap();
    assert!(answered_whole(&run), "{:?}", conversation(&run));
    assert_eq!(Fleet::states(&fleet.primary), [BreakerState::Closed; 3]);
    for (stage, probe) in fleet.last().iter().enumerate() {
        let stats = probe.stats();
        assert!(stats.served_primary >= 1, "stage {stage}: {stats:?}");
        assert_eq!(stats.served_secondary, 0, "stage {stage}: {stats:?}");
    }
    for breaker in &fleet.primary {
        assert_eq!(breaker.stats().probes, 1, "{}", breaker.name());
    }
}

/// Mid-call deaths of all three stages in the first turn: the STT session drops after its first
/// interim, the LLM's request is refused (503), the TTS session drops before its first audio.
fn mid_call_faults(s: &SimSession) -> LlmScript {
    s.stt
        .push_session_faults(vec![StreamFault::Disconnect { after_events: 1 }]);
    s.tts
        .push_session_faults(vec![StreamFault::Disconnect { after_events: 0 }]);
    s.llm
        .push(LlmScript::text(["refused"]).with_fault(LlmFault::ErrorResponse { status: 503 }));
    LlmScript::text(ANSWER.split_inclusive(' '))
}

#[tokio::test(start_paused = true)]
async fn mid_call_deaths_of_every_stage_lose_nothing_of_the_turn() {
    let clock = SimClock::shared();
    let fleet = Fleet::new(&clock, config());
    let s = call(&clock, mid_call_faults);
    let run = s
        .run_with(&fleet.factory(), &scenario(), 4 * SEC)
        .await
        .unwrap();
    run.outcome.as_ref().unwrap();
    assert!(answered_whole(&run), "{:?}", conversation(&run));
    let [stt, llm, tts] = fleet.last().map(|p| p.stats());
    assert_eq!(stt.recoveries, 1, "{stt:?}");
    assert!(stt.replayed > 0, "{stt:?}");
    assert_eq!((llm.served_secondary, llm.retries), (1, 1), "{llm:?}");
    assert_eq!(tts.recoveries, 1, "{tts:?}");
    assert!(tts.replayed > 0, "{tts:?}");

    // Control: the same faults without the layer cost the first turn (the transcript is cut at
    // the STT death, and the refused request leaves the turn unanswered: no retries here).
    let s = call(&clock, mid_call_faults);
    let run = s
        .run_with(&base_factory(), &scenario(), 4 * SEC)
        .await
        .unwrap();
    run.outcome.as_ref().unwrap();
    assert!(!answered_whole(&run), "{:?}", conversation(&run));
}

#[tokio::test(start_paused = true)]
async fn both_providers_down_and_budgets_spent_end_the_call_cleanly() {
    let clock = SimClock::shared();
    let config = ResilienceConfig {
        session_budget: BudgetConfig {
            capacity: 1,
            refill_every: 600 * SEC,
        },
        ..config()
    };
    let fleet = Fleet::new(&clock, config);
    fleet.primary_switch.set_down(true);
    fleet.secondary_switch.set_down(true);
    let factory = fleet.factory();
    let tasks = TaskCounter::start().unwrap();
    let s = call(&clock, plain);
    let run = s.run_with(&factory, &scenario(), 4 * SEC).await.unwrap();
    // The pipeline itself is fine: the stages degrade as they do when a provider is down.
    run.outcome.as_ref().unwrap();
    assert!(
        !run.output.iter().any(|r| r.kind == "audio"),
        "no bot audio without providers"
    );
    assert!(s.llm.requests().is_empty(), "no request reached a provider");
    // Bounded contact: each primary until its breaker opened (2 failures), each secondary at
    // most once per stage (the call's budget of one retry); everything else was refused by the
    // layer without a request.
    assert!(
        fleet.primary_switch.refused() <= 3 * 2,
        "{}",
        fleet.primary_switch.refused()
    );
    assert!(
        fleet.secondary_switch.refused() <= 3,
        "{}",
        fleet.secondary_switch.refused()
    );
    let [stt, llm, tts] = fleet.last().map(|p| (p.stats(), p.budget_available()));
    // STT: its one retry went to the secondary; the engine's reconnects then met a spent budget
    // and, once the primary's breaker had opened, were refused without contacting anyone.
    assert_eq!(stt.1, 0, "{stt:?}");
    assert_eq!((stt.0.retries, stt.0.budget_denied), (1, 1), "{stt:?}");
    assert!(stt.0.unavailable >= 1, "{stt:?}");
    // TTS: one connect at the start (primary, then the secondary with the one retry); no round
    // ever had text, so nothing more.
    assert_eq!((tts.0.retries, tts.1), (1, 0), "{tts:?}");
    // LLM: no transcript ever reached it.
    assert_eq!(llm.0.attempts, 0, "{llm:?}");
    assert_eq!(Fleet::states(&fleet.primary)[0], BreakerState::Open);
    tasks.wait_for_baseline(&*clock, 5 * SEC).await.unwrap();
}

/// When the bot's first audio of each turn reached the caller.
fn first_audio(run: &SessionRun) -> Vec<Duration> {
    let mut out: Vec<(u32, Duration)> = Vec::new();
    for r in run.output.iter().filter(|r| r.kind == "audio") {
        let epoch = r.epoch.unwrap_or(0);
        if !out.iter().any(|(e, _)| *e == epoch) {
            out.push((epoch, r.at));
        }
    }
    out.into_iter().map(|(_, at)| at).collect()
}

#[tokio::test(start_paused = true)]
async fn a_quiet_call_refreshes_its_tts_session_without_a_gap() {
    let clock = SimClock::shared();
    let mut runs = Vec::new();
    for refresh in [IdleRefresh::After(2 * SEC), IdleRefresh::Off] {
        let fleet = Fleet::new(
            &clock,
            ResilienceConfig {
                idle_refresh: refresh,
                ..config()
            },
        );
        let s = call(&clock, plain);
        let run = s
            .run_with(&fleet.factory(), &scenario(), 4 * SEC)
            .await
            .unwrap();
        assert!(answered_whole(&run), "{:?}", conversation(&run));
        let tts = fleet.last()[2].stats();
        // Every connect after the first was a refresh made while the call was quiet: the
        // second turn found a fresh session and never connected on its own.
        assert_eq!(s.tts.connects() as u64, 1 + tts.refreshes, "{tts:?}");
        runs.push((tts.refreshes, first_audio(&run)));
    }
    let [(refreshes, with), (none, without)] = [runs[0].clone(), runs[1].clone()];
    assert!(refreshes >= 1, "the 3 s pause holds a 2 s refresh");
    assert_eq!(none, 0);
    assert_eq!(
        with, without,
        "the swap is not heard: same first-audio times"
    );
}

#[tokio::test(start_paused = true)]
async fn a_barge_in_through_the_layer_leaves_nothing_open_and_nothing_stale() {
    let clock = SimClock::shared();
    let fleet = Fleet::new(
        &clock,
        ResilienceConfig {
            idle_refresh: IdleRefresh::After(2 * SEC),
            ..config()
        },
    );
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("tell me a long story", 900 * MS)
        .silence(1_800 * MS)
        .speak("okay stop", 600 * MS)
        .silence(5_000 * MS)
        .speak("anything else", 600 * MS)
        .with_tail(500 * MS);
    let s = SimSession::new(clock.clone());
    s.llm.push(LlmScript::text([
        "Once upon a time there was a voice agent that talked and talked ",
        "and never stopped talking until somebody interrupted it.",
    ]));
    s.llm.push(LlmScript::text(["Sure, stopping."]));
    s.llm.push(LlmScript::text(["Nothing else."]));
    let run = s
        .run_with(&fleet.factory(), &scenario, 4 * SEC)
        .await
        .unwrap();
    run.outcome.as_ref().unwrap();
    assert_eq!(
        run.output.iter().filter(|r| r.kind == "interrupt").count(),
        1,
        "one barge-in"
    );
    run.check_no_stale_audio_after_interrupt().unwrap();
    let said: Vec<String> = conversation(&run)
        .into_iter()
        .filter(|(role, _)| *role == Role::Assistant)
        .map(|(_, text)| text)
        .collect();
    assert_eq!(said.len(), 3, "{said:?}");
    assert!(said[0].starts_with("Once upon a time"), "{said:?}");
    assert_ne!(
        said[0],
        "Once upon a time there was a voice agent that talked and talked and never stopped \
         talking until somebody interrupted it.",
        "the story was cut"
    );
    assert_eq!(said[1..], ["Sure, stopping.", "Nothing else."]);
    // The cancelled answer left nothing open in the layer: the pause after the second answer
    // holds a refresh, and every connect after the first was a refresh.
    let tts = fleet.last()[2].stats();
    assert!(tts.refreshes >= 1, "{tts:?}");
    assert_eq!(s.tts.connects() as u64, 1 + tts.refreshes, "{tts:?}");
    assert_eq!(tts.recovery_failures, 0, "{tts:?}");
}

#[tokio::test(start_paused = true)]
async fn hung_primaries_fail_over_at_the_attempt_timeout_within_the_engine_deadlines() {
    let clock = SimClock::shared();
    let fleet = Fleet::new(&clock, config());
    // A blackholed primary: connects and requests never answer (no 503).
    fleet.primary_switch.set_hanging(true);
    // Every first attempt costs 2 s: the caller starts after the STT failover (the engine does
    // not transcribe audio from before its STT session is up), and a longer pause keeps the
    // second question after the first answer.
    let scenario = Scenario::new()
        .silence(2_500 * MS)
        .speak("first question please", 800 * MS)
        .silence(6_000 * MS)
        .speak("second question please", 800 * MS)
        .with_tail(500 * MS);
    let s = call(&clock, plain);
    let run = s
        .run_with(&fleet.factory(), &scenario, 6 * SEC)
        .await
        .unwrap();
    run.outcome.as_ref().unwrap();
    assert!(answered_whole(&run), "{:?}", conversation(&run));
    for (stage, probe) in fleet.last().iter().enumerate() {
        let stats = probe.stats();
        assert!(stats.served_secondary >= 1, "stage {stage}: {stats:?}");
        assert_eq!(stats.served_primary, 0, "stage {stage}: {stats:?}");
    }
    // Each attempt on a primary hung until the attempt timeout and counted as a failure: one
    // connect each for STT and TTS, one request per turn for the LLM (whose breaker opened).
    assert_eq!(fleet.primary_switch.hung(), 4);
    assert_eq!(
        Fleet::states(&fleet.primary),
        [
            BreakerState::Closed,
            BreakerState::Open,
            BreakerState::Closed
        ]
    );
    for breaker in &fleet.primary {
        assert!(breaker.stats().failures >= 1, "{}", breaker.name());
    }
}

#[tokio::test(start_paused = true)]
async fn without_a_secondary_a_half_open_probe_does_not_refuse_the_other_calls() {
    let clock = SimClock::shared();
    let fleet = Fleet::new(&clock, config());
    let factory = fleet.single_factory();
    // An outage opens the STT and TTS breakers in two calls (no transcript ever reaches the
    // LLM).
    fleet.primary_switch.set_down(true);
    for _ in 0..2 {
        let s = call(&clock, plain);
        let run = s.run_with(&factory, &scenario(), 4 * SEC).await.unwrap();
        run.outcome.as_ref().unwrap();
        assert!(!answered_whole(&run));
    }
    let states = Fleet::states(&fleet.primary);
    assert_eq!([states[0], states[2]], [BreakerState::Open; 2]);

    // The provider is back and the open period over: two calls start at once. One of them
    // carries each probe; the other is admitted as soon as the probe connected, instead of
    // being refused until the probe proved healthy.
    fleet.primary_switch.set_down(false);
    clock.sleep(60 * SEC).await;
    let (a, b) = (call(&clock, plain), call(&clock, plain));
    let scenario = scenario();
    let (ra, rb) = tokio::join!(
        a.run_with(&factory, &scenario, 4 * SEC),
        b.run_with(&factory, &scenario, 4 * SEC)
    );
    for run in [ra.unwrap(), rb.unwrap()] {
        run.outcome.as_ref().unwrap();
        assert!(answered_whole(&run), "{:?}", conversation(&run));
    }
    assert_eq!(Fleet::states(&fleet.primary), [BreakerState::Closed; 3]);
    for breaker in [&fleet.primary[0], &fleet.primary[2]] {
        assert_eq!(breaker.stats().probes, 1, "{}", breaker.name());
    }
    for probes in fleet.probes.lock().unwrap().iter().rev().take(2) {
        for probe in probes {
            assert_eq!(probe.stats().unavailable, 0, "{:?}", probe.stats());
        }
    }
}
