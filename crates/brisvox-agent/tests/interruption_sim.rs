//! Simulator-level tests of the interruption guards.
//!
//! Each test plays a whole scripted call through a [`TurnMachine`] with the simulator's pieces:
//! the caller is a `brisvox_sim::scenario::Scenario` (what is said and when), the VAD timing is
//! the simulator energy VAD's `VadConfig` (a start after `start_after` of speech, a stop only
//! after `stop_after` of silence, backdated to the end of the speech) and STT is modelled as
//! cumulative interims every 200 ms plus a final after the stop. The "driver" around the machine
//! is a thin deterministic event loop (virtual time, no sleeping): it delivers the inputs in
//! delivery-time order, fires the machine's single timer at its deadline, answers every ended
//! user turn with a scripted bot response and flushes the stale bot audio of an interrupted
//! response, like the real `TurnDriver` must.
//!
//! The reference pipeline of `brisvox-sim` (`TurnManager`, scenarios s01..s11) does not use
//! `TurnMachine` yet: the production pipeline wires it in. Until then these tests are the guards'
//! end-to-end evidence: every guard in a scripted call, plus the s-suite style invariants
//! checked on each run (epochs only grow, `Interrupt` is followed by its `StartUserTurn`, every
//! user turn that starts also ends, no stale bot audio reaches the machine after an interrupt).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::time::Duration;

use brisvox_agent::{
    EndReason, InputKind, InterruptionPolicy, StateKind, TurnAction, TurnInput, TurnMachine,
    TurnPolicy, VetoReason, count_words,
};
use brisvox_core::TurnEpoch;
use brisvox_sim::scenario::{FRAME, Scenario, Step};
use brisvox_sim::vad::VadConfig;
use tokio::time::Instant;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn ep(n: u32) -> TurnEpoch {
    TurnEpoch::new(n)
}

#[allow(clippy::disallowed_methods)]
fn origin() -> Instant {
    Instant::now()
}

/// One scheduled delivery to the machine.
#[derive(Debug, Clone)]
enum Ev {
    /// A VAD start dated at the given call time.
    VadStart(Duration),
    /// A VAD stop dated at the given call time (the backdated end of the speech).
    VadStop(Duration),
    Interim(String),
    Final(String),
    BotStart(TurnEpoch),
    BotEnd(TurnEpoch),
}

impl Ev {
    fn bot_epoch(&self) -> Option<TurnEpoch> {
        match self {
            Ev::BotStart(e) | Ev::BotEnd(e) => Some(*e),
            _ => None,
        }
    }
}

/// The simulated surroundings of the machine.
#[derive(Clone)]
struct Sim {
    vad: VadConfig,
    /// STT emits a cumulative interim every this long while the caller speaks.
    interim_every: Duration,
    /// The final arrives this long after the VAD stop was delivered.
    final_lag: Duration,
    /// An ended user turn is answered with audio starting this long after the end.
    response_latency: Duration,
    /// How long the bot's response audio lasts.
    bot_len: Duration,
    /// Report `BotAudioStarted` for every 20 ms chunk instead of once per response.
    chunk_reports: bool,
    /// The fixed silence that ends a user turn.
    silence: Duration,
}

impl Default for Sim {
    fn default() -> Self {
        Self {
            vad: VadConfig::default(),
            interim_every: ms(200),
            final_lag: ms(100),
            response_latency: ms(600),
            bot_len: ms(6000),
            chunk_reports: false,
            silence: ms(700),
        }
    }
}

/// What a finished call looked like.
struct Run {
    log: Vec<(Duration, TurnAction)>,
    machine: TurnMachine,
    /// Stale bot-audio events the driver dropped at an interrupt.
    flushed: usize,
}

impl Run {
    fn interrupts(&self) -> Vec<(Duration, TurnEpoch)> {
        self.log
            .iter()
            .filter_map(|(at, a)| match a {
                TurnAction::Interrupt { new_epoch } => Some((*at, *new_epoch)),
                _ => None,
            })
            .collect()
    }

    fn started_turns(&self) -> Vec<TurnEpoch> {
        self.log
            .iter()
            .filter_map(|(_, a)| match a {
                TurnAction::StartUserTurn { epoch } => Some(*epoch),
                _ => None,
            })
            .collect()
    }

    fn vetoed(&self, reason: VetoReason) -> u64 {
        self.machine.interruption_stats().vetoed(reason)
    }

    /// The first user turn ended at 1700 ms (the script of [`call`]): the bot answers from 2300.
    fn assert_first_turn_ended(&self) {
        assert!(
            self.log.iter().any(|(at, a)| {
                *at == ms(1700)
                    && matches!(a, TurnAction::EndUserTurn { epoch, .. } if *epoch == TurnEpoch::ZERO)
            }),
            "the first turn ends at 1700: {:?}",
            self.log
        );
    }

    /// The simulator-suite invariants every call must keep, whatever the guards decided.
    fn check_invariants(&self) {
        // Epochs only grow; every Interrupt is immediately followed by the StartUserTurn of the
        // same epoch; every started turn ends in its own epoch.
        let mut epoch = TurnEpoch::ZERO;
        let mut starts = 0usize;
        let mut ends = 0usize;
        for (i, (_, action)) in self.log.iter().enumerate() {
            match action {
                TurnAction::Interrupt { new_epoch } => {
                    assert!(*new_epoch > epoch, "epoch went back: {action:?}");
                    epoch = *new_epoch;
                    assert_eq!(
                        self.log.get(i + 1).map(|(_, a)| a),
                        Some(&TurnAction::StartUserTurn { epoch }),
                        "an Interrupt must be followed by its StartUserTurn"
                    );
                }
                TurnAction::StartUserTurn { epoch: e } => {
                    assert!(*e >= epoch, "turn epoch went back: {action:?}");
                    epoch = *e;
                    starts += 1;
                }
                TurnAction::EndUserTurn { epoch: e, .. } => {
                    assert_eq!(*e, epoch, "a turn ends in its own epoch");
                    ends += 1;
                }
                _ => {}
            }
        }
        assert_eq!(starts, ends, "every user turn that starts also ends");
        // No stale bot audio reached the machine, and no final was stray: a vetoed or promoted
        // candidate leaves no illegal input behind.
        let stats = self.machine.stats();
        assert_eq!(stats.ignored(InputKind::BotAudioStarted), 0);
        assert_eq!(stats.ignored(InputKind::BotAudioEnded), 0);
        assert_eq!(stats.ignored(InputKind::SttFinal), 0);
        // The call ran to the end: no timer is pending and no candidate is left over.
        assert!(self.machine.armed_deadline().is_none());
        assert!(self.machine.barge_in_candidate().is_none());
    }
}

/// The speech segments of a scenario with their text: (start, end, text).
fn segments(scenario: &Scenario) -> Vec<(Duration, Duration, String)> {
    let texts = scenario.steps().iter().filter_map(|step| match step {
        Step::Speak { text, duration } if *duration >= FRAME => Some(text.clone()),
        _ => None,
    });
    scenario
        .speech_segments()
        .into_iter()
        .zip(texts)
        .map(|((s, e), t)| (s, e, t))
        .collect()
}

/// The first words of `text` that STT would have heard after `elapsed` of an utterance of
/// `length` (a proportional prefix, rounded up, at least one word).
fn word_prefix(text: &str, elapsed: Duration, length: Duration) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    let n = words.len() as u128;
    let shown = (n * elapsed.as_millis())
        .div_ceil(length.as_millis().max(1))
        .max(1);
    let shown = usize::try_from(shown)
        .unwrap_or(usize::MAX)
        .min(words.len());
    words[..shown].join(" ")
}

fn schedule(queue: &mut Vec<(Duration, u64, Ev)>, seq: &mut u64, when: Duration, ev: Ev) {
    *seq += 1;
    queue.push((when, *seq, ev));
}

impl Sim {
    /// Plays `scenario` against a machine with `policy`.
    fn play(&self, scenario: &Scenario, policy: InterruptionPolicy) -> Run {
        let base = origin();
        let at = |d: Duration| base + d;
        let mut machine = TurnMachine::with_interruption(TurnPolicy::fixed(self.silence), policy);
        let mut queue: Vec<(Duration, u64, Ev)> = Vec::new();
        let mut seq = 0u64;

        // The caller: what the energy VAD and the STT make of each speech segment.
        for (start, end, text) in segments(scenario) {
            let length = end - start;
            if length < self.vad.start_after {
                continue; // too short for the VAD to notice at all
            }
            let vad_start_delivered = start + self.vad.start_after;
            schedule(
                &mut queue,
                &mut seq,
                vad_start_delivered,
                Ev::VadStart(start),
            );
            if count_words(&text) > 0 {
                let mut k = 1u32;
                while self.interim_every * k < length {
                    let elapsed = self.interim_every * k;
                    let when = (start + elapsed).max(vad_start_delivered);
                    let interim = Ev::Interim(word_prefix(&text, elapsed, length));
                    schedule(&mut queue, &mut seq, when, interim);
                    k += 1;
                }
            }
            let stop_delivered = end + self.vad.stop_after;
            schedule(&mut queue, &mut seq, stop_delivered, Ev::VadStop(end));
            let fin = Ev::Final(text);
            schedule(&mut queue, &mut seq, stop_delivered + self.final_lag, fin);
        }

        let mut log = Vec::new();
        let mut armed: Option<Duration> = None;
        let mut flushed = 0usize;
        let mut out = Vec::new();
        loop {
            let next = queue
                .iter()
                .enumerate()
                .min_by_key(|(_, (when, seq, _))| (*when, *seq))
                .map(|(i, (when, _, _))| (i, *when));
            let (now, input) = match (next, armed) {
                (None, None) => break,
                (Some((_, event_at)), Some(deadline)) if deadline <= event_at => {
                    armed = None;
                    (deadline, TurnInput::Tick { now: at(deadline) })
                }
                (None, Some(deadline)) => {
                    armed = None;
                    (deadline, TurnInput::Tick { now: at(deadline) })
                }
                (Some((i, event_at)), _) => {
                    let (_, _, ev) = queue.remove(i);
                    let input = match ev {
                        Ev::VadStart(d) => TurnInput::VadStart { at: at(d) },
                        Ev::VadStop(d) => TurnInput::VadStop { at: at(d) },
                        Ev::Interim(text) => TurnInput::SttInterim {
                            text,
                            at: at(event_at),
                        },
                        Ev::Final(text) => TurnInput::SttFinal {
                            text,
                            at: at(event_at),
                        },
                        Ev::BotStart(epoch) => TurnInput::BotAudioStarted {
                            epoch,
                            at: at(event_at),
                        },
                        Ev::BotEnd(epoch) => TurnInput::BotAudioEnded {
                            epoch,
                            at: at(event_at),
                        },
                    };
                    (event_at, input)
                }
            };
            out.clear();
            machine.handle(input, &mut out);
            for action in out.drain(..) {
                match &action {
                    TurnAction::ArmTimer { deadline } => {
                        armed = Some(deadline.saturating_duration_since(base));
                    }
                    TurnAction::CancelTimer => armed = None,
                    TurnAction::EndUserTurn { epoch, reason } if *reason != EndReason::Hangup => {
                        // The scripted bot answers: audio from `first` for `bot_len`.
                        let first = now + self.response_latency;
                        schedule(&mut queue, &mut seq, first, Ev::BotStart(*epoch));
                        if self.chunk_reports {
                            let chunks = self.bot_len.as_millis() / FRAME.as_millis();
                            for c in 1..chunks {
                                let when = first + FRAME * u32::try_from(c).unwrap_or(0);
                                schedule(&mut queue, &mut seq, when, Ev::BotStart(*epoch));
                            }
                        }
                        let end = first + self.bot_len;
                        schedule(&mut queue, &mut seq, end, Ev::BotEnd(*epoch));
                    }
                    TurnAction::Interrupt { new_epoch } => {
                        // Flush: the old response's audio never reaches the caller again.
                        let before = queue.len();
                        queue.retain(|(_, _, ev)| ev.bot_epoch().is_none_or(|e| e >= *new_epoch));
                        flushed += before - queue.len();
                    }
                    _ => {}
                }
                log.push((now, action));
            }
        }
        Run {
            log,
            machine,
            flushed,
        }
    }
}

/// The recommended policy tuned to the simulator VAD (its hangover is `stop_after`).
fn policy(sim: &Sim) -> InterruptionPolicy {
    InterruptionPolicy {
        vad_hangover: sim.vad.stop_after,
        ..InterruptionPolicy::recommended()
    }
}

/// A call that opens with "hello there" (200..1000 ms): the VAD stop is delivered at 1300, the
/// turn ends at 1700 and the bot's audio starts at 2300 (it would last until 8300). Then the
/// caller says `text` for `duration_ms` starting at `at_ms`.
fn call(at_ms: u64, text: &str, duration_ms: u64) -> Scenario {
    Scenario::new()
        .silence(ms(200))
        .speak("hello there", ms(800))
        .silence(ms(at_ms - 1000))
        .speak(text, ms(duration_ms))
        .with_tail(ms(2000))
}

#[test]
fn without_guards_every_voice_interrupts_at_once() {
    // The guard-less behaviour (the baseline): a "mm-hmm" at 4000 interrupts when the VAD starts
    // (delivered 40 ms into the speech).
    let sim = Sim::default();
    let run = sim.play(&call(4000, "mm-hmm", 400), InterruptionPolicy::disabled());
    run.assert_first_turn_ended();
    assert_eq!(run.interrupts(), [(ms(4040), ep(1))]);
    run.check_invariants();
}

#[test]
fn a_backchannel_over_the_bot_does_not_interrupt() {
    for text in ["mm-hmm", "hı-hı", "evet", "uh-huh"] {
        let sim = Sim::default();
        let run = sim.play(&call(4000, text, 400), policy(&sim));
        run.assert_first_turn_ended();
        assert!(run.interrupts().is_empty(), "{text}: {:?}", run.log);
        assert_eq!(run.started_turns(), [TurnEpoch::ZERO], "{text}");
        assert_eq!(run.vetoed(VetoReason::Backchannel), 1, "{text}");
        // The bot talked on to its end.
        assert_eq!(run.machine.state().kind(), StateKind::Idle);
        assert_eq!(run.machine.epoch(), TurnEpoch::ZERO);
        assert_eq!(run.flushed, 0);
        run.check_invariants();
    }
}

#[test]
fn a_backchannel_that_grows_into_speech_interrupts() {
    // "yeah but wait a second" (1.2 s): the first interim is only "yeah", the second already
    // holds a real word. Min speech (300 ms + the 300 ms VAD hangover) then decides: 600 ms.
    let sim = Sim::default();
    let run = sim.play(&call(4000, "yeah but wait a second", 1200), policy(&sim));
    assert_eq!(run.interrupts(), [(ms(4600), ep(1))]);
    assert_eq!(run.machine.interruption_stats().confirmed(), 1);
    assert_eq!(run.flushed, 1, "the old response's end was flushed");
    run.check_invariants();
}

#[test]
fn the_bots_own_echo_without_words_does_not_interrupt() {
    // A 240 ms echo burst 100 ms into the bot's audio (2300): inside the 400 ms window, with no
    // STT words.
    let sim = Sim::default();
    let run = sim.play(&call(2400, "", 240), policy(&sim));
    run.assert_first_turn_ended();
    assert!(run.interrupts().is_empty(), "{:?}", run.log);
    assert_eq!(run.vetoed(VetoReason::EchoGuard), 1);
    assert_eq!(run.machine.epoch(), TurnEpoch::ZERO);
    run.check_invariants();
}

#[test]
fn real_words_inside_the_echo_window_interrupt() {
    // The caller talks over the very start of the bot's audio: STT words lift the echo guard.
    let sim = Sim::default();
    let echo_only = InterruptionPolicy {
        min_speech: Duration::ZERO,
        backchannels: Vec::new(),
        ..policy(&sim)
    };
    let run = sim.play(&call(2400, "wait stop it", 1000), echo_only);
    // The first interim ("wait", 200 ms in) arrives at 2600, 300 ms into the bot audio.
    assert_eq!(run.interrupts(), [(ms(2600), ep(1))]);
    assert_eq!(run.machine.interruption_stats().total_vetoed(), 0);
    run.check_invariants();
}

#[test]
fn a_cough_is_vetoed_even_though_the_vad_stop_is_late() {
    // 220 ms of voice, no words. The simulator VAD reports its stop 300 ms after the cough
    // ended (at 4520), later than min speech (300 ms) on the clock (4300).
    let sim = Sim::default();
    let run = sim.play(&call(4000, "", 220), policy(&sim));
    assert!(run.interrupts().is_empty(), "{:?}", run.log);
    assert_eq!(run.vetoed(VetoReason::MinSpeech), 1);
    assert_eq!(run.machine.epoch(), TurnEpoch::ZERO);
    run.check_invariants();

    // Without the hangover in the policy the clock confirms the cough at 4350 (the transcript
    // wait, then min speech), before the VAD has said it was over: the failure `vad_hangover`
    // exists for.
    let naive = InterruptionPolicy {
        vad_hangover: Duration::ZERO,
        ..policy(&sim)
    };
    let run = sim.play(&call(4000, "", 220), naive);
    assert_eq!(run.interrupts(), [(ms(4350), ep(1))]);
}

#[test]
fn min_words_holds_the_interrupt_until_enough_words_are_heard() {
    let sim = Sim::default();
    let words = InterruptionPolicy {
        min_words: 2,
        ..InterruptionPolicy::default()
    };
    // One word: nothing to confirm it, the stop vetoes the candidate.
    let run = sim.play(&call(4000, "wait", 400), words.clone());
    assert!(run.interrupts().is_empty(), "{:?}", run.log);
    assert_eq!(run.vetoed(VetoReason::MinWords), 1);
    run.check_invariants();
    // Four words over a second: the 400 ms interim ("wait a") already holds two.
    let run = sim.play(&call(4000, "wait a second please", 1000), words);
    assert_eq!(run.interrupts(), [(ms(4400), ep(1))]);
    run.check_invariants();
}

#[test]
fn a_short_real_interruption_is_kept_at_the_stop() {
    // "stop" in 300 ms: too short for min speech, but STT heard a word and the utterance is over.
    let sim = Sim::default();
    let run = sim.play(&call(4000, "stop", 300), policy(&sim));
    // The stop (dated 4300) is delivered after the VAD hangover, at 4600.
    assert_eq!(run.interrupts(), [(ms(4600), ep(1))]);
    assert_eq!(run.machine.interruption_stats().total_vetoed(), 0);
    run.check_invariants();
}

#[test]
fn a_long_real_interruption_interrupts_when_min_speech_is_proven() {
    let sim = Sim::default();
    let run = sim.play(
        &call(4000, "wait I have a question about the bill", 2000),
        policy(&sim),
    );
    assert_eq!(run.interrupts(), [(ms(4600), ep(1))]);
    // The new turn runs to its end and the bot answers it.
    assert_eq!(run.started_turns(), [TurnEpoch::ZERO, ep(1)]);
    assert_eq!(run.machine.state().kind(), StateKind::Idle);
    run.check_invariants();
}

#[test]
fn per_chunk_bot_audio_reports_cannot_block_a_wordless_interruption() {
    // The driver reports BotAudioStarted for every 20 ms chunk. STT lost the words of a 1 s
    // interruption at 4000; VAD alone must still interrupt once the echo window (to 2700) is
    // over, instead of being vetoed as echo for the whole response.
    let sim = Sim {
        chunk_reports: true,
        ..Sim::default()
    };
    let no_backchannels = InterruptionPolicy {
        backchannels: Vec::new(),
        ..policy(&sim)
    };
    let run = sim.play(&call(4000, "", 1000), no_backchannels);
    assert_eq!(run.interrupts().len(), 1, "{:?}", run.log);
    assert_eq!(run.vetoed(VetoReason::EchoGuard), 0);
    run.check_invariants();
}
