//! The smallest whole calls: one question and one answer with the timings spelled out, and the
//! session tests (barge-in, provider faults, a pause inside a turn) that follow it. Each is a
//! function generic over the factory, so the production pipeline runs the same bodies.

use std::time::Duration;

use brisvox_core::{TurnEpoch, TurnEvent};
use brisvox_providers::Role;

use super::{Profile, assertion, ensure, ensure_eq, nth, sim_clock, some};
use crate::error::Result;
use crate::factory::SessionFactory;
use crate::latency::LatencyProfile;
use crate::mock_common::StreamFault;
use crate::mock_llm::{LlmFault, LlmScript};
use crate::scenario::Scenario;
use crate::session::{SessionRun, SimSession};

const MS: Duration = Duration::from_millis(1);

/// One full turn end to end: "what time is it" answered with "It is noon.".
///
/// Exact (reference): 11 characters at 60 ms of synthetic speech = 660 ms of audio in turn 0;
/// the caller stops at 1.2 s, the VAD stop fires 280 ms later (the 15th silent frame, 300 ms
/// with its own length) and zero-latency mocks answer at once: first audio at exactly 1480 ms,
/// end-of-turn delay and response latency 280 ms. Budget: the audio length is still exact
/// (it is the mock's), the rest within the ranges of the [suite table](super).
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn one_full_turn<F: SessionFactory>(factory: &F, profile: Profile) -> Result<()> {
    let session = SimSession::new(sim_clock());
    session.llm.push(LlmScript::text(["It is", " noon."]));
    let scenario = Scenario::new()
        .silence(200 * MS)
        .speak("what time is it", 1_000 * MS)
        .with_tail(500 * MS);
    let run = session.run_with(factory, &scenario, 3_000 * MS).await?;
    ensure_eq!(run.outcome, Ok(()), "pipeline outcome");

    ensure_eq!(
        run.audio_of(TurnEpoch::ZERO),
        660 * MS,
        "bot audio of turn 0"
    );
    let first_audio = some(
        run.output.iter().find(|r| r.kind == "audio").map(|r| r.at),
        "bot audio",
    )?;
    profile.duration(
        "first bot audio",
        first_audio,
        1_480 * MS,
        1_480 * MS..=1_700 * MS,
    )?;

    let turn = nth(&run.turns, 0, "turn")?;
    let m = turn.metrics();
    let offset = |event: TurnEvent| -> Result<Duration> {
        let at = some(turn.at(event), "turn event")?;
        Ok(at.saturating_duration_since(run.origin))
    };
    profile.duration(
        "user speech start",
        offset(TurnEvent::UserSpeechStart)?,
        200 * MS,
        180 * MS..=220 * MS,
    )?;
    profile.duration(
        "user speech end",
        offset(TurnEvent::UserSpeechEnd)?,
        1_200 * MS,
        1_180 * MS..=1_220 * MS,
    )?;
    profile.duration(
        "end of turn delay",
        some(m.end_of_turn_delay, "end_of_turn_delay")?,
        280 * MS,
        280 * MS..=470 * MS,
    )?;
    profile.duration(
        "response latency",
        some(m.response_latency, "response_latency")?,
        280 * MS,
        280 * MS..=470 * MS,
    )?;
    ensure!(
        turn.at(TurnEvent::Interrupted).is_none(),
        "the turn was interrupted"
    );

    let history = run.probes.history();
    ensure_eq!(history.len(), 2, "conversation length");
    let (user, bot) = (nth(&history, 0, "message")?, nth(&history, 1, "message")?);
    ensure_eq!(
        (user.role, &*user.content),
        (Role::User, "what time is it"),
        "user message"
    );
    ensure_eq!(
        (bot.role, &*bot.content),
        (Role::Assistant, "It is noon."),
        "assistant message"
    );
    let words: Vec<String> = run.probes.words().iter().map(|w| w.1.to_string()).collect();
    ensure_eq!(words, ["It", "is", "noon."], "TTS word log");
    Ok(())
}

fn roles(run: &SessionRun) -> Vec<Role> {
    run.probes.history().iter().map(|m| m.role).collect()
}

fn interrupts(run: &SessionRun) -> usize {
    run.output.iter().filter(|r| r.kind == "interrupt").count()
}

/// A barge-in into a long answer silences the bot and starts a new turn.
///
/// Exact (reference): speech starts at 2.4 s and the VAD confirms it on the second speech frame,
/// so the interrupt reaches the transport at exactly 2420 ms; turn 0 played 1240 to 1280 ms
/// (31 or 32 chunks of 40 ms) and the context holds exactly the words that started playing.
/// Budget: the interrupt comes within 40 ms of the speech start, the silence at the output is
/// immediate (within 50 ms), and the context is a short word prefix of the answer that equals
/// the played text (up to one word fewer, see the suite table).
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn barge_in_silences_the_bot_and_starts_a_new_turn<F: SessionFactory>(
    factory: &F,
    profile: Profile,
) -> Result<()> {
    let session = SimSession::new(sim_clock());
    // A long answer (about 6 s of audio) for turn 0, a short one for turn 1.
    session.llm.push(LlmScript::text([
        "Here is a very long answer that keeps going for quite a while, ",
        "long enough for the caller to lose patience and interrupt it.",
    ]));
    session.llm.push(LlmScript::text(["Sure."]));
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("tell me everything", 800 * MS)
        .silence(1_500 * MS) // bot starts at 1.2 s; caller barges in at 2.4 s
        .speak("stop please", 600 * MS)
        .with_tail(500 * MS);
    let run = session.run_with(factory, &scenario, 2_000 * MS).await?;
    ensure_eq!(run.outcome, Ok(()), "pipeline outcome");
    run.check_no_stale_audio_after_interrupt()?;
    profile.context_holds_played(&run)?;

    let interrupt_at = some(
        run.output
            .iter()
            .find(|r| r.kind == "interrupt")
            .map(|r| r.at),
        "interrupt",
    )?;
    // Barge-in speech starts at 2.4 s; the VAD confirms it when the second speech frame (2.42 s)
    // arrives, and the interrupt reaches the transport at once (the pacer abandons its wait).
    profile.duration(
        "interrupt at the output",
        interrupt_at,
        2_420 * MS,
        2_400 * MS..=2_440 * MS,
    )?;
    ensure_eq!(interrupts(&run), 1, "interrupts");
    let played0 = run.audio_of(TurnEpoch::ZERO);
    let history = run.probes.history();
    let bot = nth(&history, 1, "message")?;
    ensure_eq!(bot.role, Role::Assistant, "role of the second message");
    ensure!(
        bot.content.starts_with("Here is"),
        "context {:?} does not start with the answer",
        bot.content
    );
    ensure!(
        bot.content.len() < 40,
        "context holds too much: {:?}",
        bot.content
    );
    if profile.is_exact() {
        // Turn 0 played 40 ms chunks from 1.18 s (stop at 0.9 s + 280 ms) until the barge-in at
        // 2.42 s: 31 chunks, or 32 if the chunk due exactly at 2.42 s went out first.
        ensure!(
            (1_240 * MS..=1_280 * MS).contains(&played0),
            "turn 0 played {played0:?}"
        );
        ensure_eq!(
            run.probes.played(TurnEpoch::ZERO),
            played0,
            "playout log of turn 0"
        );
        // The context keeps only the words that started playing (60 ms per char).
        let heard: Vec<String> = run
            .probes
            .words()
            .iter()
            .filter(|(e, _, start)| *e == TurnEpoch::ZERO && *start < played0)
            .map(|(_, w, _)| w.to_string())
            .collect();
        ensure_eq!(&*bot.content, heard.join(" "), "context text");
    }
    ensure_eq!(
        run.audio_of(TurnEpoch::new(1)),
        300 * MS,
        "\"Sure.\" = 5 chars"
    );

    ensure_eq!(run.turns.len(), 2, "turns");
    let interrupted = nth(&run.turns, 0, "turn")?;
    ensure!(
        interrupted.at(TurnEvent::Interrupted).is_some(),
        "turn 0 has no interrupt mark"
    );
    profile.duration(
        "time from barge-in to silence at the output",
        some(interrupted.metrics().time_to_silence, "time_to_silence")?,
        Duration::ZERO,
        Duration::ZERO..=50 * MS,
    )?;
    let start = some(
        nth(&run.turns, 1, "turn")?.at(TurnEvent::UserSpeechStart),
        "speech start of turn 1",
    )?;
    profile.duration(
        "barge-in speech start",
        start.saturating_duration_since(run.origin),
        2_400 * MS,
        2_380 * MS..=2_420 * MS,
    )?;
    Ok(())
}

/// An LLM error ends the turn quietly (no empty assistant message) and the next turn is not a
/// barge-in. All checks are structural and identical in both profiles.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn llm_error_ends_the_turn_quietly_and_the_next_turn_is_not_a_barge_in<
    F: SessionFactory,
>(
    factory: &F,
    _profile: Profile,
) -> Result<()> {
    let session = SimSession::new(sim_clock());
    session
        .llm
        .push(LlmScript::text(["never"]).with_fault(LlmFault::ErrorResponse { status: 500 }));
    session.llm.push(LlmScript::text(["Second try."]));
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("hello", 500 * MS)
        .silence(1_000 * MS)
        .speak("hello again", 500 * MS)
        .with_tail(500 * MS);
    let run = session.run_with(factory, &scenario, 2_000 * MS).await?;
    ensure_eq!(run.outcome, Ok(()), "pipeline outcome");
    ensure_eq!(interrupts(&run), 0, "interrupts");
    ensure_eq!(
        run.audio_of(TurnEpoch::ZERO),
        Duration::ZERO,
        "the failed turn got audio"
    );
    ensure_eq!(
        run.audio_of(TurnEpoch::new(1)),
        660 * MS,
        "\"Second try.\" = 11 chars"
    );
    ensure_eq!(
        roles(&run),
        [Role::User, Role::User, Role::Assistant],
        "no empty assistant message"
    );
    Ok(())
}

/// A barge-in before any audio drops the whole response: nothing of the old turn plays and it
/// leaves no assistant message. All checks are structural and identical in both profiles.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn barge_in_before_any_audio_drops_the_whole_response<F: SessionFactory>(
    factory: &F,
    _profile: Profile,
) -> Result<()> {
    let session = SimSession::new(sim_clock());
    // Mock clones share state: this sets the session's LLM latency.
    let _ = session.llm.clone().with_latency(
        LatencyProfile::Fixed(1_000 * MS),
        LatencyProfile::ZERO,
        0,
    );
    session.llm.push(LlmScript::text(["Too late."]));
    session.llm.push(LlmScript::text(["Okay."]));
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("question", 500 * MS)
        .silence(600 * MS) // turn ends at 0.88 s; the caller speaks again at 1.2 s
        .speak("never mind", 500 * MS)
        .with_tail(500 * MS);
    let run = session.run_with(factory, &scenario, 3_000 * MS).await?;
    ensure_eq!(run.outcome, Ok(()), "pipeline outcome");
    run.check_no_stale_audio_after_interrupt()?;
    ensure!(interrupts(&run) >= 1, "no interrupt reached the output");
    ensure_eq!(
        run.audio_of(TurnEpoch::ZERO),
        Duration::ZERO,
        "nothing of turn 0 may play"
    );
    ensure_eq!(run.audio_of(TurnEpoch::new(1)), 300 * MS, "audio of turn 1");
    // Turn 0's response was never heard: it leaves no assistant message.
    ensure_eq!(
        roles(&run),
        [Role::User, Role::User, Role::Assistant],
        "roles"
    );
    ensure!(
        nth(&run.turns, 0, "turn")?
            .at(TurnEvent::Interrupted)
            .is_some(),
        "turn 0 has no interrupt mark"
    );
    Ok(())
}

/// A pause inside the turn while the STT final is slow keeps one turn: the final arrives after
/// speech resumed and the two parts join. All checks are structural and identical in both
/// profiles.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn a_pause_inside_the_turn_with_slow_stt_keeps_one_turn<F: SessionFactory>(
    factory: &F,
    _profile: Profile,
) -> Result<()> {
    let session = SimSession::new(sim_clock());
    // Mock clones share state: this sets the session's STT latency.
    let _ = session.stt.clone().with_latency(
        LatencyProfile::ZERO,
        LatencyProfile::Fixed(100 * MS),
        LatencyProfile::Fixed(400 * MS),
        0,
    );
    session.llm.push(LlmScript::text(["Got it."]));
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("book a table", 600 * MS)
        .silence(320 * MS) // VAD stops; the final arrives 400 ms later, after speech resumed
        .speak("for two", 500 * MS)
        .with_tail(800 * MS);
    let run = session.run_with(factory, &scenario, 2_000 * MS).await?;
    ensure_eq!(run.outcome, Ok(()), "pipeline outcome");
    let history = run.probes.history();
    ensure_eq!(history.len(), 2, "conversation length: {history:?}");
    ensure_eq!(
        &*nth(&history, 0, "message")?.content,
        "book a table for two",
        "the joined user message"
    );
    ensure_eq!(run.turns.len(), 1, "turns");
    ensure_eq!(interrupts(&run), 0, "interrupts");
    Ok(())
}

/// An STT disconnect loses that utterance, ends the turn, and the next one reconnects and is
/// answered. All checks are structural and identical in both profiles.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check.
pub async fn stt_disconnect_ends_the_turn_and_the_next_one_reconnects<F: SessionFactory>(
    factory: &F,
    _profile: Profile,
) -> Result<()> {
    let session = SimSession::new(sim_clock());
    session
        .stt
        // A one-word utterance has no interims: its final is the first event, and the
        // connection drops exactly when it is due (after the Finalize consumed the utterance).
        .push_session_faults(vec![StreamFault::Disconnect { after_events: 0 }]);
    session.llm.push(LlmScript::text(["Hi."]));
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("lost", 500 * MS)
        .silence(1_500 * MS)
        .speak("hello", 500 * MS)
        .with_tail(500 * MS);
    let run = session.run_with(factory, &scenario, 2_000 * MS).await?;
    ensure_eq!(run.outcome, Ok(()), "pipeline outcome");
    let history = run.probes.history();
    // The first utterance was lost with the session; the second one was answered.
    ensure_eq!(history.len(), 2, "conversation length: {history:?}");
    ensure_eq!(
        &*nth(&history, 0, "message")?.content,
        "hello",
        "the surviving user message"
    );
    ensure_eq!(run.audio_of(TurnEpoch::new(1)), 180 * MS, "audio of turn 1");
    Ok(())
}

/// Every test of this module, each on a fresh call.
///
/// # Errors
/// The first failed check, naming the test.
pub async fn all<F: SessionFactory>(factory: &F, profile: Profile) -> Result<()> {
    macro_rules! run {
        ($($name:ident),+ $(,)?) => {$(
            $name(factory, profile).await.map_err(|e| match e {
                crate::Error::Assertion(m) => assertion(format!("{}: {m}", stringify!($name))),
                other => other,
            })?;
        )+};
    }
    run!(
        one_full_turn,
        barge_in_silences_the_bot_and_starts_a_new_turn,
        llm_error_ends_the_turn_quietly_and_the_next_turn_is_not_a_barge_in,
        barge_in_before_any_audio_drops_the_whole_response,
        a_pause_inside_the_turn_with_slow_stt_keeps_one_turn,
        stt_disconnect_ends_the_turn_and_the_next_one_reconnects,
    );
    Ok(())
}
