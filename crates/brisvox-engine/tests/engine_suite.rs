//! The shared simulator suite on the all-production pipeline (`docs/PIPELINE.md`
//! section 9): `EngineFactory` builds the chain of `build_session_pipeline` over the loopback
//! transport, and `brisvox_sim::suite` judges it under the Budget profile on a paused clock.
//!
//! The Exact profile stays on the reference pipeline (`brisvox-sim`'s own tests). Every suite
//! function runs each call twice and requires identical fingerprints (production against
//! production only). The divergences from the reference are listed in `docs/TESTING.md`.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::SimClock;
use brisvox_engine::sim::{EngineFactory, TIME_IN_RESULT};
use brisvox_sim::latency::LatencyProfile;
use brisvox_sim::mock_llm::LlmScript;
use brisvox_sim::scenario::Scenario;
use brisvox_sim::session::SimSession;
use brisvox_sim::suite::{self, Profile};

const MS: Duration = Duration::from_millis(1);

/// Every call the factory built kept the clear hook wired (`note_clear` before `Clear`), and the
/// test-only tap stage lost no upstream frame (its `sink` inbox makes that lane 4 frames deep).
fn clear_hook_stayed_wired(factory: &EngineFactory) {
    let calls = factory.handles();
    assert!(!calls.is_empty());
    for call in calls {
        assert_eq!(call.unnoted_clear_cuts(), 0);
        assert_eq!(call.tap_upstream_dropped(), 0, "tap upstream drops");
    }
}

#[tokio::test(start_paused = true)]
async fn all_scenarios_pass_with_fixed_silence_and_the_recommended_guards() {
    let factory = EngineFactory::new();
    suite::scenarios::all(&factory, Profile::Budget)
        .await
        .unwrap();
    clear_hook_stayed_wired(&factory);
}

#[tokio::test(start_paused = true)]
async fn all_scenarios_pass_with_every_guard_off() {
    let factory = EngineFactory::new().with_guards_off();
    suite::scenarios::all(&factory, Profile::Budget)
        .await
        .unwrap();
    clear_hook_stayed_wired(&factory);
}

#[tokio::test(start_paused = true)]
async fn all_scenarios_pass_with_the_adaptive_policy_and_a_fixed_model() {
    let factory = EngineFactory::new().with_smart_turn(0.9);
    suite::scenarios::all(&factory, Profile::Budget)
        .await
        .unwrap();
    clear_hook_stayed_wired(&factory);
}

#[tokio::test(start_paused = true)]
async fn all_scenarios_pass_with_a_registered_tool() {
    let factory = EngineFactory::new().with_time_tool();
    suite::scenarios::all(&factory, Profile::Budget)
        .await
        .unwrap();
    clear_hook_stayed_wired(&factory);
}

#[tokio::test(start_paused = true)]
async fn s08_runs_the_registered_tool_and_speaks_both_rounds() {
    // The reworked s08: the runner executes `time_in`, the result follows the call in the
    // context, the model's second round is spoken in the same turn.
    for factory in [
        EngineFactory::new().with_time_tool(),
        EngineFactory::new().with_time_tool().with_smart_turn(0.9),
    ] {
        suite::scenarios::s08_with_a_registered_tool(&factory, Profile::Budget, TIME_IN_RESULT)
            .await
            .unwrap();
        clear_hook_stayed_wired(&factory);
    }
}

#[tokio::test(start_paused = true)]
async fn the_basic_suite_passes_with_fixed_silence() {
    // Guards off: the basic barge-in budget is that of an immediate barge-in, which the
    // recommended guards deliberately delay (measured: the interrupt at 3 s instead of 2.42 s;
    // docs/TESTING.md, divergences). s02 and s10 run with the guards in the scenario tests.
    let factory = EngineFactory::new().with_guards_off();
    suite::basic::all(&factory, Profile::Budget).await.unwrap();
    clear_hook_stayed_wired(&factory);
}

#[tokio::test(start_paused = true)]
async fn the_basic_suite_passes_with_the_adaptive_policy() {
    let factory = EngineFactory::new().with_guards_off().with_smart_turn(0.9);
    suite::basic::all(&factory, Profile::Budget).await.unwrap();
    clear_hook_stayed_wired(&factory);
}

#[tokio::test(start_paused = true)]
async fn real_speech_in_english_turkish_and_both_is_answered_within_600_ms() {
    // The three committed fixtures (en, tr, mixed): first audio at most 600 ms after each
    // utterance's end, one turn per utterance, the conversation is the transcripts and answers.
    for factory in [
        EngineFactory::new(),
        EngineFactory::new().with_smart_turn(0.9),
    ] {
        suite::fixtures::all(&factory, Profile::Budget)
            .await
            .unwrap();
        clear_hook_stayed_wired(&factory);
    }
}

#[tokio::test(start_paused = true)]
async fn the_played_text_suite_passes_on_wire_time() {
    // The suite barges in 0.4 s after the first utterance: with the 450 ms silence of the other
    // tests the caller would "resume" the first turn (docs/TESTING.md, divergences).
    let factory = EngineFactory::new().with_fixed_silence(300 * MS);
    suite::played_text::all(&factory, Profile::Budget)
        .await
        .unwrap();
    clear_hook_stayed_wired(&factory);
}

#[tokio::test(start_paused = true)]
async fn every_scenario_but_s06_passes_with_the_default_retry_configuration() {
    // The factory sets `llm.retries = 0` for s06 (its failing response is followed by the NEXT
    // turn's script, which a retry would consume): every other scenario runs with the engine
    // default (one retry), so the accommodation hides nothing else.
    use suite::scenarios as s;
    let factory = EngineFactory::new().with_default_retries();
    let profile = Profile::Budget;
    macro_rules! each {
        ($($name:ident),+ $(,)?) => {$(
            s::$name(&factory, profile).await.unwrap_or_else(|e| {
                panic!("{}: {e}", stringify!($name))
            });
        )+};
    }
    each!(
        s01_happy_path_with_realistic_latency,
        s02_barge_in,
        s03_double_utterance_is_one_turn,
        s04_long_silence_stays_quiet,
        s05_provider_stall_delays_but_completes,
        s07_stt_disconnect_then_reconnect,
        s08_tool_call_with_text,
        s09_back_to_back_turns,
        s10_interrupt_during_tts_first_byte_wait,
        s11_tts_disconnect_mid_answer_then_recovery,
    );
}

/// One run of `scenario` on `factory` after `setup` scripted the mocks.
async fn run_one(
    factory: &EngineFactory,
    setup: impl FnOnce(&SimSession),
    scenario: &Scenario,
    linger: Duration,
) -> brisvox_sim::session::SessionRun {
    let session = SimSession::new(Arc::new(SimClock::new()));
    setup(&session);
    session.run_with(factory, scenario, linger).await.unwrap()
}

#[tokio::test(start_paused = true)]
async fn with_no_lead_margin_the_context_is_exactly_the_heard_prefix_after_every_barge_in() {
    // The Budget profile allows a cut turn to lack one heard word, for the 20 ms lead margin
    // (docs/TESTING.md, divergence 3). With the margin at 0 nothing excuses a missing word: a
    // regression that drops the last heard word at every barge-in (the margin applied twice, a
    // `<` for a `<=` at a word start) fails here even though the Budget suite would pass it.
    let story = || {
        LlmScript::text([
            "Once upon a time there was a voice agent that talked and talked ",
            "and never stopped talking until somebody interrupted it.",
        ])
    };
    let s02 = Scenario::new()
        .silence(100 * MS)
        .speak("tell me a long story", 900 * MS)
        .silence(1_800 * MS)
        .speak("okay stop", 600 * MS)
        .with_tail(500 * MS);
    let basic = Scenario::new()
        .silence(100 * MS)
        .speak("tell me everything", 800 * MS)
        .silence(1_500 * MS)
        .speak("stop please", 600 * MS)
        .with_tail(500 * MS);
    let s10 = Scenario::new()
        .silence(100 * MS)
        .speak("question", 500 * MS)
        .silence(500 * MS)
        .speak("wait no", 500 * MS)
        .with_tail(500 * MS);
    let no_lead = |factory: EngineFactory| factory.with_config(|c| c.lead_margin = Duration::ZERO);
    for factory in [
        no_lead(EngineFactory::new()),
        no_lead(EngineFactory::new().with_guards_off()),
        no_lead(EngineFactory::new().with_smart_turn(0.9)),
    ] {
        let mut runs = Vec::new();
        for scenario in [&s02, &basic] {
            let setup = |s: &SimSession| {
                s.llm.push(story());
                s.llm.push(LlmScript::text(["Sure, stopping."]));
            };
            runs.push(run_one(&factory, setup, scenario, 2_000 * MS).await);
        }
        let setup = |s: &SimSession| {
            let _ = s.tts.clone().with_latency(
                LatencyProfile::ZERO,
                LatencyProfile::Fixed(1_000 * MS),
                LatencyProfile::ZERO,
                0,
            );
            s.llm.push(LlmScript::text(["This will never be heard."]));
            s.llm.push(LlmScript::text(["Okay."]));
        };
        runs.push(run_one(&factory, setup, &s10, 3_000 * MS).await);
        for (name, run) in ["s02", "basic barge-in", "s10"].iter().zip(&runs) {
            assert_eq!(run.outcome, Ok(()), "{name}");
            let interrupts = run.output.iter().filter(|r| r.kind == "interrupt").count();
            assert_eq!(interrupts, 1, "{name}: one barge-in");
            run.check_no_stale_audio_after_interrupt()
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            run.check_context_matches_played_text()
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        clear_hook_stayed_wired(&factory);
    }
}
