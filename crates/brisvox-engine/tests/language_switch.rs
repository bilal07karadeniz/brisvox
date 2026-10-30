//! Language switching mid-call (`brisvox_engine::language`): the STT stage reports each finished
//! utterance, the tracker switches after N consecutive utterances in another configured
//! language, and the switch shows at the next turn boundary only: the LLM request pins the reply
//! language, the TTS stage speaks it with that language's voice and model.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use brisvox_agent::Language;
use brisvox_core::frame::{ControlFrame, ControlKind, DataFrame, Frame, TextFrame};
use brisvox_core::{Links, Pipeline, SimClock, TimelineRecorder, TurnEpoch};
use brisvox_engine::sim::EngineFactory;
use brisvox_engine::{
    CallRatesCell, EngineConfig, LanguageProfile, LanguageSettings, LanguageTracker, Offload,
    PlayoutBook, TtsStage, TtsStageConfig, Utterance,
};
use brisvox_providers::{Role, SpokenLanguage};
use brisvox_sim::mock_common::StreamFault;
use brisvox_sim::mock_llm::LlmScript;
use brisvox_sim::mock_tts::MockTts;
use brisvox_sim::scenario::Scenario;
use brisvox_sim::session::SimSession;
use brisvox_transport::CallRates;
use common::{MS, Recorded, Recorder};
use tokio_util::sync::CancellationToken;

const SECOND: Duration = Duration::from_secs(1);

fn settings() -> Arc<LanguageSettings> {
    let profiles = vec![
        LanguageProfile::new("en", Language::En, "en-voice"),
        LanguageProfile::new("tr", Language::Tr, "tr-voice").with_tts_model("tr-model"),
    ];
    Arc::new(LanguageSettings::new(profiles, "en").unwrap())
}

fn said(text: &str, language: &str) -> Utterance {
    let mut u = Utterance::new();
    u.push(text, Some(&SpokenLanguage::new(language)));
    u
}

fn text(epoch: u32, text: &str) -> Frame {
    Frame::Data(DataFrame::LlmText(TextFrame {
        epoch: TurnEpoch::new(epoch),
        text: Arc::from(text),
    }))
}

fn control(epoch: u32, kind: ControlKind) -> Frame {
    Frame::Control(ControlFrame {
        epoch: TurnEpoch::new(epoch),
        kind,
    })
}

/// The leading system message of a provider request (the pinned language line is its last
/// paragraph).
fn system_of(request: &brisvox_providers::LlmRequest) -> String {
    let first = request.messages.first().unwrap();
    assert_eq!(first.role, Role::System, "{:?}", request.messages);
    first.content.to_string()
}

// ---------------------------------------------------------------------------------------------
// The TTS stage

struct TtsRig {
    stats: brisvox_engine::TtsStats,
    running: brisvox_core::RunningPipeline,
    clock: brisvox_core::SharedClock,
    tts: MockTts,
    tracker: Arc<LanguageTracker>,
    _rec: Recorded,
}

impl TtsRig {
    async fn new() -> Self {
        let clock = SimClock::shared();
        let tts = MockTts::new(clock.clone());
        let tracker = Arc::new(LanguageTracker::new(settings()));
        let stage = TtsStage::new(
            Arc::new(tts.clone()),
            TtsStageConfig::from_engine(&EngineConfig::DEFAULT),
            clock.clone(),
            TimelineRecorder::new(),
        )
        .with_book(PlayoutBook::shared(16))
        .with_rates(CallRatesCell::new(CallRates::symmetric(
            brisvox_core::frame::SampleRate::Hz16000,
        )))
        .with_offload(Offload::Inline(clock.clone()))
        .with_languages(Arc::clone(&tracker));
        let stats = stage.stats();
        let rec = Recorded::default();
        let running = Pipeline::new("tts")
            .then(stage)
            .then(Recorder(rec.clone()))
            .start(Links::default(), &CancellationToken::new())
            .await
            .unwrap();
        for _ in 0..30 {
            tokio::task::yield_now().await;
        }
        Self {
            stats,
            running,
            clock,
            tts,
            tracker,
            _rec: rec,
        }
    }

    async fn feed(&self, frame: Frame) {
        self.running.input().send(frame).await.unwrap();
    }

    async fn run_for(&self, d: Duration) {
        self.clock.sleep(d).await;
    }

    /// The caller said two Turkish utterances: the tracker switches.
    fn caller_switches_to_turkish(&self) {
        assert!(
            self.tracker
                .observe(&said("bana yardım eder misin", "tr"))
                .is_none()
        );
        assert!(
            self.tracker
                .observe(&said("siparişim nerede acaba", "tr"))
                .is_some()
        );
        assert_eq!(self.tracker.current_code(), "tr");
    }
}

#[tokio::test(start_paused = true)]
async fn the_tts_session_opens_with_the_default_language_voice() {
    let rig = TtsRig::new().await;
    let configs = rig.tts.configs();
    assert_eq!(configs.len(), 1);
    assert_eq!(&*configs[0].voice, "en-voice");
    assert_eq!(configs[0].language.as_deref(), Some("en"));
    assert_eq!(configs[0].model, None);
}

#[tokio::test(start_paused = true)]
async fn a_switch_decided_during_a_response_changes_the_voice_at_the_next_response() {
    let rig = TtsRig::new().await;
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "Your order is on its way. ")).await;
    rig.run_for(100 * MS).await;
    // The caller switches while response 1 is still being produced.
    rig.caller_switches_to_turkish();
    rig.feed(text(1, "It leaves today.")).await;
    rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
    rig.run_for(2 * SECOND).await;
    assert_eq!(
        rig.tts.connects(),
        1,
        "the response in progress keeps its session and voice"
    );

    // Response 2 is in Turkish: a session with the Turkish voice and model opens before its text.
    rig.feed(control(2, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(2, "Siparişiniz yolda.")).await;
    rig.feed(control(2, ControlKind::LlmResponseEnded)).await;
    rig.run_for(2 * SECOND).await;
    let configs = rig.tts.configs();
    assert_eq!(configs.len(), 2, "{configs:?}");
    assert_eq!(&*configs[1].voice, "tr-voice");
    assert_eq!(configs[1].language.as_deref(), Some("tr"));
    assert_eq!(configs[1].model.as_deref(), Some("tr-model"));
    let texts: Vec<String> = rig.tts.texts().into_iter().map(|(_, t)| t).collect();
    assert_eq!(texts.last().map(String::as_str), Some("Siparişiniz yolda."));
}

#[tokio::test(start_paused = true)]
async fn a_response_after_the_switch_back_uses_the_first_voice_again() {
    let rig = TtsRig::new().await;
    rig.caller_switches_to_turkish();
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "Merhaba.")).await;
    rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
    rig.run_for(SECOND).await;
    assert_eq!(rig.tts.connects(), 2);
    assert!(
        rig.tracker
            .observe(&said("where is my order", "en"))
            .is_none()
    );
    assert!(
        rig.tracker
            .observe(&said("please help me now", "en"))
            .is_some()
    );
    rig.feed(control(2, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(2, "Of course.")).await;
    rig.feed(control(2, ControlKind::LlmResponseEnded)).await;
    rig.run_for(SECOND).await;
    let configs = rig.tts.configs();
    assert_eq!(configs.len(), 3, "{configs:?}");
    assert_eq!(&*configs[2].voice, "en-voice");
}

// ---------------------------------------------------------------------------------------------
// The whole call

#[tokio::test(start_paused = true)]
async fn the_agent_follows_the_caller_into_turkish_after_two_utterances_and_not_before() {
    let session = SimSession::new(Arc::new(SimClock::new()));
    for reply in ["Hello.", "Hi again.", "Tamam."] {
        session.llm.push(LlmScript::text([reply]));
    }
    let factory = EngineFactory::new()
        .with_guards_off()
        .with_languages(settings());
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak_in("en", "hello there how are you", 800 * MS)
        .silence(4 * SECOND)
        .speak_in("tr", "merhaba nasılsın bugün", 800 * MS)
        .silence(4 * SECOND)
        .speak_in("tr", "siparişim nerede acaba", 800 * MS)
        .with_tail(6 * SECOND);
    let run = session
        .run_with(&factory, &scenario, 2 * SECOND)
        .await
        .unwrap();
    assert_eq!(run.outcome, Ok(()));

    let requests = session.llm.requests();
    assert_eq!(requests.len(), 3, "{requests:?}");
    assert!(system_of(&requests[0]).ends_with("Reply in English."));
    assert!(
        system_of(&requests[1]).ends_with("Reply in English."),
        "one Turkish utterance is not enough"
    );
    assert!(
        system_of(&requests[2]).ends_with("Reply in Turkish."),
        "the second one switches, and the reply to it is in Turkish"
    );
    // The stored conversation never carries the pinned line.
    let handles = factory.handles();
    let context = handles.last().unwrap().context.lock().unwrap().clone();
    assert!(
        !format!("{:?}", context.messages()).contains("Reply in"),
        "{context:?}"
    );

    let configs = session.tts.configs();
    assert_eq!(configs.len(), 2, "{configs:?}");
    assert_eq!(&*configs[0].voice, "en-voice");
    assert_eq!(&*configs[1].voice, "tr-voice");
    assert_eq!(configs[1].model.as_deref(), Some("tr-model"));
    let stats = handles.last().unwrap().language.as_ref().unwrap().stats();
    assert_eq!((stats.observed, stats.switches), (3, 1), "{stats:?}");
}

#[tokio::test(start_paused = true)]
async fn an_unconfigured_language_never_switches_the_call() {
    let session = SimSession::new(Arc::new(SimClock::new()));
    for reply in ["Hello.", "Sorry.", "Sorry again."] {
        session.llm.push(LlmScript::text([reply]));
    }
    let factory = EngineFactory::new()
        .with_guards_off()
        .with_languages(settings());
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak_in("de", "wo ist meine bestellung", 800 * MS)
        .silence(4 * SECOND)
        .speak_in("de", "ich brauche hilfe bitte", 800 * MS)
        .silence(4 * SECOND)
        .speak_in("de", "hallo wie geht es dir", 800 * MS)
        .with_tail(6 * SECOND);
    let run = session
        .run_with(&factory, &scenario, 2 * SECOND)
        .await
        .unwrap();
    assert_eq!(run.outcome, Ok(()));
    let requests = session.llm.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        requests
            .iter()
            .all(|r| system_of(r).ends_with("Reply in English."))
    );
    assert_eq!(session.tts.configs().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn without_a_provider_report_the_text_decides() {
    let session = SimSession::new(Arc::new(SimClock::new()));
    for reply in ["Hello.", "Tamam."] {
        session.llm.push(LlmScript::text([reply]));
    }
    let factory = EngineFactory::new()
        .with_guards_off()
        .with_languages(settings());
    let turkish =
        "Merhaba, siparişimin nerede olduğunu öğrenmek istiyorum lütfen bana yardım eder misiniz";
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak(turkish, 800 * MS)
        .silence(4 * SECOND)
        .speak(turkish, 800 * MS)
        .with_tail(6 * SECOND);
    let run = session
        .run_with(&factory, &scenario, 2 * SECOND)
        .await
        .unwrap();
    assert_eq!(run.outcome, Ok(()));
    let requests = session.llm.requests();
    assert_eq!(requests.len(), 2);
    assert!(system_of(&requests[0]).ends_with("Reply in English."));
    assert!(system_of(&requests[1]).ends_with("Reply in Turkish."));
    let stats = factory
        .handles()
        .last()
        .unwrap()
        .language
        .as_ref()
        .unwrap()
        .stats();
    assert_eq!((stats.from_text, stats.switches), (2, 1), "{stats:?}");
}

/// The `en_tr_mixed` fixture (real speech: an English sentence, then a Turkish one) through the
/// whole production pipeline: the mock STT reports no language, so the text of each final
/// decides, and one Turkish utterance is enough with `switch_after = 1`.
#[tokio::test(start_paused = true)]
async fn the_mixed_english_turkish_fixture_switches_at_the_turkish_sentence() {
    let fixture =
        brisvox_sim::fixture::Fixture::load(&brisvox_sim::fixture::fixtures_dir(), "en_tr_mixed")
            .unwrap();
    let mut settings = (*settings()).clone();
    settings.switch_after = 1;
    let session = SimSession::new(Arc::new(SimClock::new()));
    for reply in ["The islands are off the coast.", "Tamam."] {
        session.llm.push(LlmScript::text([reply]));
    }
    let factory = EngineFactory::new()
        .with_guards_off()
        .with_languages(Arc::new(settings));
    let run = session
        .run_with(&factory, &fixture, 6 * SECOND)
        .await
        .unwrap();
    assert_eq!(run.outcome, Ok(()));
    let requests = session.llm.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(system_of(&requests[0]).ends_with("Reply in English."));
    assert!(system_of(&requests[1]).ends_with("Reply in Turkish."));
    let configs = session.tts.configs();
    assert_eq!(
        configs.iter().map(|c| &*c.voice).collect::<Vec<_>>(),
        ["en-voice", "tr-voice"]
    );
}

/// A barge-in during a response in language A, whose own utterance makes the switch to B: the
/// new epoch's request, voice and voice session are B, and the context holds the A response cut
/// to what was played, with no trace of the pinned line.
#[tokio::test(start_paused = true)]
async fn a_barge_in_that_switches_the_language_answers_in_the_new_language_with_the_played_text_only()
 {
    let mut settings = (*settings()).clone();
    settings.switch_after = 1;
    let session = SimSession::new(Arc::new(SimClock::new()));
    let long = [
        "Your order is on its way and ",
        "it will arrive tomorrow morning, ",
        "unless the carrier is late, ",
        "in which case we will call you, ",
        "and you can also follow it online, ",
        "using the number we sent by mail.",
    ];
    session.llm.push(LlmScript::text(long));
    session
        .llm
        .push(LlmScript::text(["Tamam, hemen bakıyorum."]));
    let factory = EngineFactory::new()
        .with_guards_off()
        .with_languages(Arc::new(settings));
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak_in("en", "where is my order please", 800 * MS)
        .silence(2 * SECOND)
        // The bot is still speaking its long answer.
        .speak_in("tr", "siparişim nerede acaba şimdi", 800 * MS)
        .with_tail(8 * SECOND);
    let run = session
        .run_with(&factory, &scenario, 2 * SECOND)
        .await
        .unwrap();
    assert_eq!(run.outcome, Ok(()));
    run.check_no_stale_audio_after_interrupt().unwrap();

    let requests = session.llm.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(system_of(&requests[0]).ends_with("Reply in English."));
    assert!(system_of(&requests[1]).ends_with("Reply in Turkish."));
    // The first answer reached the second request cut to what was played: shorter than what the
    // LLM produced, and only that text.
    let full = long.concat();
    let heard: Vec<&str> = requests[1]
        .messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .map(|m| &*m.content)
        .collect();
    assert_eq!(heard.len(), 1, "{:?}", requests[1].messages);
    assert!(
        heard[0].len() < full.len() && full.starts_with(heard[0].trim_end()),
        "{:?}",
        heard[0]
    );
    for message in &requests[1].messages {
        if message.role != Role::System {
            assert!(!message.content.contains("Reply in"), "{message:?}");
        }
    }
    // The Turkish answer is spoken by the Turkish voice session.
    let configs = session.tts.configs();
    assert_eq!(
        configs.iter().map(|c| &*c.voice).collect::<Vec<_>>(),
        ["en-voice", "tr-voice"]
    );
    let handles = factory.handles();
    let stored = handles.last().unwrap().context.lock().unwrap().clone();
    assert!(!format!("{:?}", stored.messages()).contains("Reply in"));
}

#[tokio::test(start_paused = true)]
async fn a_failed_voice_session_for_the_new_language_is_retried_like_any_connect() {
    let rig = TtsRig::new().await;
    // The replacement session, the next one that connects, is refused.
    rig.tts
        .push_session_faults(vec![StreamFault::ConnectError { status: 503 }]);
    rig.caller_switches_to_turkish();
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "Merhaba.")).await;
    rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
    rig.run_for(100 * MS).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.language_reconnects, 1);
    assert_eq!(stats.connect_failures, 1, "{stats:?}");
    assert!(stats.text_dropped >= 1, "{stats:?}");
    assert_eq!(rig.tts.connects(), 2);

    // After the backoff the next response connects with the Turkish voice and is spoken: the
    // failed replacement did not leave the stage on the English voice or out of budget.
    rig.run_for(2 * SECOND).await;
    rig.feed(control(2, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(2, "Siparişiniz yolda.")).await;
    rig.feed(control(2, ControlKind::LlmResponseEnded)).await;
    rig.run_for(2 * SECOND).await;
    let configs = rig.tts.configs();
    assert_eq!(configs.len(), 3, "{configs:?}");
    assert_eq!(&*configs[2].voice, "tr-voice");
    let stats = rig.stats.snapshot();
    assert_eq!(stats.degraded, 0, "{stats:?}");
    let texts: Vec<String> = rig.tts.texts().into_iter().map(|(_, t)| t).collect();
    assert_eq!(texts.last().map(String::as_str), Some("Siparişiniz yolda."));
}
