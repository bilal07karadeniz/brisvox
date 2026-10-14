//! Typed user messages (`brisvox_engine::text_input`) in a whole call: `run_call` over
//! the loopback transport, the simulator's mocks and scripted caller, paused time.
//!
//! The rules under test: a typed message while the bot is idle is a user turn of its own; while
//! the bot speaks it is a barge-in first; while the user speaks it joins the open turn; with
//! `run_immediately: false` it waits for the next turn. In every case the text reaches the LLM
//! as (part of) a user message.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{SharedClock, SimClock};
use brisvox_engine::sim::{LoopbackCall, energy_vad_setup, open_loopback};
use brisvox_engine::{
    Call, EngineConfig, InterruptionConfig, Offload, SessionBuilder, TextTurn, TextTurnSender,
    run_call, text_turns,
};
use brisvox_sim::input::{InputSink, InputTarget, SendFuture};
use brisvox_sim::mock_llm::LlmScript;
use brisvox_sim::scenario::{Scenario, play_input};
use brisvox_sim::session::SimSession;
use brisvox_transport::ClientEvent;
use tokio_util::sync::CancellationToken;

const MS: Duration = Duration::from_millis(1);

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
    text: TextTurnSender,
}

fn config() -> EngineConfig {
    let mut config = EngineConfig::DEFAULT;
    config.lead_margin = Duration::ZERO;
    config.llm.retries = 0;
    config.interruption = InterruptionConfig::OFF;
    config.turn = brisvox_agent::TurnPolicy::fixed(450 * MS);
    config
}

async fn start(scenario: &Scenario, scripts: Vec<LlmScript>) -> Rig {
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
    } = open_loopback(&clock, 65_536).await.unwrap();
    let offload = Offload::Inline(clock.clone());
    let (text, input) = text_turns(4);
    let builder = SessionBuilder::new(
        config(),
        clock.clone(),
        brisvox_core::TimelineRecorder::new(),
        offload.clone(),
        Arc::new(sim.stt.clone()),
        Arc::new(sim.llm.clone()),
        Arc::new(sim.tts.clone()),
    )
    .unwrap()
    .with_vad(energy_vad_setup(offload, &sim.vad, &config()))
    .with_text_input(input);
    let call = run_call(session, builder, &cancel).await.unwrap();
    Rig {
        sim,
        clock,
        caller: Arc::new(Caller {
            client: tokio::sync::Mutex::new(client),
        }),
        cancel,
        call,
        text,
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

    /// The user messages of request `n` (their `Debug` text).
    fn request(&self, n: usize) -> String {
        format!("{:?}", self.sim.llm.requests()[n].messages)
    }

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
}

fn answer() -> LlmScript {
    LlmScript::text(["Sure thing."])
}

#[tokio::test(start_paused = true)]
async fn a_held_message_joins_the_next_turn_and_a_message_to_an_idle_bot_is_answered() {
    let scenario = Scenario::new()
        .silence(300 * MS)
        .speak("what is my name", 800 * MS)
        .with_tail(600 * MS);
    let rig = start(&scenario, vec![answer(), answer()]).await;
    rig.clock.sleep(100 * MS).await;

    // Held: nothing runs now.
    rig.text
        .try_send(TextTurn {
            text: Arc::from("my name is Ada"),
            run_immediately: false,
        })
        .unwrap();
    rig.clock.sleep(200 * MS).await;
    assert!(
        rig.sim.llm.requests().is_empty(),
        "a held message runs nothing"
    );
    assert_eq!(rig.call.handles().driver.snapshot().text_held, 1);

    // The next (spoken) turn carries it.
    rig.play(&scenario).await;
    rig.clock.sleep(2_000 * MS).await;
    assert_eq!(rig.sim.llm.requests().len(), 1);
    let first = rig.request(0);
    assert!(
        first.contains("my name is Ada what is my name"),
        "held text first, then the speech: {first}"
    );

    // The bot is idle again: a typed message is a turn of its own, no barge-in.
    rig.text.try_send(TextTurn::now("thanks a lot")).unwrap();
    rig.clock.sleep(2_000 * MS).await;
    assert_eq!(rig.sim.llm.requests().len(), 2);
    let second = rig.request(1);
    assert!(second.contains("thanks a lot"), "{second}");
    let driver = rig.call.handles().driver.snapshot();
    assert_eq!(driver.text_turns, 1, "{driver:?}");
    assert_eq!(driver.interrupts, 0, "{driver:?}");
    assert_eq!(driver.turns_ended, 2, "{driver:?}");
    // The typed turn was answered with speech.
    let events = rig.events(50 * MS).await;
    let epochs: Vec<u32> = events
        .iter()
        .filter_map(|e| match e {
            ClientEvent::Audio { frame, .. } => Some(frame.epoch().get()),
            _ => None,
        })
        .collect();
    assert!(epochs.contains(&1), "the typed turn (epoch 1) was spoken");
}

#[tokio::test(start_paused = true)]
async fn a_message_while_the_bot_speaks_interrupts_it_and_is_answered() {
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("tell me a long story", 900 * MS)
        .with_tail(600 * MS);
    let story = LlmScript::text([
        "Once upon a time there was a voice agent that talked and talked ",
        "and never stopped talking until somebody interrupted it.",
    ]);
    let rig = start(&scenario, vec![story, answer()]).await;
    rig.play(&scenario).await;
    // The bot is speaking (it started about 0.5 s after the end of the question).
    rig.clock.sleep(800 * MS).await;
    assert!(rig.call.handles().out.snapshot().clears == 0);
    rig.text.try_send(TextTurn::now("stop please")).unwrap();
    rig.clock.sleep(2_000 * MS).await;

    let driver = rig.call.handles().driver.snapshot();
    assert_eq!(driver.interrupts, 1, "{driver:?}");
    assert_eq!(driver.text_turns, 1, "{driver:?}");
    assert_eq!(rig.sim.llm.requests().len(), 2);
    let second = rig.request(1);
    assert!(second.contains("stop please"), "{second}");
    // The context holds what was played of the story, not all of it (played-text truncation).
    assert!(
        !second.contains("somebody interrupted it."),
        "the unplayed tail is not in the context: {second}"
    );
    let events = rig.events(50 * MS).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ClientEvent::Clear { .. })),
        "the transport was cleared"
    );
}

#[tokio::test(start_paused = true)]
async fn a_message_while_the_user_speaks_joins_the_open_turn() {
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("hello there", 900 * MS)
        .with_tail(600 * MS);
    let rig = start(&scenario, vec![answer(), answer()]).await;
    let typed = async {
        // In the middle of the speech.
        rig.clock.sleep(500 * MS).await;
        rig.text.try_send(TextTurn::now("and more")).unwrap();
    };
    tokio::join!(rig.play(&scenario), typed);
    rig.clock.sleep(2_000 * MS).await;

    assert_eq!(rig.sim.llm.requests().len(), 1, "one turn, one answer");
    let first = rig.request(0);
    assert!(
        first.contains("and more") && first.contains("hello there"),
        "{first}"
    );
    let driver = rig.call.handles().driver.snapshot();
    assert_eq!(driver.text_joined, 1, "{driver:?}");
    assert_eq!(driver.text_turns, 0, "{driver:?}");
    assert_eq!(driver.turns_ended, 1, "{driver:?}");
}
