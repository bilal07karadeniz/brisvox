//! Interruption policy tests: table rows for every guard (minimum speech, minimum words,
//! backchannels in English and Turkish, the echo guard, the transcript wait), the candidate
//! life cycle in the turn machine (pending, confirmed, vetoed, promoted, hung up), the pure
//! policy on its own, and a deterministic invariant run over random inputs.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::time::Duration;

use brisvox_agent::{
    AnalysisId, Assessment, BackchannelSet, Confirm, EndReason, InputKind, InterruptionPolicy,
    Language, Observation, StateKind, TextKind, TurnAction, TurnInput, TurnMachine, TurnPolicy,
    TurnState, VetoReason, count_words,
};
use brisvox_core::TurnEpoch;
use tokio::time::Instant;

/// The clock origin of every test. Only a reading; the machine never reads a clock itself.
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

fn start(e: u32) -> TurnAction {
    TurnAction::StartUserTurn { epoch: ep(e) }
}

fn intr(e: u32) -> TurnAction {
    TurnAction::Interrupt { new_epoch: ep(e) }
}

/// When the bot's audio starts in every scenario (ms after the origin).
const BOT_AT: u64 = 1500;

/// A machine plus a clock origin; every method feeds one input and returns the actions.
struct Rig {
    m: TurnMachine,
    base: Instant,
    out: Vec<TurnAction>,
}

impl Rig {
    fn new(ip: InterruptionPolicy) -> Self {
        Self {
            m: TurnMachine::with_interruption(TurnPolicy::fixed(ms(700)), ip),
            base: origin(),
            out: Vec::new(),
        }
    }

    fn t(&self, n: u64) -> Instant {
        self.base + ms(n)
    }

    fn arm(&self, n: u64) -> TurnAction {
        TurnAction::ArmTimer {
            deadline: self.t(n),
        }
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

    fn bot_started(&mut self, e: u32, n: u64) -> Vec<TurnAction> {
        let at = self.t(n);
        self.feed(TurnInput::BotAudioStarted { epoch: ep(e), at })
    }

    fn bot_ended(&mut self, e: u32, n: u64) -> Vec<TurnAction> {
        let at = self.t(n);
        self.feed(TurnInput::BotAudioEnded { epoch: ep(e), at })
    }

    fn hangup(&mut self, n: u64) -> Vec<TurnAction> {
        let at = self.t(n);
        self.feed(TurnInput::Hangup { at })
    }

    /// Plays the first user turn (epoch 0, ends at 1100 with the fixed silence) and leaves the
    /// machine in `AwaitingResponse`.
    fn awaiting(ip: InterruptionPolicy) -> Self {
        let mut rig = Self::new(ip);
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
        assert_eq!(rig.m.state().kind(), StateKind::AwaitingResponse);
        rig
    }

    /// As [`Rig::awaiting`], then the bot's audio starts at [`BOT_AT`].
    fn speaking(ip: InterruptionPolicy) -> Self {
        let mut rig = Self::awaiting(ip);
        assert!(rig.bot_started(0, BOT_AT).is_empty());
        assert_eq!(rig.m.state().kind(), StateKind::BotSpeaking);
        rig
    }

    fn kind(&self) -> StateKind {
        self.m.state().kind()
    }

    fn ignored(&self, kind: InputKind) -> u64 {
        self.m.stats().ignored(kind)
    }
}

fn min_speech(n: u64) -> InterruptionPolicy {
    InterruptionPolicy {
        min_speech: ms(n),
        ..InterruptionPolicy::default()
    }
}

fn min_words(n: u32) -> InterruptionPolicy {
    InterruptionPolicy {
        min_words: n,
        ..InterruptionPolicy::default()
    }
}

fn echo(n: u64) -> InterruptionPolicy {
    InterruptionPolicy {
        echo_guard: ms(n),
        ..InterruptionPolicy::default()
    }
}

/// English + Turkish backchannels with a 400 ms wait for a first interim.
fn backchannels() -> InterruptionPolicy {
    InterruptionPolicy {
        backchannels: vec![BackchannelSet::english(), BackchannelSet::turkish()],
        backchannel_wait: ms(400),
        ..InterruptionPolicy::default()
    }
}

// ---------------------------------------------------------------------------------------------
// Disabled policy = plain always-interrupt behaviour
// ---------------------------------------------------------------------------------------------

#[test]
fn the_default_policy_is_off_and_interrupts_at_once() {
    assert!(InterruptionPolicy::default().is_disabled());
    assert!(InterruptionPolicy::disabled().is_disabled());
    assert!(!InterruptionPolicy::recommended().is_disabled());

    let mut rig = Rig::speaking(InterruptionPolicy::default());
    assert_eq!(rig.vad_start(2000), [intr(1), start(1)]);
    assert_eq!(rig.kind(), StateKind::UserSpeaking);
    assert_eq!(rig.m.epoch(), ep(1));
    assert!(rig.m.barge_in_candidate().is_none());
    assert!(rig.m.armed_deadline().is_none());
    let stats = rig.m.interruption_stats();
    assert_eq!((stats.candidates(), stats.confirmed()), (1, 1));
    assert_eq!(stats.total_vetoed(), 0);
}

/// The action trace of the original guard-less machine (recorded by hand from its documented behaviour, not
/// produced by the code under test) for a script with a barge-in. Both constructors must
/// reproduce it exactly: a regression in the disabled path (an extra `CancelTimer`, a candidate
/// state) shows up here.
#[test]
fn a_disabled_policy_replays_the_t026_trace_exactly() {
    let base = origin();
    let at = |n: u64| base + ms(n);
    let arm = |n: u64| TurnAction::ArmTimer { deadline: at(n) };
    #[rustfmt::skip]
    let script: Vec<(TurnInput, Vec<TurnAction>)> = vec![
        (TurnInput::VadStart { at: at(0) }, vec![start(0)]),
        (TurnInput::SttInterim { text: "hello".into(), at: at(100) }, vec![]),
        (
            TurnInput::VadStop { at: at(400) },
            vec![TurnAction::RequestAnalysis { id: AnalysisId::new(1) }, arm(3400)],
        ),
        (TurnInput::SttFinal { text: "hello.".into(), at: at(450) }, vec![]),
        (
            TurnInput::Tick { now: at(4000) },
            vec![TurnAction::EndUserTurn { epoch: ep(0), reason: EndReason::Fallback }],
        ),
        (TurnInput::BotAudioStarted { epoch: ep(0), at: at(4500) }, vec![]),
        // Ignored: no user turn or candidate to take an interim.
        (TurnInput::SttInterim { text: "ignored".into(), at: at(4600) }, vec![]),
        // The barge-in is immediate: no candidate, no timer.
        (TurnInput::VadStart { at: at(5000) }, vec![intr(1), start(1)]),
        (
            TurnInput::VadStop { at: at(5200) },
            vec![TurnAction::RequestAnalysis { id: AnalysisId::new(2) }, arm(8200)],
        ),
        // The interrupted response's audio end is stale.
        (TurnInput::BotAudioEnded { epoch: ep(0), at: at(5300) }, vec![]),
        (
            TurnInput::Verdict {
                request: AnalysisId::new(2),
                complete: true,
                probability: 0.9,
                at: at(5400),
            },
            vec![
                TurnAction::CancelTimer,
                TurnAction::EndUserTurn { epoch: ep(1), reason: EndReason::Verdict },
            ],
        ),
    ];
    let mut machines = [
        TurnMachine::new(TurnPolicy::adaptive()),
        TurnMachine::with_interruption(TurnPolicy::adaptive(), InterruptionPolicy::default()),
    ];
    for m in &mut machines {
        for (input, expected) in &script {
            let mut out = Vec::new();
            m.handle(input.clone(), &mut out);
            assert_eq!(&out, expected, "{input:?}");
            assert!(m.barge_in_candidate().is_none(), "{input:?}");
        }
        assert_eq!(m.stats().ignored(InputKind::SttInterim), 1);
        assert_eq!(m.stats().ignored(InputKind::BotAudioEnded), 1);
        assert_eq!(m.stats().total_ignored(), 2);
        assert_eq!(m.state().kind(), StateKind::AwaitingResponse);
        let stats = m.interruption_stats();
        assert_eq!((stats.candidates(), stats.confirmed()), (1, 1));
        assert_eq!(stats.total_vetoed() + stats.promoted(), 0);
    }
}

// ---------------------------------------------------------------------------------------------
// Minimum speech
// ---------------------------------------------------------------------------------------------

#[test]
fn min_speech_holds_the_barge_in_until_the_vad_has_run_long_enough() {
    let mut rig = Rig::speaking(min_speech(300));
    // A pending candidate: no Interrupt, no epoch bump, no user turn; only the recheck timer.
    assert_eq!(rig.vad_start(2000), [rig.arm(2300)]);
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
    assert_eq!(rig.m.epoch(), ep(0));
    let candidate = rig.m.barge_in_candidate().expect("pending");
    assert_eq!(candidate.started_at(), rig.t(2000));
    assert_eq!(candidate.recheck_at(), Some(rig.t(2300)));

    // A tick before the deadline re-arms it.
    assert_eq!(rig.tick(2200), [rig.arm(2300)]);
    assert_eq!(rig.kind(), StateKind::BotSpeaking);

    // At the deadline the barge-in is real.
    assert_eq!(rig.tick(2300), [intr(1), start(1)]);
    assert_eq!(rig.kind(), StateKind::UserSpeaking);
    assert_eq!(rig.m.epoch(), ep(1));
    assert!(rig.m.barge_in_candidate().is_none());
    assert!(rig.m.armed_deadline().is_none());
}

#[test]
fn a_short_burst_is_vetoed_without_interrupt_or_epoch_bump() {
    let mut rig = Rig::speaking(min_speech(300));
    rig.vad_start(2000);
    // The VAD stop at 2200 (200 ms of speech): vetoed, the timer is cancelled.
    assert_eq!(rig.vad_stop(2200), [TurnAction::CancelTimer]);
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
    assert_eq!(rig.m.epoch(), ep(0));
    assert!(rig.m.barge_in_candidate().is_none());
    let stats = rig.m.interruption_stats();
    assert_eq!(stats.vetoed(VetoReason::MinSpeech), 1);
    assert_eq!(stats.confirmed(), 0);
    // The stop is owed a final: it is consumed, not counted as stray (alignment of later finals).
    assert!(rig.fin("hm", 2300).is_empty());
    assert_eq!(rig.ignored(InputKind::SttFinal), 0);
    // The bot is undisturbed: its audio ends normally, and the next speech is an ordinary turn.
    assert!(rig.bot_ended(0, 3000).is_empty());
    assert_eq!(rig.kind(), StateKind::Idle);
    assert_eq!(rig.vad_start(4000), [start(1)]);
}

#[test]
fn a_vad_stop_after_the_threshold_but_before_the_tick_still_confirms() {
    let mut rig = Rig::speaking(min_speech(300));
    rig.vad_start(2000);
    // 400 ms of speech; the timer tick has not been delivered yet. The barge-in is real and
    // the stop is an ordinary stop of the new turn.
    assert_eq!(
        rig.vad_stop(2400),
        [intr(1), start(1), rig.arm(3100)],
        "interrupt, then the pause deadline replaces the recheck timer"
    );
    assert_eq!(rig.kind(), StateKind::UserPaused);
    assert_eq!(rig.m.epoch(), ep(1));
}

#[test]
fn three_vetoes_then_a_real_barge_in_uses_the_next_epoch_only() {
    let mut rig = Rig::speaking(min_speech(300));
    for i in 0..3u64 {
        let t0 = 2000 + i * 500;
        rig.vad_start(t0);
        rig.vad_stop(t0 + 100);
        rig.fin("x", t0 + 150);
        assert_eq!(rig.m.epoch(), ep(0), "veto {i} keeps the epoch");
    }
    assert_eq!(rig.m.interruption_stats().vetoed(VetoReason::MinSpeech), 3);
    rig.vad_start(4000);
    assert_eq!(rig.tick(4300), [intr(1), start(1)]);
    // The interrupted response's late end is stale.
    assert!(rig.bot_ended(0, 4400).is_empty());
    assert_eq!(rig.ignored(InputKind::BotAudioEnded), 1);
    assert_eq!(rig.kind(), StateKind::UserSpeaking);
}

#[test]
fn a_second_vad_start_during_a_pending_candidate_is_a_counted_duplicate() {
    let mut rig = Rig::speaking(min_speech(300));
    rig.vad_start(2000);
    assert!(rig.vad_start(2100).is_empty());
    assert_eq!(rig.ignored(InputKind::VadStart), 1);
    assert_eq!(rig.m.interruption_stats().candidates(), 1);
    assert_eq!(
        rig.m.barge_in_candidate().expect("pending").started_at(),
        rig.t(2000)
    );
}

// ---------------------------------------------------------------------------------------------
// Minimum words
// ---------------------------------------------------------------------------------------------

#[test]
fn min_words_waits_for_enough_interim_words() {
    let mut rig = Rig::speaking(min_words(2));
    assert!(
        rig.vad_start(2000).is_empty(),
        "no timer: only words can confirm"
    );
    assert!(rig.m.armed_deadline().is_none());
    assert!(rig.interim("wait", 2100).is_empty());
    assert_eq!(rig.m.barge_in_candidate().expect("pending").words(), 1);
    assert_eq!(rig.interim("wait a second", 2200), [intr(1), start(1)]);
    assert_eq!(rig.kind(), StateKind::UserSpeaking);
}

#[test]
fn the_word_count_never_goes_down_when_the_stt_restarts_its_interim() {
    let mut rig = Rig::speaking(min_words(3));
    rig.vad_start(2000);
    assert!(rig.interim("one two", 2100).is_empty());
    // A new interim segment starts over with one word: the count stays at two.
    assert!(rig.interim("three", 2200).is_empty());
    assert_eq!(rig.m.barge_in_candidate().expect("pending").words(), 2);
    assert!(rig.interim("   ", 2250).is_empty());
    assert_eq!(rig.interim("three four", 2300), [] as [TurnAction; 0]);
    assert_eq!(rig.interim("one two three", 2400), [intr(1), start(1)]);
}

#[test]
fn too_few_words_at_the_vad_stop_is_a_veto() {
    let mut rig = Rig::speaking(min_words(2));
    rig.vad_start(2000);
    rig.interim("hm", 2100);
    assert!(rig.vad_stop(2300).is_empty());
    assert_eq!(rig.m.interruption_stats().vetoed(VetoReason::MinWords), 1);
    assert_eq!(rig.m.epoch(), ep(0));
}

#[test]
fn all_requires_both_guards_and_any_requires_one() {
    let both = |confirm| InterruptionPolicy {
        min_speech: ms(1000),
        min_words: 1,
        confirm,
        ..InterruptionPolicy::default()
    };
    // Any: one word is enough, long before the speech guard.
    let mut any = Rig::speaking(both(Confirm::Any));
    assert_eq!(any.vad_start(2000), [any.arm(3000)]);
    assert_eq!(
        any.interim("stop", 2100),
        [intr(1), start(1), TurnAction::CancelTimer]
    );

    // Any: time alone is enough too.
    let mut any = Rig::speaking(both(Confirm::Any));
    any.vad_start(2000);
    assert_eq!(any.tick(3000), [intr(1), start(1)]);

    // All: the word is not enough ...
    let mut all = Rig::speaking(both(Confirm::All));
    all.vad_start(2000);
    assert!(all.interim("stop", 2100).is_empty());
    assert_eq!(all.kind(), StateKind::BotSpeaking);
    // ... nor is time without a word (the VAD stop is a veto) ...
    let mut all2 = Rig::speaking(both(Confirm::All));
    all2.vad_start(2000);
    all2.tick(3000);
    assert_eq!(all2.kind(), StateKind::BotSpeaking);
    assert_eq!(all2.vad_stop(3100), [] as [TurnAction; 0]);
    assert_eq!(all2.m.interruption_stats().vetoed(VetoReason::MinWords), 1);
    // ... both together are.
    assert_eq!(all.tick(3000), [intr(1), start(1)]);
}

// ---------------------------------------------------------------------------------------------
// Backchannels
// ---------------------------------------------------------------------------------------------

/// `(text, interrupts)`: what a candidate does when its first interim is `text`.
const BACKCHANNEL_ROWS: &[(&str, bool)] = &[
    // English backchannels: the bot keeps talking.
    ("yeah", false),
    ("Yeah.", false),
    ("uh-huh", false),
    ("Uh huh", false),
    ("mm-hmm", false),
    ("okay", false),
    ("OK!", false),
    ("right", false),
    ("yeah yeah", false),
    ("yeah, okay, right", false),
    ("I see", false),
    ("got it", false),
    // English that is not (only) a backchannel.
    ("yeah wait", true),
    ("okay but", true),
    ("stop", true),
    ("no", true),
    ("yes", true),
    ("wait a second", true),
    ("hello", true),
    // Turkish backchannels.
    ("evet", false),
    ("Evet.", false),
    ("Hı-hı", false),
    ("hı hı", false),
    ("tamam", false),
    ("Tamam.", false),
    ("he", false),
    ("aynen", false),
    ("anladım", false),
    ("Anladım.", false),
    ("tamam tamam", false),
    ("evet tamam", false),
    ("öyle mi", false),
    // Turkish that is not.
    ("tamam dur", true),
    ("hayır", true),
    ("bir dakika", true),
    ("dur", true),
    // Mixed languages count as backchannels when every piece is one.
    ("yeah tamam", false),
    // Noise-only text has no word at all: it neither confirms nor blocks by itself.
    ("...", false),
];

#[test]
fn backchannel_table_english_and_turkish_defaults() {
    for (text, interrupts) in BACKCHANNEL_ROWS {
        let mut rig = Rig::speaking(backchannels());
        // The candidate waits for a first interim (400 ms).
        assert_eq!(rig.vad_start(2000), [rig.arm(2400)], "{text}");
        let out = rig.interim(text, 2100);
        if *interrupts {
            assert_eq!(
                out,
                [intr(1), start(1), TurnAction::CancelTimer],
                "{text}: interrupts"
            );
            assert_eq!(rig.m.epoch(), ep(1), "{text}");
        } else {
            assert_eq!(rig.m.epoch(), ep(0), "{text}: no epoch bump");
            assert_eq!(
                rig.kind(),
                StateKind::BotSpeaking,
                "{text}: bot keeps talking"
            );
            assert!(
                !out.iter().any(|a| matches!(
                    a,
                    TurnAction::Interrupt { .. } | TurnAction::StartUserTurn { .. }
                )),
                "{text}: {out:?}"
            );
            // Staying quiet is final for a pure backchannel: no later tick interrupts either.
            if count_words(text) > 0 {
                assert!(rig.m.armed_deadline().is_none(), "{text}: no recheck timer");
                assert!(
                    rig.tick(5000).is_empty(),
                    "{text}: a stray tick does nothing"
                );
                // When the user stops, the candidate is vetoed as a backchannel.
                assert!(rig.vad_stop(2500).is_empty(), "{text}");
                assert_eq!(
                    rig.m.interruption_stats().vetoed(VetoReason::Backchannel),
                    1,
                    "{text}"
                );
            }
            assert_eq!(rig.m.epoch(), ep(0), "{text}");
        }
    }
}

#[test]
fn a_backchannel_that_grows_into_speech_interrupts() {
    let mut rig = Rig::speaking(backchannels());
    rig.vad_start(2000);
    rig.interim("yeah", 2100);
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
    assert_eq!(rig.interim("yeah but wait", 2300), [intr(1), start(1)]);
}

#[test]
fn a_vetoed_backchannel_leaves_the_bot_and_the_epoch_alone() {
    let mut rig = Rig::speaking(backchannels());
    rig.vad_start(2000);
    rig.interim("uh-huh", 2100);
    rig.vad_stop(2400);
    assert!(rig.fin("uh-huh", 2450).is_empty());
    assert_eq!(
        rig.ignored(InputKind::SttFinal),
        0,
        "the stop's final is consumed"
    );
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
    assert_eq!(rig.m.epoch(), ep(0));
    // A later real interruption works as usual.
    rig.vad_start(3000);
    assert_eq!(
        rig.interim("excuse me", 3100),
        [intr(1), start(1), TurnAction::CancelTimer],
        "the wait timer of this candidate is cancelled"
    );
}

#[test]
fn the_language_sets_are_independent_and_configurable() {
    // English only: a Turkish "tamam" is speech.
    let en_only = InterruptionPolicy {
        backchannels: vec![BackchannelSet::english()],
        backchannel_wait: ms(400),
        ..InterruptionPolicy::default()
    };
    let mut rig = Rig::speaking(en_only.clone());
    rig.vad_start(2000);
    assert_eq!(rig.interim("tamam", 2100).len(), 3, "interrupts");
    let mut rig = Rig::speaking(en_only);
    rig.vad_start(2000);
    assert!(!rig.interim("yeah", 2100).contains(&intr(1)));

    // Turkish only: an English "yeah" is speech.
    let tr_only = InterruptionPolicy {
        backchannels: vec![BackchannelSet::turkish()],
        backchannel_wait: ms(400),
        ..InterruptionPolicy::default()
    };
    let mut rig = Rig::speaking(tr_only);
    rig.vad_start(2000);
    assert!(rig.interim("yeah", 2100).contains(&intr(1)));

    // A custom list.
    let custom = InterruptionPolicy {
        backchannels: vec![BackchannelSet::new(Language::En, ["sure thing", "go on"])],
        backchannel_wait: ms(400),
        ..InterruptionPolicy::default()
    };
    let mut rig = Rig::speaking(custom.clone());
    rig.vad_start(2000);
    assert!(!rig.interim("Go on!", 2100).contains(&intr(1)));
    let mut rig = Rig::speaking(custom);
    rig.vad_start(2000);
    assert!(
        rig.interim("yeah", 2100).contains(&intr(1)),
        "not in the custom list"
    );
}

#[test]
fn without_a_first_interim_the_candidate_fails_open_after_the_wait() {
    let mut rig = Rig::speaking(backchannels());
    assert_eq!(rig.vad_start(2000), [rig.arm(2400)]);
    // STT is silent: the user is assumed to be speaking for real once the wait is over.
    assert_eq!(rig.tick(2400), [intr(1), start(1)]);

    // A VAD stop before the wait with no interim at all is noise.
    let mut rig = Rig::speaking(backchannels());
    rig.vad_start(2000);
    assert_eq!(rig.vad_stop(2200), [TurnAction::CancelTimer]);
    assert_eq!(
        rig.m.interruption_stats().vetoed(VetoReason::Unconfirmed),
        1
    );
}

// ---------------------------------------------------------------------------------------------
// Echo guard
// ---------------------------------------------------------------------------------------------

#[test]
fn echo_guard_blocks_vad_alone_during_the_first_ms_of_bot_audio() {
    let mut rig = Rig::speaking(echo(500));
    // Bot audio started at 1500; VAD at 1700 is inside the 500 ms window (until 2000).
    assert_eq!(rig.vad_start(1700), [rig.arm(2000)]);
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
    assert_eq!(rig.m.epoch(), ep(0));
    // A VAD that is still running after the window is real.
    assert_eq!(rig.tick(2000), [intr(1), start(1)]);
}

#[test]
fn echo_guard_is_lifted_by_stt_words_inside_the_window() {
    let mut rig = Rig::speaking(echo(500));
    rig.vad_start(1700);
    assert_eq!(
        rig.interim("hello there", 1800),
        [intr(1), start(1), TurnAction::CancelTimer]
    );
    assert_eq!(rig.m.epoch(), ep(1));
}

#[test]
fn echo_guard_needs_the_configured_number_of_words() {
    let policy = InterruptionPolicy {
        echo_guard: ms(500),
        echo_min_words: 2,
        ..InterruptionPolicy::default()
    };
    let mut rig = Rig::speaking(policy);
    rig.vad_start(1700);
    assert!(rig.interim("hello", 1800).is_empty());
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
    assert_eq!(
        rig.interim("hello there", 1850),
        [intr(1), start(1), TurnAction::CancelTimer]
    );
}

#[test]
fn an_echo_burst_without_words_is_vetoed_at_the_vad_stop() {
    let mut rig = Rig::speaking(echo(500));
    rig.vad_start(1700);
    assert_eq!(rig.vad_stop(1900), [TurnAction::CancelTimer]);
    assert_eq!(rig.m.interruption_stats().vetoed(VetoReason::EchoGuard), 1);
    assert_eq!(rig.m.epoch(), ep(0));
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
}

#[test]
fn the_echo_guard_still_applies_at_the_vad_stop_when_a_word_waives_min_speech() {
    // Two words are needed inside the 500 ms window. One echoed word is substantive (it
    // waives min speech at the stop) but does not lift the echo guard.
    let policy = InterruptionPolicy {
        echo_guard: ms(500),
        echo_min_words: 2,
        min_speech: ms(300),
        ..InterruptionPolicy::default()
    };
    let mut rig = Rig::speaking(policy.clone());
    rig.vad_start(1700);
    rig.interim("hello", 1750);
    assert_eq!(rig.vad_stop(1900), [TurnAction::CancelTimer]);
    let stats = rig.m.interruption_stats();
    assert_eq!(stats.vetoed(VetoReason::EchoGuard), 1);
    assert_eq!(stats.total_vetoed(), 1);
    assert_eq!(rig.m.epoch(), ep(0));
    assert_eq!(rig.kind(), StateKind::BotSpeaking);

    // Two words in the same window: the guard lifts and the short utterance is kept.
    let mut rig = Rig::speaking(policy);
    rig.vad_start(1700);
    rig.interim("hello there", 1750);
    let out = rig.vad_stop(1900);
    assert_eq!(out[..2], [intr(1), start(1)], "{out:?}");
    assert_eq!(rig.m.interruption_stats().total_vetoed(), 0);
}

#[test]
fn echo_window_boundaries() {
    // 499 ms of bot audio: inside. 500 ms: outside (the window is [0, N)).
    let mut rig = Rig::speaking(echo(500));
    assert_eq!(rig.vad_start(1999), [rig.arm(2000)]);
    let mut rig = Rig::speaking(echo(500));
    assert_eq!(rig.vad_start(2000), [intr(1), start(1)]);
    // Long after the window VAD alone interrupts at once.
    let mut rig = Rig::speaking(echo(500));
    assert_eq!(rig.vad_start(5000), [intr(1), start(1)]);
}

#[test]
fn the_echo_guard_does_not_apply_before_the_bot_makes_a_sound() {
    // AwaitingResponse: no bot audio, so there is no echo to guard against.
    let mut rig = Rig::awaiting(echo(500));
    assert_eq!(rig.vad_start(1300), [intr(1), start(1)]);
}

#[test]
fn the_window_starts_when_the_bot_audio_starts_even_with_a_pending_candidate() {
    let policy = InterruptionPolicy {
        min_speech: ms(200),
        echo_guard: ms(500),
        ..InterruptionPolicy::default()
    };
    let mut rig = Rig::awaiting(policy);
    // The candidate begins while the response is still being prepared.
    assert_eq!(rig.vad_start(1400), [rig.arm(1600)]);
    // The bot's audio starts at 1450: the candidate is judged again, now inside the window.
    assert!(
        rig.bot_started(0, 1450).is_empty(),
        "the recheck deadline is unchanged"
    );
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
    // Min speech is satisfied at 1600 but the echo window (to 1950) still blocks; recheck then.
    assert_eq!(rig.tick(1600), [rig.arm(1950)]);
    assert_eq!(rig.tick(1950), [intr(1), start(1)]);
}

// ---------------------------------------------------------------------------------------------
// Candidate life cycle
// ---------------------------------------------------------------------------------------------

#[test]
fn a_candidate_still_blocked_when_the_bot_audio_ends_stays_pending() {
    let mut rig = Rig::speaking(min_speech(300));
    rig.vad_start(2000);
    // The bot's audio ends at 2100 with only 100 ms of speech: the minimum speech guard still
    // blocks, so the candidate is detached (state Idle) and stays pending; the recheck timer is
    // unchanged.
    assert!(rig.bot_ended(0, 2100).is_empty());
    assert_eq!(rig.kind(), StateKind::Idle);
    assert_eq!(rig.m.epoch(), ep(0));
    assert_eq!(
        rig.m.barge_in_candidate().expect("pending").recheck_at(),
        Some(rig.t(2300))
    );
    assert_eq!(rig.m.interruption_stats().promoted(), 0);
    // A duplicate VadStart is still a duplicate.
    assert!(rig.vad_start(2150).is_empty());
    assert_eq!(rig.ignored(InputKind::VadStart), 1);
    // Once the speech guard holds, it is an ordinary user turn: StartUserTurn, no Interrupt.
    assert_eq!(rig.tick(2300), [start(1)]);
    assert_eq!(rig.kind(), StateKind::UserSpeaking);
    assert_eq!(rig.m.epoch(), ep(1));
    assert!(rig.m.barge_in_candidate().is_none());
    let stats = rig.m.interruption_stats();
    assert_eq!((stats.promoted(), stats.confirmed()), (1, 0));
    // And the user's stop continues like any other turn.
    assert!(rig.vad_stop(2600).contains(&TurnAction::ArmTimer {
        deadline: rig.t(3300)
    }));
    assert_eq!(rig.kind(), StateKind::UserPaused);
}

#[test]
fn a_candidate_the_policy_confirms_at_the_bot_audio_end_is_promoted_at_once() {
    // Min speech satisfied (400 ms) when the bot audio ends: nothing is left to cut.
    let mut rig = Rig::speaking(min_speech(300));
    rig.vad_start(2000);
    assert_eq!(rig.bot_ended(0, 2400), [start(1), TurnAction::CancelTimer]);
    assert_eq!(rig.kind(), StateKind::UserSpeaking);
    assert_eq!(rig.m.epoch(), ep(1));
    assert_eq!(rig.m.interruption_stats().promoted(), 1);
    assert_eq!(rig.m.interruption_stats().confirmed(), 0);
}

#[test]
fn a_backchannel_over_the_end_of_the_bot_audio_does_not_become_a_turn() {
    // "...is that clear?" and the user says "mm-hmm" over the last word.
    let mut rig = Rig::speaking(InterruptionPolicy::recommended());
    // 500 ms into the bot audio: outside the 400 ms echo window. The first recheck is the
    // 350 ms wait for a first interim (min speech is 300 ms + 100 ms VAD hangover = 400 ms).
    assert_eq!(rig.vad_start(2000), [rig.arm(2350)]);
    assert_eq!(rig.interim("mm-hmm", 2100), [rig.arm(3500)]);
    // The bot audio ends while VAD is still active: the backchannel still blocks.
    assert!(rig.bot_ended(0, 2200).is_empty());
    assert_eq!(rig.kind(), StateKind::Idle);
    assert_eq!(rig.m.epoch(), ep(0));
    assert!(rig.m.barge_in_candidate().is_some());
    // The stop vetoes it: no turn, no epoch bump, nothing for the LLM.
    assert_eq!(rig.vad_stop(2400), [TurnAction::CancelTimer]);
    assert!(rig.m.barge_in_candidate().is_none());
    assert_eq!(rig.m.epoch(), ep(0));
    assert_eq!(rig.kind(), StateKind::Idle);
    let stats = rig.m.interruption_stats();
    assert_eq!(stats.vetoed(VetoReason::Backchannel), 1);
    assert_eq!((stats.promoted(), stats.confirmed()), (0, 0));
    // The stop's final is consumed; the next utterance is an ordinary turn.
    assert!(rig.fin("mm-hmm", 2450).is_empty());
    assert_eq!(rig.ignored(InputKind::SttFinal), 0);
    assert_eq!(rig.vad_start(5000), [start(1)]);
}

#[test]
fn a_backchannel_that_grows_into_speech_after_the_bot_audio_ended_is_a_turn() {
    let mut rig = Rig::speaking(InterruptionPolicy::recommended());
    rig.vad_start(2000);
    rig.interim("yeah", 2100);
    rig.bot_ended(0, 2200);
    assert_eq!(
        rig.interim("yeah but wait", 2500),
        [start(1), TurnAction::CancelTimer]
    );
    assert_eq!(rig.m.interruption_stats().promoted(), 1);
}

#[test]
fn a_short_burst_over_the_end_of_the_bot_audio_is_vetoed() {
    // A cough (150 ms of VAD, no words) that overlaps the end of the bot's audio.
    let mut rig = Rig::speaking(InterruptionPolicy::recommended());
    rig.vad_start(2000);
    assert!(rig.bot_ended(0, 2100).is_empty());
    assert_eq!(rig.kind(), StateKind::Idle);
    assert_eq!(rig.vad_stop(2150), [TurnAction::CancelTimer]);
    assert_eq!(rig.m.epoch(), ep(0));
    assert_eq!(rig.m.interruption_stats().vetoed(VetoReason::MinSpeech), 1);
    assert_eq!(rig.m.interruption_stats().promoted(), 0);

    // The same with a minimum word count: too few words at the stop.
    let mut rig = Rig::speaking(min_words(2));
    rig.vad_start(2000);
    rig.interim("hm", 2050);
    rig.bot_ended(0, 2100);
    assert_eq!(rig.kind(), StateKind::Idle);
    assert!(rig.vad_stop(2300).is_empty());
    assert_eq!(rig.m.interruption_stats().vetoed(VetoReason::MinWords), 1);
    assert_eq!(rig.m.epoch(), ep(0));
}

#[test]
fn the_bot_audio_ending_without_a_response_keeps_a_blocked_candidate_pending() {
    // AwaitingResponse -> BotAudioEnded (no audio at all): the same detach.
    let mut rig = Rig::awaiting(min_words(2));
    rig.vad_start(1300);
    rig.interim("one", 1350);
    assert!(rig.bot_ended(0, 1400).is_empty());
    assert_eq!(rig.kind(), StateKind::Idle);
    assert!(rig.m.barge_in_candidate().is_some());
    // Words arrive: a plain turn.
    assert_eq!(rig.interim("one two", 1500), [start(1)]);
}

#[test]
fn hangup_with_a_pending_candidate_closes_cleanly() {
    let mut rig = Rig::speaking(min_speech(300));
    rig.vad_start(2000);
    // No user turn was open, so no EndUserTurn; the timer is cancelled.
    assert_eq!(rig.hangup(2100), [TurnAction::CancelTimer]);
    assert_eq!(rig.m.state(), TurnState::Closed);
    assert!(rig.m.barge_in_candidate().is_none());
    assert!(rig.m.armed_deadline().is_none());
    assert!(rig.tick(2300).is_empty());
}

#[test]
fn interims_without_a_candidate_are_still_ignored_and_counted() {
    let mut rig = Rig::speaking(min_speech(300));
    assert!(rig.interim("hello", 2000).is_empty());
    assert_eq!(rig.ignored(InputKind::SttInterim), 1);
    // After a veto too: the candidate is gone.
    rig.vad_start(2100);
    rig.vad_stop(2200);
    assert!(rig.interim("late", 2250).is_empty());
    assert_eq!(rig.ignored(InputKind::SttInterim), 2);
}

#[test]
fn guards_apply_while_the_response_is_pending_as_well() {
    // AwaitingResponse is a barge-in context too (the bot is thinking).
    let mut rig = Rig::awaiting(min_words(2));
    rig.vad_start(1300);
    assert!(rig.interim("one", 1350).is_empty());
    assert_eq!(rig.interim("one two", 1400), [intr(1), start(1)]);
}

#[test]
fn a_stale_epoch_bot_audio_never_touches_a_candidate() {
    let mut rig = Rig::speaking(min_speech(300));
    rig.vad_start(2000);
    assert!(rig.bot_ended(7, 2100).is_empty());
    assert!(rig.bot_started(7, 2100).is_empty());
    assert_eq!(rig.ignored(InputKind::BotAudioEnded), 1);
    assert_eq!(rig.ignored(InputKind::BotAudioStarted), 1);
    assert!(rig.m.barge_in_candidate().is_some());
}

// ---------------------------------------------------------------------------------------------
// Review-fix cases
// ---------------------------------------------------------------------------------------------

#[test]
fn the_echo_window_restarts_when_the_bot_audio_resumes_after_a_gap() {
    let mut rig = Rig::speaking(echo(500));
    // First round: the window [1500, 2000) is over at 2500, so VAD alone interrupts.
    // Second round (a new TTS round after a tool call): audio resumes at 4000.
    assert!(rig.bot_started(0, 4000).is_empty());
    // 200 ms into the second round: inside the restarted window.
    assert_eq!(rig.vad_start(4200), [rig.arm(4500)]);
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
    assert_eq!(rig.m.interruption_stats().confirmed(), 0);
    assert_eq!(rig.tick(4500), [intr(1), start(1)]);
}

#[test]
fn a_repeated_or_late_bot_audio_report_never_moves_the_echo_window_backwards() {
    // Window [1500, 2000). A report older than (or equal to) the stored start is ignored: it
    // must not pull the window back, which would let a VAD-only echo blip through.
    for at in [BOT_AT - 100, BOT_AT] {
        let mut rig = Rig::speaking(echo(500));
        assert!(rig.bot_started(0, at).is_empty());
        assert_eq!(
            rig.ignored(InputKind::BotAudioStarted),
            0,
            "accepted, no effect"
        );
        assert_eq!(
            rig.vad_start(1950),
            [rig.arm(2000)],
            "the window still ends at 2000 (report at {at})"
        );
    }
    // The ignored report did not become the reference of a later gap either: had the report
    // at 1000 been stored, 1700 would be a 700 ms gap and restart the window. It is only 200 ms
    // after the stored 1500, so the first window stands and VAD at 2100 interrupts.
    let mut rig = Rig::speaking(echo(500));
    assert!(rig.bot_started(0, 1000).is_empty());
    assert!(rig.bot_started(0, 1700).is_empty());
    assert_eq!(rig.vad_start(2100), [intr(1), start(1)]);
}

#[test]
fn per_chunk_bot_audio_reports_do_not_keep_the_echo_window_open() {
    // A driver that reports every 20 ms chunk of continuous playback: the window must still
    // end 400 ms after the first audio, so a wordless real interruption can get through.
    let mut rig = Rig::speaking(echo(400));
    for chunk in 1..=50 {
        assert!(rig.bot_started(0, BOT_AT + chunk * 20).is_empty());
    }
    // 1 s into the playback, the window [1500, 1900) is long over.
    assert_eq!(rig.vad_start(2500), [intr(1), start(1)]);
    assert_eq!(rig.m.interruption_stats().confirmed(), 1);
    assert_eq!(rig.ignored(InputKind::BotAudioStarted), 0);
}

#[test]
fn the_echo_window_restarts_only_after_a_gap_of_the_guard_since_the_last_report() {
    let mut rig = Rig::speaking(echo(500));
    // 400 ms after the last report: no gap, the window stays [1500, 2000).
    assert!(rig.bot_started(0, 1900).is_empty());
    // The gap counts from the latest report: 2400 is 500 ms after 1900, so it restarts.
    assert!(rig.bot_started(0, 2400).is_empty());
    assert_eq!(rig.vad_start(2450), [rig.arm(2900)]);

    // And the first report did not restart the window: VAD at 2100 would interrupt.
    let mut rig = Rig::speaking(echo(500));
    assert!(rig.bot_started(0, 1900).is_empty());
    assert_eq!(rig.vad_start(2100), [intr(1), start(1)]);
}

#[test]
fn repeated_bot_audio_reports_are_accepted_without_effect_when_the_echo_guard_is_off() {
    let mut rig = Rig::speaking(min_speech(300));
    for at in [1520, 1520, 1400, 4000] {
        assert!(rig.bot_started(0, at).is_empty());
    }
    assert_eq!(rig.ignored(InputKind::BotAudioStarted), 0);
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
    assert!(rig.m.barge_in_candidate().is_none());
}

#[test]
fn a_cough_is_not_confirmed_by_the_clock_before_the_vad_reports_its_stop() {
    // The VAD reports a stop only after its hangover (100 ms) and backdates it. A 220 ms cough
    // against min_speech 300: its stop (at = 2220) is delivered at 2320.
    let hang = InterruptionPolicy {
        vad_hangover: ms(100),
        ..min_speech(300)
    };
    let mut rig = Rig::speaking(hang.clone());
    // The recheck waits for the hangover as well: 300 + 100.
    assert_eq!(rig.vad_start(2000), [rig.arm(2400)]);
    // A tick before that (a timer that fires early) re-arms and decides nothing.
    assert_eq!(rig.tick(2300), [rig.arm(2400)]);
    assert_eq!(rig.vad_stop(2220), [TurnAction::CancelTimer]);
    assert_eq!(rig.m.interruption_stats().vetoed(VetoReason::MinSpeech), 1);
    assert_eq!(rig.m.interruption_stats().confirmed(), 0);
    assert_eq!(rig.m.epoch(), ep(0));

    // Real speech that lasts: confirmed by the tick at min_speech + hangover.
    let mut rig = Rig::speaking(hang.clone());
    rig.vad_start(2000);
    assert_eq!(rig.tick(2400), [intr(1), start(1)]);

    // Other evidence before the stop cannot skip the hangover either: an interim at 2310
    // (310 ms on the clock, 210 ms of speech) does not confirm.
    let mut rig = Rig::speaking(hang);
    rig.vad_start(2000);
    let out = rig.interim("uh", 2310);
    assert!(!out.contains(&intr(1)), "{out:?}");
    assert_eq!(rig.kind(), StateKind::BotSpeaking);

    // Without a hangover configured the same cough IS confirmed at 2300 (the documented
    // default: the VAD is assumed to report stops without delay).
    let mut rig = Rig::speaking(min_speech(300));
    assert_eq!(rig.vad_start(2000), [rig.arm(2300)]);
    assert_eq!(rig.tick(2300), [intr(1), start(1)]);
}

#[test]
fn the_vad_hangover_does_not_shorten_what_the_stop_path_measures() {
    // At the stop the duration is backdated and exact: nothing is subtracted a second time.
    let hang = InterruptionPolicy {
        vad_hangover: ms(100),
        ..min_speech(300)
    };
    let mut rig = Rig::speaking(hang);
    rig.vad_start(2000);
    // 300 ms of speech, stop delivered at 2400 but dated 2300: confirmed at the stop.
    let out = rig.vad_stop(2300);
    assert_eq!(out[..2], [intr(1), start(1)], "{out:?}");
}

#[test]
fn a_restart_of_the_echo_window_re_judges_a_pending_candidate() {
    let policy = InterruptionPolicy {
        min_speech: ms(1000),
        echo_guard: ms(500),
        ..InterruptionPolicy::default()
    };
    let mut rig = Rig::speaking(policy);
    // VAD starts at 2600, past the first window: only min speech (until 3600) blocks.
    assert_eq!(rig.vad_start(2600), [rig.arm(3600)]);
    // The audio resumes at 3000: the restarted window (to 3500) is the next recheck.
    assert_eq!(rig.bot_started(0, 3000), [rig.arm(3500)]);
    assert_eq!(rig.tick(3500), [rig.arm(3600)]);
    assert_eq!(rig.tick(3600), [intr(1), start(1)]);
}

#[test]
fn backchannels_with_a_zero_wait_use_the_default_wait() {
    // `backchannel_wait` left at zero must not turn the guard into a no-op.
    let policy = InterruptionPolicy {
        backchannels: vec![BackchannelSet::english()],
        ..InterruptionPolicy::default()
    };
    assert!(!policy.is_disabled());
    let mut rig = Rig::speaking(policy);
    // The candidate waits for a first interim (350 ms by default) instead of interrupting.
    assert_eq!(rig.vad_start(2000), [rig.arm(2350)]);
    let out = rig.interim("yeah", 2100);
    assert!(!out.contains(&intr(1)), "{out:?}");
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
    assert_eq!(rig.m.epoch(), ep(0));
    // Without any interim the wait ends and the user may interrupt.
    let policy = InterruptionPolicy {
        backchannels: vec![BackchannelSet::english()],
        ..InterruptionPolicy::default()
    };
    let mut rig = Rig::speaking(policy);
    rig.vad_start(2000);
    assert_eq!(rig.tick(2350), [intr(1), start(1)]);
}

#[test]
fn a_stale_backchannel_interim_cannot_swallow_a_long_utterance() {
    // "okay" arrives, then STT stalls while the user keeps talking for seconds.
    let mut rig = Rig::speaking(InterruptionPolicy::recommended());
    assert_eq!(rig.vad_start(2000), [rig.arm(2350)]);
    // The backchannel blocks, but only until the cap (1.5 s of VAD).
    assert_eq!(rig.interim("okay", 2100), [rig.arm(3500)]);
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
    assert_eq!(rig.tick(3500), [intr(1), start(1)]);
    assert_eq!(rig.m.epoch(), ep(1));

    // The same when the tick is late and the stop comes first.
    let mut rig = Rig::speaking(InterruptionPolicy::recommended());
    rig.vad_start(2000);
    rig.interim("okay", 2100);
    let out = rig.vad_stop(7000);
    assert_eq!(out[..2], [intr(1), start(1)]);
    assert_eq!(rig.m.interruption_stats().total_vetoed(), 0);

    // A short backchannel is still vetoed.
    let mut rig = Rig::speaking(InterruptionPolicy::recommended());
    rig.vad_start(2000);
    rig.interim("okay", 2100);
    rig.vad_stop(2800);
    assert_eq!(
        rig.m.interruption_stats().vetoed(VetoReason::Backchannel),
        1
    );
}

#[test]
fn a_short_substantive_word_is_not_lost_to_the_speech_guard() {
    // "stop" in 250 ms with the recommended policy: the utterance is over and STT heard a word.
    let mut rig = Rig::speaking(InterruptionPolicy::recommended());
    assert_eq!(rig.vad_start(2000), [rig.arm(2350)]);
    // The word ends the transcript wait; min speech (300 + 100 hangover) is the only guard left.
    assert_eq!(rig.interim("stop", 2100), [rig.arm(2400)]);
    assert_eq!(
        rig.vad_stop(2250),
        [intr(1), start(1), rig.arm(2950)],
        "interrupt, then the ordinary pause"
    );
    assert_eq!(rig.m.interruption_stats().total_vetoed(), 0);

    // Without a transcript the same burst is a veto: the documented cost of the guard.
    let mut rig = Rig::speaking(InterruptionPolicy::recommended());
    rig.vad_start(2000);
    assert_eq!(rig.vad_stop(2250), [TurnAction::CancelTimer]);
    assert_eq!(rig.m.interruption_stats().vetoed(VetoReason::MinSpeech), 1);
    assert_eq!(rig.m.epoch(), ep(0));
}

#[test]
fn a_huge_interim_is_rejected_without_tokenising_all_of_it() {
    let p = InterruptionPolicy::recommended();
    let huge = "yeah ".repeat(200_000);
    assert!(!p.is_only_backchannel(&huge));
    assert_eq!(p.classify(&huge), TextKind::Speech);
    // Exactly at the bound still matches, one past does not (the early exit is exact).
    assert!(p.is_only_backchannel(&"yeah ".repeat(16)));
    assert!(!p.is_only_backchannel(&"yeah ".repeat(17)));
    assert!(!p.is_only_backchannel(&format!("{}yeah", "yeah ".repeat(16))));
}

/// An adaptive machine (3 s fallback, optional required final) with `ip`.
fn adaptive_rig(ip: InterruptionPolicy, require_final: bool) -> Rig {
    Rig {
        m: TurnMachine::with_interruption(
            TurnPolicy::Adaptive {
                fallback_silence: ms(3000),
                require_final,
                final_grace: ms(800),
            },
            ip,
        ),
        base: origin(),
        out: Vec::new(),
    }
}

fn verdict(id: u64, complete: bool, rig: &Rig, n: u64) -> TurnInput {
    TurnInput::Verdict {
        request: AnalysisId::new(id),
        complete,
        probability: 0.9,
        at: rig.t(n),
    }
}

#[test]
fn a_late_verdict_while_a_candidate_is_pending_is_ignored() {
    let mut rig = adaptive_rig(min_speech(300), false);
    rig.vad_start(0);
    rig.vad_stop(400);
    rig.fin("hello", 450);
    // The turn ends by the fallback; a verdict for the first pause never arrived.
    assert_eq!(rig.tick(3400).len(), 1);
    assert_eq!(rig.kind(), StateKind::AwaitingResponse);
    assert_eq!(rig.vad_start(4000), [rig.arm(4300)]);
    // The late verdict does not touch the candidate or the timer.
    let v = verdict(1, true, &rig, 4100);
    assert!(rig.feed(v).is_empty());
    assert_eq!(rig.ignored(InputKind::Verdict), 1);
    assert_eq!(
        rig.m.barge_in_candidate().expect("pending").recheck_at(),
        Some(rig.t(4300))
    );
    assert_eq!(rig.m.armed_deadline(), Some(rig.t(4300)));
    // The candidate resolves normally; the old verdict is still stale afterwards.
    assert_eq!(rig.tick(4300), [intr(1), start(1)]);
    let v = verdict(1, true, &rig, 4400);
    assert!(rig.feed(v).is_empty());
    assert_eq!(rig.ignored(InputKind::Verdict), 2);
}

#[test]
fn a_stale_tick_while_a_candidate_is_pending_re_arms_the_recheck() {
    let mut rig = Rig::awaiting(min_speech(300));
    // The pause timer was cancelled at 1100; a tick of it is still in flight.
    assert_eq!(rig.vad_start(2000), [rig.arm(2300)]);
    assert_eq!(rig.tick(1150), [rig.arm(2300)], "consumed, then re-armed");
    assert_eq!(rig.m.armed_deadline(), Some(rig.t(2300)));
    assert!(rig.m.barge_in_candidate().is_some());
    assert_eq!(rig.m.epoch(), ep(0));
    assert_eq!(rig.tick(2300), [intr(1), start(1)]);
}

#[test]
fn a_final_of_the_previous_turn_while_a_candidate_is_pending_is_consumed() {
    // The first turn ended before its final arrived.
    let mut rig = Rig::new(min_speech(300));
    rig.vad_start(0);
    rig.vad_stop(400);
    rig.tick(1100);
    assert_eq!(rig.kind(), StateKind::AwaitingResponse);
    assert_eq!(rig.vad_start(2000), [rig.arm(2300)]);
    // The old final answers the old stop: consumed without effect, the candidate is untouched.
    assert!(rig.fin("hello", 2050).is_empty());
    assert_eq!(rig.ignored(InputKind::SttFinal), 0);
    assert!(rig.m.barge_in_candidate().is_some());
    // The candidate's own stop (vetoed) is owed a final, which is consumed too.
    assert_eq!(rig.vad_stop(2200), [TurnAction::CancelTimer]);
    assert!(rig.fin("hm", 2250).is_empty());
    assert_eq!(rig.ignored(InputKind::SttFinal), 0);
    // And nothing more is owed: a stray final is counted.
    assert!(rig.fin("stray", 2300).is_empty());
    assert_eq!(rig.ignored(InputKind::SttFinal), 1);
}

#[test]
fn a_vetoed_stops_final_does_not_end_the_next_turns_pause() {
    // Adaptive with a required final: the next real turn must wait for its OWN final.
    let mut rig = adaptive_rig(min_speech(300), true);
    rig.vad_start(0);
    rig.vad_stop(400);
    rig.fin("hello.", 450);
    let out = rig.feed(verdict(1, true, &rig, 500));
    assert_eq!(
        out,
        [
            TurnAction::CancelTimer,
            TurnAction::EndUserTurn {
                epoch: ep(0),
                reason: EndReason::Verdict
            }
        ]
    );
    assert!(rig.bot_started(0, 1500).is_empty());
    // A vetoed burst (owes one final) ...
    rig.vad_start(2000);
    assert_eq!(rig.vad_stop(2100), [TurnAction::CancelTimer]);
    // ... then a real barge-in whose stop is owed the second final.
    rig.vad_start(3000);
    assert_eq!(rig.tick(3300), [intr(1), start(1)]);
    let out = rig.vad_stop(3800);
    assert_eq!(
        out[0],
        TurnAction::RequestAnalysis {
            id: AnalysisId::new(2)
        }
    );
    // The vetoed stop's final arrives first: it answers an earlier stop and is consumed.
    assert!(rig.fin("hm", 3850).is_empty());
    assert_eq!(rig.ignored(InputKind::SttFinal), 0);
    // A complete verdict must still wait for the pause's own final.
    assert!(
        !rig.feed(verdict(2, true, &rig, 3900))
            .iter()
            .any(|a| matches!(a, TurnAction::EndUserTurn { .. }))
    );
    assert_eq!(rig.kind(), StateKind::UserPaused);
    // The pause's own final ends it.
    let out = rig.fin("stop it.", 3950);
    assert_eq!(
        out.last(),
        Some(&TurnAction::EndUserTurn {
            epoch: ep(1),
            reason: EndReason::Verdict
        })
    );
    assert_eq!(rig.ignored(InputKind::SttFinal), 0);
}

// ---------------------------------------------------------------------------------------------
// The pure policy
// ---------------------------------------------------------------------------------------------

fn obs(speech: u64, words: u32, text: TextKind, bot_age: Option<u64>) -> Observation {
    Observation {
        speech: ms(speech),
        words,
        text,
        bot_audio_age: bot_age.map(ms),
    }
}

#[test]
fn assess_table() {
    use Assessment::{Blocked, Confirmed};
    use TextKind::{Backchannel, None as NoText, Speech};
    let rec = InterruptionPolicy::recommended();
    // The transcript wait on its own (recommended()'s 400 ms min speech hides it).
    let waiting = backchannels();
    // recommended: 300 ms speech (+100 ms VAD hangover live), 400 ms echo window with 1 word,
    // backchannels, 350 ms wait.
    let rows: &[(&str, &InterruptionPolicy, Observation, Assessment)] = &[
        (
            "off",
            &InterruptionPolicy::default(),
            obs(0, 0, NoText, Some(0)),
            Confirmed,
        ),
        (
            "short",
            &rec,
            obs(100, 3, Speech, None),
            Blocked(VetoReason::MinSpeech),
        ),
        // Live, the VAD hangover (100 ms) is not speech: 300 ms of min speech needs 400 ms.
        (
            "min speech not proven before the hangover",
            &rec,
            obs(399, 1, Speech, None),
            Blocked(VetoReason::MinSpeech),
        ),
        ("long enough", &rec, obs(400, 1, Speech, None), Confirmed),
        (
            "backchannel wins",
            &rec,
            obs(1000, 9, Backchannel, None),
            Blocked(VetoReason::Backchannel),
        ),
        (
            "a stale backchannel stops blocking at the cap",
            &rec,
            obs(1500, 9, Backchannel, None),
            Confirmed,
        ),
        (
            "echo, no words",
            &rec,
            obs(350, 0, NoText, Some(100)),
            Blocked(VetoReason::EchoGuard),
        ),
        (
            "echo, words",
            &rec,
            obs(400, 1, Speech, Some(100)),
            Confirmed,
        ),
        (
            "after echo window",
            &rec,
            obs(400, 0, Speech, Some(400)),
            Confirmed,
        ),
        (
            "no text yet",
            &waiting,
            obs(320, 0, NoText, None),
            Blocked(VetoReason::Unconfirmed),
        ),
        (
            "no text, wait over",
            &waiting,
            obs(400, 0, NoText, None),
            Confirmed,
        ),
    ];
    for (name, policy, o, expected) in rows {
        assert_eq!(&policy.assess(o), expected, "{name}");
    }
}

#[test]
fn assess_at_stop_table() {
    use Assessment::{Blocked, Confirmed};
    use TextKind::{Backchannel, None as NoText, Speech};
    // min_speech (2 s) above the backchannel cap (1.5 s), so the waiver is observable.
    let p = InterruptionPolicy {
        min_speech: ms(2000),
        echo_guard: ms(500),
        echo_min_words: 2,
        backchannels: vec![BackchannelSet::english()],
        backchannel_wait: ms(350),
        backchannel_max_speech: ms(1500),
        ..InterruptionPolicy::default()
    };
    let rows: &[(&str, Observation, Assessment)] = &[
        (
            "a short substantive word waives min speech",
            obs(100, 1, Speech, None),
            Confirmed,
        ),
        (
            "no word, no waiver",
            obs(100, 0, Speech, None),
            Blocked(VetoReason::MinSpeech),
        ),
        (
            "a short backchannel stays blocked",
            obs(400, 1, Backchannel, None),
            Blocked(VetoReason::Backchannel),
        ),
        (
            "a backchannel past the cap is substantive and waives min speech",
            obs(1600, 1, Backchannel, None),
            Confirmed,
        ),
        (
            "the echo guard still applies to a substantive word",
            obs(300, 1, Speech, Some(200)),
            Blocked(VetoReason::EchoGuard),
        ),
        (
            "two words lift the echo guard",
            obs(300, 2, Speech, Some(200)),
            Confirmed,
        ),
        (
            "no text and no word: min speech blocks",
            obs(100, 0, NoText, None),
            Blocked(VetoReason::MinSpeech),
        ),
    ];
    for (name, o, expected) in rows {
        assert_eq!(&p.assess_at_stop(o), expected, "{name}");
    }
    // The live assessment of the same observations has no waiver.
    assert_eq!(
        p.assess(&obs(1600, 1, Backchannel, None)),
        Blocked(VetoReason::MinSpeech)
    );
    assert_eq!(
        p.assess(&obs(100, 1, Speech, None)),
        Blocked(VetoReason::MinSpeech)
    );

    // The minimum word count is an explicit choice: never waived.
    let words = InterruptionPolicy {
        min_words: 3,
        ..p.clone()
    };
    assert_eq!(
        words.assess_at_stop(&obs(100, 1, Speech, None)),
        Blocked(VetoReason::MinWords)
    );
    // The transcript wait still applies at the stop when min speech is off.
    let wait = backchannels();
    assert_eq!(
        wait.assess_at_stop(&obs(100, 0, NoText, None)),
        Blocked(VetoReason::Unconfirmed)
    );
    // The hangover is not subtracted at the stop (the stop time is already backdated).
    let hang = InterruptionPolicy {
        vad_hangover: ms(100),
        ..min_speech(300)
    };
    assert_eq!(hang.assess_at_stop(&obs(300, 0, NoText, None)), Confirmed);
    assert_eq!(
        hang.assess(&obs(300, 0, NoText, None)),
        Blocked(VetoReason::MinSpeech)
    );
}

#[test]
fn echo_min_words_zero_still_needs_one_word() {
    let p = InterruptionPolicy {
        echo_guard: ms(500),
        echo_min_words: 0,
        ..InterruptionPolicy::default()
    };
    assert_eq!(
        p.assess(&obs(100, 0, TextKind::None, Some(100))),
        Assessment::Blocked(VetoReason::EchoGuard)
    );
    assert_eq!(
        p.assess(&obs(100, 1, TextKind::Speech, Some(100))),
        Assessment::Confirmed
    );
}

#[test]
fn recheck_in_names_the_next_time_driven_change() {
    let rec = InterruptionPolicy::recommended();
    // 100 ms in: min speech (300 + 100 hangover) in 300, echo window (age 50 of 400) in 350,
    // wait in 250.
    assert_eq!(
        rec.recheck_in(&obs(100, 0, TextKind::None, Some(50))),
        Some(ms(250))
    );
    // Min speech alone: 300 ms + the 100 ms hangover.
    assert_eq!(
        min_speech(300).recheck_in(&obs(100, 0, TextKind::None, None)),
        Some(ms(200))
    );
    let hang = InterruptionPolicy {
        vad_hangover: ms(100),
        ..min_speech(300)
    };
    assert_eq!(
        hang.recheck_in(&obs(100, 0, TextKind::None, None)),
        Some(ms(300))
    );
    // A backchannel waits for new text, or for the cap (1.5 s of VAD).
    assert_eq!(
        rec.recheck_in(&obs(100, 1, TextKind::Backchannel, None)),
        Some(ms(1400))
    );
    // Without a cap it waits for new text only.
    let uncapped = InterruptionPolicy {
        backchannel_max_speech: Duration::ZERO,
        ..InterruptionPolicy::recommended()
    };
    assert_eq!(
        uncapped.recheck_in(&obs(100, 1, TextKind::Backchannel, None)),
        None
    );
    // Past every time guard: nothing to wait for.
    assert_eq!(
        rec.recheck_in(&obs(900, 0, TextKind::Speech, Some(900))),
        None
    );
    // Only the echo window is left.
    assert_eq!(
        rec.recheck_in(&obs(400, 0, TextKind::Speech, Some(100))),
        Some(ms(300))
    );
    // Words are not time.
    assert_eq!(
        min_words(2).recheck_in(&obs(0, 0, TextKind::None, None)),
        None
    );
    // Pure: the same observation, the same answer.
    let o = obs(100, 0, TextKind::None, Some(50));
    assert_eq!(rec.recheck_in(&o), rec.recheck_in(&o));
    assert_eq!(rec.assess(&o), rec.assess(&o));
}

#[test]
fn classify_and_count_words() {
    let p = InterruptionPolicy::recommended();
    assert_eq!(p.classify(""), TextKind::None);
    assert_eq!(p.classify(" ... "), TextKind::None);
    assert_eq!(p.classify("Yeah."), TextKind::Backchannel);
    assert_eq!(p.classify("Yeah, but no."), TextKind::Speech);
    // Without backchannels configured everything with a word is speech.
    assert_eq!(
        InterruptionPolicy::default().classify("yeah"),
        TextKind::Speech
    );

    assert_eq!(count_words(""), 0);
    assert_eq!(count_words("  -  "), 0);
    assert_eq!(count_words("uh-huh"), 1);
    assert_eq!(count_words("wait a second"), 3);
    assert_eq!(count_words("bir dakika lütfen."), 3);
}

#[test]
fn backchannel_matching_is_bounded_and_normalised() {
    let p = InterruptionPolicy::recommended();
    // 16 tokens still match, 17 never do (cost bound).
    let sixteen = "yeah ".repeat(16);
    let seventeen = "yeah ".repeat(17);
    assert!(p.is_only_backchannel(&sixteen));
    assert!(!p.is_only_backchannel(&seventeen));
    assert!(!p.is_only_backchannel(""));
    // Case, punctuation and hyphens do not matter.
    for text in ["UH-HUH!", "uh... huh", "Mm-Hmm", "mmhmm", "Hı-Hı", "TAMAM."] {
        assert!(p.is_only_backchannel(text), "{text}");
    }
    // The Turkish dotless i is distinct: "hi hi" is not "hı hı".
    assert!(!p.is_only_backchannel("hi hi"));
    // The dotted capital İ folds to i.
    let set = BackchannelSet::new(Language::Tr, ["iyi"]);
    assert!(set.contains("İyi"));
    assert!(set.contains("IYI"));
}

#[test]
fn default_backchannel_lists_hold_the_documented_phrases() {
    let en = BackchannelSet::english();
    for phrase in ["yeah", "uh-huh", "okay", "right", "mm-hmm"] {
        assert!(en.contains(phrase), "en: {phrase}");
    }
    let tr = BackchannelSet::turkish();
    for phrase in ["evet", "hı-hı", "tamam", "he", "aynen", "anladım"] {
        assert!(tr.contains(phrase), "tr: {phrase}");
    }
    // Answers are not backchannels.
    assert!(!en.contains("yes") && !en.contains("no"));
    assert!(!tr.contains("hayır"));
    assert_eq!(en.language(), Language::En);
    assert_eq!(tr.language(), Language::Tr);
    assert!(!en.is_empty() && en.len() > 5);
    assert!(BackchannelSet::new(Language::En, ["", "  ", "--"]).is_empty());
}

// ---------------------------------------------------------------------------------------------
// Invariants over random inputs
// ---------------------------------------------------------------------------------------------

/// A small deterministic generator.
struct Lcg(u64);

impl Lcg {
    fn draw(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.draw() % n
    }
}

fn fuzz_policies() -> Vec<(&'static str, InterruptionPolicy)> {
    vec![
        ("recommended", InterruptionPolicy::recommended()),
        (
            "any",
            InterruptionPolicy {
                min_speech: ms(500),
                min_words: 2,
                confirm: Confirm::Any,
                echo_guard: ms(300),
                echo_min_words: 2,
                ..InterruptionPolicy::default()
            },
        ),
        ("echo", echo(600)),
        ("words", min_words(3)),
        (
            "hangover",
            InterruptionPolicy {
                min_speech: ms(300),
                echo_guard: ms(400),
                vad_hangover: ms(150),
                ..InterruptionPolicy::default()
            },
        ),
    ]
}

#[test]
fn random_inputs_keep_every_invariant_with_guards_on() {
    let base = origin();
    let texts = [
        "yeah",
        "wait a second",
        "hello",
        "uh-huh",
        "",
        "tamam",
        "hayır dur",
        "one two three four",
    ];
    for (name, ip) in fuzz_policies() {
        for turn_policy in [TurnPolicy::fixed(ms(700)), TurnPolicy::adaptive()] {
            let mut rng = Lcg(0x5EED_0027 ^ name.len() as u64);
            let mut m = TurnMachine::with_interruption(turn_policy, ip.clone());
            let mut model: Option<Instant> = None;
            let mut open = false;
            let mut last_id = 0u64;
            let mut now = base;
            let mut out = Vec::new();
            let mut dropped_by_hangup = 0u64;
            let mut confirmed_seen = 0u64;
            for step in 0..20_000 {
                now += ms(rng.below(400));
                if m.state() == TurnState::Closed && rng.below(3) == 0 {
                    m = TurnMachine::with_interruption(turn_policy, ip.clone());
                    model = None;
                    open = false;
                    last_id = 0;
                    dropped_by_hangup = 0;
                    confirmed_seen = 0;
                    continue;
                }
                let epoch_before = m.epoch();
                let candidate_before = m.barge_in_candidate().is_some();
                let vetoed_before = m.interruption_stats().total_vetoed();
                let input = if rng.below(80) == 0 {
                    TurnInput::Hangup { at: now }
                } else {
                    match rng.below(11) {
                        0 | 1 => TurnInput::VadStart { at: now },
                        2 => TurnInput::VadStop { at: now },
                        3 | 4 => TurnInput::SttInterim {
                            text: texts[rng.below(texts.len() as u64) as usize].into(),
                            at: now,
                        },
                        5 => TurnInput::SttFinal {
                            text: "ok.".into(),
                            at: now,
                        },
                        6 => TurnInput::Verdict {
                            request: AnalysisId::new(if last_id > 0 && rng.below(2) == 0 {
                                last_id
                            } else {
                                1 + rng.below(4)
                            }),
                            complete: rng.below(2) == 0,
                            probability: 0.5,
                            at: now,
                        },
                        7 => {
                            let when = match model {
                                Some(d) => match rng.below(3) {
                                    0 => d,
                                    1 => d + ms(1 + rng.below(3000)),
                                    _ => now,
                                },
                                None => now,
                            };
                            TurnInput::Tick { now: when }
                        }
                        8 => TurnInput::BotAudioStarted {
                            epoch: ep(m.epoch().get().saturating_sub(rng.below(2) as u32)),
                            at: now,
                        },
                        9 => TurnInput::BotAudioEnded {
                            epoch: ep(m.epoch().get().saturating_sub(rng.below(2) as u32)),
                            at: now,
                        },
                        _ => TurnInput::VadStart { at: now },
                    }
                };
                let is_hangup = matches!(input, TurnInput::Hangup { .. });
                if let TurnInput::Tick { now: fired } = input
                    && model.is_some_and(|d| fired >= d)
                {
                    model = None;
                }
                out.clear();
                m.handle(input, &mut out);
                if is_hangup && candidate_before {
                    dropped_by_hangup += 1;
                }

                let arms = out
                    .iter()
                    .filter(|a| matches!(a, TurnAction::ArmTimer { .. }))
                    .count();
                assert!(
                    arms <= 1,
                    "{name} step {step}: at most one ArmTimer: {out:?}"
                );
                let mut started = false;
                for (i, action) in out.iter().enumerate() {
                    match action {
                        TurnAction::ArmTimer { deadline } => model = Some(*deadline),
                        TurnAction::CancelTimer => {
                            assert!(model.is_some(), "{name}: cancel without a timer");
                            model = None;
                        }
                        TurnAction::StartUserTurn { epoch } => {
                            assert!(!open, "{name}: turn already open");
                            open = true;
                            started = true;
                            assert_eq!(*epoch, m.epoch(), "{name}: turn epoch");
                        }
                        TurnAction::EndUserTurn { .. } => {
                            assert!(open, "{name}: end without a turn");
                            open = false;
                        }
                        TurnAction::Interrupt { new_epoch } => {
                            assert_eq!(*new_epoch, m.epoch(), "{name}: interrupt epoch");
                            assert_eq!(
                                out.get(i + 1),
                                Some(&TurnAction::StartUserTurn { epoch: *new_epoch }),
                                "{name}: Interrupt is followed by StartUserTurn"
                            );
                            confirmed_seen += 1;
                        }
                        TurnAction::RequestAnalysis { id } => {
                            assert!(id.get() > last_id, "{name}: ids increase");
                            last_id = id.get();
                        }
                        TurnAction::StartBotTurn { .. } => {
                            panic!("{name}: no bot line was asked for")
                        }
                    }
                }
                // The epoch moves only with a user turn start, and by at most one.
                if started {
                    assert!(m.epoch().get() <= epoch_before.get() + 1, "{name}");
                } else {
                    assert_eq!(
                        m.epoch(),
                        epoch_before,
                        "{name}: epoch moved without a turn"
                    );
                }
                // A veto leaves no Interrupt, no turn and no epoch change.
                let vetoed_now = m.interruption_stats().total_vetoed();
                if vetoed_now > vetoed_before && !started {
                    assert!(
                        !out.iter()
                            .any(|a| matches!(a, TurnAction::Interrupt { .. })),
                        "{name}"
                    );
                }
                // The timer slot agrees with the actions, and is held only by a pause or a
                // pending candidate (whose recheck it is).
                assert_eq!(m.armed_deadline(), model, "{name} step {step}: timer slot");
                match (m.state(), m.barge_in_candidate()) {
                    (TurnState::UserPaused(_), None) => {
                        assert!(model.is_some(), "{name}: pause is timed");
                    }
                    (
                        TurnState::Idle | TurnState::AwaitingResponse | TurnState::BotSpeaking,
                        Some(c),
                    ) => {
                        assert_eq!(c.recheck_at(), model, "{name}: candidate timer");
                    }
                    (_, Some(_)) => panic!("{name}: candidate while a user turn is open"),
                    (_, None) => assert!(model.is_none(), "{name}: stray timer"),
                }
                assert_eq!(
                    open,
                    matches!(
                        m.state(),
                        TurnState::UserSpeaking | TurnState::UserPaused(_)
                    ),
                    "{name}: a turn is open exactly while the user is in one"
                );
                // Every candidate has exactly one outcome (or is still pending).
                let s = m.interruption_stats();
                assert_eq!(
                    s.candidates(),
                    s.confirmed()
                        + s.promoted()
                        + s.total_vetoed()
                        + u64::from(m.barge_in_candidate().is_some())
                        + dropped_by_hangup,
                    "{name} step {step}: candidate accounting"
                );
            }
            let s = m.interruption_stats();
            assert_eq!(
                s.confirmed(),
                confirmed_seen,
                "{name}: confirmed == Interrupt actions"
            );
        }
    }
}

/// Exercises every outcome at least once, so the accounting above is not vacuous.
#[test]
fn the_fuzz_policies_reach_every_outcome() {
    let base = origin();
    let texts = [
        "yeah",
        "wait a second",
        "hello",
        "tamam",
        "one two three four",
    ];
    let mut confirmed = 0u64;
    let mut promoted = 0u64;
    let mut vetoed = [0u64; 5];
    for (_, ip) in fuzz_policies() {
        let mut rng = Lcg(0xFEED_0027);
        let mut m = TurnMachine::with_interruption(TurnPolicy::fixed(ms(700)), ip);
        let mut model: Option<Instant> = None;
        let mut now = base;
        let mut out = Vec::new();
        for _ in 0..30_000 {
            now += ms(rng.below(400));
            if m.state() == TurnState::Closed {
                break;
            }
            let input = match rng.below(10) {
                0..=2 => TurnInput::VadStart { at: now },
                3 => TurnInput::VadStop { at: now },
                4 | 5 => TurnInput::SttInterim {
                    text: texts[rng.below(texts.len() as u64) as usize].into(),
                    at: now,
                },
                6 => TurnInput::SttFinal {
                    text: "ok".into(),
                    at: now,
                },
                7 => TurnInput::Tick {
                    now: model.unwrap_or(now).max(now),
                },
                8 => TurnInput::BotAudioStarted {
                    epoch: m.epoch(),
                    at: now,
                },
                _ => TurnInput::BotAudioEnded {
                    epoch: m.epoch(),
                    at: now,
                },
            };
            if let TurnInput::Tick { now: fired } = input
                && model.is_some_and(|d| fired >= d)
            {
                model = None;
            }
            out.clear();
            m.handle(input, &mut out);
            for action in &out {
                match action {
                    TurnAction::ArmTimer { deadline } => model = Some(*deadline),
                    TurnAction::CancelTimer => model = None,
                    _ => {}
                }
            }
        }
        let s = m.interruption_stats();
        confirmed += s.confirmed();
        promoted += s.promoted();
        for reason in VetoReason::ALL {
            let slot = &mut vetoed[VetoReason::ALL.iter().position(|r| *r == reason).unwrap()];
            *slot += s.vetoed(reason);
        }
    }
    assert!(confirmed > 0, "no confirmed barge-in");
    assert!(promoted > 0, "no promoted candidate");
    for (reason, count) in VetoReason::ALL.iter().zip(vetoed) {
        assert!(
            count > 0 || *reason == VetoReason::Unconfirmed,
            "no veto for {reason:?}"
        );
    }
}

/// A typed message while a barge-in candidate is pending means the user is talking: the
/// candidate is confirmed at once (the barge-in), the turn stays open for the speech (the caller
/// adds the text to it), and a later typed message in the open turn is the caller's business.
#[test]
fn a_typed_message_confirms_a_pending_barge_in_candidate() {
    let mut rig = Rig::speaking(min_speech(500));
    // The guard holds the candidate (300 ms of speech is not enough yet).
    let out = rig.vad_start(2000);
    assert!(
        out.iter()
            .all(|a| !matches!(a, TurnAction::Interrupt { .. })),
        "{out:?}"
    );
    assert!(rig.m.barge_in_candidate().is_some());
    let at = rig.t(2100);
    let out = rig.feed(TurnInput::UserText { at });
    assert_eq!(
        out.iter()
            .filter(|a| !matches!(a, TurnAction::ArmTimer { .. } | TurnAction::CancelTimer))
            .copied()
            .collect::<Vec<_>>(),
        [
            TurnAction::Interrupt { new_epoch: ep(1) },
            TurnAction::StartUserTurn { epoch: ep(1) },
        ]
    );
    assert_eq!(rig.kind(), StateKind::UserSpeaking);
    assert!(rig.m.barge_in_candidate().is_none());
    assert_eq!(rig.m.interruption_stats().confirmed(), 1);
    // In the open turn the machine ignores the next typed message.
    let at = rig.t(2200);
    assert!(rig.feed(TurnInput::UserText { at }).is_empty());
    assert_eq!(rig.ignored(InputKind::UserText), 1);
    assert_eq!(rig.kind(), StateKind::UserSpeaking);
}

/// Without a candidate a typed message while the bot speaks is a barge-in whose turn
/// ends at once, and the bot's audio of the old epoch no longer counts.
#[test]
fn a_typed_message_interrupts_the_bot_and_ends_its_turn_at_once() {
    let mut rig = Rig::speaking(InterruptionPolicy::disabled());
    let at = rig.t(2000);
    assert_eq!(
        rig.feed(TurnInput::UserText { at }),
        [
            TurnAction::Interrupt { new_epoch: ep(1) },
            TurnAction::StartUserTurn { epoch: ep(1) },
            TurnAction::EndUserTurn {
                epoch: ep(1),
                reason: EndReason::Text
            },
        ]
    );
    assert_eq!(rig.kind(), StateKind::AwaitingResponse);
    // The old epoch's end is stale; the new response's audio is the current one.
    assert!(rig.bot_ended(0, 2100).is_empty());
    assert_eq!(rig.kind(), StateKind::AwaitingResponse);
    assert!(rig.bot_started(1, 2200).is_empty());
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
}

// -- bot lines ------------------------------------------------------------------------

fn bot_turn(rig: &mut Rig, interrupt: bool, n: u64) -> Vec<TurnAction> {
    let at = rig.t(n);
    rig.feed(TurnInput::BotTurn { interrupt, at })
}

/// A bot line from `Idle` is a response of a new epoch: its audio moves the machine to
/// `BotSpeaking`, the caller can barge in on it, and its end returns the machine to `Idle`.
#[test]
fn a_bot_line_from_idle_is_a_response_the_user_can_barge_in_on() {
    let mut rig = Rig::speaking(InterruptionPolicy::default());
    assert!(rig.bot_ended(0, 2000).is_empty());
    assert_eq!(rig.kind(), StateKind::Idle);
    assert_eq!(
        bot_turn(&mut rig, false, 2100),
        [TurnAction::StartBotTurn { epoch: ep(1) }]
    );
    assert_eq!(rig.kind(), StateKind::AwaitingResponse);
    assert!(rig.bot_started(1, 2200).is_empty());
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
    // The user talks over the line: an ordinary barge-in into the next epoch.
    assert_eq!(rig.vad_start(2300), [intr(2), start(2)]);
    assert_eq!(rig.kind(), StateKind::UserSpeaking);
}

/// Without `interrupt` a busy bot keeps its response: the line is ignored (counted) and the
/// caller offers it again later. A pending candidate or an open user turn always defers it.
#[test]
fn a_bot_line_waits_for_a_busy_bot_or_a_talking_user() {
    let mut rig = Rig::speaking(min_speech(300));
    assert!(bot_turn(&mut rig, false, 1600).is_empty());
    assert_eq!(rig.kind(), StateKind::BotSpeaking);
    assert_eq!(rig.ignored(InputKind::BotTurn), 1);
    // A pending barge-in candidate: even an interrupting line waits.
    assert_eq!(rig.vad_start(2000), [rig.arm(2300)]);
    assert!(bot_turn(&mut rig, true, 2100).is_empty());
    assert_eq!(rig.ignored(InputKind::BotTurn), 2);
    assert_eq!(rig.m.epoch(), ep(0));
    // The user's own turn: the line waits too.
    assert_eq!(rig.tick(2300), [intr(1), start(1)]);
    assert!(bot_turn(&mut rig, true, 2400).is_empty());
    assert_eq!(rig.kind(), StateKind::UserSpeaking);
}

/// With `interrupt` a line cuts the running response like a barge-in, without a user turn.
#[test]
fn an_interrupting_bot_line_cuts_the_response() {
    let mut rig = Rig::speaking(InterruptionPolicy::default());
    assert_eq!(
        bot_turn(&mut rig, true, 1600),
        [intr(1), TurnAction::StartBotTurn { epoch: ep(1) }]
    );
    assert_eq!(rig.kind(), StateKind::AwaitingResponse);
    // The old epoch's audio end is stale; the line's own end closes it.
    assert!(rig.bot_ended(0, 1700).is_empty());
    assert_eq!(rig.kind(), StateKind::AwaitingResponse);
    assert!(rig.bot_ended(1, 1800).is_empty());
    assert_eq!(rig.kind(), StateKind::Idle);
}
