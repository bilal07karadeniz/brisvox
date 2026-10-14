//! Turn-taking state machine tests: a transition table covering every state x input pair
//! for each policy, a table keyed by the Smart Turn and STT sub-state of a pause, scenario tests
//! for the policy rules, and a deterministic invariant run over random inputs (timer slot, epochs,
//! turn pairing, illegal inputs).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::collections::HashSet;
use std::time::Duration;

use brisvox_agent::turn::{
    Analysis, AnalysisId, EndReason, InputKind, Pause, StateKind, TurnAction, TurnInput,
    TurnMachine, TurnPolicy, TurnState,
};
use brisvox_core::TurnEpoch;
use tokio::time::Instant;

/// The clock origin of every test. Only a reading; the machine never reads a clock itself.
#[allow(clippy::disallowed_methods)]
fn origin() -> Instant {
    Instant::now()
}

fn at(base: Instant, ms: u64) -> Instant {
    base + Duration::from_millis(ms)
}

fn ep(value: u32) -> TurnEpoch {
    TurnEpoch::new(value)
}

fn start(e: u32) -> TurnAction {
    TurnAction::StartUserTurn { epoch: ep(e) }
}

fn end(e: u32, reason: EndReason) -> TurnAction {
    TurnAction::EndUserTurn {
        epoch: ep(e),
        reason,
    }
}

fn intr(e: u32) -> TurnAction {
    TurnAction::Interrupt { new_epoch: ep(e) }
}

fn req(id: u64) -> TurnAction {
    TurnAction::RequestAnalysis {
        id: AnalysisId::new(id),
    }
}

fn arm(base: Instant, ms: u64) -> TurnAction {
    TurnAction::ArmTimer {
        deadline: at(base, ms),
    }
}

fn cancel() -> TurnAction {
    TurnAction::CancelTimer
}

fn bot(e: u32) -> TurnAction {
    TurnAction::StartBotTurn { epoch: ep(e) }
}

/// The current pause, if the machine is in one.
fn pause_of(m: &TurnMachine) -> Option<Pause> {
    match m.state() {
        TurnState::UserPaused(pause) => Some(pause),
        _ => None,
    }
}

/// The analysis sub-state as a name. Exhaustive: a new sub-state fails to compile here.
fn analysis_tag(a: Analysis) -> &'static str {
    match a {
        Analysis::NotRequested => "NotRequested",
        Analysis::Pending(_) => "Pending",
        Analysis::Incomplete { .. } => "Incomplete",
        Analysis::Complete { .. } => "Complete",
    }
}

/// Slot of each input kind (exhaustive: a new variant must be placed here, which keeps
/// [`InputKind::ALL`] honest).
fn input_slot(kind: InputKind) -> usize {
    match kind {
        InputKind::VadStart => 0,
        InputKind::VadStop => 1,
        InputKind::SttInterim => 2,
        InputKind::SttFinal => 3,
        InputKind::Verdict => 4,
        InputKind::Tick => 5,
        InputKind::BotAudioStarted => 6,
        InputKind::BotAudioEnded => 7,
        InputKind::Hangup => 8,
        InputKind::UserText => 9,
        InputKind::BotTurn => 10,
    }
}

/// Slot of each state kind (exhaustive, as [`input_slot`]).
fn state_slot(kind: StateKind) -> usize {
    match kind {
        StateKind::Idle => 0,
        StateKind::UserSpeaking => 1,
        StateKind::UserPaused => 2,
        StateKind::AwaitingResponse => 3,
        StateKind::BotSpeaking => 4,
        StateKind::Closed => 5,
    }
}

/// The policies under test. `Fixed`: 700 ms silence. The adaptive cases: 3 s fallback and 800 ms
/// final grace, with and without a required STT final.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Case {
    Fixed,
    AdaptiveFinal,
    AdaptiveNoFinal,
}

impl Case {
    const ALL: [Case; 3] = [Case::Fixed, Case::AdaptiveFinal, Case::AdaptiveNoFinal];

    fn policy(self) -> TurnPolicy {
        match self {
            Case::Fixed => TurnPolicy::fixed(Duration::from_millis(700)),
            Case::AdaptiveFinal => TurnPolicy::Adaptive {
                fallback_silence: Duration::from_secs(3),
                require_final: true,
                final_grace: Duration::from_millis(800),
            },
            Case::AdaptiveNoFinal => TurnPolicy::Adaptive {
                fallback_silence: Duration::from_secs(3),
                require_final: false,
                final_grace: Duration::from_millis(800),
            },
        }
    }
}

/// The default payload of each input kind, at time `t`. Verdicts answer analysis 1 and are
/// complete; bot audio is for epoch 0; the STT final text is unpunctuated.
fn input_of(kind: InputKind, t: Instant) -> TurnInput {
    match kind {
        InputKind::VadStart => TurnInput::VadStart { at: t },
        InputKind::VadStop => TurnInput::VadStop { at: t },
        InputKind::SttInterim => TurnInput::SttInterim {
            text: "hi".into(),
            at: t,
        },
        InputKind::SttFinal => TurnInput::SttFinal {
            text: "hi".into(),
            at: t,
        },
        InputKind::Verdict => TurnInput::Verdict {
            request: AnalysisId::new(1),
            complete: true,
            probability: 0.9,
            at: t,
        },
        InputKind::Tick => TurnInput::Tick { now: t },
        InputKind::BotAudioStarted => TurnInput::BotAudioStarted {
            epoch: ep(0),
            at: t,
        },
        InputKind::BotAudioEnded => TurnInput::BotAudioEnded {
            epoch: ep(0),
            at: t,
        },
        InputKind::Hangup => TurnInput::Hangup { at: t },
        InputKind::UserText => TurnInput::UserText { at: t },
        InputKind::BotTurn => TurnInput::BotTurn {
            interrupt: true,
            at: t,
        },
    }
}

/// The inputs that reach `state` under `case`, all accepted. Every script starts in epoch 0.
fn setup(case: Case, state: StateKind, base: Instant) -> Vec<TurnInput> {
    let t = |ms| at(base, ms);
    let vad_start = TurnInput::VadStart { at: t(0) };
    let vad_stop = TurnInput::VadStop { at: t(1000) };
    let verdict = TurnInput::Verdict {
        request: AnalysisId::new(1),
        complete: true,
        probability: 0.9,
        at: t(1200),
    };
    let paused = vec![vad_start.clone(), vad_stop.clone()];
    match state {
        StateKind::Idle => vec![],
        StateKind::UserSpeaking => vec![vad_start],
        StateKind::UserPaused => paused,
        StateKind::AwaitingResponse => {
            let mut script = paused;
            match case {
                // The fixed silence (700 ms after 1000 ms) ends the turn.
                Case::Fixed => script.push(TurnInput::Tick { now: t(1700) }),
                // The complete verdict waits for the final (AdaptiveFinal): grace ends it.
                Case::AdaptiveFinal => {
                    script.push(verdict);
                    script.push(TurnInput::Tick { now: t(2000) });
                }
                // The complete verdict ends the turn at once.
                Case::AdaptiveNoFinal => script.push(verdict),
            }
            script
        }
        StateKind::BotSpeaking => {
            let mut script = setup(case, StateKind::AwaitingResponse, base);
            script.push(TurnInput::BotAudioStarted {
                epoch: ep(0),
                at: t(1800),
            });
            script
        }
        StateKind::Closed => vec![TurnInput::Hangup { at: t(0) }],
    }
}

/// One table row: from `state` (reached by `setup`), `input` must produce `actions`, end in
/// `next`, and count as ignored or not.
struct Cell {
    state: StateKind,
    input: TurnInput,
    actions: Vec<TurnAction>,
    next: StateKind,
    ignored: bool,
}

/// Cells that do not depend on the policy (all inputs at `t`, where `t` is after every deadline
/// the setups arm, so nothing time-dependent happens here).
fn shared(base: Instant) -> Vec<Cell> {
    use InputKind::*;
    use StateKind::*;
    let t = at(base, 2100);
    let mut rows = Vec::new();
    let mut add = |state, kind, actions: Vec<TurnAction>, next, ignored| {
        rows.push(Cell {
            state,
            input: input_of(kind, t),
            actions,
            next,
            ignored,
        });
    };
    // Idle: a final or interim has no open stop to answer.
    add(Idle, VadStart, vec![start(0)], UserSpeaking, false);
    add(Idle, VadStop, vec![], Idle, true);
    add(Idle, SttInterim, vec![], Idle, true);
    add(Idle, SttFinal, vec![], Idle, true);
    add(Idle, Verdict, vec![], Idle, true);
    add(Idle, Tick, vec![], Idle, true);
    add(Idle, BotAudioStarted, vec![], Idle, true);
    add(Idle, BotAudioEnded, vec![], Idle, true);
    add(Idle, Hangup, vec![], Closed, false);
    // A typed message is a turn that ends at once (the first turn keeps epoch 0).
    add(
        Idle,
        UserText,
        vec![start(0), end(0, EndReason::Text)],
        AwaitingResponse,
        false,
    );
    // A line of the bot's own is a bot turn (the first turn keeps epoch 0).
    add(Idle, BotTurn, vec![bot(0)], AwaitingResponse, false);
    // UserSpeaking (VadStop is policy-specific). Nothing is owed (no stop yet), so a final is
    // stray and ignored.
    add(UserSpeaking, VadStart, vec![], UserSpeaking, true);
    add(UserSpeaking, SttInterim, vec![], UserSpeaking, false);
    add(UserSpeaking, SttFinal, vec![], UserSpeaking, true);
    add(UserSpeaking, Verdict, vec![], UserSpeaking, true);
    add(UserSpeaking, Tick, vec![], UserSpeaking, true);
    add(UserSpeaking, BotAudioStarted, vec![], UserSpeaking, true);
    add(UserSpeaking, BotAudioEnded, vec![], UserSpeaking, true);
    // A typed message while the user talks joins the open turn (the caller's business).
    add(UserSpeaking, UserText, vec![], UserSpeaking, true);
    // A bot line never talks over the user: it waits (the caller keeps it).
    add(UserSpeaking, BotTurn, vec![], UserSpeaking, true);
    add(
        UserSpeaking,
        Hangup,
        vec![end(0, EndReason::Hangup)],
        Closed,
        false,
    );
    // UserPaused (Verdict and Tick are policy-specific; the sub-states are in `sub_rows`).
    add(UserPaused, VadStart, vec![cancel()], UserSpeaking, false);
    add(UserPaused, VadStop, vec![], UserPaused, true);
    add(UserPaused, SttInterim, vec![], UserPaused, false);
    add(UserPaused, SttFinal, vec![], UserPaused, false);
    add(UserPaused, BotAudioStarted, vec![], UserPaused, true);
    add(UserPaused, BotAudioEnded, vec![], UserPaused, true);
    add(UserPaused, UserText, vec![], UserPaused, true);
    add(UserPaused, BotTurn, vec![], UserPaused, true);
    add(
        UserPaused,
        Hangup,
        vec![cancel(), end(0, EndReason::Hangup)],
        Closed,
        false,
    );
    // AwaitingResponse: the stop of the ended turn is still owed its final, which is consumed.
    add(
        AwaitingResponse,
        VadStart,
        vec![intr(1), start(1)],
        UserSpeaking,
        false,
    );
    add(AwaitingResponse, VadStop, vec![], AwaitingResponse, true);
    add(AwaitingResponse, SttInterim, vec![], AwaitingResponse, true);
    add(AwaitingResponse, SttFinal, vec![], AwaitingResponse, false);
    add(AwaitingResponse, Verdict, vec![], AwaitingResponse, true);
    add(AwaitingResponse, Tick, vec![], AwaitingResponse, true);
    add(
        AwaitingResponse,
        BotAudioStarted,
        vec![],
        BotSpeaking,
        false,
    );
    add(AwaitingResponse, BotAudioEnded, vec![], Idle, false);
    add(AwaitingResponse, Hangup, vec![], Closed, false);
    // A typed message while the bot thinks: a barge-in whose turn ends at once.
    add(
        AwaitingResponse,
        UserText,
        vec![intr(1), start(1), end(1, EndReason::Text)],
        AwaitingResponse,
        false,
    );
    // An interrupting bot line cuts the response it replaces.
    add(
        AwaitingResponse,
        BotTurn,
        vec![intr(1), bot(1)],
        AwaitingResponse,
        false,
    );
    // BotSpeaking: a repeated BotAudioStarted of the same epoch (one per chunk) has no effect.
    add(
        BotSpeaking,
        VadStart,
        vec![intr(1), start(1)],
        UserSpeaking,
        false,
    );
    add(BotSpeaking, VadStop, vec![], BotSpeaking, true);
    add(BotSpeaking, SttInterim, vec![], BotSpeaking, true);
    add(BotSpeaking, SttFinal, vec![], BotSpeaking, false);
    add(BotSpeaking, Verdict, vec![], BotSpeaking, true);
    add(BotSpeaking, Tick, vec![], BotSpeaking, true);
    add(BotSpeaking, BotAudioStarted, vec![], BotSpeaking, false);
    add(BotSpeaking, BotAudioEnded, vec![], Idle, false);
    add(BotSpeaking, Hangup, vec![], Closed, false);
    add(
        BotSpeaking,
        UserText,
        vec![intr(1), start(1), end(1, EndReason::Text)],
        AwaitingResponse,
        false,
    );
    add(
        BotSpeaking,
        BotTurn,
        vec![intr(1), bot(1)],
        AwaitingResponse,
        false,
    );
    // Closed: terminal, every input (Hangup included) is ignored.
    for kind in InputKind::ALL {
        add(Closed, kind, vec![], Closed, true);
    }
    rows
}

/// The cells whose expectations depend on the policy.
fn specific(case: Case, base: Instant) -> Vec<Cell> {
    let t = at(base, 2100);
    let stop = TurnInput::VadStop { at: t };
    let verdict = TurnInput::Verdict {
        request: AnalysisId::new(1),
        complete: true,
        probability: 0.9,
        at: t,
    };
    match case {
        Case::Fixed => vec![
            Cell {
                state: StateKind::UserSpeaking,
                input: stop,
                actions: vec![arm(base, 2800)],
                next: StateKind::UserPaused,
                ignored: false,
            },
            // No analysis is requested under the fixed policy: the verdict is stale.
            Cell {
                state: StateKind::UserPaused,
                input: verdict,
                actions: vec![],
                next: StateKind::UserPaused,
                ignored: true,
            },
            Cell {
                state: StateKind::UserPaused,
                input: TurnInput::Tick {
                    now: at(base, 1700),
                },
                actions: vec![end(0, EndReason::FixedSilence)],
                next: StateKind::AwaitingResponse,
                ignored: false,
            },
        ],
        Case::AdaptiveFinal => vec![
            Cell {
                state: StateKind::UserSpeaking,
                input: stop,
                actions: vec![req(1), arm(base, 5100)],
                next: StateKind::UserPaused,
                ignored: false,
            },
            // Complete verdict, final still awaited: the grace (800 ms after 2100 ms) is earlier
            // than the fallback, so the timer is replaced.
            Cell {
                state: StateKind::UserPaused,
                input: verdict,
                actions: vec![arm(base, 2900)],
                next: StateKind::UserPaused,
                ignored: false,
            },
            Cell {
                state: StateKind::UserPaused,
                input: TurnInput::Tick {
                    now: at(base, 4000),
                },
                actions: vec![end(0, EndReason::Fallback)],
                next: StateKind::AwaitingResponse,
                ignored: false,
            },
        ],
        Case::AdaptiveNoFinal => vec![
            Cell {
                state: StateKind::UserSpeaking,
                input: stop,
                actions: vec![req(1), arm(base, 5100)],
                next: StateKind::UserPaused,
                ignored: false,
            },
            Cell {
                state: StateKind::UserPaused,
                input: verdict,
                actions: vec![cancel(), end(0, EndReason::Verdict)],
                next: StateKind::AwaitingResponse,
                ignored: false,
            },
            Cell {
                state: StateKind::UserPaused,
                input: TurnInput::Tick {
                    now: at(base, 4000),
                },
                actions: vec![end(0, EndReason::Fallback)],
                next: StateKind::AwaitingResponse,
                ignored: false,
            },
        ],
    }
}

/// Runs one cell: the setup must reach the cell's state with nothing ignored, and the input must
/// produce exactly the expected actions, state and ignored count.
fn run_cell(case: Case, cell: &Cell, base: Instant) {
    let label = format!("{case:?} {:?} x {:?}", cell.state, cell.input.kind());
    let mut machine = TurnMachine::new(case.policy());
    let mut out = Vec::new();
    for step in setup(case, cell.state, base) {
        machine.handle(step, &mut out);
    }
    assert_eq!(machine.stats().total_ignored(), 0, "{label}: setup ignored");
    assert_eq!(machine.state().kind(), cell.state, "{label}: setup state");

    let kind = cell.input.kind();
    let before = machine.stats();
    out.clear();
    machine.handle(cell.input.clone(), &mut out);
    let after = machine.stats();

    assert_eq!(out, cell.actions, "{label}: actions");
    assert_eq!(machine.state().kind(), cell.next, "{label}: next state");
    let ignored_now = after.ignored(kind) - before.ignored(kind);
    assert_eq!(ignored_now, u64::from(cell.ignored), "{label}: ignored");
    assert_eq!(
        after.total_ignored() - before.total_ignored(),
        u64::from(cell.ignored),
        "{label}: total ignored"
    );
}

#[test]
fn transition_table_covers_every_state_input_pair_for_every_policy() {
    let base = origin();
    for case in Case::ALL {
        let mut cells = shared(base);
        cells.extend(specific(case, base));

        let covered: HashSet<(StateKind, InputKind)> =
            cells.iter().map(|c| (c.state, c.input.kind())).collect();
        assert_eq!(cells.len(), 66, "{case:?}: one row per pair");
        assert_eq!(covered.len(), 66, "{case:?}: no pair listed twice");
        for state in StateKind::ALL {
            for kind in InputKind::ALL {
                assert!(
                    covered.contains(&(state, kind)),
                    "{case:?}: no row for {state:?} x {kind:?}"
                );
            }
        }
        for cell in &cells {
            run_cell(case, cell, base);
        }
    }
}

#[test]
fn kind_lists_name_every_variant_exactly_once() {
    let mut seen_inputs = [false; 11];
    for kind in InputKind::ALL {
        let slot = input_slot(kind);
        assert!(!seen_inputs[slot], "{kind:?} listed twice");
        seen_inputs[slot] = true;
    }
    assert!(seen_inputs.iter().all(|s| *s), "an input kind is missing");

    let mut seen_states = [false; 6];
    for kind in StateKind::ALL {
        let slot = state_slot(kind);
        assert!(!seen_states[slot], "{kind:?} listed twice");
        seen_states[slot] = true;
    }
    assert!(seen_states.iter().all(|s| *s), "a state kind is missing");
}

/// A pause's starting point: the script that reaches it, and what the pause must hold then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pre {
    /// VAD stop, nothing else.
    Fresh,
    /// The stop's final arrived while the analysis was pending, punctuated.
    FinalPunct,
    /// The stop's final arrived while the analysis was pending, unpunctuated.
    FinalPlain,
    /// The verdict was incomplete at 1200 ms, no final.
    Incomplete,
    /// Incomplete at 1200 ms, then a punctuated final at 1300 ms (deadline 2100 ms).
    IncompleteFinalPunct,
    /// Incomplete at 1200 ms, then an unpunctuated final at 1300 ms.
    IncompleteFinalPlain,
    /// The verdict was complete at 1200 ms, no final (the grace runs to 2000 ms).
    Complete,
    /// The verdict was complete at 3900 ms: its grace would end after the fallback, so the
    /// fallback (4000 ms) stays.
    CompleteLate,
    /// Incomplete at 3900 ms, then a punctuated final at 3950 ms: the fallback (4000 ms) stays.
    IncompleteLate,
}

impl Pre {
    /// The inputs that reach the pause (VAD start at 0 ms, VAD stop at 1000 ms, then this).
    fn script(self, base: Instant) -> Vec<TurnInput> {
        let t = |ms| at(base, ms);
        let mut script = vec![
            TurnInput::VadStart { at: t(0) },
            TurnInput::VadStop { at: t(1000) },
        ];
        let verdict = |complete, ms| TurnInput::Verdict {
            request: AnalysisId::new(1),
            complete,
            probability: 0.5,
            at: t(ms),
        };
        let final_of = |text: &str, ms| TurnInput::SttFinal {
            text: text.into(),
            at: t(ms),
        };
        match self {
            Pre::Fresh => {}
            Pre::FinalPunct => script.push(final_of("Evet.", 1100)),
            Pre::FinalPlain => script.push(final_of("bana", 1100)),
            Pre::Incomplete => script.push(verdict(false, 1200)),
            Pre::IncompleteFinalPunct => {
                script.push(verdict(false, 1200));
                script.push(final_of("Evet.", 1300));
            }
            Pre::IncompleteFinalPlain => {
                script.push(verdict(false, 1200));
                script.push(final_of("bana", 1300));
            }
            Pre::Complete => script.push(verdict(true, 1200)),
            Pre::CompleteLate => script.push(verdict(true, 3900)),
            Pre::IncompleteLate => {
                script.push(verdict(false, 3900));
                script.push(final_of("Evet.", 3950));
            }
        }
        script
    }

    /// The analysis name and the final (punctuated?) the pause holds after `script`.
    fn expect(self, case: Case) -> (&'static str, Option<bool>) {
        let tag = match (case, self) {
            (Case::Fixed, _) => "NotRequested",
            (_, Pre::Fresh | Pre::FinalPunct | Pre::FinalPlain) => "Pending",
            (
                _,
                Pre::Incomplete
                | Pre::IncompleteFinalPunct
                | Pre::IncompleteFinalPlain
                | Pre::IncompleteLate,
            ) => "Incomplete",
            (_, Pre::Complete | Pre::CompleteLate) => "Complete",
        };
        let final_punct = match self {
            Pre::FinalPunct | Pre::IncompleteFinalPunct | Pre::IncompleteLate => Some(true),
            Pre::FinalPlain | Pre::IncompleteFinalPlain => Some(false),
            _ => None,
        };
        (tag, final_punct)
    }
}

/// One row of the sub-state table: from a pause (`case`, `pre`), `input` must produce `actions`,
/// end in `next` with the timer at `armed` (ms after the origin, or no timer), and count as
/// ignored or not.
struct SubRow {
    case: Case,
    pre: Pre,
    input: TurnInput,
    actions: Vec<TurnAction>,
    next: StateKind,
    armed: Option<u64>,
    ignored: bool,
}

/// The sub-state table: `UserPaused` x {SttFinal, Verdict, Tick} for every analysis sub-state
/// (NotRequested, Pending, Incomplete, Complete), with and without an STT final, punctuated and
/// not, and with the deadline cap (a late verdict or final never extends the fallback).
fn sub_rows(base: Instant) -> Vec<SubRow> {
    use Case::*;
    use Pre::*;
    use StateKind::{AwaitingResponse as AR, UserPaused as UP};
    let fin = |text: &str, ms: u64| TurnInput::SttFinal {
        text: text.into(),
        at: at(base, ms),
    };
    let ver = |id: u64, complete: bool, ms: u64| TurnInput::Verdict {
        request: AnalysisId::new(id),
        complete,
        probability: 0.5,
        at: at(base, ms),
    };
    let tick = |ms: u64| TurnInput::Tick { now: at(base, ms) };
    let row = |case, pre, input, actions, next, armed, ignored| SubRow {
        case,
        pre,
        input,
        actions,
        next,
        armed,
        ignored,
    };
    vec![
        // AdaptiveFinal, Pending (fallback deadline 4000 ms).
        row(
            AdaptiveFinal,
            Fresh,
            fin("Evet.", 1400),
            vec![],
            UP,
            Some(4000),
            false,
        ),
        row(
            AdaptiveFinal,
            Fresh,
            fin("bana", 1400),
            vec![],
            UP,
            Some(4000),
            false,
        ),
        row(
            AdaptiveFinal,
            Fresh,
            ver(1, true, 1400),
            vec![arm(base, 2200)],
            UP,
            Some(2200),
            false,
        ),
        row(
            AdaptiveFinal,
            Fresh,
            ver(1, false, 1400),
            vec![],
            UP,
            Some(4000),
            false,
        ),
        row(
            AdaptiveFinal,
            Fresh,
            ver(2, true, 1400),
            vec![],
            UP,
            Some(4000),
            true,
        ),
        row(
            AdaptiveFinal,
            Fresh,
            tick(3999),
            vec![arm(base, 4000)],
            UP,
            Some(4000),
            false,
        ),
        row(
            AdaptiveFinal,
            Fresh,
            tick(4000),
            vec![end(0, EndReason::Fallback)],
            AR,
            None,
            false,
        ),
        // AdaptiveFinal, Pending with a punctuated final (no shortening while pending).
        row(
            AdaptiveFinal,
            FinalPunct,
            fin("bana", 1400),
            vec![],
            UP,
            Some(4000),
            true,
        ),
        row(
            AdaptiveFinal,
            FinalPunct,
            ver(1, true, 1400),
            vec![cancel(), end(0, EndReason::Verdict)],
            AR,
            None,
            false,
        ),
        row(
            AdaptiveFinal,
            FinalPunct,
            ver(1, false, 1400),
            vec![arm(base, 1900)],
            UP,
            Some(1900),
            false,
        ),
        row(
            AdaptiveFinal,
            FinalPunct,
            tick(4000),
            vec![end(0, EndReason::Fallback)],
            AR,
            None,
            false,
        ),
        // AdaptiveFinal, Pending with an unpunctuated final.
        row(
            AdaptiveFinal,
            FinalPlain,
            fin("Evet.", 1400),
            vec![],
            UP,
            Some(4000),
            true,
        ),
        row(
            AdaptiveFinal,
            FinalPlain,
            ver(1, true, 1400),
            vec![cancel(), end(0, EndReason::Verdict)],
            AR,
            None,
            false,
        ),
        row(
            AdaptiveFinal,
            FinalPlain,
            ver(1, false, 1400),
            vec![],
            UP,
            Some(4000),
            false,
        ),
        // AdaptiveFinal, Incomplete (no final, then a final).
        row(
            AdaptiveFinal,
            Incomplete,
            fin("Evet.", 1400),
            vec![arm(base, 2200)],
            UP,
            Some(2200),
            false,
        ),
        row(
            AdaptiveFinal,
            Incomplete,
            fin("bana", 1400),
            vec![],
            UP,
            Some(4000),
            false,
        ),
        row(
            AdaptiveFinal,
            Incomplete,
            ver(1, true, 1400),
            vec![],
            UP,
            Some(4000),
            true,
        ),
        row(
            AdaptiveFinal,
            Incomplete,
            tick(3999),
            vec![arm(base, 4000)],
            UP,
            Some(4000),
            false,
        ),
        row(
            AdaptiveFinal,
            Incomplete,
            tick(4000),
            vec![end(0, EndReason::Fallback)],
            AR,
            None,
            false,
        ),
        // AdaptiveFinal, Incomplete with a punctuated final (deadline 2100 ms).
        row(
            AdaptiveFinal,
            IncompleteFinalPunct,
            fin("bana", 1400),
            vec![],
            UP,
            Some(2100),
            true,
        ),
        row(
            AdaptiveFinal,
            IncompleteFinalPunct,
            ver(1, true, 1400),
            vec![],
            UP,
            Some(2100),
            true,
        ),
        row(
            AdaptiveFinal,
            IncompleteFinalPunct,
            tick(2099),
            vec![arm(base, 2100)],
            UP,
            Some(2100),
            false,
        ),
        row(
            AdaptiveFinal,
            IncompleteFinalPunct,
            tick(2100),
            vec![end(0, EndReason::SttPunctuation)],
            AR,
            None,
            false,
        ),
        // AdaptiveFinal, Incomplete with an unpunctuated final: no shortening.
        row(
            AdaptiveFinal,
            IncompleteFinalPlain,
            fin("Evet.", 1400),
            vec![],
            UP,
            Some(4000),
            true,
        ),
        row(
            AdaptiveFinal,
            IncompleteFinalPlain,
            tick(4000),
            vec![end(0, EndReason::Fallback)],
            AR,
            None,
            false,
        ),
        // AdaptiveFinal, Complete: the final (any text) ends the turn; otherwise the grace.
        row(
            AdaptiveFinal,
            Complete,
            fin("Evet.", 1400),
            vec![cancel(), end(0, EndReason::Verdict)],
            AR,
            None,
            false,
        ),
        row(
            AdaptiveFinal,
            Complete,
            fin("bana", 1400),
            vec![cancel(), end(0, EndReason::Verdict)],
            AR,
            None,
            false,
        ),
        row(
            AdaptiveFinal,
            Complete,
            ver(1, true, 1400),
            vec![],
            UP,
            Some(2000),
            true,
        ),
        row(
            AdaptiveFinal,
            Complete,
            ver(1, false, 1400),
            vec![],
            UP,
            Some(2000),
            true,
        ),
        row(
            AdaptiveFinal,
            Complete,
            tick(1999),
            vec![arm(base, 2000)],
            UP,
            Some(2000),
            false,
        ),
        row(
            AdaptiveFinal,
            Complete,
            tick(2000),
            vec![end(0, EndReason::FinalGrace)],
            AR,
            None,
            false,
        ),
        // AdaptiveFinal, Complete late: the grace would pass the fallback, so the 4000 ms stays.
        row(
            AdaptiveFinal,
            CompleteLate,
            tick(3999),
            vec![arm(base, 4000)],
            UP,
            Some(4000),
            false,
        ),
        row(
            AdaptiveFinal,
            CompleteLate,
            fin("Evet.", 3950),
            vec![cancel(), end(0, EndReason::Verdict)],
            AR,
            None,
            false,
        ),
        row(
            AdaptiveFinal,
            CompleteLate,
            tick(4000),
            vec![end(0, EndReason::FinalGrace)],
            AR,
            None,
            false,
        ),
        // AdaptiveFinal, Incomplete late: the punctuated final would pass the fallback.
        row(
            AdaptiveFinal,
            IncompleteLate,
            tick(3999),
            vec![arm(base, 4000)],
            UP,
            Some(4000),
            false,
        ),
        row(
            AdaptiveFinal,
            IncompleteLate,
            tick(4000),
            vec![end(0, EndReason::Fallback)],
            AR,
            None,
            false,
        ),
        // AdaptiveNoFinal, Pending: a complete verdict ends the turn without a final.
        row(
            AdaptiveNoFinal,
            Fresh,
            fin("Evet.", 1400),
            vec![],
            UP,
            Some(4000),
            false,
        ),
        row(
            AdaptiveNoFinal,
            Fresh,
            ver(1, true, 1400),
            vec![cancel(), end(0, EndReason::Verdict)],
            AR,
            None,
            false,
        ),
        row(
            AdaptiveNoFinal,
            Fresh,
            ver(1, false, 1400),
            vec![],
            UP,
            Some(4000),
            false,
        ),
        row(
            AdaptiveNoFinal,
            Fresh,
            ver(2, true, 1400),
            vec![],
            UP,
            Some(4000),
            true,
        ),
        row(
            AdaptiveNoFinal,
            Fresh,
            tick(4000),
            vec![end(0, EndReason::Fallback)],
            AR,
            None,
            false,
        ),
        // AdaptiveNoFinal, Incomplete.
        row(
            AdaptiveNoFinal,
            Incomplete,
            fin("Evet.", 1400),
            vec![arm(base, 2200)],
            UP,
            Some(2200),
            false,
        ),
        row(
            AdaptiveNoFinal,
            Incomplete,
            ver(1, true, 1400),
            vec![],
            UP,
            Some(4000),
            true,
        ),
        row(
            AdaptiveNoFinal,
            Incomplete,
            tick(4000),
            vec![end(0, EndReason::Fallback)],
            AR,
            None,
            false,
        ),
        // AdaptiveNoFinal, Incomplete with a punctuated final.
        row(
            AdaptiveNoFinal,
            IncompleteFinalPunct,
            tick(2100),
            vec![end(0, EndReason::SttPunctuation)],
            AR,
            None,
            false,
        ),
        row(
            AdaptiveNoFinal,
            IncompleteFinalPunct,
            fin("bana", 1400),
            vec![],
            UP,
            Some(2100),
            true,
        ),
        // AdaptiveNoFinal, Pending with a punctuated final: the verdict still ends it at once.
        row(
            AdaptiveNoFinal,
            FinalPunct,
            ver(1, true, 1400),
            vec![cancel(), end(0, EndReason::Verdict)],
            AR,
            None,
            false,
        ),
        // Fixed silence: no analysis, so verdicts are stale and finals have no effect.
        row(
            Fixed,
            Fresh,
            fin("Evet.", 1400),
            vec![],
            UP,
            Some(1700),
            false,
        ),
        row(
            Fixed,
            Fresh,
            ver(1, true, 1400),
            vec![],
            UP,
            Some(1700),
            true,
        ),
        row(
            Fixed,
            Fresh,
            tick(1699),
            vec![arm(base, 1700)],
            UP,
            Some(1700),
            false,
        ),
        row(
            Fixed,
            Fresh,
            tick(1700),
            vec![end(0, EndReason::FixedSilence)],
            AR,
            None,
            false,
        ),
        row(
            Fixed,
            FinalPunct,
            ver(1, true, 1400),
            vec![],
            UP,
            Some(1700),
            true,
        ),
        row(
            Fixed,
            FinalPunct,
            tick(1700),
            vec![end(0, EndReason::FixedSilence)],
            AR,
            None,
            false,
        ),
        row(
            Fixed,
            FinalPunct,
            fin("bana", 1400),
            vec![],
            UP,
            Some(1700),
            true,
        ),
    ]
}

/// Runs one sub-state row: the script must reach the pause the row names, then the input must
/// produce the expected actions, state, timer and ignored count.
fn run_sub(row: &SubRow, base: Instant) {
    let label = format!("{:?} {:?} x {:?}", row.case, row.pre, row.input.kind());
    let mut m = TurnMachine::new(row.case.policy());
    let mut out = Vec::new();
    for step in row.pre.script(base) {
        m.handle(step, &mut out);
    }
    assert_eq!(m.stats().total_ignored(), 0, "{label}: setup ignored");
    let pause = pause_of(&m).expect("the script reaches a pause");
    let (tag, final_punct) = row.pre.expect(row.case);
    assert_eq!(analysis_tag(pause.analysis()), tag, "{label}: analysis");
    assert_eq!(
        pause.final_seen().map(|f| f.ends_sentence),
        final_punct,
        "{label}: final"
    );

    let kind = row.input.kind();
    let before = m.stats();
    out.clear();
    m.handle(row.input.clone(), &mut out);
    let after = m.stats();

    assert_eq!(out, row.actions, "{label}: actions");
    assert_eq!(m.state().kind(), row.next, "{label}: next state");
    assert_eq!(
        m.armed_deadline(),
        row.armed.map(|ms| at(base, ms)),
        "{label}: timer"
    );
    assert_eq!(
        after.ignored(kind) - before.ignored(kind),
        u64::from(row.ignored),
        "{label}: ignored"
    );
}

#[test]
fn pause_substate_table_covers_every_analysis_with_every_pause_input() {
    let base = origin();
    let rows = sub_rows(base);
    let covered: HashSet<(Case, &'static str, InputKind)> = rows
        .iter()
        .map(|r| (r.case, r.pre.expect(r.case).0, r.input.kind()))
        .collect();
    let required: [(Case, &[&'static str]); 3] = [
        (Case::Fixed, &["NotRequested"]),
        (Case::AdaptiveFinal, &["Pending", "Incomplete", "Complete"]),
        (Case::AdaptiveNoFinal, &["Pending", "Incomplete"]),
    ];
    for (case, tags) in required {
        for tag in tags {
            for kind in [InputKind::SttFinal, InputKind::Verdict, InputKind::Tick] {
                assert!(
                    covered.contains(&(case, *tag, kind)),
                    "{case:?}: no sub-state row for {tag} x {kind:?}"
                );
            }
        }
    }
    for row in &rows {
        run_sub(row, base);
    }
}

#[test]
fn fixed_silence_turn_ends_exactly_at_the_deadline() {
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::fixed(Duration::from_millis(700)));
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    assert_eq!(out, vec![start(0)]);

    out.clear();
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    assert_eq!(out, vec![arm(base, 1700)]);
    assert_eq!(m.armed_deadline(), Some(at(base, 1700)));

    out.clear();
    m.handle(
        TurnInput::Tick {
            now: at(base, 1699),
        },
        &mut out,
    );
    assert_eq!(out, vec![arm(base, 1700)], "early tick re-arms");
    assert!(matches!(m.state(), TurnState::UserPaused(_)));

    out.clear();
    m.handle(
        TurnInput::Tick {
            now: at(base, 1700),
        },
        &mut out,
    );
    assert_eq!(out, vec![end(0, EndReason::FixedSilence)]);
    assert_eq!(m.state(), TurnState::AwaitingResponse);
    assert_eq!(m.armed_deadline(), None);
}

#[test]
fn adaptive_complete_verdict_ends_the_turn_immediately() {
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::adaptive());
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    out.clear();
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    assert_eq!(out, vec![req(1), arm(base, 4000)]);

    out.clear();
    m.handle(
        TurnInput::Verdict {
            request: AnalysisId::new(1),
            complete: true,
            probability: 0.97,
            at: at(base, 1200),
        },
        &mut out,
    );
    assert_eq!(out, vec![cancel(), end(0, EndReason::Verdict)]);
    assert_eq!(m.state(), TurnState::AwaitingResponse);
}

#[test]
fn required_final_waits_for_the_final_then_ends_on_it() {
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::Adaptive {
        fallback_silence: Duration::from_secs(3),
        require_final: true,
        final_grace: Duration::from_millis(800),
    });
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    out.clear();
    m.handle(
        TurnInput::Verdict {
            request: AnalysisId::new(1),
            complete: true,
            probability: 0.97,
            at: at(base, 1200),
        },
        &mut out,
    );
    assert_eq!(out, vec![arm(base, 2000)], "grace replaces the fallback");

    out.clear();
    m.handle(
        TurnInput::SttFinal {
            text: "ok.".into(),
            at: at(base, 1400),
        },
        &mut out,
    );
    assert_eq!(out, vec![cancel(), end(0, EndReason::Verdict)]);
}

#[test]
fn required_final_grace_expiry_ends_the_turn() {
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::Adaptive {
        fallback_silence: Duration::from_secs(3),
        require_final: true,
        final_grace: Duration::from_millis(800),
    });
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    m.handle(
        TurnInput::Verdict {
            request: AnalysisId::new(1),
            complete: true,
            probability: 0.97,
            at: at(base, 1200),
        },
        &mut out,
    );
    out.clear();
    m.handle(
        TurnInput::Tick {
            now: at(base, 2000),
        },
        &mut out,
    );
    assert_eq!(out, vec![end(0, EndReason::FinalGrace)]);
}

#[test]
fn incomplete_verdict_then_fallback_timeout_ends_the_turn() {
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::adaptive());
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    out.clear();
    m.handle(
        TurnInput::Verdict {
            request: AnalysisId::new(1),
            complete: false,
            probability: 0.12,
            at: at(base, 1200),
        },
        &mut out,
    );
    assert!(out.is_empty(), "incomplete verdict keeps the deadline");
    assert_eq!(m.armed_deadline(), Some(at(base, 4000)));

    out.clear();
    m.handle(
        TurnInput::Tick {
            now: at(base, 3999),
        },
        &mut out,
    );
    assert_eq!(out, vec![arm(base, 4000)], "early tick re-arms");
    m.handle(
        TurnInput::Tick {
            now: at(base, 4000),
        },
        &mut out,
    );
    assert_eq!(out, vec![arm(base, 4000), end(0, EndReason::Fallback)]);
}

#[test]
fn user_resumes_during_analysis_and_the_stale_verdict_is_ignored() {
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::adaptive());
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);

    out.clear();
    m.handle(TurnInput::VadStart { at: at(base, 1500) }, &mut out);
    assert_eq!(out, vec![cancel()], "resume: same turn, timer cancelled");
    assert_eq!(m.state(), TurnState::UserSpeaking);
    assert_eq!(m.epoch(), ep(0), "a resumed pause keeps the epoch");

    out.clear();
    m.handle(
        TurnInput::Verdict {
            request: AnalysisId::new(1),
            complete: true,
            probability: 0.97,
            at: at(base, 1600),
        },
        &mut out,
    );
    assert!(out.is_empty(), "stale verdict has no effect");
    assert_eq!(m.stats().ignored(InputKind::Verdict), 1);
    assert_eq!(m.state(), TurnState::UserSpeaking);

    out.clear();
    m.handle(
        TurnInput::Tick {
            now: at(base, 4000),
        },
        &mut out,
    );
    assert!(out.is_empty(), "a late timer after the cancel is ignored");

    out.clear();
    m.handle(TurnInput::VadStop { at: at(base, 2000) }, &mut out);
    assert_eq!(out, vec![req(2), arm(base, 5000)], "a fresh analysis");

    out.clear();
    m.handle(
        TurnInput::Verdict {
            request: AnalysisId::new(1),
            complete: true,
            probability: 0.97,
            at: at(base, 2100),
        },
        &mut out,
    );
    assert!(out.is_empty(), "the first analysis stays stale");

    out.clear();
    m.handle(
        TurnInput::Verdict {
            request: AnalysisId::new(2),
            complete: true,
            probability: 0.97,
            at: at(base, 2200),
        },
        &mut out,
    );
    assert_eq!(out, vec![cancel(), end(0, EndReason::Verdict)]);
    assert_eq!(m.epoch(), ep(0), "one user turn, one epoch");
}

#[test]
fn a_final_for_an_abandoned_pause_is_not_the_current_pauses() {
    // The first utterance's final arrives after the user resumed and stopped again. It answers
    // the first stop, so the second pause still waits for its own final.
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::Adaptive {
        fallback_silence: Duration::from_secs(3),
        require_final: true,
        final_grace: Duration::from_millis(800),
    });
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    m.handle(TurnInput::VadStart { at: at(base, 1100) }, &mut out);
    m.handle(TurnInput::VadStop { at: at(base, 1500) }, &mut out);

    out.clear();
    m.handle(
        TurnInput::SttFinal {
            text: "Merhaba".into(),
            at: at(base, 1550),
        },
        &mut out,
    );
    assert!(
        out.is_empty(),
        "the stale final does not end the second pause"
    );
    assert_eq!(m.armed_deadline(), Some(at(base, 4500)));
    assert_eq!(m.stats().total_ignored(), 0, "a final owed to an old stop");

    out.clear();
    m.handle(
        TurnInput::Verdict {
            request: AnalysisId::new(2),
            complete: true,
            probability: 0.97,
            at: at(base, 1600),
        },
        &mut out,
    );
    assert_eq!(out, vec![arm(base, 2400)], "waits for the second final");
    assert!(matches!(m.state(), TurnState::UserPaused(_)));

    out.clear();
    m.handle(
        TurnInput::SttFinal {
            text: "Tamam".into(),
            at: at(base, 2000),
        },
        &mut out,
    );
    assert_eq!(out, vec![cancel(), end(0, EndReason::Verdict)]);
}

#[test]
fn barge_in_during_bot_speech_bumps_the_epoch_and_starts_a_turn() {
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::fixed(Duration::from_millis(700)));
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    m.handle(
        TurnInput::Tick {
            now: at(base, 1700),
        },
        &mut out,
    );
    m.handle(
        TurnInput::BotAudioStarted {
            epoch: ep(0),
            at: at(base, 1800),
        },
        &mut out,
    );
    assert_eq!(m.state(), TurnState::BotSpeaking);

    out.clear();
    m.handle(TurnInput::VadStart { at: at(base, 2000) }, &mut out);
    assert_eq!(out, vec![intr(1), start(1)], "interrupt, then the new turn");
    assert_eq!(m.epoch(), ep(1));

    // Audio of the old epoch is stale now.
    out.clear();
    m.handle(
        TurnInput::BotAudioEnded {
            epoch: ep(0),
            at: at(base, 2100),
        },
        &mut out,
    );
    assert!(out.is_empty());
    assert_eq!(m.stats().ignored(InputKind::BotAudioEnded), 1);

    out.clear();
    m.handle(TurnInput::VadStop { at: at(base, 2500) }, &mut out);
    assert_eq!(out, vec![arm(base, 3200)]);
    out.clear();
    m.handle(
        TurnInput::Tick {
            now: at(base, 3200),
        },
        &mut out,
    );
    assert_eq!(out, vec![end(1, EndReason::FixedSilence)]);
}

#[test]
fn stale_bot_audio_started_after_a_barge_in_is_ignored() {
    // The old epoch's first audio arrives after the barge-in: it must not move the machine to
    // BotSpeaking, or the next user turn would emit a spurious Interrupt.
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::fixed(Duration::from_millis(700)));
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    m.handle(
        TurnInput::Tick {
            now: at(base, 1700),
        },
        &mut out,
    );
    m.handle(TurnInput::VadStart { at: at(base, 2000) }, &mut out);
    assert_eq!(
        m.state(),
        TurnState::UserSpeaking,
        "a barge-in from AwaitingResponse"
    );
    m.handle(TurnInput::VadStop { at: at(base, 2500) }, &mut out);
    m.handle(
        TurnInput::Tick {
            now: at(base, 3200),
        },
        &mut out,
    );
    assert_eq!(m.state(), TurnState::AwaitingResponse);
    assert_eq!(m.epoch(), ep(1));

    out.clear();
    m.handle(
        TurnInput::BotAudioStarted {
            epoch: ep(0),
            at: at(base, 3300),
        },
        &mut out,
    );
    assert!(out.is_empty());
    assert_eq!(
        m.state(),
        TurnState::AwaitingResponse,
        "stale audio has no effect"
    );
    assert_eq!(m.stats().ignored(InputKind::BotAudioStarted), 1);

    out.clear();
    m.handle(
        TurnInput::BotAudioStarted {
            epoch: ep(1),
            at: at(base, 3400),
        },
        &mut out,
    );
    assert!(out.is_empty());
    assert_eq!(m.state(), TurnState::BotSpeaking);
}

#[test]
fn repeated_bot_audio_started_has_no_effect_and_is_not_counted() {
    // The caller may send BotAudioStarted per chunk: only the first one moves the state.
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::fixed(Duration::from_millis(700)));
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    m.handle(
        TurnInput::Tick {
            now: at(base, 1700),
        },
        &mut out,
    );
    for ms in [1800, 1900, 2000] {
        out.clear();
        m.handle(
            TurnInput::BotAudioStarted {
                epoch: ep(0),
                at: at(base, ms),
            },
            &mut out,
        );
        assert!(out.is_empty());
    }
    assert_eq!(m.state(), TurnState::BotSpeaking);
    assert_eq!(m.stats().ignored(InputKind::BotAudioStarted), 0);
}

#[test]
fn barge_in_while_awaiting_the_response_also_interrupts() {
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::adaptive());
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    m.handle(
        TurnInput::Verdict {
            request: AnalysisId::new(1),
            complete: true,
            probability: 0.9,
            at: at(base, 1200),
        },
        &mut out,
    );
    assert_eq!(m.state(), TurnState::AwaitingResponse);
    out.clear();
    m.handle(TurnInput::VadStart { at: at(base, 1300) }, &mut out);
    assert_eq!(out, vec![intr(1), start(1)]);
}

#[test]
fn every_user_turn_after_the_first_bumps_the_epoch() {
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::fixed(Duration::from_millis(700)));
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    assert_eq!(out, vec![start(0)], "the first turn uses epoch zero");
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    m.handle(
        TurnInput::Tick {
            now: at(base, 1700),
        },
        &mut out,
    );
    m.handle(
        TurnInput::BotAudioStarted {
            epoch: ep(0),
            at: at(base, 1800),
        },
        &mut out,
    );
    m.handle(
        TurnInput::BotAudioEnded {
            epoch: ep(0),
            at: at(base, 3000),
        },
        &mut out,
    );
    assert_eq!(m.state(), TurnState::Idle);

    out.clear();
    m.handle(TurnInput::VadStart { at: at(base, 5000) }, &mut out);
    assert_eq!(
        out,
        vec![start(1)],
        "a later turn bumps the epoch, no interrupt"
    );
}

#[test]
fn empty_turn_needs_bot_audio_ended_to_return_to_idle() {
    // An empty transcript makes no LLM request. The caller reports that with BotAudioEnded; then
    // the next user turn is not a barge-in. Without the report the machine stays in
    // AwaitingResponse and the next VadStart emits an Interrupt against nothing.
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::fixed(Duration::from_millis(700)));
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    m.handle(
        TurnInput::Tick {
            now: at(base, 1700),
        },
        &mut out,
    );
    out.clear();
    m.handle(
        TurnInput::BotAudioEnded {
            epoch: ep(0),
            at: at(base, 1800),
        },
        &mut out,
    );
    assert!(out.is_empty());
    assert_eq!(m.state(), TurnState::Idle);

    out.clear();
    m.handle(TurnInput::VadStart { at: at(base, 5000) }, &mut out);
    assert_eq!(out, vec![start(1)], "no Interrupt after an empty turn");
}

#[test]
fn punctuated_final_shortens_the_wait_after_an_incomplete_verdict() {
    let base = origin();
    for text in ["Nasılsın?", "Tamam.  ", "Bitti!", "Bekle…"] {
        let mut m = TurnMachine::new(TurnPolicy::adaptive());
        let mut out = Vec::new();
        m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
        m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
        m.handle(
            TurnInput::Verdict {
                request: AnalysisId::new(1),
                complete: false,
                probability: 0.2,
                at: at(base, 1200),
            },
            &mut out,
        );
        out.clear();
        m.handle(
            TurnInput::SttFinal {
                text: text.into(),
                at: at(base, 1300),
            },
            &mut out,
        );
        assert_eq!(
            out,
            vec![arm(base, 2100)],
            "{text:?}: final grace after the final"
        );

        out.clear();
        m.handle(
            TurnInput::Tick {
                now: at(base, 2100),
            },
            &mut out,
        );
        assert_eq!(out, vec![end(0, EndReason::SttPunctuation)], "{text:?}");
    }
}

#[test]
fn sentence_final_punctuation_is_the_documented_set() {
    // The shortening happens exactly for the texts whose last non-whitespace character is one of
    // `. ? ! …`.
    let base = origin();
    let cases = [
        ("Tamam…", true),
        ("Tamam…  ", true),
        ("Evet?", true),
        ("Harika!", true),
        ("3.", true),
        ("Evet,", false),
        ("Evet", false),
        ("Evet.)", false),
        ("", false),
    ];
    for (text, shortens) in cases {
        let mut m = TurnMachine::new(TurnPolicy::adaptive());
        let mut out = Vec::new();
        m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
        m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
        m.handle(
            TurnInput::Verdict {
                request: AnalysisId::new(1),
                complete: false,
                probability: 0.2,
                at: at(base, 1200),
            },
            &mut out,
        );
        out.clear();
        m.handle(
            TurnInput::SttFinal {
                text: text.into(),
                at: at(base, 1300),
            },
            &mut out,
        );
        let expected = if shortens {
            vec![arm(base, 2100)]
        } else {
            vec![]
        };
        assert_eq!(out, expected, "{text:?}");
    }
}

#[test]
fn unpunctuated_final_keeps_the_fallback_deadline() {
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::adaptive());
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    m.handle(
        TurnInput::Verdict {
            request: AnalysisId::new(1),
            complete: false,
            probability: 0.2,
            at: at(base, 1200),
        },
        &mut out,
    );
    out.clear();
    m.handle(
        TurnInput::SttFinal {
            text: "bana bir bakar mısın".into(),
            at: at(base, 1300),
        },
        &mut out,
    );
    assert!(out.is_empty(), "no punctuation, no shortening");
    assert_eq!(
        m.armed_deadline(),
        Some(at(base, 4000)),
        "deadline unchanged"
    );
    m.handle(
        TurnInput::Tick {
            now: at(base, 4000),
        },
        &mut out,
    );
    assert_eq!(out, vec![end(0, EndReason::Fallback)]);
}

#[test]
fn a_second_final_in_the_same_pause_is_ignored() {
    // One final answers one stop. The second one has nothing to answer: it neither moves the
    // deadline later nor earlier.
    let base = origin();
    for (second, expected_ms) in [("bana bir bakar misin", 2100), ("Tamam.", 2100)] {
        let mut m = TurnMachine::new(TurnPolicy::adaptive());
        let mut out = Vec::new();
        m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
        m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
        m.handle(
            TurnInput::Verdict {
                request: AnalysisId::new(1),
                complete: false,
                probability: 0.2,
                at: at(base, 1200),
            },
            &mut out,
        );
        out.clear();
        m.handle(
            TurnInput::SttFinal {
                text: "Evet.".into(),
                at: at(base, 1300),
            },
            &mut out,
        );
        assert_eq!(out, vec![arm(base, 2100)]);

        out.clear();
        m.handle(
            TurnInput::SttFinal {
                text: second.into(),
                at: at(base, 1400),
            },
            &mut out,
        );
        assert!(out.is_empty(), "{second:?}: no effect");
        assert_eq!(m.armed_deadline(), Some(at(base, expected_ms)));
        assert_eq!(m.stats().ignored(InputKind::SttFinal), 1, "{second:?}");

        out.clear();
        m.handle(
            TurnInput::Tick {
                now: at(base, 2100),
            },
            &mut out,
        );
        assert_eq!(out, vec![end(0, EndReason::SttPunctuation)]);
    }
}

#[test]
fn late_complete_verdict_never_extends_the_fallback() {
    // A verdict 100 ms before the fallback would wait until 4700 ms. The fallback (4000 ms) stays.
    // The end reason is FinalGrace, as documented for a complete verdict whose final did not
    // arrive in time (or whose fallback came first).
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::Adaptive {
        fallback_silence: Duration::from_secs(3),
        require_final: true,
        final_grace: Duration::from_millis(800),
    });
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    out.clear();
    m.handle(
        TurnInput::Verdict {
            request: AnalysisId::new(1),
            complete: true,
            probability: 0.97,
            at: at(base, 3900),
        },
        &mut out,
    );
    assert!(
        out.is_empty(),
        "the armed 4000 ms deadline already is the earlier one"
    );
    assert_eq!(m.armed_deadline(), Some(at(base, 4000)));

    out.clear();
    m.handle(
        TurnInput::Tick {
            now: at(base, 4000),
        },
        &mut out,
    );
    assert_eq!(out, vec![end(0, EndReason::FinalGrace)]);
}

#[test]
fn late_punctuated_final_never_extends_the_fallback() {
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::adaptive());
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    m.handle(
        TurnInput::Verdict {
            request: AnalysisId::new(1),
            complete: false,
            probability: 0.2,
            at: at(base, 3900),
        },
        &mut out,
    );
    out.clear();
    m.handle(
        TurnInput::SttFinal {
            text: "Evet.".into(),
            at: at(base, 3950),
        },
        &mut out,
    );
    assert!(
        out.is_empty(),
        "3950 + 800 would pass the fallback: no change"
    );
    assert_eq!(m.armed_deadline(), Some(at(base, 4000)));

    out.clear();
    m.handle(
        TurnInput::Tick {
            now: at(base, 4000),
        },
        &mut out,
    );
    assert_eq!(out, vec![end(0, EndReason::Fallback)]);
}

#[test]
fn final_during_speech_answers_the_owed_final_without_effect() {
    // The first utterance's final arrives while the user is already speaking again.
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::adaptive());
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    m.handle(TurnInput::VadStart { at: at(base, 1100) }, &mut out);
    out.clear();
    m.handle(
        TurnInput::SttFinal {
            text: "Merhaba.".into(),
            at: at(base, 1500),
        },
        &mut out,
    );
    assert!(out.is_empty());
    assert_eq!(m.state(), TurnState::UserSpeaking);
    assert_eq!(m.stats().total_ignored(), 0, "accepted, not ignored");
}

#[test]
fn unsolicited_final_is_ignored_and_counted() {
    // No VAD stop is owed a final: the final is stray.
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::adaptive());
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    out.clear();
    m.handle(
        TurnInput::SttFinal {
            text: "Merhaba.".into(),
            at: at(base, 500),
        },
        &mut out,
    );
    assert!(out.is_empty());
    assert_eq!(m.state(), TurnState::UserSpeaking);
    assert_eq!(m.stats().ignored(InputKind::SttFinal), 1);
}

#[test]
fn final_before_a_complete_verdict_ends_the_turn_on_the_verdict() {
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::Adaptive {
        fallback_silence: Duration::from_secs(3),
        require_final: true,
        final_grace: Duration::from_millis(800),
    });
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    out.clear();
    m.handle(
        TurnInput::SttFinal {
            text: "ok.".into(),
            at: at(base, 1100),
        },
        &mut out,
    );
    assert!(out.is_empty(), "the final alone does not end the turn");
    m.handle(
        TurnInput::Verdict {
            request: AnalysisId::new(1),
            complete: true,
            probability: 0.97,
            at: at(base, 1200),
        },
        &mut out,
    );
    assert_eq!(out, vec![cancel(), end(0, EndReason::Verdict)]);
}

#[test]
fn a_timer_is_replaced_never_stacked() {
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::Adaptive {
        fallback_silence: Duration::from_secs(3),
        require_final: true,
        final_grace: Duration::from_millis(800),
    });
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    out.clear();
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    assert_eq!(
        out.iter()
            .filter(|a| matches!(a, TurnAction::ArmTimer { .. }))
            .count(),
        1
    );
    out.clear();
    m.handle(
        TurnInput::Verdict {
            request: AnalysisId::new(1),
            complete: true,
            probability: 0.97,
            at: at(base, 1200),
        },
        &mut out,
    );
    // The earlier 4000 ms deadline is replaced by the grace; no cancel in between.
    assert_eq!(out, vec![arm(base, 2000)]);
    assert_eq!(m.armed_deadline(), Some(at(base, 2000)));
}

#[test]
fn hangup_closes_an_open_turn_and_cancels_the_timer() {
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::fixed(Duration::from_millis(700)));
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    out.clear();
    m.handle(TurnInput::Hangup { at: at(base, 1100) }, &mut out);
    assert_eq!(out, vec![cancel(), end(0, EndReason::Hangup)]);
    assert_eq!(m.state(), TurnState::Closed);
    assert_eq!(m.armed_deadline(), None);

    out.clear();
    m.handle(TurnInput::VadStart { at: at(base, 2000) }, &mut out);
    m.handle(
        TurnInput::Tick {
            now: at(base, 3000),
        },
        &mut out,
    );
    assert!(out.is_empty());
    assert_eq!(m.stats().total_ignored(), 2);
}

#[test]
fn hangup_during_speech_closes_the_turn_without_a_response() {
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::adaptive());
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    out.clear();
    m.handle(TurnInput::Hangup { at: at(base, 500) }, &mut out);
    assert_eq!(out, vec![end(0, EndReason::Hangup)]);
    assert_eq!(m.state(), TurnState::Closed);
}

#[test]
fn hangup_after_the_turn_ended_emits_nothing() {
    let base = origin();
    let mut m = TurnMachine::new(TurnPolicy::fixed(Duration::from_millis(700)));
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    m.handle(
        TurnInput::Tick {
            now: at(base, 1700),
        },
        &mut out,
    );
    out.clear();
    m.handle(TurnInput::Hangup { at: at(base, 1800) }, &mut out);
    assert!(out.is_empty(), "the turn was already closed");
    assert_eq!(m.state(), TurnState::Closed);
}

/// The inputs that reach `target` (AwaitingResponse or BotSpeaking) in epoch 1: the script of
/// [`setup`] to epoch 0's response, a barge-in, and a second user turn that ends.
fn epoch1_script(case: Case, target: StateKind, base: Instant) -> Vec<TurnInput> {
    let t = |ms| at(base, ms);
    let mut script = setup(case, StateKind::AwaitingResponse, base);
    script.push(TurnInput::VadStart { at: t(5000) });
    script.push(TurnInput::VadStop { at: t(6000) });
    match case {
        Case::Fixed => script.push(TurnInput::Tick { now: t(6700) }),
        Case::AdaptiveFinal => {
            script.push(TurnInput::Verdict {
                request: AnalysisId::new(2),
                complete: true,
                probability: 0.9,
                at: t(6200),
            });
            script.push(TurnInput::Tick { now: t(7000) });
        }
        Case::AdaptiveNoFinal => script.push(TurnInput::Verdict {
            request: AnalysisId::new(2),
            complete: true,
            probability: 0.9,
            at: t(6200),
        }),
    }
    if target == StateKind::BotSpeaking {
        script.push(TurnInput::BotAudioStarted {
            epoch: ep(1),
            at: t(8000),
        });
    }
    script
}

#[test]
fn stale_bot_audio_is_ignored_in_the_bot_states_of_a_later_epoch() {
    // A late end/start of the interrupted response (epoch 0) must neither end the current
    // response (the machine would go Idle and the next barge-in would emit no Interrupt) nor
    // start it. Also checked for an epoch from the future.
    let base = origin();
    for case in Case::ALL {
        for target in [StateKind::AwaitingResponse, StateKind::BotSpeaking] {
            for stale in [0, 2] {
                for kind in [InputKind::BotAudioEnded, InputKind::BotAudioStarted] {
                    let label = format!("{case:?} {target:?} stale epoch {stale} {kind:?}");
                    let mut m = TurnMachine::new(case.policy());
                    let mut out = Vec::new();
                    for step in epoch1_script(case, target, base) {
                        m.handle(step, &mut out);
                    }
                    assert_eq!(m.state().kind(), target, "{label}: setup state");
                    assert_eq!(m.epoch(), ep(1), "{label}: setup epoch");
                    assert_eq!(m.stats().total_ignored(), 0, "{label}: setup ignored");

                    out.clear();
                    let t = at(base, 9000);
                    let input = match kind {
                        InputKind::BotAudioEnded => TurnInput::BotAudioEnded {
                            epoch: ep(stale),
                            at: t,
                        },
                        _ => TurnInput::BotAudioStarted {
                            epoch: ep(stale),
                            at: t,
                        },
                    };
                    m.handle(input, &mut out);
                    assert!(out.is_empty(), "{label}: no actions");
                    assert_eq!(m.state().kind(), target, "{label}: state unchanged");
                    assert_eq!(m.stats().ignored(kind), 1, "{label}: counted");
                    assert_eq!(m.stats().total_ignored(), 1, "{label}: only that one");

                    // The machine still treats the next user speech as a barge-in.
                    out.clear();
                    m.handle(TurnInput::VadStart { at: at(base, 9500) }, &mut out);
                    assert_eq!(out, vec![intr(2), start(2)], "{label}: barge-in intact");
                }
            }
        }
    }
}

#[test]
fn a_tick_later_than_the_deadline_ends_the_pause_like_one_at_the_deadline() {
    // Real timers fire a bit after their deadline: the pause must end, with the right reason,
    // and leave the timer slot empty. (policy case, pause start, deadline ms, reason)
    let base = origin();
    let rows: [(Case, Pre, u64, EndReason); 6] = [
        (Case::Fixed, Pre::Fresh, 1700, EndReason::FixedSilence),
        (Case::AdaptiveNoFinal, Pre::Fresh, 4000, EndReason::Fallback),
        (
            Case::AdaptiveNoFinal,
            Pre::Incomplete,
            4000,
            EndReason::Fallback,
        ),
        (
            Case::AdaptiveFinal,
            Pre::Complete,
            2000,
            EndReason::FinalGrace,
        ),
        (
            Case::AdaptiveFinal,
            Pre::IncompleteFinalPunct,
            2100,
            EndReason::SttPunctuation,
        ),
        (
            Case::AdaptiveNoFinal,
            Pre::IncompleteFinalPunct,
            2100,
            EndReason::SttPunctuation,
        ),
    ];
    for (case, pre, deadline, reason) in rows {
        for late in [1u64, 5000] {
            let label = format!("{case:?} {pre:?} tick at deadline+{late}");
            let mut m = TurnMachine::new(case.policy());
            let mut out = Vec::new();
            for step in pre.script(base) {
                m.handle(step, &mut out);
            }
            assert_eq!(
                m.armed_deadline(),
                Some(at(base, deadline)),
                "{label}: armed"
            );
            out.clear();
            m.handle(
                TurnInput::Tick {
                    now: at(base, deadline + late),
                },
                &mut out,
            );
            assert_eq!(out, vec![end(0, reason)], "{label}: actions");
            assert_eq!(m.state(), TurnState::AwaitingResponse, "{label}: state");
            assert_eq!(m.armed_deadline(), None, "{label}: timer empty");
            assert_eq!(m.stats().total_ignored(), 0, "{label}: not ignored");
        }
    }
}

#[test]
fn closed_ignores_a_final_still_owed_by_the_stop_open_at_hangup() {
    // After a hang-up the machine may still owe finals (a stop was open). They must be ignored
    // and counted, with no effect, whatever state the call hung up from.
    let base = origin();
    let t = |ms| at(base, ms);
    for case in Case::ALL {
        let mut priors: Vec<(String, Vec<TurnInput>)> = StateKind::ALL
            .iter()
            .filter(|k| **k != StateKind::Closed)
            .map(|k| (format!("{k:?}"), setup(case, *k, base)))
            .collect();
        // Resumed speech with an earlier stop still owed its final.
        priors.push((
            "UserSpeaking after a resume".into(),
            vec![
                TurnInput::VadStart { at: t(0) },
                TurnInput::VadStop { at: t(1000) },
                TurnInput::VadStart { at: t(1100) },
            ],
        ));
        for (name, mut script) in priors {
            script.push(TurnInput::Hangup { at: t(9000) });
            for kind in InputKind::ALL {
                let label = format!("{case:?} hung up from {name}, then {kind:?}");
                let mut m = TurnMachine::new(case.policy());
                let mut out = Vec::new();
                for step in script.clone() {
                    m.handle(step, &mut out);
                }
                assert_eq!(m.state(), TurnState::Closed, "{label}: closed");
                let before = m.stats();
                out.clear();
                m.handle(input_of(kind, t(9500)), &mut out);
                assert!(out.is_empty(), "{label}: no actions");
                assert_eq!(m.state(), TurnState::Closed, "{label}: still closed");
                assert_eq!(m.armed_deadline(), None, "{label}: no timer");
                assert_eq!(
                    m.stats().ignored(kind) - before.ignored(kind),
                    1,
                    "{label}: counted"
                );
            }
        }
    }
}

#[test]
fn huge_durations_clamp_instead_of_panicking() {
    // `Duration::MAX` overflows the clock: every deadline kind must clamp to a far deadline (no
    // panic, no early end).
    let base = origin();
    let year = Duration::from_secs(365 * 24 * 3600);
    let far = |deadline: Instant, from: Instant| deadline > from + year;
    let no_end = |out: &[TurnAction]| {
        !out.iter()
            .any(|a| matches!(a, TurnAction::EndUserTurn { .. }))
    };

    // FixedSilence.
    let mut m = TurnMachine::new(TurnPolicy::fixed(Duration::MAX));
    let mut out = Vec::new();
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    out.clear();
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    let deadline = m.armed_deadline().expect("armed");
    assert!(far(deadline, at(base, 1000)), "fixed: far deadline");
    out.clear();
    m.handle(
        TurnInput::Tick {
            now: at(base, 1000) + Duration::from_secs(3600),
        },
        &mut out,
    );
    assert!(no_end(&out), "fixed: no early end: {out:?}");
    assert_eq!(m.state().kind(), StateKind::UserPaused);

    // Adaptive: fallback, verdict grace and final grace all huge.
    let policy = TurnPolicy::Adaptive {
        fallback_silence: Duration::MAX,
        require_final: true,
        final_grace: Duration::MAX,
    };
    let verdict = |complete: bool| TurnInput::Verdict {
        request: AnalysisId::new(1),
        complete,
        probability: 0.5,
        at: at(base, 1200),
    };
    let punctuated = TurnInput::SttFinal {
        text: "Evet.".into(),
        at: at(base, 1300),
    };
    // Fallback (pending), then an incomplete verdict, then a punctuated final (its grace).
    let mut m = TurnMachine::new(policy);
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    out.clear();
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    let deadline = m.armed_deadline().expect("armed");
    assert!(far(deadline, at(base, 1000)), "pending: far deadline");
    for input in [verdict(false), punctuated] {
        out.clear();
        m.handle(input, &mut out);
        assert!(no_end(&out), "adaptive incomplete: no early end: {out:?}");
        let deadline = m.armed_deadline().expect("still timed");
        assert!(far(deadline, at(base, 1000)), "incomplete: far deadline");
    }
    // A complete verdict waiting for the final (its grace).
    let mut m = TurnMachine::new(policy);
    m.handle(TurnInput::VadStart { at: at(base, 0) }, &mut out);
    m.handle(TurnInput::VadStop { at: at(base, 1000) }, &mut out);
    out.clear();
    m.handle(verdict(true), &mut out);
    assert!(no_end(&out), "complete: no early end: {out:?}");
    let deadline = m.armed_deadline().expect("timed");
    assert!(far(deadline, at(base, 1000)), "complete: far deadline");
    assert_eq!(m.state().kind(), StateKind::UserPaused);
}

/// A small deterministic generator (no external crate, same inputs every run).
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

/// Checks one `handle` call against an independent description of the transition rules: the next
/// state kind, the actions' shape, the end reason, stale-epoch bot audio, ticks before and after
/// the armed deadline, and the ignored counter.
#[allow(clippy::too_many_arguments)]
fn check_against_oracle(
    case: Case,
    input: &TurnInput,
    before: TurnState,
    pause: Option<Pause>,
    armed: Option<Instant>,
    epoch_before: TurnEpoch,
    m: &TurnMachine,
    out: &[TurnAction],
    ignored_before: u64,
) {
    use StateKind::*;
    let label = format!("{case:?} {:?} x {:?}", before.kind(), input.kind());
    let after = m.state().kind();
    let ignored = m.stats().total_ignored() - ignored_before;
    let ends: Vec<EndReason> = out
        .iter()
        .filter_map(|a| match a {
            TurnAction::EndUserTurn { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect();
    let has_action = |f: fn(&TurnAction) -> bool| out.iter().any(f);
    // `None` expected: the input must be ignored (state unchanged, no actions, counted).
    let expect_ignored = |why: &str| {
        assert_eq!(after, before.kind(), "{label}: {why}: state unchanged");
        assert!(out.is_empty(), "{label}: {why}: no actions");
        assert_eq!(ignored, 1, "{label}: {why}: counted");
    };
    if before.kind() == Closed {
        expect_ignored("closed");
        return;
    }
    match *input {
        TurnInput::VadStart { .. } => match before.kind() {
            UserSpeaking => expect_ignored("already speaking"),
            Idle => {
                assert_eq!(after, UserSpeaking, "{label}");
                assert!(
                    matches!(out, [TurnAction::StartUserTurn { .. }]),
                    "{label}: {out:?}"
                );
            }
            UserPaused => {
                assert_eq!(after, UserSpeaking, "{label}: resume");
                assert!(!has_action(|a| matches!(
                    a,
                    TurnAction::StartUserTurn { .. }
                )));
            }
            AwaitingResponse | BotSpeaking => {
                assert_eq!(after, UserSpeaking, "{label}: barge-in");
                let new_epoch = epoch_before.next();
                assert_eq!(
                    out,
                    [
                        TurnAction::Interrupt { new_epoch },
                        TurnAction::StartUserTurn { epoch: new_epoch }
                    ],
                    "{label}"
                );
            }
            Closed => {}
        },
        TurnInput::VadStop { .. } => {
            if before.kind() == UserSpeaking {
                assert_eq!(after, UserPaused, "{label}");
            } else {
                expect_ignored("no speech to stop");
            }
        }
        TurnInput::SttInterim { .. } => {
            if matches!(before.kind(), UserSpeaking | UserPaused) {
                assert_eq!(after, before.kind(), "{label}");
                assert!(out.is_empty(), "{label}");
            } else {
                expect_ignored("interim outside a user turn");
            }
        }
        TurnInput::SttFinal { .. } | TurnInput::Verdict { .. } => {
            if before.kind() == UserPaused {
                // Either the pause goes on or the verdict/final completes it.
                if after == AwaitingResponse {
                    assert_eq!(ends.len(), 1, "{label}: one end");
                    assert_eq!(ends[0], EndReason::Verdict, "{label}: reason");
                } else {
                    assert_eq!(after, UserPaused, "{label}");
                    assert!(ends.is_empty(), "{label}");
                }
            } else {
                assert_eq!(after, before.kind(), "{label}: only a pause reacts");
                assert!(out.is_empty(), "{label}: no actions outside a pause");
            }
        }
        TurnInput::Tick { now } => match (before, armed, pause) {
            (TurnState::UserPaused(_), Some(deadline), Some(p)) => {
                if now >= deadline {
                    // A tick at or after the armed deadline always ends the pause.
                    assert_eq!(after, AwaitingResponse, "{label}: late tick ends the pause");
                    assert_eq!(ends.len(), 1, "{label}: one end");
                    let allowed: &[EndReason] = match (case, p.analysis()) {
                        (Case::Fixed, _) => &[EndReason::FixedSilence],
                        (_, Analysis::Complete { .. }) => &[EndReason::FinalGrace],
                        (_, Analysis::Incomplete { .. }) => {
                            if p.final_seen().is_some_and(|f| f.ends_sentence) {
                                &[EndReason::Fallback, EndReason::SttPunctuation]
                            } else {
                                &[EndReason::Fallback]
                            }
                        }
                        _ => &[EndReason::Fallback],
                    };
                    assert!(allowed.contains(&ends[0]), "{label}: reason {:?}", ends[0]);
                } else {
                    assert_eq!(after, UserPaused, "{label}: early tick re-arms");
                    assert!(ends.is_empty(), "{label}");
                    assert!(
                        matches!(out, [TurnAction::ArmTimer { .. }]),
                        "{label}: re-armed: {out:?}"
                    );
                }
            }
            _ => expect_ignored("tick outside a pause"),
        },
        TurnInput::BotAudioStarted { epoch, .. } => {
            if epoch != epoch_before {
                expect_ignored("stale epoch");
            } else if before.kind() == AwaitingResponse {
                assert_eq!(after, BotSpeaking, "{label}");
                assert!(out.is_empty(), "{label}");
            } else if before.kind() == BotSpeaking {
                assert_eq!(after, BotSpeaking, "{label}");
                assert_eq!(ignored, 0, "{label}: repeat is not counted");
            } else {
                expect_ignored("no response expected");
            }
        }
        TurnInput::BotAudioEnded { epoch, .. } => {
            if epoch != epoch_before {
                expect_ignored("stale epoch");
            } else if matches!(before.kind(), AwaitingResponse | BotSpeaking) {
                assert_eq!(after, Idle, "{label}");
                assert!(out.is_empty(), "{label}");
            } else {
                expect_ignored("no response running");
            }
        }
        TurnInput::UserText { .. } => match before.kind() {
            // No barge-in candidate without an interruption policy: a turn that ends at once,
            // after an `Interrupt` when the bot was busy.
            Idle | AwaitingResponse | BotSpeaking => {
                assert_eq!(after, AwaitingResponse, "{label}");
                assert_eq!(ends, vec![EndReason::Text], "{label}");
                assert_eq!(
                    has_action(|a| matches!(a, TurnAction::Interrupt { .. })),
                    before.kind() != Idle,
                    "{label}: interrupt only when the bot is busy"
                );
                assert!(
                    has_action(|a| matches!(a, TurnAction::StartUserTurn { .. })),
                    "{label}: the turn starts"
                );
                assert_eq!(ignored, 0, "{label}");
            }
            _ => expect_ignored("a user turn is open"),
        },
        TurnInput::BotTurn { interrupt, .. } => match before.kind() {
            // No barge-in candidate without an interruption policy.
            Idle => {
                assert_eq!(after, AwaitingResponse, "{label}");
                assert!(
                    matches!(out, [TurnAction::StartBotTurn { .. }]),
                    "{label}: {out:?}"
                );
                assert_eq!(ignored, 0, "{label}");
            }
            AwaitingResponse | BotSpeaking if interrupt => {
                assert_eq!(after, AwaitingResponse, "{label}");
                assert!(
                    matches!(
                        out,
                        [
                            TurnAction::Interrupt { .. },
                            TurnAction::StartBotTurn { .. }
                        ]
                    ),
                    "{label}: {out:?}"
                );
                assert_eq!(ignored, 0, "{label}");
            }
            _ => expect_ignored("the line waits"),
        },
        TurnInput::Hangup { .. } => {
            assert_eq!(after, Closed, "{label}");
            let open = matches!(before.kind(), UserSpeaking | UserPaused);
            assert_eq!(
                ends,
                if open {
                    vec![EndReason::Hangup]
                } else {
                    vec![]
                },
                "{label}"
            );
        }
    }
}

#[test]
fn random_inputs_keep_every_invariant() {
    let base = origin();
    let texts = ["Merhaba.", "hi", "", "  ?  ", "ok!", "Tamam…"];
    for case in Case::ALL {
        let mut rng = Lcg(0x5EED_0026 ^ u64::from(case as u8));
        let mut m = TurnMachine::new(case.policy());
        let mut model: Option<Instant> = None;
        // Whether a user turn is open, as the caller sees it (Start/End pairing).
        let mut open = false;
        let mut last_id: u64 = 0;
        let mut now = base;
        let mut out = Vec::new();
        let mut reasons: HashSet<String> = HashSet::new();
        let mut barge_ins = 0u32;
        for _ in 0..20_000 {
            now += Duration::from_millis(rng.below(500));
            // A hung-up machine is replaced after a few ignored inputs (so that Closed is
            // exercised, but most steps run on a live machine), on the same clock.
            if m.state() == TurnState::Closed && rng.below(3) == 0 {
                m = TurnMachine::new(case.policy());
                model = None;
                open = false;
                last_id = 0; // a fresh machine numbers its analyses from 1 again
                out.clear();
                continue;
            }
            let input = if rng.below(60) == 0 {
                TurnInput::Hangup { at: now }
            } else if rng.below(40) == 0 {
                TurnInput::UserText { at: now }
            } else if rng.below(40) == 0 {
                TurnInput::BotTurn {
                    interrupt: rng.below(2) == 0,
                    at: now,
                }
            } else {
                match rng.below(10) {
                    0 => TurnInput::VadStart { at: now },
                    1 => TurnInput::VadStop { at: now },
                    2 => TurnInput::SttInterim {
                        text: "x".into(),
                        at: now,
                    },
                    3 => TurnInput::SttFinal {
                        text: texts[rng.below(texts.len() as u64) as usize].into(),
                        at: now,
                    },
                    4 | 5 => {
                        let id = if rng.below(2) == 0 && last_id > 0 {
                            last_id
                        } else {
                            1 + rng.below(4)
                        };
                        TurnInput::Verdict {
                            request: AnalysisId::new(id),
                            complete: rng.below(2) == 0,
                            probability: 0.5,
                            at: now,
                        }
                    }
                    6 => {
                        // The timer fires exactly at, or (like real timers) after, its deadline.
                        let when = match model {
                            Some(d) => match rng.below(3) {
                                0 => d,
                                1 => d + Duration::from_millis(1 + rng.below(5000)),
                                _ => now,
                            },
                            None => now,
                        };
                        TurnInput::Tick { now: when }
                    }
                    7 => TurnInput::BotAudioStarted {
                        epoch: ep(m.epoch().get().saturating_sub(rng.below(3) as u32)),
                        at: now,
                    },
                    8 => TurnInput::BotAudioEnded {
                        epoch: ep(m.epoch().get().saturating_sub(rng.below(3) as u32)),
                        at: now,
                    },
                    // Resume or barge in more often than the rest.
                    _ => TurnInput::VadStart { at: now },
                }
            };
            // The caller's timer fires: it is consumed before the tick is delivered.
            if let TurnInput::Tick { now: fired } = input
                && model.is_some_and(|d| fired >= d)
            {
                model = None;
            }
            let epoch_before = m.epoch();
            let state_before = m.state();
            let pause_before = pause_of(&m);
            let armed_before = m.armed_deadline();
            let ignored_before = m.stats().total_ignored();
            let input_for_oracle = input.clone();
            out.clear();
            m.handle(input, &mut out);
            check_against_oracle(
                case,
                &input_for_oracle,
                state_before,
                pause_before,
                armed_before,
                epoch_before,
                &m,
                &out,
                ignored_before,
            );

            let arms = out
                .iter()
                .filter(|a| matches!(a, TurnAction::ArmTimer { .. }))
                .count();
            assert!(arms <= 1, "{case:?}: at most one ArmTimer per input");
            for action in &out {
                match action {
                    TurnAction::ArmTimer { deadline } => model = Some(*deadline),
                    TurnAction::CancelTimer => {
                        assert!(model.is_some(), "{case:?}: cancel without an armed timer");
                        model = None;
                    }
                    TurnAction::StartUserTurn { epoch } => {
                        assert!(!open, "{case:?}: a turn starts only when none is open");
                        open = true;
                        assert_eq!(*epoch, m.epoch(), "{case:?}: turn epoch");
                    }
                    TurnAction::EndUserTurn { epoch, reason } => {
                        assert!(open, "{case:?}: a turn ends only when one is open");
                        open = false;
                        assert_eq!(*epoch, m.epoch(), "{case:?}: turn epoch");
                        reasons.insert(format!("{reason:?}"));
                    }
                    TurnAction::Interrupt { new_epoch } => {
                        assert_eq!(*new_epoch, m.epoch(), "{case:?}: interrupt epoch");
                        barge_ins += 1;
                    }
                    TurnAction::RequestAnalysis { id } => {
                        assert!(id.get() > last_id, "{case:?}: analysis ids increase");
                        last_id = id.get();
                    }
                    TurnAction::StartBotTurn { epoch } => {
                        assert!(
                            !open,
                            "{case:?}: a bot line never starts inside a user turn"
                        );
                        assert_eq!(*epoch, m.epoch(), "{case:?}: bot turn epoch");
                    }
                }
            }
            assert_eq!(m.armed_deadline(), model, "{case:?}: timer slot agrees");
            match m.state() {
                TurnState::UserPaused(_) => assert!(model.is_some(), "{case:?}: pause is timed"),
                _ => assert!(model.is_none(), "{case:?}: only a pause holds a timer"),
            }
            assert_eq!(
                open,
                matches!(
                    m.state(),
                    TurnState::UserSpeaking | TurnState::UserPaused(_)
                ),
                "{case:?}: a user turn is open exactly while the user is in a turn"
            );
            assert!(m.epoch() >= epoch_before, "{case:?}: epochs never go back");
        }
        assert!(barge_ins > 0, "{case:?}: barge-ins exercised");
        let expected: &[&str] = match case {
            Case::Fixed => &["FixedSilence", "Hangup", "Text"],
            Case::AdaptiveFinal => &[
                "Verdict",
                "FinalGrace",
                "Fallback",
                "SttPunctuation",
                "Hangup",
                "Text",
            ],
            Case::AdaptiveNoFinal => &["Verdict", "Fallback", "SttPunctuation", "Hangup", "Text"],
        };
        for reason in expected {
            assert!(
                reasons.contains(*reason),
                "{case:?}: end reason {reason} never observed: {reasons:?}"
            );
        }
    }
}
