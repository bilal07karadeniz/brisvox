//! Hybrid gate 4 (`docs/PIPELINE.md` section 9): the engine's input chain (pump,
//! `AudioIn`, VAD worker), `SttStage`, `TurnDriver` and `LlmRunner`, then the engine's OUTPUT chain
//! (`SegmenterStage`, `TtsStage`, `TransportOut`) writing to a real `TransportSession` over the
//! loopback transport. The reference session harness (`brisvox-sim`) plays the scripted caller
//! and judges the call; the shared suite runs under the Budget profile on a paused clock.
//!
//! What is real here: the caller's audio goes through the loopback client, the session and the
//! pump; the bot's audio goes through the TTS stage's 24 kHz to 16 kHz conversion, the transport
//! stage and the session's pacer to the loopback client; end marks are echoed by the client and
//! come back through the pump as `BotAudioEnded`; barge-ins clear the transport on the fast path.
//! What the runner cuts interrupted responses to is read from the `PlayoutBook` the stages write
//! (the TTS stage's sent text, the transport stage's words and audio, the pump's positions and
//! mark echoes) through the production `PlayoutTruncation`. The wire records and the
//! playout log of the probes come from what the loopback client received, with its arrival
//! times. The factory is `common/output_rig.rs`.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;
#[path = "common/output_rig.rs"]
mod output_rig;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use brisvox_agent::context::Message;
use brisvox_core::{SimClock, TurnEpoch, TurnEvent};
use brisvox_engine::InterruptionConfig;
use brisvox_sim::leak::TaskCounter;
use brisvox_sim::mock_llm::LlmScript;
use brisvox_sim::output::WireKind;
use brisvox_sim::scenario::Scenario;
use brisvox_sim::session::SimSession;
use brisvox_sim::suite::{self, Profile};
use common::MS;
use output_rig::{HybridOutput, Policy};

fn fixed_recommended() -> HybridOutput {
    HybridOutput::new(Policy::Fixed, InterruptionConfig::RECOMMENDED)
}

fn fixed_open() -> HybridOutput {
    HybridOutput::new(Policy::Fixed, InterruptionConfig::OFF)
}

fn smart_recommended() -> HybridOutput {
    HybridOutput::new(Policy::Smart(0.9), InterruptionConfig::RECOMMENDED)
}

fn smart_open() -> HybridOutput {
    HybridOutput::new(Policy::Smart(0.9), InterruptionConfig::OFF)
}

#[tokio::test(start_paused = true)]
async fn all_scenarios_pass_with_fixed_silence_and_the_recommended_guards() {
    suite::scenarios::all(&fixed_recommended(), Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn all_scenarios_pass_with_fixed_silence_and_every_guard_off() {
    suite::scenarios::all(&fixed_open(), Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn all_scenarios_pass_with_the_adaptive_policy_and_a_fixed_mock_model() {
    suite::scenarios::all(&smart_recommended(), Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn every_scenario_but_s06_passes_with_the_default_retry_configuration() {
    // See hybrid_llm.rs: s06 scripts a failing response followed by the NEXT turn's answer, so
    // the other scenarios are the ones that run with the default retry.
    use brisvox_sim::suite::scenarios as s;
    let factory = fixed_recommended().with_default_retries();
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

#[tokio::test(start_paused = true)]
async fn all_scenarios_pass_with_a_registered_tool() {
    suite::scenarios::all(&fixed_recommended().with_tools(), Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_basic_suite_passes_with_fixed_silence() {
    suite::basic::all(&fixed_open(), Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_basic_suite_passes_with_the_adaptive_policy() {
    suite::basic::all(&smart_open(), Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn real_speech_is_answered_after_each_utterance() {
    suite::fixtures::a_real_speech_call_is_answered_after_each_utterance(
        &fixed_recommended(),
        Profile::Budget,
    )
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_tool_turn_with_a_second_script_speaks_both_rounds_as_two_utterances() {
    let ms = |n: u64| Duration::from_millis(n);
    let scenario = Scenario::new()
        .silence(ms(100))
        .speak("what time is it in Tokyo", ms(900))
        .with_tail(ms(500));
    let factory = fixed_recommended().with_tools();
    let session = SimSession::new(Arc::new(SimClock::new()));
    session
        .llm
        .push(LlmScript::text(["Let me check."]).with_tool_call(
            "call_1",
            "time_in",
            r#"{"city":"Tokyo"}"#,
        ));
    session
        .llm
        .push(LlmScript::text(["It is nine in the morning."]));
    let run = session
        .run_with(&factory, &scenario, ms(2_500))
        .await
        .unwrap();
    assert_eq!(run.outcome, Ok(()));
    let context = run.probes.context.lock().unwrap().clone();
    context.validate().unwrap();
    let kinds: Vec<&str> = context
        .messages()
        .iter()
        .map(|m| match m {
            Message::System { .. } => "system",
            Message::User { .. } => "user",
            Message::Assistant { tool_calls, .. } if tool_calls.is_empty() => "assistant",
            Message::Assistant { .. } => "assistant+calls",
            Message::Tool { .. } => "tool",
        })
        .collect();
    assert_eq!(kinds, ["user", "assistant+calls", "tool", "assistant"]);
    assert_eq!(context.messages()[3].text(), "It is nine in the morning.");
    // Both rounds were spoken, in order, each its own utterance of the same epoch.
    for call in factory.handles() {
        let tts = call.tts.snapshot();
        assert_eq!(
            tts.contexts, 2,
            "one utterance per text-bearing round: {tts:?}"
        );
        assert_eq!(tts.utterance_starts, 2, "{tts:?}");
        assert_eq!(tts.tts_ended, 1, "one end for the whole response: {tts:?}");
        let out = call.out.snapshot();
        assert_eq!(out.marks_sent, 1, "one end mark: {out:?}");
        assert_eq!(out.mark_timeouts, 0, "{out:?}");
        let book = &call.input.book;
        let first = book
            .snapshot(brisvox_engine::RoundKey::new(TurnEpoch::ZERO, 0))
            .unwrap();
        let second = book
            .snapshot(brisvox_engine::RoundKey::new(TurnEpoch::ZERO, 1))
            .unwrap();
        assert_eq!(first.played.sent(), "Let me check.");
        assert_eq!(second.played.sent(), "It is nine in the morning.");
        assert!(first.total_audio > Duration::ZERO && second.total_audio > Duration::ZERO);
        assert!(
            second.marked_played,
            "the end mark belongs to the last utterance"
        );
    }
    // The client heard 'Let me check.' and then the answer: the audio of one epoch, in order.
    let heard: Duration = run
        .wire
        .iter()
        .filter(|r| r.kind == WireKind::Audio)
        .filter_map(|r| r.audio)
        .sum();
    let expected = Duration::from_millis(
        60 * ("Let me check.".len() + "It is nine in the morning.".len()) as u64,
    );
    assert!(
        heard.abs_diff(expected) <= 40 * MS,
        "heard {heard:?}, expected about {expected:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_output_chain_counts_what_a_barge_in_scenario_does() {
    let factory = fixed_open();
    suite::scenarios::s02_barge_in(&factory, Profile::Budget)
        .await
        .unwrap();
    let handles = factory.handles();
    assert!(!handles.is_empty());
    for call in &handles {
        let driver = call.driver.snapshot();
        assert_eq!(driver.interrupts, 1, "{driver:?}");
        let stt = call.stt.snapshot();
        assert_eq!(stt.finals_synthetic, 0, "{stt:?}");
        let segmenter = call.segmenter.snapshot();
        assert!(segmenter.chunks >= 2, "{segmenter:?}");
        assert_eq!(
            segmenter.resets, 1,
            "the barge-in reset the segmenter: {segmenter:?}"
        );
        let tts = call.tts.snapshot();
        assert!(
            tts.cancels >= 1,
            "the cut response's context was cancelled: {tts:?}"
        );
        assert_eq!(tts.session_deaths, 0, "{tts:?}");
        assert_eq!(tts.resampler_failures, 0, "{tts:?}");
        assert_eq!(tts.text_dropped, 0, "{tts:?}");
        assert_eq!(
            tts.resampler_builds, 1,
            "24 kHz to 16 kHz, built once: {tts:?}"
        );
        let out = call.out.snapshot();
        assert_eq!(out.interrupts, 1, "{out:?}");
        assert_eq!(out.send_failures, 0, "{out:?}");
        assert_eq!(out.clear_failures, 0, "{out:?}");
        // The first turn (epoch 0) needs no clear; the barge-in's clear to epoch 1 (the stage's is
        // idempotent with the driver's) also announces the second turn's audio.
        assert!(out.clears >= 1, "{out:?}");
        let book = call.input.book.stats();
        assert_eq!(book.conflicting_registrations, 0, "{book:?}");
        assert_eq!(book.unknown_updates, 0, "{book:?}");
        let llm = call.llm.snapshot();
        assert_eq!(llm.validate_failures, 0, "{llm:?}");
        assert_eq!(llm.cut_errors, 0, "{llm:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn the_barge_in_cut_leaves_nothing_of_the_old_epoch_on_the_wire() {
    let ms = |n: u64| Duration::from_millis(n);
    let scenario = Scenario::new()
        .silence(ms(100))
        .speak("tell me a story", ms(900))
        .silence(ms(1_800))
        .speak("okay stop", ms(600))
        .with_tail(ms(500));
    let session = SimSession::new(Arc::new(SimClock::new()));
    session.llm.push(LlmScript::text([
        "A very long story that goes on and on and on, and on.",
    ]));
    session.llm.push(LlmScript::text(["Sure."]));
    let run = session
        .run_with(&fixed_open(), &scenario, ms(2_500))
        .await
        .unwrap();
    assert_eq!(run.outcome, Ok(()));
    let clear = run
        .wire
        .iter()
        .rposition(|r| r.kind == WireKind::Clear && r.epoch.is_some_and(|e| e >= 1))
        .expect("the barge-in cleared the client");
    let newest = run.wire[clear].epoch.unwrap();
    assert!(
        run.wire[clear..]
            .iter()
            .filter(|r| r.kind == WireKind::Audio)
            .all(|r| r.epoch.is_some_and(|e| e >= newest)),
        "stale audio after the clear: {:?}",
        &run.wire[clear..]
    );
    // The first epoch was cut: less audio reached the client than the whole story needs.
    let story: Duration = run
        .wire
        .iter()
        .filter(|r| r.kind == WireKind::Audio && r.epoch.is_some_and(|e| e < 1))
        .filter_map(|r| r.audio)
        .sum();
    assert!(story < ms(60 * 52), "{story:?}");
    // And both the cut context and the next answer are what the caller heard.
    let history = run.probes.history();
    let first = &history[1].content;
    assert!(
        !first.is_empty()
            && "A very long story that goes on and on and on, and on.".starts_with(&**first),
        "{first:?}"
    );
    assert_eq!(&*history[3].content, "Sure.");
    // The timeline has the real handoff and the stop.
    assert!(
        run.turns
            .iter()
            .any(|t| t.at(TurnEvent::AudioOutStopped).is_some())
    );
    assert!(
        run.turns
            .iter()
            .all(|t| t.at(TurnEvent::FirstAudioOut).is_some()
                || t.at(TurnEvent::LlmRequest).is_none())
    );
}

#[tokio::test(start_paused = true)]
async fn nothing_outlives_a_hybrid_call() {
    let clock = SimClock::new();
    let counter = TaskCounter::start_settled(&clock, 20 * MS).await.unwrap();
    suite::scenarios::s02_barge_in(&smart_recommended(), Profile::Budget)
        .await
        .unwrap();
    suite::scenarios::s08_tool_call_with_text(&smart_recommended().with_tools(), Profile::Budget)
        .await
        .unwrap();
    counter
        .wait_for_baseline(&clock, Duration::from_secs(1))
        .await
        .expect("the pump, a stage task, the TTS reader, a timer or a tool task leaked");
}

#[tokio::test(start_paused = true)]
async fn a_silent_turn_ends_for_the_driver_and_the_next_turn_still_speaks() {
    let ms = |n: u64| Duration::from_millis(n);
    let scenario = Scenario::new()
        .silence(ms(100))
        .speak("hello are you there", ms(900))
        .silence(ms(2_000))
        .speak("please say something now", ms(900))
        .with_tail(ms(500));
    let factory = fixed_recommended();
    let session = SimSession::new(Arc::new(SimClock::new()));
    // The first answer has no text at all; the second one speaks.
    session.llm.push(LlmScript::text(Vec::<String>::new()));
    session.llm.push(LlmScript::text(["Yes, I am here."]));
    let run = session
        .run_with(&factory, &scenario, ms(2_500))
        .await
        .unwrap();
    assert_eq!(run.outcome, Ok(()));
    for call in factory.handles() {
        let driver = call.driver.snapshot();
        assert_eq!(driver.turns_ended, 2, "{driver:?}");
        assert_eq!(
            driver.response_timeouts, 0,
            "no backstop was needed: {driver:?}"
        );
        let llm = call.llm.snapshot();
        assert_eq!(llm.silent_turns, 1, "{llm:?}");
        let tts = call.tts.snapshot();
        // The TTS stage ends every response itself, once, the silent one included.
        assert_eq!(tts.tts_ended, 2, "{tts:?}");
        assert_eq!(tts.contexts, 1, "only the second turn has text: {tts:?}");
        let out = call.out.snapshot();
        assert_eq!(
            out.direct_ends, 1,
            "the silent response ends without a mark: {out:?}"
        );
        assert_eq!(out.marks_sent, 1, "{out:?}");
        // What the driver gets: for the silent turn the runner's own `TtsEnded` AND the
        // transport stage's direct end (they travel different ways and are not deduplicated
        // between stages; the turn machine ignores `BotAudioEnded` while idle), for the spoken
        // turn the mark echo's. So three in total, not one per turn, and harmless.
        assert_eq!(
            call.ends_at_driver.load(Ordering::Relaxed),
            3,
            "{driver:?} {out:?}"
        );
    }
    let heard: Duration = run
        .wire
        .iter()
        .filter(|r| r.kind == WireKind::Audio)
        .filter_map(|r| r.audio)
        .sum();
    let expected = Duration::from_millis(60 * "Yes, I am here.".len() as u64);
    assert!(
        heard.abs_diff(expected) <= 40 * MS,
        "the second turn was spoken: heard {heard:?}, expected about {expected:?}"
    );
}
