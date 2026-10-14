//! Whole simulated calls driven by real speech (FLEURS fixtures, see `fixtures/NOTICE`).

use std::time::Duration;

use brisvox_core::TurnEpoch;
use brisvox_providers::Role;

use super::{Profile, ensure, ensure_eq, nth, sim_clock, some};
use crate::error::Result;
use crate::factory::SessionFactory;
use crate::fixture::{Fixture, fixtures_dir};
use crate::mock_llm::LlmScript;
use crate::session::SimSession;

const MS: Duration = Duration::from_millis(1);

/// First audio of a turn must follow the end of the utterance by at most this (both profiles).
pub const FIRST_AUDIO_WITHIN: Duration = Duration::from_millis(600);

/// The committed real-speech fixtures (two utterances each): English, Turkish, one of each.
pub const FIXTURES: [&str; 3] = ["en_two_utterances", "tr_two_utterances", "en_tr_mixed"];

/// The scripted answer to an utterance in `lang` (`"tr"` gets a Turkish one).
fn answer(lang: &str) -> &'static str {
    if lang == "tr" {
        "Teşekkürler."
    } else {
        "Thanks."
    }
}

/// [`a_real_speech_call_is_answered_after_each_utterance_in`] on `en_tr_mixed` (one English and
/// one Turkish utterance).
///
/// # Errors
/// As [`a_real_speech_call_is_answered_after_each_utterance_in`].
pub async fn a_real_speech_call_is_answered_after_each_utterance<F: SessionFactory>(
    factory: &F,
    profile: Profile,
) -> Result<()> {
    a_real_speech_call_is_answered_after_each_utterance_in(factory, profile, "en_tr_mixed").await
}

/// Runs [`a_real_speech_call_is_answered_after_each_utterance_in`] on every fixture of
/// [`FIXTURES`].
///
/// # Errors
/// The first failure, prefixed with the fixture's name.
pub async fn all<F: SessionFactory>(factory: &F, profile: Profile) -> Result<()> {
    for name in FIXTURES {
        a_real_speech_call_is_answered_after_each_utterance_in(factory, profile, name)
            .await
            .map_err(|e| super::assertion(format!("{name}: {e}")))?;
    }
    Ok(())
}

/// A real-speech call on the fixture `name` (two utterances) is answered after each utterance:
/// one turn per utterance, no barge-in, each turn's own first audio follows its own utterance's
/// end within 600 ms (both profiles), and the conversation is the transcripts with the scripted
/// answers (English "Thanks.", Turkish "Teşekkürler.").
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check; [`crate::Error::Fixture`] if the
/// fixture cannot be loaded.
pub async fn a_real_speech_call_is_answered_after_each_utterance_in<F: SessionFactory>(
    factory: &F,
    profile: Profile,
    name: &str,
) -> Result<()> {
    let fixture = Fixture::load(&fixtures_dir(), name)?;
    let session = SimSession::new(sim_clock());
    for segment in &fixture.segments {
        session.llm.push(LlmScript::text([answer(&segment.lang)]));
    }
    let run = session.run_with(factory, &fixture, 2_000 * MS).await?;
    ensure_eq!(run.outcome, Ok(()), "pipeline outcome");
    ensure_eq!(run.turns.len(), 2, "one turn per utterance");
    ensure!(
        run.output.iter().all(|r| r.kind != "interrupt"),
        "the bot finished before the next utterance: no barge-in"
    );
    run.check_no_stale_audio_after_interrupt()?;
    profile.context_holds_played(&run)?;
    // Each turn's own first audio follows its own utterance's end (turn 1's audio must not
    // count for turn 2).
    for (n, (_, end)) in fixture.speech_segments().into_iter().enumerate() {
        let epoch = u32::try_from(n).unwrap_or(u32::MAX);
        let first = some(
            run.output
                .iter()
                .find(|r| r.kind == "audio" && r.epoch == Some(epoch))
                .map(|r| r.at),
            "audio of the turn",
        )?;
        ensure!(
            first >= end,
            "turn {n}: audio at {first:?} before speech end {end:?}"
        );
        ensure!(
            first - end <= FIRST_AUDIO_WITHIN,
            "turn {n}: audio {:?} after speech end",
            first - end
        );
        ensure!(
            run.audio_of(TurnEpoch::new(epoch)) > Duration::ZERO,
            "turn {n} has no audio"
        );
    }
    let history: Vec<(Role, String)> = run
        .probes
        .history()
        .iter()
        .map(|m| (m.role, m.content.to_string()))
        .collect();
    let (first, second) = (
        nth(&fixture.segments, 0, "segment")?,
        nth(&fixture.segments, 1, "segment")?,
    );
    ensure_eq!(
        history,
        [
            (Role::User, first.text.clone()),
            (Role::Assistant, answer(&first.lang).to_owned()),
            (Role::User, second.text.clone()),
            (Role::Assistant, answer(&second.lang).to_owned()),
        ],
        "conversation"
    );
    Ok(())
}
