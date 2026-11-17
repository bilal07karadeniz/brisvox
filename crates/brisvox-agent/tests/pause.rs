//! Pause-then-decide in the turn machine: pause, resume, commit by words, by command word, by the
//! deadline and by overflow; epoch and counter invariants; hang-up during a pause; tightening.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::time::Duration;

use brisvox_agent::{
    EndReason, InterruptionPolicy, PauseOutcome, PausePolicy, StateKind, TurnAction, TurnInput,
    TurnMachine, TurnPolicy,
};
use brisvox_core::TurnEpoch;
use tokio::time::Instant;

#[allow(clippy::disallowed_methods)]
fn origin() -> Instant {
    Instant::now()
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn ep(value: u32) -> TurnEpoch {
    TurnEpoch::new(value)
}

/// When the bot's audio starts.
const BOT_AT: u64 = 1500;

struct Rig {
    m: TurnMachine,
    base: Instant,
    out: Vec<TurnAction>,
}

impl Rig {
    fn t(&self, n: u64) -> Instant {
        self.base + ms(n)
    }

    fn feed(&mut self, input: TurnInput) -> Vec<TurnAction> {
        self.out.clear();
        self.m.handle(input, &mut self.out);
        self.out.clone()
    }

    fn vad_start(&mut self, n: u64) -> Vec<TurnAction> {
        let at = self.t(n);
        self.feed(TurnInput::VadStart { at })
    }

    fn vad_stop(&mut self, n: u64) -> Vec<TurnAction> {
        let at = self.t(n);
        self.feed(TurnInput::VadStop { at })
    }

    fn interim(&mut self, text: &str, n: u64) -> Vec<TurnAction> {
        let at = self.t(n);
        self.feed(TurnInput::SttInterim {
            text: text.into(),
            at,
        })
    }

    fn fin(&mut self, text: &str, n: u64) -> Vec<TurnAction> {
        let at = self.t(n);
        self.feed(TurnInput::SttFinal {
            text: text.into(),
            at,
        })
    }

    fn tick(&mut self, n: u64) -> Vec<TurnAction> {
        let now = self.t(n);
        self.feed(TurnInput::Tick { now })
    }

    fn bot_ended(&mut self, e: u32, n: u64) -> Vec<TurnAction> {
        let at = self.t(n);
        self.feed(TurnInput::BotAudioEnded { epoch: ep(e), at })
    }

    fn hangup(&mut self, n: u64) -> Vec<TurnAction> {
        let at = self.t(n);
        self.feed(TurnInput::Hangup { at })
    }

    fn kind(&self) -> StateKind {
        self.m.state().kind()
    }

    /// The first user turn, then the bot speaking in epoch 0.
    fn speaking(ip: InterruptionPolicy, pause: PausePolicy) -> Self {
        let mut rig = Self {
            m: TurnMachine::with_interruption(TurnPolicy::fixed(ms(700)), ip),
            base: origin(),
            out: Vec::new(),
        };
        rig.vad_start(0);
        rig.vad_stop(400);
        rig.fin("hello", 450);
        let out = rig.tick(1100);
        assert_eq!(
            out,
            [TurnAction::EndUserTurn {
                epoch: ep(0),
                reason: EndReason::FixedSilence
            }]
        );
        let at = rig.t(BOT_AT);
        assert!(
            rig.feed(TurnInput::BotAudioStarted { epoch: ep(0), at })
                .is_empty()
        );
        assert_eq!(rig.kind(), StateKind::BotSpeaking);
        rig.m.set_pause(Some(pause));
        rig
    }

    fn words() -> Self {
        Self::speaking(InterruptionPolicy::default(), PausePolicy::words())
    }

    fn pause_action(&self) -> TurnAction {
        TurnAction::PausePlayout { epoch: ep(0) }
    }

    fn arm(&self, n: u64) -> TurnAction {
        TurnAction::ArmTimer {
            deadline: self.t(n),
        }
    }
}

fn committed(outcome: PauseOutcome, paused_ms: u64) -> TurnAction {
    TurnAction::PauseCommitted {
        outcome,
        paused_for: ms(paused_ms),
    }
}

#[test]
fn a_candidate_pauses_at_once_and_arms_the_decision_window() {
    let mut rig = Rig::words();
    let out = rig.vad_start(2000);
    assert_eq!(out, [rig.pause_action(), rig.arm(3500)]);
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
    assert!(rig.m.barge_in_candidate().unwrap().is_paused());
    assert_eq!(rig.m.epoch(), ep(0));
    assert_eq!(rig.m.pause_stats().pauses, 1);
}

#[test]
fn no_words_and_an_empty_final_resume_without_an_epoch_change() {
    let mut rig = Rig::words();
    rig.vad_start(2000);
    // The stop alone decides nothing: the final may still bring words.
    assert!(rig.vad_stop(2300).is_empty());
    let out = rig.fin("", 2500);
    assert_eq!(
        out,
        [
            TurnAction::ResumePlayout {
                epoch: ep(0),
                paused_for: ms(500)
            },
            TurnAction::CancelTimer
        ]
    );
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
    assert_eq!(rig.m.epoch(), ep(0));
    assert!(rig.m.barge_in_candidate().is_none());
    let istats = rig.m.interruption_stats();
    assert_eq!((istats.confirmed(), istats.resumed()), (0, 1));
    assert_eq!(rig.m.pause_stats().outcome(PauseOutcome::Resumed), 1);
}

#[test]
fn the_window_ends_without_words_in_a_resume_and_a_late_final_is_swallowed() {
    let mut rig = Rig::words();
    rig.vad_start(2000);
    rig.vad_stop(2300);
    // The tick consumed the timer: no cancel is needed.
    let out = rig.tick(3500);
    assert_eq!(
        out,
        [TurnAction::ResumePlayout {
            epoch: ep(0),
            paused_for: ms(1500)
        }]
    );
    // The stop is owed a final; it changes nothing now.
    assert!(rig.fin("", 3700).is_empty());
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
}

#[test]
fn a_word_commits_at_once_as_the_old_interruption() {
    let mut rig = Rig::words();
    rig.vad_start(2000);
    let out = rig.interim("wait a second", 2200);
    assert_eq!(
        out[..3],
        [
            committed(PauseOutcome::Committed, 200),
            TurnAction::Interrupt { new_epoch: ep(1) },
            TurnAction::StartUserTurn { epoch: ep(1) },
        ]
    );
    assert_eq!(rig.kind(), StateKind::UserSpeaking);
    assert_eq!(rig.m.epoch(), ep(1));
    let istats = rig.m.interruption_stats();
    assert_eq!((istats.confirmed(), istats.resumed()), (1, 0));
}

#[test]
fn a_command_word_commits_at_once() {
    let mut rig = Rig::words();
    rig.vad_start(2000);
    let out = rig.interim("dur", 2100);
    assert_eq!(out[0], committed(PauseOutcome::Committed, 100));
    assert_eq!(out[1], TurnAction::Interrupt { new_epoch: ep(1) });
}

#[test]
fn the_bots_own_echo_does_not_count_as_caller_words() {
    let mut rig = Rig::words();
    rig.vad_start(2000);
    rig.m.set_echo(true);
    assert!(rig.interim("the weather is nice today", 2200).is_empty());
    assert!(rig.m.barge_in_candidate().unwrap().is_paused());
    // The speech stops and the window ends: still no caller words, so the bot resumes.
    rig.vad_stop(2400);
    let out = rig.tick(3500);
    assert!(matches!(out[0], TurnAction::ResumePlayout { .. }));
}

#[test]
fn a_backchannel_is_not_a_caller_word() {
    let ip = InterruptionPolicy::recommended();
    let mut rig = Rig::speaking(ip, PausePolicy::words());
    rig.vad_start(2000);
    let out = rig.interim("uh-huh", 2200);
    assert!(
        !out.iter()
            .any(|a| matches!(a, TurnAction::Interrupt { .. }))
    );
    assert!(rig.m.barge_in_candidate().unwrap().is_paused());
    rig.vad_stop(2300);
    let out = rig.fin("uh-huh", 2400);
    assert!(matches!(out[0], TurnAction::ResumePlayout { .. }));
}

#[test]
fn the_deadline_commits_when_the_rule_waits() {
    // The never rule waits while the speech goes on and the guards block (three words needed).
    let ip = InterruptionPolicy {
        min_words: 3,
        ..InterruptionPolicy::default()
    };
    let mut rig = Rig::speaking(ip, PausePolicy::never());
    let out = rig.vad_start(2000);
    assert_eq!(out[0], rig.pause_action());
    let out = rig.tick(3500);
    assert_eq!(
        out[..3],
        [
            committed(PauseOutcome::Deadline, 1500),
            TurnAction::Interrupt { new_epoch: ep(1) },
            TurnAction::StartUserTurn { epoch: ep(1) },
        ]
    );
    assert_eq!(rig.m.pause_stats().outcome(PauseOutcome::Deadline), 1);
}

#[test]
fn the_never_rule_is_the_old_interruption_when_the_policy_confirms() {
    // With the policy off a candidate commits on its start: no pause at all.
    let mut rig = Rig::speaking(InterruptionPolicy::default(), PausePolicy::never());
    let out = rig.vad_start(2000);
    assert_eq!(
        out,
        [
            TurnAction::Interrupt { new_epoch: ep(1) },
            TurnAction::StartUserTurn { epoch: ep(1) },
        ]
    );
    assert_eq!(rig.m.pause_stats().pauses, 0);
}

#[test]
fn a_full_hold_buffer_commits() {
    let mut rig = Rig::words();
    rig.vad_start(2000);
    rig.m.set_hold_full();
    let out = rig.tick(2100);
    assert_eq!(out[0], committed(PauseOutcome::Overflow, 100));
    assert_eq!(out[1], TurnAction::Interrupt { new_epoch: ep(1) });
    assert_eq!(rig.m.pause_stats().outcome(PauseOutcome::Overflow), 1);
}

#[test]
fn a_final_with_words_after_the_stop_commits_and_the_turn_ends_normally() {
    let mut rig = Rig::words();
    rig.vad_start(2000);
    rig.vad_stop(2300);
    let out = rig.fin("stop it please", 2500);
    assert_eq!(out[0], committed(PauseOutcome::Committed, 500));
    assert_eq!(out[1], TurnAction::Interrupt { new_epoch: ep(1) });
    assert_eq!(out[2], TurnAction::StartUserTurn { epoch: ep(1) });
    assert_eq!(rig.kind(), StateKind::UserPaused);
    // The fixed silence after the stop ends the turn; the final was consumed by it.
    let out = rig.tick(3000);
    assert_eq!(
        out,
        [TurnAction::EndUserTurn {
            epoch: ep(1),
            reason: EndReason::FixedSilence
        }]
    );
}

#[test]
fn a_hang_up_during_a_pause_leaves_the_machine_closed_and_clean() {
    let mut rig = Rig::words();
    rig.vad_start(2000);
    let out = rig.hangup(2100);
    assert_eq!(out, [TurnAction::CancelTimer]);
    assert_eq!(rig.kind(), StateKind::Closed);
    assert!(rig.m.barge_in_candidate().is_none());
    assert_eq!(rig.m.pause_stats().abandoned, 1);
    // Nothing is emitted after the hang-up, a late tick included.
    assert!(rig.tick(3500).is_empty());
}

#[test]
fn the_end_of_the_bots_audio_during_a_pause_releases_the_hold() {
    // A policy that still blocks keeps the candidate pending (detached) after the bot's end.
    let ip = InterruptionPolicy {
        min_words: 3,
        ..InterruptionPolicy::default()
    };
    let mut rig = Rig::speaking(ip, PausePolicy::words());
    rig.vad_start(2000);
    let out = rig.bot_ended(0, 2100);
    assert_eq!(
        out[0],
        TurnAction::PauseReleased {
            epoch: ep(0),
            paused_for: ms(100)
        }
    );
    assert_eq!(rig.kind(), StateKind::Idle);
    assert!(!rig.m.barge_in_candidate().unwrap().is_paused());
    assert_eq!(rig.m.pause_stats().outcome(PauseOutcome::Ended), 1);
    assert_eq!(rig.m.pause_stats().outcome(PauseOutcome::Resumed), 0);
}

#[test]
fn the_speech_resuming_keeps_the_same_paused_candidate() {
    let mut rig = Rig::words();
    rig.vad_start(2000);
    rig.vad_stop(2200);
    // The VAD starts again before the window ends: the same candidate goes on, no new pause.
    assert!(rig.vad_start(2400).is_empty());
    assert_eq!(rig.m.pause_stats().pauses, 1);
    let out = rig.interim("excuse me", 2500);
    assert_eq!(out[0], committed(PauseOutcome::Committed, 500));
}

#[test]
fn repeated_resumed_pauses_tighten_the_rule_until_a_word_is_heard() {
    let mut rig = Rig::words();
    // Two false pauses within a minute.
    for (start, stop) in [(2000, 2200), (4000, 4200)] {
        rig.vad_start(start);
        rig.vad_stop(stop);
        let out = rig.tick(start + 1500);
        assert!(matches!(out[0], TurnAction::ResumePlayout { .. }));
        rig.fin("", start + 1600);
    }
    assert!(rig.m.tightened(rig.t(6000)));
    // The third candidate does not pause the bot: it stays talking.
    let out = rig.vad_start(6000);
    assert!(
        !out.iter()
            .any(|a| matches!(a, TurnAction::PausePlayout { .. }))
    );
    assert_eq!(rig.m.pause_stats().tightened_skips, 1);
    // Not even the plain (disabled) policy cuts the bot without a word.
    assert!(rig.m.barge_in_candidate().is_some());
    assert_eq!(rig.m.epoch(), ep(0));
    // A word commits at once, with no pause.
    let out = rig.interim("hold on", 6200);
    assert_eq!(out[0], TurnAction::Interrupt { new_epoch: ep(1) });
    assert_eq!(rig.m.pause_stats().pauses, 2);
}

#[test]
fn a_tightened_candidate_that_ends_without_words_is_dropped() {
    let mut rig = Rig::words();
    for (start, stop) in [(2000, 2200), (4000, 4200)] {
        rig.vad_start(start);
        rig.vad_stop(stop);
        rig.tick(start + 1500);
        rig.fin("", start + 1600);
    }
    rig.vad_start(6000);
    // The stop alone drops nothing: the final may still carry a word.
    let out = rig.vad_stop(6300);
    assert!(
        !out.iter()
            .any(|a| matches!(a, TurnAction::Interrupt { .. }))
    );
    assert!(rig.m.barge_in_candidate().is_some());
    // An empty final ends it, and the bot never paused.
    let out = rig.fin("", 6500);
    assert!(
        !out.iter()
            .any(|a| matches!(a, TurnAction::Interrupt { .. }))
    );
    assert!(rig.m.barge_in_candidate().is_none());
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
    assert_eq!(rig.m.epoch(), ep(0));
    assert_eq!(rig.m.pause_stats().pauses, 2);
}

#[test]
fn the_tightening_expires_with_its_window() {
    let mut rig = Rig::words();
    for (start, stop) in [(2000, 2200), (4000, 4200)] {
        rig.vad_start(start);
        rig.vad_stop(stop);
        rig.tick(start + 1500);
        rig.fin("", start + 1600);
    }
    assert!(rig.m.tightened(rig.t(60_000)));
    assert!(!rig.m.tightened(rig.t(70_000)));
    let out = rig.vad_start(70_000);
    assert_eq!(out[0], rig.pause_action());
}

#[test]
fn a_tightened_candidate_with_a_silent_recogniser_commits_after_the_window() {
    // Fail open: the speech still goes on after the decision window, so the plain policy decides.
    let mut rig = Rig::words();
    for (start, stop) in [(2000, 2200), (4000, 4200)] {
        rig.vad_start(start);
        rig.vad_stop(stop);
        rig.tick(start + 1500);
        rig.fin("", start + 1600);
    }
    rig.vad_start(6000);
    let out = rig.tick(7500);
    assert_eq!(out[0], TurnAction::Interrupt { new_epoch: ep(1) });
}

/// Two false pauses within a minute: the rule is tightened.
fn tightened() -> Rig {
    let mut rig = Rig::words();
    for (start, stop) in [(2000, 2200), (4000, 4200)] {
        rig.vad_start(start);
        rig.vad_stop(stop);
        rig.tick(start + 1500);
        rig.fin("", start + 1600);
    }
    rig
}

#[test]
fn a_tightened_stop_waits_for_a_final_that_carries_a_command() {
    let mut rig = tightened();
    rig.vad_start(6000);
    let out = rig.vad_stop(6300);
    assert!(
        !out.iter()
            .any(|a| matches!(a, TurnAction::Interrupt { .. }))
    );
    // The word was in the final: it is not swallowed.
    let out = rig.fin("stop it", 6400);
    assert_eq!(out[0], TurnAction::Interrupt { new_epoch: ep(1) });
    assert_eq!(out[1], TurnAction::StartUserTurn { epoch: ep(1) });
    assert_eq!(rig.kind(), StateKind::UserPaused);
    let out = rig.tick(7000);
    assert_eq!(
        out,
        [TurnAction::EndUserTurn {
            epoch: ep(1),
            reason: EndReason::FixedSilence
        }]
    );
}

#[test]
fn a_tightened_stop_with_no_final_falls_to_the_policy_at_the_window() {
    let mut rig = tightened();
    rig.vad_start(6000);
    rig.vad_stop(6300);
    // No final ever comes: after the window the plain (disabled) policy decides, at the stop.
    let out = rig.tick(7800);
    assert_eq!(out[0], TurnAction::Interrupt { new_epoch: ep(1) });
}

#[test]
fn the_end_of_speech_with_the_vad_still_active_commits_at_the_window() {
    // A lagging recogniser: no words in 1.5 s of speech. The speech goes on, so it is not
    // resumed (the candidate would be lost) but committed, as the deadline.
    let mut rig = Rig::words();
    rig.vad_start(2000);
    let out = rig.tick(3500);
    assert_eq!(out[0], committed(PauseOutcome::Deadline, 1500));
    assert_eq!(out[1], TurnAction::Interrupt { new_epoch: ep(1) });
}

#[test]
fn an_early_empty_final_does_not_resume_while_the_speech_goes_on() {
    let mut rig = Rig::words();
    rig.vad_start(2000);
    // Words or not, a final before any stop answers nothing of the candidate's.
    let out = rig.fin("", 2100);
    assert!(!out.iter().any(|a| matches!(
        a,
        TurnAction::ResumePlayout { .. } | TurnAction::Interrupt { .. }
    )));
    assert!(rig.m.barge_in_candidate().unwrap().is_paused());
    // The stop, then the final of that speech: resumed.
    rig.vad_stop(2300);
    let out = rig.fin("", 2500);
    assert!(matches!(out[0], TurnAction::ResumePlayout { .. }));
}

#[test]
fn a_late_final_of_an_earlier_stop_is_not_taken_for_the_candidates_own() {
    let mut rig = Rig::words();
    // Candidate one resumes at its window with its stop's final still owed.
    rig.vad_start(2000);
    rig.vad_stop(2200);
    assert!(matches!(
        rig.tick(3500)[0],
        TurnAction::ResumePlayout { .. }
    ));
    // Candidate two begins; the late final of the first arrives, with words: owed first.
    rig.vad_start(4000);
    let out = rig.fin("stop", 4100);
    assert!(
        !out.iter()
            .any(|a| matches!(a, TurnAction::Interrupt { .. }))
    );
    assert!(rig.m.barge_in_candidate().unwrap().is_paused());
    // Its own stop and final decide it.
    rig.vad_stop(4300);
    let out = rig.fin("", 4500);
    assert!(matches!(out[0], TurnAction::ResumePlayout { .. }));
    assert_eq!(rig.m.epoch(), ep(0));
}

#[test]
fn a_failed_pause_hands_the_candidate_back_to_the_policy() {
    // With the guards off the candidate is interrupted at once, as without the feature.
    let mut rig = Rig::words();
    rig.vad_start(2000);
    rig.m.pause_failed();
    let out = rig.tick(2010);
    assert_eq!(out[0], TurnAction::Interrupt { new_epoch: ep(1) });
    assert!(rig.m.pause_policy().is_none());
}

#[test]
fn a_stop_after_a_failed_pause_resolves_the_candidate_by_the_policy() {
    let ip = InterruptionPolicy {
        min_words: 3,
        ..InterruptionPolicy::default()
    };
    let mut rig = Rig::speaking(ip, PausePolicy::words());
    rig.vad_start(2000);
    rig.m.pause_failed();
    // The guards still block it at the stop: it is dropped, not left pending for ever.
    let out = rig.vad_stop(2300);
    assert!(
        !out.iter()
            .any(|a| matches!(a, TurnAction::Interrupt { .. }))
    );
    assert!(rig.m.barge_in_candidate().is_none());
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
    assert_eq!(rig.m.interruption_stats().total_vetoed(), 1);
}

#[test]
fn a_response_with_no_candidate_can_be_aborted_without_a_user_turn() {
    let mut rig = Rig::words();
    rig.vad_start(2000);
    // With a candidate pending nothing is aborted.
    let mut out = Vec::new();
    assert!(!rig.m.abort_response(&mut out));
    rig.vad_stop(2300);
    rig.tick(3500);
    assert!(rig.m.barge_in_candidate().is_none());
    assert!(rig.m.abort_response(&mut out));
    assert_eq!(out, [TurnAction::Interrupt { new_epoch: ep(1) }]);
    assert_eq!((rig.kind(), rig.m.epoch()), (StateKind::Idle, ep(1)));
    // The late end of the interrupted response is stale.
    assert!(rig.bot_ended(0, 4000).is_empty());
}

/// A candidate that begins while the response has not produced audio yet (the first turn ended,
/// the bot's audio starts during the candidate): the pause still ends within its window.
#[test]
fn a_candidate_before_the_bots_first_audio_is_still_decided_within_the_window() {
    let mut rig = Rig {
        m: TurnMachine::with_interruption(
            TurnPolicy::fixed(ms(700)),
            InterruptionPolicy::default(),
        ),
        base: origin(),
        out: Vec::new(),
    };
    rig.vad_start(0);
    rig.vad_stop(400);
    rig.fin("hello", 450);
    rig.tick(1100);
    rig.m.set_pause(Some(PausePolicy::words()));
    // A voice begins at 1300, before the bot's audio (1500), and goes on until 3400.
    let started = rig.vad_start(1300);
    let at = rig.t(BOT_AT);
    let mut all = started;
    all.extend(rig.feed(TurnInput::BotAudioStarted { epoch: ep(0), at }));
    all.extend(rig.vad_stop(3400));
    for t in [1500u64, 2000, 2800, 3000, 3500, 4000, 5000] {
        all.extend(rig.tick(t));
    }
    let ended = all.iter().any(|a| {
        matches!(
            a,
            TurnAction::ResumePlayout { .. }
                | TurnAction::PauseCommitted { .. }
                | TurnAction::PauseReleased { .. }
                | TurnAction::Interrupt { .. }
        )
    });
    assert!(ended, "the pause never ended: {all:?}");
    assert!(rig.m.barge_in_candidate().is_none_or(|c| !c.is_paused()));
}

mod properties {
    use super::*;
    use proptest::prelude::*;

    const TEXTS: [&str; 6] = [
        "",
        "stop",
        "hello there",
        "mm-hmm",
        "dur",
        "wait a second please",
    ];

    fn track(armed: &mut Option<Instant>, out: Vec<TurnAction>, all: &mut Vec<TurnAction>) {
        for a in &out {
            match a {
                TurnAction::ArmTimer { deadline } => *armed = Some(*deadline),
                TurnAction::CancelTimer => *armed = None,
                _ => {}
            }
        }
        all.extend(out);
    }

    /// Plays a random but well-formed call against a machine that pauses: a stop is followed by
    /// exactly one final, in order, and the armed timer is delivered when it falls due.
    fn play(
        rule_words: bool,
        decide_ms: u64,
        after_ms: u64,
        ops: &[(u8, u8, u16)],
    ) -> (Vec<TurnAction>, u64) {
        let policy = if rule_words {
            PausePolicy::words()
        } else {
            PausePolicy::never()
        };
        let policy = PausePolicy {
            decide: ms(decide_ms),
            after: ms(after_ms),
            ..policy
        };
        let mut rig = Rig::speaking(InterruptionPolicy::recommended(), policy);
        let mut all: Vec<TurnAction> = Vec::new();
        let mut now = BOT_AT;
        let (mut speaking, mut owed) = (false, 0u32);
        let mut armed: Option<Instant> = None;
        for &(kind, text, delta) in ops {
            now += u64::from(delta) % 700 + 1;
            // A timer that fell due before this step.
            while let Some(deadline) = armed {
                if deadline > rig.t(now) {
                    break;
                }
                armed = None;
                let out = rig.feed(TurnInput::Tick { now: deadline });
                track(&mut armed, out, &mut all);
            }
            let text = TEXTS[usize::from(text) % TEXTS.len()];
            let out = match kind % 6 {
                0 if !speaking => {
                    speaking = true;
                    rig.vad_start(now)
                }
                1 if speaking => {
                    speaking = false;
                    owed += 1;
                    rig.vad_stop(now)
                }
                2 if owed > 0 => {
                    owed -= 1;
                    rig.fin(text, now)
                }
                3 => rig.interim(text, now),
                4 => rig.tick(now),
                5 => rig.bot_ended(rig.m.epoch().get(), now),
                _ => Vec::new(),
            };
            track(&mut armed, out, &mut all);
        }
        (
            all,
            rig.m.stats().ignored(brisvox_agent::InputKind::SttFinal),
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        #[test]
        fn every_pause_ends_exactly_once_and_the_epoch_only_grows(
            rule_words in any::<bool>(),
            decide in 300u64..1750,
            after_ms in 0u64..600,
            ops in proptest::collection::vec((0u8..6, 0u8..6, 0u16..2000), 1..60),
        ) {
            let (actions, ignored_finals) = play(rule_words, decide, after_ms, &ops);
            // Every stop owes one final and each final answers one stop: none is left over.
            prop_assert_eq!(ignored_finals, 0, "a final was taken twice or for no stop: {:?}", actions);
            let (mut open, mut epoch) = (false, 0u32);
            for a in &actions {
                match a {
                    TurnAction::PausePlayout { .. } => {
                        prop_assert!(!open, "a pause began inside a pause: {actions:?}");
                        open = true;
                    }
                    TurnAction::ResumePlayout { .. } | TurnAction::PauseReleased { .. } => {
                        prop_assert!(open, "a resume or release without a pause: {actions:?}");
                        open = false;
                    }
                    TurnAction::PauseCommitted { .. } => {
                        prop_assert!(open, "a commit without a pause: {actions:?}");
                        open = false;
                    }
                    TurnAction::Interrupt { new_epoch } => {
                        prop_assert!(new_epoch.get() > epoch, "epoch went back: {actions:?}");
                        epoch = new_epoch.get();
                    }
                    TurnAction::StartBotTurn { epoch: e } | TurnAction::StartUserTurn { epoch: e } => {
                        prop_assert!(e.get() >= epoch, "stale epoch: {actions:?}");
                    }
                    _ => {}
                }
            }
        }
    }
}

/// A detached candidate (the bot's audio ended during its pause) whose speech stops and starts
/// again goes on as one candidate and every stop still gets its final.
#[test]
fn a_detached_candidate_that_stops_and_starts_again_keeps_its_finals_aligned() {
    let mut rig = Rig::speaking(InterruptionPolicy::recommended(), PausePolicy::words());
    rig.vad_start(BOT_AT + 1);
    rig.vad_stop(BOT_AT + 2);
    rig.bot_ended(0, BOT_AT + 3);
    rig.vad_start(BOT_AT + 100);
    rig.vad_stop(BOT_AT + 351);
    rig.fin("", BOT_AT + 352);
    rig.fin("", BOT_AT + 353);
    assert_eq!(rig.m.stats().ignored(brisvox_agent::InputKind::SttFinal), 0);
}

fn after(ms_: u64) -> PausePolicy {
    PausePolicy {
        after: ms(ms_),
        ..PausePolicy::words()
    }
}

#[test]
fn the_bot_is_paused_only_after_the_speech_has_lasted_the_delay() {
    let mut rig = Rig::speaking(InterruptionPolicy::default(), after(250));
    // The candidate begins: nothing is paused yet, a look is armed for 250 ms into the speech.
    let out = rig.vad_start(2000);
    assert_eq!(out, [rig.arm(2250)]);
    assert!(!rig.m.barge_in_candidate().unwrap().is_paused());
    // The look: the speech still goes on, the bot is paused now.
    let out = rig.tick(2250);
    assert_eq!(out[0], rig.pause_action());
    assert!(rig.m.barge_in_candidate().unwrap().is_paused());
}

#[test]
fn a_burst_shorter_than_the_delay_never_pauses_and_an_empty_final_drops_it() {
    let mut rig = Rig::speaking(InterruptionPolicy::default(), after(250));
    rig.vad_start(2000);
    // A click: 120 ms of speech.
    let out = rig.vad_stop(2120);
    assert!(
        !out.iter()
            .any(|a| matches!(a, TurnAction::PausePlayout { .. })),
        "{out:?}"
    );
    let out = rig.fin("", 2400);
    assert!(
        out.iter().all(|a| !matches!(
            a,
            TurnAction::Interrupt { .. } | TurnAction::PausePlayout { .. }
        )),
        "{out:?}"
    );
    assert!(rig.m.barge_in_candidate().is_none());
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
    assert_eq!(rig.m.pause_stats().pauses, 0);
}

#[test]
fn a_word_before_the_delay_commits_at_once_and_a_short_command_in_the_final_commits() {
    let mut rig = Rig::speaking(InterruptionPolicy::default(), after(400));
    rig.vad_start(2000);
    let out = rig.interim("wait", 2150);
    assert!(
        out.iter()
            .any(|a| matches!(a, TurnAction::Interrupt { .. })),
        "{out:?}"
    );
    // A command that arrives only in the final, after a short utterance.
    let mut rig = Rig::speaking(InterruptionPolicy::default(), after(400));
    rig.vad_start(2000);
    rig.vad_stop(2300);
    let out = rig.fin("dur", 2600);
    assert!(
        out.iter()
            .any(|a| matches!(a, TurnAction::Interrupt { .. })),
        "{out:?}"
    );
}

/// Two resumed pauses within a minute: the rule is tightened.
fn tightened_by(mut rig: Rig, first_wait: u64) -> Rig {
    for (start, stop) in [(2000u64, 2400u64), (4000, 4400)] {
        rig.vad_start(start);
        rig.tick(start + first_wait);
        rig.vad_stop(stop);
        rig.tick(start + first_wait + 1500);
        rig.fin("", start + first_wait + 1600);
    }
    assert!(rig.m.tightened(rig.t(6000)));
    rig
}

#[test]
fn a_tightened_candidate_that_restarts_before_its_first_final_still_gets_every_final() {
    let mut rig = tightened_by(Rig::words(), 0);
    rig.vad_start(6000);
    rig.vad_stop(6100);
    // The speech goes on before the recogniser's final of the first part.
    rig.vad_start(6200);
    let out = rig.fin("", 6300);
    assert!(
        out.iter()
            .all(|a| !matches!(a, TurnAction::Interrupt { .. })),
        "{out:?}"
    );
    rig.vad_stop(6500);
    rig.fin("", 6600);
    assert_eq!(rig.m.stats().ignored(brisvox_agent::InputKind::SttFinal), 0);
    assert!(rig.m.barge_in_candidate().is_none());
    assert_eq!(rig.m.epoch(), ep(0));
}

#[test]
fn a_blocked_stop_of_a_candidate_with_an_earlier_stop_owes_both_finals() {
    let mut rig = Rig::speaking(InterruptionPolicy::recommended(), PausePolicy::words());
    rig.vad_start(BOT_AT + 1);
    rig.vad_stop(BOT_AT + 2);
    // The bot's audio ends during the pause: the candidate goes on, detached.
    rig.bot_ended(0, BOT_AT + 3);
    rig.vad_start(BOT_AT + 100);
    // 120 ms of speech in all: the plain policy blocks it at the stop (`min_speech`).
    let out = rig.vad_stop(BOT_AT + 120);
    assert!(
        out.iter()
            .all(|a| !matches!(a, TurnAction::StartUserTurn { .. })),
        "{out:?}"
    );
    assert!(rig.m.barge_in_candidate().is_none());
    rig.fin("", BOT_AT + 200);
    rig.fin("", BOT_AT + 300);
    assert_eq!(rig.m.stats().ignored(brisvox_agent::InputKind::SttFinal), 0);
}

#[test]
fn the_pause_delay_and_tightening_together_fall_to_the_window_and_count_one_skip() {
    let mut rig = tightened_by(
        Rig::speaking(InterruptionPolicy::default(), after(300)),
        300,
    );
    assert_eq!(rig.m.pause_stats().tightened_skips, 0);
    // Tightened: not paused at the start, a look is armed for the delay.
    let out = rig.vad_start(6000);
    assert!(
        !out.iter()
            .any(|a| matches!(a, TurnAction::PausePlayout { .. })),
        "{out:?}"
    );
    assert_eq!(rig.m.pause_stats().tightened_skips, 1);
    // The delay is over: still no pause (tightened), no second skip, the next look is the end
    // of the window (1.5 s from the speech start).
    let out = rig.tick(6300);
    assert!(
        !out.iter()
            .any(|a| matches!(a, TurnAction::PausePlayout { .. })),
        "{out:?}"
    );
    assert_eq!(rig.m.pause_stats().tightened_skips, 1);
    assert!(out.contains(&rig.arm(7500)), "{out:?}");
    assert!(rig.m.barge_in_candidate().is_some());
    // The window ends with the speech still on and no word: the plain policy decides (here:
    // everything off, so it interrupts).
    let out = rig.tick(7500);
    assert!(
        out.iter()
            .any(|a| matches!(a, TurnAction::Interrupt { .. })),
        "{out:?}"
    );
}
