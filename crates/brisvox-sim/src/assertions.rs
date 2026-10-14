//! Assertions on a finished simulated call ([`SessionRun`]).
//!
//! Each `check_*` returns `Err(Error::Assertion(..))` with a readable explanation instead of
//! panicking (library code), so tests write `run.check_no_stale_audio_after_interrupt().unwrap()`.

use std::time::Duration;

use brisvox_core::{TurnEpoch, TurnMetrics};
use brisvox_providers::Role;

use crate::error::{Error, Result};
use crate::output::WireKind;
use crate::session::SessionRun;

fn fail(message: String) -> Result<()> {
    Err(Error::Assertion(message))
}

/// A comparable summary of a run: everything observable, times relative to the call start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFingerprint {
    /// (time, kind, epoch, audio) of every transport record.
    pub output: Vec<(Duration, &'static str, Option<u32>, Option<Duration>)>,
    /// (role, content) of the conversation.
    pub history: Vec<(Role, String)>,
    /// (epoch, word, offset) of every TTS word.
    pub words: Vec<(u32, String, Duration)>,
    /// Per-turn metrics.
    pub metrics: Vec<TurnMetrics>,
}

impl SessionRun {
    /// Assertions need every transport record.
    fn complete(&self) -> Result<()> {
        if self.dropped_output > 0 {
            return fail(format!(
                "{} transport records were dropped (recorder limit): raise it for this run",
                self.dropped_output
            ));
        }
        Ok(())
    }

    /// The first bot audio handed to the transport at or after `after` (call time).
    pub fn first_bot_audio_after(&self, after: Duration) -> Option<Duration> {
        self.output
            .iter()
            .find(|r| r.kind == "audio" && r.at >= after)
            .map(|r| r.at)
    }

    /// Bot audio of turn `epoch` that reached the transport.
    pub fn audio_of(&self, epoch: TurnEpoch) -> Duration {
        self.output
            .iter()
            .filter(|r| r.kind == "audio" && r.epoch == Some(epoch.get()))
            .filter_map(|r| r.audio)
            .sum()
    }

    /// The playback position of turn `epoch` as the caller heard it: at the first barge-in that
    /// ends the turn (the first interrupt to a later turn), or all of its audio if none did. The
    /// position is read from the playout log at that instant, so a chunk in flight counts only
    /// the part that had played.
    pub fn heard_position(&self, epoch: TurnEpoch) -> Duration {
        let barge_in = self
            .output
            .iter()
            .find(|r| r.kind == "interrupt" && r.epoch.is_some_and(|e| e > epoch.get()));
        match barge_in {
            Some(record) => self.probes.played_at(epoch, self.origin + record.at),
            None => self.audio_of(epoch),
        }
    }

    /// The words of turn `epoch` that started before the caller heard the cut (see
    /// [`Self::heard_position`]), joined by spaces (a word counts as heard once it started).
    pub fn played_text(&self, epoch: TurnEpoch) -> String {
        let played = self.heard_position(epoch);
        self.probes
            .words()
            .iter()
            .filter(|(e, _, start)| *e == epoch && *start < played)
            .map(|(_, word, _)| word.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The bot answered within `max` of `after` (typically the user's end of speech).
    ///
    /// # Errors
    /// [`Error::Assertion`] if no audio followed, or it came too late.
    pub fn check_bot_audio_within(&self, after: Duration, max: Duration) -> Result<()> {
        self.complete()?;
        match self.first_bot_audio_after(after) {
            None => fail(format!("no bot audio after {after:?}")),
            Some(at) if at.saturating_sub(after) > max => fail(format!(
                "bot audio at {at:?}, {:?} after {after:?} (max {max:?})",
                at.saturating_sub(after)
            )),
            Some(_) => Ok(()),
        }
    }

    /// No audio of an older turn reached the transport after an interrupt to a newer one, and
    /// (for runs with a wire, [`Self::wire`]) no client received audio of an older turn after a
    /// clear to a newer one.
    ///
    /// The output log orders audio by time and files audio received at the same instant as an
    /// `Interrupt` before it, so it only sees stale audio that arrives at least one tick later.
    /// The wire records keep the client's real arrival order: they catch stale audio that
    /// follows the clear in the same instant.
    ///
    /// # Errors
    /// [`Error::Assertion`] naming the first stale frame.
    pub fn check_no_stale_audio_after_interrupt(&self) -> Result<()> {
        self.complete()?;
        let mut current = 0;
        for record in &self.output {
            match (record.kind, record.epoch) {
                ("interrupt", Some(e)) => current = current.max(e),
                ("audio", Some(e)) if e < current => {
                    return fail(format!(
                        "turn-{e} audio at {:?} after the interrupt to turn {current}",
                        record.at
                    ));
                }
                _ => {}
            }
        }
        self.check_no_stale_wire_audio_after_clear()
    }

    /// The wire half of [`Self::check_no_stale_audio_after_interrupt`]: in the client's arrival
    /// order, no audio of turn `e` follows a clear to a turn after `e`. A no-op for runs without
    /// a wire.
    ///
    /// # Errors
    /// [`Error::Assertion`] naming the first stale frame.
    pub fn check_no_stale_wire_audio_after_clear(&self) -> Result<()> {
        let mut current = 0;
        for record in &self.wire {
            match (record.kind, record.epoch) {
                (WireKind::Clear, Some(e)) => current = current.max(e),
                (WireKind::Audio, Some(e)) if e < current => {
                    return fail(format!(
                        "turn-{e} audio received at {:?} after the clear to turn {current}",
                        record.at
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// What the bot said in each turn according to the conversation: the non-empty assistant
    /// texts between two user messages, joined by spaces (a turn with tool rounds has several
    /// assistant messages), turns without any text left out.
    pub fn said_per_turn(&self) -> Vec<String> {
        let mut turns = Vec::new();
        let mut current = String::new();
        for message in self.probes.history() {
            match message.role {
                Role::User => {
                    if !current.is_empty() {
                        turns.push(std::mem::take(&mut current));
                    }
                }
                Role::Assistant if !message.content.trim().is_empty() => {
                    if !current.is_empty() {
                        current.push(' ');
                    }
                    current.push_str(message.content.trim());
                }
                _ => {}
            }
        }
        if !current.is_empty() {
            turns.push(current);
        }
        turns
    }

    /// What the caller heard of each turn that played audio ([`Self::played_text`]), in order,
    /// turns where no word started left out.
    pub fn heard_per_turn(&self) -> Vec<String> {
        self.heard_turns()
            .into_iter()
            .map(|(_, text)| text)
            .collect()
    }

    /// [`Self::heard_per_turn`] with each turn's epoch.
    fn heard_turns(&self) -> Vec<(TurnEpoch, String)> {
        let mut epochs: Vec<u32> = self
            .output
            .iter()
            .filter(|r| r.kind == "audio")
            .filter_map(|r| r.epoch)
            .collect();
        epochs.dedup();
        epochs
            .into_iter()
            .map(|e| {
                let epoch = TurnEpoch::new(e);
                (epoch, self.played_text(epoch))
            })
            .filter(|(_, t)| !t.is_empty())
            .collect()
    }

    /// Whether a barge-in ended turn `epoch`: an `Interrupt` to the next turn reached the output.
    /// Every user turn moves the epoch on by one, and a barge-in into the bot's turn `e`
    /// interrupts to `e + 1`; a turn the caller let finish has no such record.
    pub fn cut_by_barge_in(&self, epoch: TurnEpoch) -> bool {
        let next = epoch.next().get();
        self.output
            .iter()
            .any(|r| r.kind == "interrupt" && r.epoch == Some(next))
    }

    /// What the conversation says the bot said in each turn ([`Self::said_per_turn`]) equals what
    /// the caller heard of it ([`Self::heard_per_turn`]), in order.
    ///
    /// # Errors
    /// [`Error::Assertion`] on the first mismatch.
    pub fn check_context_matches_played_text(&self) -> Result<()> {
        self.check_context_matches_played_text_within(0)
    }

    /// Like [`Self::check_context_matches_played_text`], with a slack of `words` for pipelines
    /// that estimate playback conservatively (a production pipeline counts a frame released to
    /// the wire as played only after a lead margin, so a word that started in the last few
    /// milliseconds before a barge-in may be left out). The slack applies only to turns a
    /// barge-in cut ([`Self::cut_by_barge_in`]): such a turn's text in the conversation must be a
    /// word prefix of what was heard of it and at most `words` words shorter, and the
    /// conversation may leave it out entirely only if it was heard for at most `words` words.
    /// Every other turn played to its end and must match exactly. The conversation never holds
    /// MORE than was heard. With `words == 0` this is an exact match.
    ///
    /// # Errors
    /// [`Error::Assertion`] naming both lists on the first mismatch.
    pub fn check_context_matches_played_text_within(&self, words: usize) -> Result<()> {
        self.complete()?;
        let turns = self.heard_turns();
        let heard: Vec<&str> = turns.iter().map(|(_, text)| text.as_str()).collect();
        let said = self.said_per_turn();
        let mismatch = || {
            fail(format!(
                "context does not match played text (slack {words} words on turns a barge-in \
                 cut):\n  context: {said:?}\n  played:  {heard:?}"
            ))
        };
        let mut said_turns = said.iter().peekable();
        for (epoch, turn) in &turns {
            let slack = if self.cut_by_barge_in(*epoch) {
                words
            } else {
                0
            };
            let played: Vec<&str> = turn.split_whitespace().collect();
            let kept = said_turns.peek().and_then(|text| {
                let kept: Vec<&str> = text.split_whitespace().collect();
                (played.starts_with(&kept) && played.len() - kept.len() <= slack).then_some(())
            });
            if kept.is_some() {
                said_turns.next();
            } else if played.len() > slack {
                return mismatch();
            }
        }
        if said_turns.next().is_some() {
            return mismatch();
        }
        Ok(())
    }

    /// For calls where the TTS lost part of a response (a disconnect leaves an untimed gap): every
    /// assistant message must be a word prefix of some turn's played text, in order. The context
    /// may hold less than was played, never more.
    ///
    /// # Errors
    /// [`Error::Assertion`] for the first message that is not a played prefix.
    pub fn check_context_is_played_prefix(&self) -> Result<()> {
        self.complete()?;
        let mut epochs: Vec<u32> = self
            .output
            .iter()
            .filter(|r| r.kind == "audio")
            .filter_map(|r| r.epoch)
            .collect();
        epochs.dedup();
        let heard: Vec<String> = epochs
            .into_iter()
            .map(|e| self.played_text(TurnEpoch::new(e)))
            .filter(|t| !t.is_empty())
            .collect();
        let said: Vec<String> = self
            .probes
            .history()
            .into_iter()
            .filter(|m| m.role == Role::Assistant)
            .map(|m| m.content.to_string())
            .collect();
        let mut turns = heard.iter();
        for message in &said {
            let want: Vec<&str> = message.split_whitespace().collect();
            let found = turns.by_ref().any(|turn| {
                let played: Vec<&str> = turn.split_whitespace().collect();
                played.starts_with(&want)
            });
            if !found {
                return fail(format!(
                    "context holds more than was played:\n  context: {said:?}\n  played:  {heard:?}"
                ));
            }
        }
        Ok(())
    }

    /// Everything observable, for determinism checks (two runs of a scenario must match).
    pub fn fingerprint(&self) -> RunFingerprint {
        RunFingerprint {
            output: self
                .output
                .iter()
                .map(|r| (r.at, r.kind, r.epoch, r.audio))
                .collect(),
            history: self
                .probes
                .history()
                .into_iter()
                .map(|m| (m.role, m.content.to_string()))
                .collect(),
            words: self
                .probes
                .words()
                .into_iter()
                .map(|(e, w, d)| (e.get(), w.to_string(), d))
                .collect(),
            metrics: self.turns.iter().map(|t| t.metrics()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::time::Instant;

    use super::*;
    use crate::reference::{Playout, SessionProbes};
    use crate::scenario::OutputRecord;

    const MS: Duration = Duration::from_millis(1);

    fn record(at: u64, kind: &'static str, epoch: Option<u32>, audio: u64) -> OutputRecord {
        OutputRecord {
            at: Duration::from_millis(at),
            kind,
            epoch,
            audio: (audio > 0).then(|| Duration::from_millis(audio)),
        }
    }

    /// A run: `output`, assistant/user `history`, TTS `words` (epoch, word, start ms).
    #[allow(clippy::disallowed_methods)] // a fixed origin for a hand-built run
    fn run(
        output: Vec<OutputRecord>,
        history: Vec<(Role, &str)>,
        words: Vec<(u32, &str, u64)>,
    ) -> SessionRun {
        let origin = Instant::now();
        // The playout log mirrors the audio records: each chunk starts playing when released.
        let mut playout: std::collections::HashMap<TurnEpoch, Playout> =
            std::collections::HashMap::new();
        for r in output.iter().filter(|r| r.kind == "audio") {
            if let (Some(epoch), Some(audio)) = (r.epoch, r.audio) {
                playout
                    .entry(TurnEpoch::new(epoch))
                    .or_default()
                    .push(origin + r.at, audio);
            }
        }
        SessionRun {
            origin,
            output,
            turns: Vec::new(),
            probes: SessionProbes {
                context: Arc::new(Mutex::new(context_of(history))),
                words: Arc::new(Mutex::new(
                    words
                        .into_iter()
                        .map(|(e, w, s)| {
                            (TurnEpoch::new(e), Arc::from(w), Duration::from_millis(s))
                        })
                        .collect(),
                )),
                played: Arc::new(Mutex::new(playout)),
                truncations: Arc::default(),
            },
            outcome: Ok(()),
            dropped_output: 0,
            wire: Vec::new(),
        }
    }

    /// The agent context holding the hand-built conversation.
    fn context_of(history: Vec<(Role, &str)>) -> brisvox_agent::context::Context {
        let mut context = brisvox_agent::context::Context::new();
        for (role, text) in history {
            context.push(match role {
                Role::System => brisvox_agent::context::Message::system(text),
                Role::User => brisvox_agent::context::Message::user(text),
                _ => brisvox_agent::context::Message::assistant(text),
            });
        }
        context
    }

    fn is_assertion(result: Result<()>) -> bool {
        matches!(result, Err(Error::Assertion(_)))
    }

    #[test]
    fn stale_audio_after_an_interrupt_is_caught() {
        let ok = run(
            vec![
                record(0, "audio", Some(0), 40),
                record(40, "interrupt", Some(1), 0),
                record(40, "audio", Some(1), 40),
            ],
            vec![],
            vec![],
        );
        assert!(ok.check_no_stale_audio_after_interrupt().is_ok());
        let stale = run(
            vec![
                record(0, "audio", Some(0), 40),
                record(40, "interrupt", Some(1), 0),
                record(40, "audio", Some(0), 40),
            ],
            vec![],
            vec![],
        );
        assert!(is_assertion(stale.check_no_stale_audio_after_interrupt()));
    }

    #[test]
    fn stale_audio_after_a_clear_in_the_same_instant_is_caught_on_the_wire() {
        use crate::output::WireRecord;
        let wire = |at: u64, kind: WireKind, epoch: u32| WireRecord {
            at: Duration::from_millis(at),
            kind,
            epoch: Some(epoch),
            audio: (kind == WireKind::Audio).then_some(20 * MS),
        };
        // The output log files audio received at the instant of the interrupt before it, so it
        // cannot see this; the wire keeps the arrival order: Clear{1} then Audio{0}, both at 40.
        let mut r = run(
            vec![
                record(0, "audio", Some(0), 40),
                record(40, "audio", Some(0), 20),
                record(40, "interrupt", Some(1), 0),
            ],
            vec![],
            vec![],
        );
        r.wire = vec![
            wire(0, WireKind::Audio, 0),
            wire(40, WireKind::Clear, 1),
            wire(40, WireKind::Audio, 0),
        ];
        let message = match r.check_no_stale_audio_after_interrupt() {
            Err(Error::Assertion(message)) => message,
            other => panic!("not caught: {other:?}"),
        };
        assert!(message.contains("after the clear"), "{message}");
        // Audio before the clear, and audio of the new turn after it, are fine.
        r.wire = vec![
            wire(0, WireKind::Audio, 0),
            wire(40, WireKind::Audio, 0),
            wire(40, WireKind::Clear, 1),
            wire(40, WireKind::Audio, 1),
        ];
        assert!(r.check_no_stale_audio_after_interrupt().is_ok());
    }

    #[test]
    fn the_word_slack_applies_only_to_turns_a_barge_in_ended() {
        // Turn 0 played to its end ("okay"); turn 1 ("hello there") was cut by a barge-in to 2.
        let output = vec![
            record(0, "audio", Some(0), 200),
            record(1_000, "audio", Some(1), 400),
            record(1_400, "interrupt", Some(2), 0),
        ];
        let words = vec![(0, "okay", 0), (1, "hello", 0), (1, "there", 300)];
        let with = |history| run(output.clone(), history, words.clone());
        // One word short of the cut turn: inside the slack.
        let short_cut = with(vec![
            (Role::User, "a"),
            (Role::Assistant, "okay"),
            (Role::User, "b"),
            (Role::Assistant, "hello"),
        ]);
        assert!(
            short_cut
                .check_context_matches_played_text_within(1)
                .is_ok()
        );
        assert!(is_assertion(short_cut.check_context_matches_played_text()));
        // The finished one-word turn left out: never excused.
        let dropped_finished = with(vec![
            (Role::User, "a"),
            (Role::User, "b"),
            (Role::Assistant, "hello there"),
        ]);
        assert!(is_assertion(
            dropped_finished.check_context_matches_played_text_within(1)
        ));
        assert!(short_cut.cut_by_barge_in(TurnEpoch::new(1)));
        assert!(!short_cut.cut_by_barge_in(TurnEpoch::ZERO));
    }

    #[test]
    fn late_or_missing_bot_audio_is_caught() {
        let r = run(vec![record(900, "audio", Some(0), 40)], vec![], vec![]);
        assert!(r.check_bot_audio_within(500 * MS, 400 * MS).is_ok());
        assert!(is_assertion(r.check_bot_audio_within(400 * MS, 400 * MS)));
        assert!(is_assertion(r.check_bot_audio_within(1_000 * MS, 400 * MS)));
        let silent = run(vec![], vec![], vec![]);
        assert!(is_assertion(
            silent.check_bot_audio_within(Duration::ZERO, MS)
        ));
    }

    #[test]
    fn context_must_equal_the_heard_words() {
        // Turn 0 played 400 ms: "hello" (0) and "there" (300) started, "friend" (600) did not.
        let output = vec![
            record(0, "audio", Some(0), 200),
            record(200, "audio", Some(0), 200),
            record(400, "interrupt", Some(1), 0),
        ];
        let words = vec![(0, "hello", 0), (0, "there", 300), (0, "friend", 600)];
        let good = run(
            output.clone(),
            vec![(Role::User, "hi"), (Role::Assistant, "hello there")],
            words.clone(),
        );
        assert!(good.check_context_matches_played_text().is_ok());
        for history in [
            vec![(Role::User, "hi"), (Role::Assistant, "hello there friend")], // unplayed word
            vec![(Role::User, "hi"), (Role::Assistant, "hello")],              // too short
            vec![(Role::User, "hi")],                                          // missing
            vec![
                (Role::User, "hi"),
                (Role::Assistant, "hello there"),
                (Role::Assistant, "extra"),
            ],
        ] {
            let bad = run(output.clone(), history, words.clone());
            assert!(is_assertion(bad.check_context_matches_played_text()));
        }
    }

    #[test]
    fn turns_without_audio_leave_no_message_and_repeated_records_count_once() {
        let r = run(
            vec![
                record(0, "audio", Some(1), 40),
                record(40, "audio", Some(1), 40),
                record(80, "audio", Some(2), 40),
            ],
            vec![
                (Role::User, "a"),
                (Role::User, "b"),
                (Role::Assistant, "one"),
                (Role::User, "c"),
                (Role::Assistant, "two"),
            ],
            vec![(1, "one", 0), (2, "two", 0)],
        );
        assert!(r.check_context_matches_played_text().is_ok());
    }

    #[test]
    fn dropped_records_fail_every_check() {
        let mut r = run(vec![record(0, "audio", Some(0), 40)], vec![], vec![]);
        r.dropped_output = 3;
        assert!(is_assertion(r.check_no_stale_audio_after_interrupt()));
        assert!(is_assertion(r.check_bot_audio_within(Duration::ZERO, MS)));
        assert!(is_assertion(r.check_context_matches_played_text()));
    }
}
