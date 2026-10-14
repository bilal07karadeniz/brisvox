//! Simulator scenarios s01..s11: whole calls with realistic latencies, provider faults, barge-ins
//! and recovery, each run twice to prove determinism, all checked with the assertion API and an
//! independent oracle (the scripted LLM text).

use std::time::Duration;

use brisvox_core::{TurnEpoch, TurnEvent};
use brisvox_providers::Role;

use super::{Profile, ensure, ensure_eq, nth, sim_clock, some};
use crate::error::Result;
use crate::factory::SessionFactory;
use crate::latency::LatencyProfile;
use crate::mock_common::StreamFault;
use crate::mock_llm::{LlmFault, LlmScript};
use crate::scenario::Scenario;
use crate::session::{SessionRun, SimSession};

const MS: Duration = Duration::from_millis(1);

/// Realistic seeded latencies for every provider (clones share the mocks' state).
fn realistic(s: &SimSession) {
    realistic_seeded(s, 0);
}

fn realistic_seeded(s: &SimSession, seed: u64) {
    let _ = s.stt.clone().with_latency(
        LatencyProfile::Fixed(50 * MS),
        LatencyProfile::Fixed(100 * MS),
        LatencyProfile::LogNormal {
            median: 150 * MS,
            sigma: 0.3,
        },
        seed + 1,
    );
    let _ = s.llm.clone().with_latency(
        LatencyProfile::LogNormal {
            median: 350 * MS,
            sigma: 0.3,
        },
        LatencyProfile::Fixed(30 * MS),
        seed + 2,
    );
    let _ = s.tts.clone().with_latency(
        LatencyProfile::Fixed(50 * MS),
        LatencyProfile::LogNormal {
            median: 150 * MS,
            sigma: 0.3,
        },
        LatencyProfile::Fixed(20 * MS),
        seed + 3,
    );
}

async fn run_once<F: SessionFactory>(
    factory: &F,
    setup: &dyn Fn(&SimSession),
    scenario: &Scenario,
    linger: Duration,
) -> Result<SessionRun> {
    let session = SimSession::new(sim_clock());
    setup(&session);
    session.run_with(factory, scenario, linger).await
}

/// Runs the scenario twice on fresh sessions, requires identical observable behaviour, and
/// applies the invariants every scenario must keep.
async fn run_twice<F: SessionFactory>(
    factory: &F,
    profile: Profile,
    setup: &dyn Fn(&SimSession),
    scenario: &Scenario,
    linger: Duration,
) -> Result<SessionRun> {
    let first = run_once(factory, setup, scenario, linger).await?;
    let second = run_once(factory, setup, scenario, linger).await?;
    ensure!(
        first.fingerprint() == second.fingerprint(),
        "run is not deterministic"
    );
    ensure_eq!(first.outcome, Ok(()), "outcome of the first run");
    ensure_eq!(second.outcome, Ok(()), "outcome of the second run");
    first.check_no_stale_audio_after_interrupt()?;
    profile.context_holds_played(&first)?;
    Ok(first)
}

fn user_end(scenario: &Scenario, utterance: usize) -> Result<Duration> {
    Ok(nth(&scenario.speech_segments(), utterance, "speech segment")?.1)
}

fn roles(run: &SessionRun) -> Vec<Role> {
    run.probes.history().iter().map(|m| m.role).collect()
}

/// `Interrupt` frames that reached the output tap (not client clears: those also fire on epoch
/// bumps).
fn interrupts(run: &SessionRun) -> usize {
    run.output.iter().filter(|r| r.kind == "interrupt").count()
}

/// Independent oracle: the `n`-th assistant message is a word-prefix of what the LLM was
/// scripted to say (`complete`: all of it).
fn assert_said(run: &SessionRun, n: usize, script: &str, complete: bool) -> Result<()> {
    let said: Vec<String> = run
        .probes
        .history()
        .into_iter()
        .filter(|m| m.role == Role::Assistant)
        .map(|m| m.content.to_string())
        .collect();
    let text = nth(&said, n, "assistant message")?;
    let message: Vec<&str> = text.split_whitespace().collect();
    let scripted: Vec<&str> = script.split_whitespace().collect();
    ensure!(!message.is_empty(), "assistant message {n} is empty");
    ensure!(
        scripted.starts_with(&message),
        "assistant message {n} ({message:?}) is not a prefix of the script ({scripted:?})"
    );
    if complete {
        ensure_eq!(
            message.len(),
            scripted.len(),
            "turn {n} was complete but the text is cut"
        );
    }
    Ok(())
}

/// s01: a happy path with realistic latencies; the seed changes the call.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn s01_happy_path_with_realistic_latency<F: SessionFactory>(
    factory: &F,
    profile: Profile,
) -> Result<()> {
    let scenario = Scenario::new()
        .silence(300 * MS)
        .speak("what is the weather like today", 1_500 * MS)
        .with_tail(500 * MS);
    let setup = |s: &SimSession| {
        realistic(s);
        s.llm.push(LlmScript::text(["It is sunny ", "and warm."]));
    };
    let run = run_twice(factory, profile, &setup, &scenario, 4_000 * MS).await?;
    let end = user_end(&scenario, 0)?;
    // VAD hangover (280 ms) + STT final (~150) + LLM first token (~350) + TTS first byte (~150).
    run.check_bot_audio_within(end, 1_500 * MS)?;
    let first = some(
        run.first_bot_audio_after(end),
        "bot audio after the question",
    )?;
    profile.at_least(
        "first audio after the end of speech",
        first.saturating_sub(end),
        280 * MS + 300 * MS,
        280 * MS + 300 * MS,
    )?;
    let m = some(run.turns.first(), "turn 0")?.metrics();
    ensure!(
        m.llm_time_to_first_token.is_some() && m.tts_time_to_first_byte.is_some(),
        "provider timings are missing from the turn metrics: {m:?}"
    );
    let (response, end_of_turn) = (
        some(m.response_latency, "response_latency")?,
        some(m.end_of_turn_delay, "end_of_turn_delay")?,
    );
    ensure!(
        response > end_of_turn,
        "response latency {response:?} not above the end-of-turn delay {end_of_turn:?}"
    );
    ensure_eq!(
        run.played_text(TurnEpoch::ZERO),
        "It is sunny and warm.",
        "played text"
    );
    assert_said(&run, 0, "It is sunny and warm.", true)?;
    // The seed matters: another seed gives another (equally deterministic) call.
    let reseeded = |s: &SimSession| {
        realistic_seeded(s, 100);
        s.llm.push(LlmScript::text(["It is sunny ", "and warm."]));
    };
    let other = run_once(factory, &reseeded, &scenario, 4_000 * MS).await?;
    ensure!(
        run.fingerprint() != other.fingerprint(),
        "the seed had no effect"
    );
    Ok(())
}

/// s02: the caller barges in on a long story; the context keeps exactly what was heard.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn s02_barge_in<F: SessionFactory>(factory: &F, profile: Profile) -> Result<()> {
    let story = "Once upon a time there was a voice agent that talked and talked \
                 and never stopped talking until somebody interrupted it.";
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("tell me a long story", 900 * MS)
        .silence(1_800 * MS)
        .speak("okay stop", 600 * MS)
        .with_tail(500 * MS);
    let setup = |s: &SimSession| {
        s.llm.push(LlmScript::text([
            "Once upon a time there was a voice agent that talked and talked ",
            "and never stopped talking until somebody interrupted it.",
        ]));
        s.llm.push(LlmScript::text(["Sure, stopping."]));
    };
    let run = run_twice(factory, profile, &setup, &scenario, 2_000 * MS).await?;
    ensure_eq!(interrupts(&run), 1, "interrupts");
    let turn = some(run.turns.first(), "turn 0")?;
    ensure!(
        turn.at(TurnEvent::Interrupted).is_some(),
        "turn 0 was not marked interrupted"
    );
    profile.duration(
        "time to silence",
        some(turn.metrics().time_to_silence, "time_to_silence")?,
        Duration::ZERO,
        Duration::ZERO..=50 * MS,
    )?;
    // Turn 0 in the context is exactly what was heard: a strict prefix of the story.
    let heard = run.played_text(TurnEpoch::ZERO);
    ensure!(heard.starts_with("Once upon a time"), "heard {heard:?}");
    ensure!(
        heard.split_whitespace().count() < story.split_whitespace().count(),
        "the story was heard to the end: {heard:?}"
    );
    assert_said(&run, 0, story, false)?;
    assert_said(&run, 1, "Sure, stopping.", true)
}

/// s03: two utterances 340 ms apart are one turn.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn s03_double_utterance_is_one_turn<F: SessionFactory>(
    factory: &F,
    profile: Profile,
) -> Result<()> {
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("I want to book a table", 900 * MS)
        .silence(340 * MS)
        .speak("for four people", 700 * MS)
        .with_tail(500 * MS);
    let setup = |s: &SimSession| {
        let _ = s.stt.clone().with_latency(
            LatencyProfile::ZERO,
            LatencyProfile::Fixed(100 * MS),
            LatencyProfile::Fixed(450 * MS),
            0,
        );
        s.llm.push(LlmScript::text(["Booked."]));
    };
    let run = run_twice(factory, profile, &setup, &scenario, 2_000 * MS).await?;
    let history = run.probes.history();
    let first = nth(&history, 0, "message")?;
    ensure_eq!(
        &*first.content,
        "I want to book a table for four people",
        "the joined utterance"
    );
    ensure_eq!(run.turns.len(), 1, "turns");
    assert_said(&run, 0, "Booked.", true)
}

/// s04: ten seconds of silence after the answer: the bot stays quiet.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn s04_long_silence_stays_quiet<F: SessionFactory>(
    factory: &F,
    profile: Profile,
) -> Result<()> {
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("hello", 500 * MS)
        .silence(10_000 * MS);
    let setup = |s: &SimSession| s.llm.push(LlmScript::text(["Hi there."]));
    let run = run_twice(factory, profile, &setup, &scenario, 1_000 * MS).await?;
    ensure_eq!(run.turns.len(), 1, "turns");
    ensure_eq!(roles(&run), [Role::User, Role::Assistant], "roles");
    let last_audio = some(
        run.output.iter().rfind(|r| r.kind == "audio").map(|r| r.at),
        "bot audio",
    )?;
    ensure!(
        last_audio < 2_000 * MS,
        "the bot spoke during the silence: {last_audio:?}"
    );
    Ok(())
}

/// s05: a 2 s LLM stall delays the answer but it completes.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn s05_provider_stall_delays_but_completes<F: SessionFactory>(
    factory: &F,
    profile: Profile,
) -> Result<()> {
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("are you there", 700 * MS)
        .with_tail(500 * MS);
    let setup = |s: &SimSession| {
        s.llm
            .push(LlmScript::text(["Yes."]).with_fault(LlmFault::Stall {
                before_event: 0,
                duration: 2_000 * MS,
            }));
    };
    let run = run_twice(factory, profile, &setup, &scenario, 4_000 * MS).await?;
    let end = user_end(&scenario, 0)?;
    let first = some(
        run.first_bot_audio_after(end),
        "bot audio after the question",
    )?;
    ensure!(
        first.saturating_sub(end) >= 2_000 * MS,
        "the stall was not felt: {:?}",
        first.saturating_sub(end)
    );
    run.check_bot_audio_within(end, 2_500 * MS)?;
    let ttft = some(
        some(run.turns.first(), "turn 0")?
            .metrics()
            .llm_time_to_first_token,
        "llm_time_to_first_token",
    )?;
    profile.duration(
        "LLM time to first token",
        ttft,
        2_000 * MS,
        2_000 * MS..=2_020 * MS,
    )?;
    assert_said(&run, 0, "Yes.", true)
}

/// s06: an LLM error ends the turn quietly (no false barge-in) and the next turn recovers.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn s06_llm_error_then_recovery<F: SessionFactory>(
    factory: &F,
    profile: Profile,
) -> Result<()> {
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("first try", 600 * MS)
        .silence(1_500 * MS)
        .speak("second try", 600 * MS)
        .with_tail(500 * MS);
    let setup = |s: &SimSession| {
        s.llm
            .push(LlmScript::text(["x"]).with_fault(LlmFault::ErrorResponse { status: 503 }));
        s.llm.push(LlmScript::text(["Recovered."]));
    };
    let run = run_twice(factory, profile, &setup, &scenario, 2_000 * MS).await?;
    ensure_eq!(
        run.audio_of(TurnEpoch::ZERO),
        Duration::ZERO,
        "audio of the failed turn"
    );
    ensure_eq!(
        interrupts(&run),
        0,
        "a failed turn must not count as a barge-in"
    );
    ensure_eq!(
        roles(&run),
        [Role::User, Role::User, Role::Assistant],
        "roles"
    );
    assert_said(&run, 0, "Recovered.", true)
}

/// s07: the STT connection drops when a final is due; the next utterance reconnects.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn s07_stt_disconnect_then_reconnect<F: SessionFactory>(
    factory: &F,
    profile: Profile,
) -> Result<()> {
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("gone", 500 * MS)
        .silence(1_500 * MS)
        .speak("hello", 500 * MS)
        .with_tail(500 * MS);
    let setup = |s: &SimSession| {
        // A one-word utterance has no interims: the connection drops when its final is due.
        s.stt
            .push_session_faults(vec![StreamFault::Disconnect { after_events: 0 }]);
        s.llm.push(LlmScript::text(["Hi."]));
    };
    let run = run_twice(factory, profile, &setup, &scenario, 2_000 * MS).await?;
    let history = run.probes.history();
    ensure_eq!(history.len(), 2, "conversation length");
    ensure_eq!(
        &*nth(&history, 0, "message")?.content,
        "hello",
        "the first message"
    );
    ensure_eq!(
        run.audio_of(TurnEpoch::ZERO),
        Duration::ZERO,
        "the lost turn got an answer"
    );
    assert_said(&run, 0, "Hi.", true)
}

/// s08: the LLM answers with text and a tool call.
///
/// Both profiles: two identical runs, no barge-in, the conversation starts with the user and the
/// assistant's text, and the context equals what was played. The pipeline either ignores tools
/// (the reference; the only shape Exact accepts) or runs the tool loop (Budget
/// accepts both shapes, so the Budget ranges stay valid for the reference; a production factory
/// runs [`s08_with_a_registered_tool`], which requires the tool loop):
/// - Ignored: the arguments must not leak into the conversation and nothing follows the answer.
/// - Tool loop: the conversation is the user, the first round's text carrying the one
///   `time_in({"city":"Tokyo"})` call, the call's result, and the model's second round (the next
///   script); the caller heard both rounds' text. The result's content is the factory's tool's.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn s08_tool_call_with_text<F: SessionFactory>(
    factory: &F,
    profile: Profile,
) -> Result<()> {
    s08(factory, profile, None).await
}

/// s08 ([`s08_tool_call_with_text`]) for a pipeline that registers the `time_in` tool: the tool
/// loop shape is required, and the tool result in the conversation must be `result` (what the
/// factory's `time_in` answers; an unregistered tool's error result does not pass). Budget only:
/// the reference cannot run tools.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check, or when the tool did not run.
pub async fn s08_with_a_registered_tool<F: SessionFactory>(
    factory: &F,
    profile: Profile,
    result: &str,
) -> Result<()> {
    s08(factory, profile, Some(result)).await
}

/// s08; with `registered`, the tool loop must have run and produced that result.
async fn s08<F: SessionFactory>(
    factory: &F,
    profile: Profile,
    registered: Option<&str>,
) -> Result<()> {
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("what time is it in Tokyo", 900 * MS)
        .with_tail(500 * MS);
    let setup = |s: &SimSession| {
        s.llm
            .push(LlmScript::text(["Let me check."]).with_tool_call(
                "call_1",
                "time_in",
                r#"{"city":"Tokyo"}"#,
            ));
        // The second round of a pipeline that runs the tool (the reference never asks for it).
        s.llm.push(LlmScript::text([S08_SECOND_ROUND]));
    };
    // The structural checks are the same in both profiles: determinism, outcome, no stale audio,
    // context equals played text, no barge-in, the first message pair, the LLM mark.
    let run = run_twice(factory, profile, &setup, &scenario, 2_000 * MS).await?;
    ensure_eq!(interrupts(&run), 0, "interrupts");
    ensure!(
        roles(&run).starts_with(&[Role::User, Role::Assistant]),
        "the conversation does not start with the user and the assistant: {:?}",
        roles(&run)
    );
    assert_said(&run, 0, "Let me check.", true)?;
    ensure!(
        some(run.turns.first(), "turn 0")?
            .at(TurnEvent::LlmFirstToken)
            .is_some(),
        "no LLM first token mark"
    );
    let ran_tools = roles(&run).contains(&Role::Tool);
    ensure!(
        !(profile.is_exact() && ran_tools),
        "Exact is the reference's shape: no tool round"
    );
    ensure!(
        ran_tools || registered.is_none(),
        "the registered tool did not run: {:?}",
        roles(&run)
    );
    if ran_tools {
        s08_tool_loop(&run, registered)
    } else {
        // The pipeline ignores the tool call: the arguments must not leak into the conversation
        // and nothing follows the answer.
        ensure_eq!(
            run.played_text(TurnEpoch::ZERO),
            "Let me check.",
            "played text"
        );
        ensure_eq!(roles(&run), [Role::User, Role::Assistant], "roles");
        ensure!(
            run.probes
                .history()
                .iter()
                .all(|m| !m.content.contains("\"city\"")),
            "tool arguments leaked into the conversation"
        );
        Ok(())
    }
}

/// s08's tool loop shape, see [`s08_tool_call_with_text`].
fn s08_tool_loop(run: &SessionRun, registered: Option<&str>) -> Result<()> {
    ensure_eq!(
        roles(run),
        [Role::User, Role::Assistant, Role::Tool, Role::Assistant],
        "roles of a turn with one tool round"
    );
    let history = run.probes.history();
    let calling = nth(&history, 1, "the message that called the tool")?;
    let call = some(calling.tool_calls.first(), "the tool call")?;
    ensure_eq!(calling.tool_calls.len(), 1, "tool calls");
    ensure_eq!(&*call.name, "time_in", "tool name");
    let arguments: serde_json::Value = serde_json::from_str(&call.arguments)
        .map_err(|e| super::assertion(format!("tool arguments: {e}")))?;
    ensure_eq!(
        arguments,
        serde_json::json!({"city": "Tokyo"}),
        "tool arguments"
    );
    let result = nth(&history, 2, "the tool result")?;
    ensure!(
        result.tool_call_id.as_deref() == Some(&*call.id),
        "the message after the call {} is not its result: {result:?}",
        call.id
    );
    if let Some(expected) = registered {
        ensure_eq!(&*result.content, expected, "the registered tool's result");
    }
    assert_said(run, 1, S08_SECOND_ROUND, true)?;
    ensure_eq!(
        run.played_text(TurnEpoch::ZERO),
        format!("Let me check. {S08_SECOND_ROUND}"),
        "played text of both rounds"
    );
    Ok(())
}

/// s08's answer after the tool result (the model's second round; only a pipeline that runs
/// tools asks for it).
const S08_SECOND_ROUND: &str = "It is nine in Tokyo.";

/// s09: three back-to-back turns, none a barge-in, each answered within 1.5 s.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn s09_back_to_back_turns<F: SessionFactory>(
    factory: &F,
    profile: Profile,
) -> Result<()> {
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("one", 400 * MS)
        .silence(1_500 * MS)
        .speak("two", 400 * MS)
        .silence(1_500 * MS)
        .speak("three", 400 * MS)
        .with_tail(500 * MS);
    let answers = ["First.", "Second.", "Third."];
    let setup = |s: &SimSession| {
        realistic(s);
        for answer in answers {
            s.llm.push(LlmScript::text([answer]));
        }
    };
    let run = run_twice(factory, profile, &setup, &scenario, 2_000 * MS).await?;
    ensure_eq!(interrupts(&run), 0, "interrupts");
    ensure_eq!(run.turns.len(), 3, "turns");
    ensure_eq!(
        roles(&run),
        [
            Role::User,
            Role::Assistant,
            Role::User,
            Role::Assistant,
            Role::User,
            Role::Assistant
        ],
        "roles"
    );
    for (n, answer) in answers.into_iter().enumerate() {
        run.check_bot_audio_within(user_end(&scenario, n)?, 1_500 * MS)?;
        let epoch = TurnEpoch::new(u32::try_from(n).unwrap_or(u32::MAX));
        ensure!(
            run.audio_of(epoch) > Duration::ZERO,
            "turn {n} has no audio"
        );
        assert_said(&run, n, answer, true)?;
    }
    Ok(())
}

/// s10: the caller interrupts while the TTS is still waiting for its first byte.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn s10_interrupt_during_tts_first_byte_wait<F: SessionFactory>(
    factory: &F,
    profile: Profile,
) -> Result<()> {
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("question", 500 * MS)
        .silence(500 * MS) // turn ends at 0.88 s; TTS needs 1 s for its first byte
        .speak("wait no", 500 * MS)
        .with_tail(500 * MS);
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
    let run = run_twice(factory, profile, &setup, &scenario, 3_000 * MS).await?;
    ensure_eq!(interrupts(&run), 1, "interrupts");
    ensure_eq!(
        run.audio_of(TurnEpoch::ZERO),
        Duration::ZERO,
        "audio of the interrupted turn"
    );
    ensure_eq!(
        run.played_text(TurnEpoch::new(1)),
        "Okay.",
        "played text of turn 1"
    );
    ensure_eq!(
        roles(&run),
        [Role::User, Role::User, Role::Assistant],
        "roles"
    );
    assert_said(&run, 0, "Okay.", true)
}

/// s11: the TTS connection drops mid-answer; the broken turn still ends (no false barge-in) and
/// the next one is answered.
///
/// Exact: only "This" started playing within the first 120 ms. Budget: within one word of that.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn s11_tts_disconnect_mid_answer_then_recovery<F: SessionFactory>(
    factory: &F,
    profile: Profile,
) -> Result<()> {
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("talk to me", 600 * MS)
        .silence(3_000 * MS)
        .speak("hello again", 600 * MS)
        .with_tail(500 * MS);
    let full = "This answer is cut off by a broken connection.";
    let setup = |s: &SimSession| {
        // First session: the 9 word events (sent with the first audio) and 3 audio chunks
        // (120 ms), then the connection drops.
        s.tts
            .push_session_faults(vec![StreamFault::Disconnect { after_events: 12 }]);
        s.llm.push(LlmScript::text([full]));
        s.llm.push(LlmScript::text(["Back again."]));
    };
    let run = run_twice(factory, profile, &setup, &scenario, 2_000 * MS).await?;
    // Only "This" started playing within the first 120 ms (the next word starts at 300 ms).
    profile.heard_text(
        "played text of turn 0",
        &run.played_text(TurnEpoch::ZERO),
        "This",
        full,
    )?;
    ensure_eq!(
        interrupts(&run),
        0,
        "the broken turn must still end (no false barge-in)"
    );
    ensure_eq!(
        run.played_text(TurnEpoch::new(1)),
        "Back again.",
        "played text of turn 1"
    );
    assert_said(&run, 0, full, false)?;
    assert_said(&run, 1, "Back again.", true)
}

/// Runs s01 to s11 in order.
///
/// # Errors
/// The first failing scenario's [`crate::Error::Assertion`], prefixed with its name.
pub async fn all<F: SessionFactory>(factory: &F, profile: Profile) -> Result<()> {
    macro_rules! each {
        ($($name:ident),+ $(,)?) => {$(
            $name(factory, profile).await.map_err(|e| {
                super::assertion(format!("{}: {e}", stringify!($name)))
            })?;
        )+};
    }
    each!(
        s01_happy_path_with_realistic_latency,
        s02_barge_in,
        s03_double_utterance_is_one_turn,
        s04_long_silence_stays_quiet,
        s05_provider_stall_delays_but_completes,
        s06_llm_error_then_recovery,
        s07_stt_disconnect_then_reconnect,
        s08_tool_call_with_text,
        s09_back_to_back_turns,
        s10_interrupt_during_tts_first_byte_wait,
        s11_tts_disconnect_mid_answer_then_recovery,
    );
    Ok(())
}
