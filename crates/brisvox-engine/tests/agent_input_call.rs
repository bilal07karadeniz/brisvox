//! Bot lines and conversation edits (`brisvox_engine::agent_input`) in a whole call:
//! `run_call` over the loopback transport, the simulator's mocks and scripted caller, paused
//! time.
//!
//! The rules under test: a line to an idle bot is a bot turn of its own (spoken verbatim, no LLM
//! request, in the context); a line waits while the bot answers unless it interrupts, in which
//! case it cuts the answer like a barge-in; the caller can barge in on a line, which is then cut
//! to what was heard.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::{Arc, PoisonError};
use std::time::Duration;

use brisvox_agent::context::Message;
use brisvox_core::frame::SampleRate;
use brisvox_core::{SharedClock, SimClock};
use brisvox_engine::sim::{LoopbackCall, energy_vad_setup, open_loopback};
use brisvox_engine::{
    BotLine, Call, ConversationEdit, EditSender, EngineConfig, InterruptionConfig, Offload,
    SaySender, SessionBuilder, conversation_edits, run_call, say_input,
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
    say: SaySender,
    edits: EditSender,
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
    let (say, say_rx) = say_input(4);
    let (edits, edits_rx) = conversation_edits(8);
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
    .with_system_prompt("You are a receptionist.")
    .with_vad(energy_vad_setup(offload, &sim.vad, &config()))
    .with_say_input(say_rx)
    .with_conversation_edits(edits_rx);
    let call = run_call(session, builder, &cancel).await.unwrap();
    Rig {
        sim,
        clock,
        caller: Arc::new(Caller {
            client: tokio::sync::Mutex::new(client),
        }),
        cancel,
        call,
        say,
        edits,
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

    /// The messages of request `n` (their `Debug` text).
    fn request(&self, n: usize) -> String {
        format!("{:?}", self.sim.llm.requests()[n].messages)
    }

    /// The assistant texts of the conversation now.
    fn assistant_texts(&self) -> Vec<String> {
        let context = self.call.handles().context.clone();
        let context = context.lock().unwrap_or_else(PoisonError::into_inner);
        context
            .messages()
            .iter()
            .filter_map(|m| match m {
                Message::Assistant { text, .. } => Some(text.to_string()),
                _ => None,
            })
            .collect()
    }

    /// The epochs of the audio the caller received (in order, deduplicated).
    async fn audio_epochs(&self, quiet: Duration) -> (Vec<u32>, bool) {
        let mut epochs: Vec<u32> = Vec::new();
        let mut cleared = false;
        let mut client = self.caller.client.lock().await;
        loop {
            tokio::select! {
                event = client.recv() => match event {
                    Some(ClientEvent::Audio { frame, .. }) => {
                        let epoch = frame.epoch().get();
                        if epochs.last() != Some(&epoch) {
                            epochs.push(epoch);
                        }
                    }
                    Some(ClientEvent::Clear { .. }) => cleared = true,
                    Some(_) => {}
                    None => break,
                },
                () = self.clock.sleep(quiet) => break,
            }
        }
        (epochs, cleared)
    }
}

fn answer() -> LlmScript {
    LlmScript::text(["Sure thing."])
}

fn story() -> LlmScript {
    LlmScript::text([
        "Once upon a time there was a voice agent that talked and talked ",
        "and never stopped talking until somebody interrupted it.",
    ])
}

#[tokio::test(start_paused = true)]
async fn a_greeting_line_is_spoken_without_the_llm_and_the_next_request_sees_it() {
    let scenario = Scenario::new()
        .silence(300 * MS)
        .speak("I need a table", 800 * MS)
        .with_tail(600 * MS);
    let rig = start(&scenario, vec![answer()]).await;
    rig.say
        .try_send(BotLine::new("Welcome to Luigi's, how can I help?"))
        .unwrap();
    rig.clock.sleep(2_000 * MS).await;
    assert!(rig.sim.llm.requests().is_empty(), "a line asks no LLM");
    assert_eq!(
        rig.assistant_texts(),
        ["Welcome to Luigi's, how can I help?"]
    );
    let driver = rig.call.handles().driver.snapshot();
    assert_eq!(driver.lines_started, 1, "{driver:?}");
    assert_eq!(driver.interrupts, 0, "{driver:?}");
    let (epochs, cleared) = rig.audio_epochs(50 * MS).await;
    assert_eq!(epochs, [0], "the greeting is the first epoch's audio");
    assert!(!cleared);

    rig.play(&scenario).await;
    rig.clock.sleep(2_000 * MS).await;
    assert_eq!(rig.sim.llm.requests().len(), 1);
    let first = rig.request(0);
    let greeting = first.find("Welcome to Luigi's").unwrap();
    let question = first.find("I need a table").unwrap();
    assert!(
        greeting < question,
        "the line comes before the question: {first}"
    );
    let (epochs, _) = rig.audio_epochs(50 * MS).await;
    assert_eq!(epochs, [1], "the answer is the next epoch");
}

#[tokio::test(start_paused = true)]
async fn a_line_waits_for_the_answer_and_an_interrupting_line_cuts_it() {
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("tell me a long story", 900 * MS)
        .with_tail(600 * MS);
    let rig = start(&scenario, vec![story()]).await;
    rig.play(&scenario).await;
    // The bot is telling the story.
    rig.clock.sleep(800 * MS).await;
    rig.say.try_send(BotLine::new("One more thing.")).unwrap();
    rig.clock.sleep(300 * MS).await;
    assert_eq!(
        rig.call.handles().driver.snapshot().lines_started,
        0,
        "a plain line waits while the bot speaks"
    );
    // An interrupting line overrides the waiting one and cuts the story.
    rig.say
        .try_send(BotLine::interrupting("Sorry, I have to stop here."))
        .unwrap();
    rig.clock.sleep(4_000 * MS).await;
    let driver = rig.call.handles().driver.snapshot();
    assert_eq!(driver.lines_started, 1, "{driver:?}");
    assert_eq!(
        driver.lines_rejected, 1,
        "the waiting line was dropped: {driver:?}"
    );
    assert_eq!(driver.interrupts, 1, "{driver:?}");
    assert_eq!(rig.sim.llm.requests().len(), 1, "lines ask no LLM");
    let texts = rig.assistant_texts();
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(
        texts[0].starts_with("Once upon a time") && !texts[0].contains("somebody interrupted it."),
        "the story is cut to what was heard: {texts:?}"
    );
    assert_eq!(texts[1], "Sorry, I have to stop here.");
    assert_eq!(
        driver.clears_sent, 1,
        "the line cleared the story: {driver:?}"
    );
    let (epochs, _) = rig.audio_epochs(50 * MS).await;
    assert_eq!(epochs, [0, 1], "the story, then the line in the next epoch");
}

#[tokio::test(start_paused = true)]
async fn the_caller_can_barge_in_on_a_line() {
    let scenario = Scenario::new()
        .silence(1_500 * MS)
        .speak("wait a second", 900 * MS)
        .with_tail(600 * MS);
    let rig = start(&scenario, vec![answer()]).await;
    rig.edits
        .try_send(ConversationEdit::Append(Message::system("Caller: Ada.")))
        .unwrap();
    rig.say
        .try_send(BotLine::new(
            "Hello and welcome to our restaurant. Today we have a long list of specials \
             that I would like to read to you one by one, starting with the soup of the day.",
        ))
        .unwrap();
    // The caller talks over the long line (at 1.5 s).
    rig.play(&scenario).await;
    rig.clock.sleep(3_000 * MS).await;
    let driver = rig.call.handles().driver.snapshot();
    assert_eq!(driver.lines_started, 1, "{driver:?}");
    assert_eq!(driver.interrupts, 1, "the caller barged in: {driver:?}");
    assert_eq!(rig.sim.llm.requests().len(), 1);
    let first = rig.request(0);
    assert!(first.contains("Caller: Ada."), "the edit is in: {first}");
    assert!(first.contains("Hello and welcome"), "{first}");
    assert!(
        !first.contains("soup of the day"),
        "the line is cut to what was heard: {first}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_plain_line_is_spoken_after_the_answer_without_cutting_it() {
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("tell me a long story", 900 * MS)
        .with_tail(600 * MS);
    let rig = start(&scenario, vec![story()]).await;
    rig.play(&scenario).await;
    rig.clock.sleep(800 * MS).await;
    rig.say.try_send(BotLine::new("Anything else?")).unwrap();
    rig.clock.sleep(15_000 * MS).await;
    let driver = rig.call.handles().driver.snapshot();
    assert_eq!(driver.lines_started, 1, "{driver:?}");
    assert_eq!(driver.interrupts, 0, "{driver:?}");
    let texts = rig.assistant_texts();
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(
        texts[0].ends_with("somebody interrupted it."),
        "played in full: {texts:?}"
    );
    assert_eq!(texts[1], "Anything else?");
    assert_eq!(driver.clears_sent, 0, "no barge-in clear: {driver:?}");
    let (epochs, _) = rig.audio_epochs(50 * MS).await;
    assert_eq!(epochs, [0, 1]);
}
