//! Chaos scenarios: 100 concurrent simulated calls with random provider disconnects and
//! stalls, slow transport consumers, mid-turn barge-ins and hang-ups, in paused time.
//!
//! Checks: no panics and no failed stage in any call, the epoch / played-text invariants in every
//! call, every fault had an effect and the call recovered (second question answered), unaffected
//! calls behave exactly as they do alone (identical fingerprint) and keep the latency SLO per
//! turn, no leaked tasks (hung-up calls included), and live heap memory returns to its baseline
//! (when the caller supplies a heap probe).
//! Limitation: paused time cannot show blocking or CPU-heavy work on the runtime; real-time load
//! is covered by `brisvox-bench sim-load`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{SharedClock, TurnEpoch};

use super::{Profile, assertion, ensure, ensure_eq, nth, sim_clock, some};
use crate::error::Result;
use crate::factory::SessionFactory;
use crate::latency::{LatencyProfile, SimRng};
use crate::leak::TaskCounter;
use crate::mock_common::StreamFault;
use crate::mock_llm::{LlmFault, LlmScript};
use crate::scenario::Scenario;
use crate::session::{SessionRun, SimSession};

const MS: Duration = Duration::from_millis(1);
const SESSIONS: u64 = 100;

/// The latency SLO per turn for unaffected calls (both profiles).
pub const SLO: Duration = Duration::from_millis(1_500);

/// What goes wrong in one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chaos {
    None,
    SttDisconnect,
    SttStall,
    LlmError,
    LlmDisconnect,
    LlmStall,
    TtsDisconnect,
    TtsStall,
    BargeIn,
    SlowTransport,
    /// The caller hangs up mid-turn: the call future is dropped while the bot is answering.
    HangUp,
}

const KINDS: [Chaos; 11] = [
    Chaos::None,
    Chaos::SttDisconnect,
    Chaos::SttStall,
    Chaos::LlmError,
    Chaos::LlmDisconnect,
    Chaos::LlmStall,
    Chaos::TtsDisconnect,
    Chaos::TtsStall,
    Chaos::BargeIn,
    Chaos::SlowTransport,
    Chaos::HangUp,
];

/// When a hung-up call is dropped: during the first answer.
const HANG_UP_AT: Duration = Duration::from_millis(2_200);
/// Bound on one round (virtual time): a stuck call fails the suite instead of hanging it.
const ROUND_DEADLINE: Duration = Duration::from_secs(120);

/// Every third call is unaffected; the rest get a random fault.
fn chaos_of(index: u64, rng: &mut SimRng) -> Chaos {
    if index.is_multiple_of(3) {
        return Chaos::None;
    }
    let pick = rng.next_u64() % (KINDS.len() as u64 - 1) + 1;
    usize::try_from(pick)
        .ok()
        .and_then(|i| KINDS.get(i))
        .copied()
        .unwrap_or(Chaos::None)
}

/// The first answer of every call (words are distinct, so each token matches one timestamp).
const ANSWER: &str = "Sure, here is a fairly long answer to your question.";

/// Byte offset just past each whitespace token of `text`, with the token.
fn token_ends(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    for (offset, c) in text.char_indices() {
        match (c.is_whitespace(), start) {
            (false, None) => start = Some(offset),
            (true, Some(s)) => {
                if let Some(token) = text.get(s..offset) {
                    out.push((offset, token));
                }
                start = None;
            }
            _ => {}
        }
    }
    if let Some(token) = start.and_then(|s| text.get(s..)) {
        out.push((text.len(), token));
    }
    out
}

/// The answer's text the caller heard, exactly: its tokens in order, matched to the word log,
/// up to the last one that started before the caller stopped hearing it. A token the TTS lost
/// has no timestamp, so the heard text stops before it.
fn heard_answer_prefix(run: &SessionRun) -> &'static str {
    let zero = TurnEpoch::ZERO;
    let played = run.heard_position(zero);
    let words: Vec<(Arc<str>, Duration)> = run
        .probes
        .words()
        .into_iter()
        .filter(|(e, _, _)| *e == zero)
        .map(|(_, word, start)| (word, start))
        .collect();
    let mut next = 0usize;
    let mut end = 0usize;
    for (token_end, token) in token_ends(ANSWER) {
        let rest = words.get(next..).unwrap_or_default();
        let Some(found) = rest.iter().position(|(w, _)| &**w == token) else {
            break;
        };
        let Some((_, start)) = rest.get(found) else {
            break;
        };
        next += found + 1;
        if *start >= played {
            break;
        }
        end = token_end;
    }
    ANSWER.get(..end).unwrap_or_default()
}

/// Checks that the context keeps exactly the heard prefix of the first answer (none of it when
/// nothing was heard), and no other truncated text.
fn check_answer_is_heard_prefix(run: &SessionRun) -> Result<()> {
    let heard = heard_answer_prefix(run);
    let said: Vec<String> = run
        .probes
        .history()
        .into_iter()
        .filter(|m| m.role == brisvox_providers::Role::Assistant)
        .map(|m| m.content.to_string())
        .collect();
    if heard.is_empty() {
        return match said
            .iter()
            .find(|m| !m.is_empty() && ANSWER.starts_with(m.as_str()))
        {
            None => Ok(()),
            Some(m) => Err(assertion(format!(
                "context keeps unheard answer text {m:?}: {said:?}"
            ))),
        };
    }
    match said.first() {
        Some(first) if first == heard => Ok(()),
        _ => Err(assertion(format!(
            "context {said:?} is not the heard answer {heard:?}"
        ))),
    }
}

fn scenario_for(chaos: Chaos) -> Scenario {
    let gap = if chaos == Chaos::BargeIn {
        // The second utterance starts while the bot is answering the first.
        1_400 * MS
    } else {
        3_000 * MS
    };
    Scenario::new()
        .silence(100 * MS)
        .speak("first question please", 800 * MS)
        .silence(gap)
        .speak("second question please", 800 * MS)
        .with_tail(500 * MS)
}

/// A call configured like the realistic simulator scenarios, plus its chaos.
fn session(clock: SharedClock, seed: u64, chaos: Chaos) -> SimSession {
    let mut s = SimSession::new(clock);
    let log_normal = |median: u64| LatencyProfile::LogNormal {
        median: Duration::from_millis(median),
        sigma: 0.3,
    };
    let _ = s.stt.clone().with_latency(
        LatencyProfile::Fixed(50 * MS),
        LatencyProfile::Fixed(100 * MS),
        log_normal(150),
        seed * 4 + 1,
    );
    let _ = s.llm.clone().with_latency(
        log_normal(350),
        LatencyProfile::Fixed(30 * MS),
        seed * 4 + 2,
    );
    let _ = s.tts.clone().with_latency(
        LatencyProfile::Fixed(50 * MS),
        log_normal(150),
        LatencyProfile::Fixed(20 * MS),
        seed * 4 + 3,
    );
    // Streamed word by word, so a disconnect mid-answer really truncates it.
    let mut first = LlmScript::text(ANSWER.split_inclusive(' '));
    match chaos {
        Chaos::SttDisconnect => s
            .stt
            .push_session_faults(vec![StreamFault::Disconnect { after_events: 1 }]),
        Chaos::SttStall => s.stt.push_session_faults(vec![StreamFault::Stall {
            before_event: 1,
            duration: 2_000 * MS,
        }]),
        Chaos::LlmError => first = first.with_fault(LlmFault::ErrorResponse { status: 503 }),
        Chaos::LlmDisconnect => {
            first = first.with_fault(LlmFault::Disconnect { after_events: 1 });
        }
        Chaos::LlmStall => {
            first = first.with_fault(LlmFault::Stall {
                before_event: 0,
                duration: 3_000 * MS,
            });
        }
        Chaos::TtsDisconnect => s
            .tts
            .push_session_faults(vec![StreamFault::Disconnect { after_events: 3 }]),
        Chaos::TtsStall => s.tts.push_session_faults(vec![StreamFault::Stall {
            before_event: 2,
            duration: 1_500 * MS,
        }]),
        // Each 20 ms frame takes 30 ms to hand off: the bot audio backs up.
        Chaos::SlowTransport => s.transport_delay = 30 * MS,
        Chaos::None | Chaos::BargeIn | Chaos::HangUp => {}
    }
    s.llm.push(first);
    s.llm.push(LlmScript::text(["Second answer."]));
    s
}

/// One finished call: its chaos, its run (`None` when hung up) and the mocks' connect counts.
struct Call {
    chaos: Chaos,
    run: Option<SessionRun>,
    stt_connects: usize,
    tts_connects: usize,
}

/// Runs `SESSIONS` calls concurrently.
async fn round<F: SessionFactory>(
    factory: &F,
    clock: &SharedClock,
    seed: u64,
) -> Result<Vec<Call>> {
    let mut rng = SimRng::new(seed);
    let mut tasks = Vec::new();
    for i in 0..SESSIONS {
        let chaos = chaos_of(i, &mut rng);
        let s = session(clock.clone(), seed * 1_000 + i, chaos);
        let scenario = scenario_for(chaos);
        let clock = clock.clone();
        let factory = factory.clone();
        let (stt, tts) = (s.stt.clone(), s.tts.clone());
        tasks.push(tokio::spawn(async move {
            let run = if chaos == Chaos::HangUp {
                // Dropping the call future mid-turn is the hang-up.
                tokio::select! {
                    run = s.run_with(&factory, &scenario, 4_000 * MS) => {
                        return Err(assertion(format!(
                            "call ended before the hang-up: {:?}",
                            run.map(|r| r.outcome)
                        )));
                    }
                    () = clock.sleep(HANG_UP_AT) => None,
                }
            } else {
                Some(s.run_with(&factory, &scenario, 4_000 * MS).await?)
            };
            Ok::<Call, crate::Error>(Call {
                chaos,
                run,
                stt_connects: stt.connects(),
                tts_connects: tts.connects(),
            })
        }));
    }
    let deadline = clock.now() + ROUND_DEADLINE;
    let mut runs = Vec::new();
    for (i, mut task) in tasks.into_iter().enumerate() {
        tokio::select! {
            joined = &mut task => match joined {
                Ok(call) => runs.push(call?),
                Err(e) => {
                    return Err(assertion(format!("call {i} panicked or was cancelled: {e}")));
                }
            },
            () = clock.sleep_until(deadline) => {
                return Err(assertion(format!(
                    "call {i} did not finish within {ROUND_DEADLINE:?}"
                )));
            }
        }
    }
    Ok(runs)
}

/// The same call without its fault, alone.
async fn solo<F: SessionFactory>(
    factory: &F,
    clock: &SharedClock,
    seed: u64,
) -> Result<SessionRun> {
    let s = session(clock.clone(), seed, Chaos::None);
    s.run_with(factory, &scenario_for(Chaos::None), 4_000 * MS)
        .await
}

/// The first-audio latency of every turn of the unaffected call of each of `seeds`, played alone
/// as the chaos round's own reference calls are (the round checks the calls of seeds
/// `2_000..2_100` this way): the first bot audio of the turn minus the end of its speech. The
/// numbers behind [`SLO`]; the first-chunk policy is measured with them.
///
/// # Errors
/// A call that fails, or a turn without bot audio.
pub async fn unaffected_first_audio<F: SessionFactory>(
    factory: &F,
    seeds: impl IntoIterator<Item = u64>,
) -> Result<Vec<Duration>> {
    let clock = sim_clock();
    let segments = scenario_for(Chaos::None).speech_segments();
    let mut latencies = Vec::new();
    for seed in seeds {
        let run = solo(factory, &clock, seed).await?;
        ensure_eq!(run.outcome, Ok(()), "seed {seed} outcome");
        for (n, (_, end)) in segments.iter().enumerate() {
            let epoch = u32::try_from(n).unwrap_or(u32::MAX);
            let first = some(
                run.output
                    .iter()
                    .find(|r| r.kind == "audio" && r.epoch == Some(epoch))
                    .map(|r| r.at),
                &format!("seed {seed}: audio for turn {n}"),
            )?;
            latencies.push(first.saturating_sub(*end));
        }
    }
    Ok(latencies)
}

/// Bot audio handed to the output at or after `after` (call time).
fn audio_after(run: &SessionRun, after: Duration) -> bool {
    run.output
        .iter()
        .any(|r| r.kind == "audio" && r.at >= after)
}

/// Shortest gap between consecutive bot audio frames of the same turn.
fn min_audio_gap(run: &SessionRun) -> Option<Duration> {
    let audio: Vec<_> = run.output.iter().filter(|r| r.kind == "audio").collect();
    audio
        .windows(2)
        .filter_map(|w| match w {
            [a, b] if a.epoch == b.epoch => Some(b.at.saturating_sub(a.at)),
            _ => None,
        })
        .min()
}

/// Tags a failed check with the call it came from.
fn in_call<T>(call: &str, result: Result<T>) -> Result<T> {
    result.map_err(|e| assertion(format!("{call}: {e}")))
}

/// 100 chaotic calls stay isolated and leave nothing behind.
///
/// Runs a warm-up round and then the checked round of `SESSIONS` concurrent calls on a fresh
/// paused clock. `heap` reads the live heap bytes of the process (the test binary's counting
/// allocator); with it the suite also requires the heap to return to within 4 KiB of its
/// baseline after the round.
///
/// # Errors
/// [`crate::Error::Assertion`] on the first failed check; [`crate::Error::TaskLeak`] if tasks
/// outlive their calls.
pub async fn a_hundred_chaotic_calls_stay_isolated_and_leave_nothing_behind<F: SessionFactory>(
    factory: &F,
    profile: Profile,
    heap: Option<&dyn Fn() -> isize>,
) -> Result<()> {
    let clock = sim_clock();
    // Warm-up round: one-time allocations (runtime internals, lazies) happen here; let its
    // tasks wind down before taking the baselines.
    let before_warm_up = TaskCounter::start()?;
    drop(round(factory, &clock, 1).await?);
    before_warm_up
        .wait_for_baseline(&*clock, Duration::from_secs(5))
        .await?;
    let tasks = TaskCounter::start()?;
    let heap_before = heap.map(|read| read());

    let runs = round(factory, &clock, 2).await?;
    let mut counts = BTreeMap::new();
    // Calls whose answer the TTS disconnect cut mid-way with some of it heard (the exact check
    // is not vacuous only if such calls exist).
    let mut tts_cut_mid_answer = 0usize;
    let segments = scenario_for(Chaos::None).speech_segments();
    let second_end = nth(&segments, 1, "speech segment")?.1;
    // Both pipelines model the slow consumer (the production one as a slow reader of the
    // loopback client): its frames are at least the per-frame delay apart.
    let min_gap = 30 * MS;
    for (i, c) in runs.iter().enumerate() {
        let chaos = &c.chaos;
        *counts.entry(format!("{chaos:?}")).or_insert(0usize) += 1;
        let Some(run) = &c.run else {
            ensure_eq!(*chaos, Chaos::HangUp, "a call without a run");
            continue;
        };
        let call = format!("call {i} ({chaos:?})");
        // Every call that ran to the end: no stage failed or panicked, invariants hold.
        ensure_eq!(run.outcome, Ok(()), "{call} outcome");
        in_call(&call, run.check_no_stale_audio_after_interrupt())?;
        if *chaos == Chaos::TtsDisconnect {
            // The TTS lost text the caller never heard: the context holds exactly the heard
            // prefix of the answer, cut before the first lost word.
            in_call(&call, check_answer_is_heard_prefix(run))?;
            let heard = heard_answer_prefix(run);
            if !heard.is_empty() && heard.len() < ANSWER.len() {
                tts_cut_mid_answer += 1;
            }
        } else {
            in_call(&call, profile.context_holds_played(run))?;
        }
        ensure_eq!(run.dropped_output, 0, "{call} dropped output records");
        let alone = solo(factory, &clock, 2 * 1_000 + i as u64).await?;
        if *chaos == Chaos::None {
            // Unaffected calls: the same behaviour as alone, and the latency SLO per turn.
            ensure!(
                run.fingerprint() == alone.fingerprint(),
                "{call} was affected by its neighbours"
            );
            for (n, (_, end)) in segments.iter().enumerate() {
                let epoch = u32::try_from(n).unwrap_or(u32::MAX);
                let first = some(
                    run.output
                        .iter()
                        .find(|r| r.kind == "audio" && r.epoch == Some(epoch))
                        .map(|r| r.at),
                    &format!("{call}: audio for turn {n}"),
                )?;
                ensure!(
                    first >= *end && first - *end <= SLO,
                    "{call}: turn {n} answered at {first:?}, speech ended {end:?}"
                );
            }
            continue;
        }
        match chaos {
            // A dropped connection is reconnected; the stage may hide it completely (the STT
            // keeps the utterance), so prove the fault fired by the reconnect itself.
            Chaos::SttDisconnect => {
                ensure!(c.stt_connects >= 2, "{call}: no STT reconnect");
            }
            Chaos::TtsDisconnect => {
                ensure!(c.tts_connects >= 2, "{call}: no TTS reconnect");
            }
            // Every other fault changes what the caller experiences.
            _ => ensure!(
                run.fingerprint() != alone.fingerprint(),
                "{call}: fault had no effect"
            ),
        }
        match chaos {
            Chaos::BargeIn => ensure!(
                run.output.iter().any(|r| r.kind == "interrupt"),
                "{call}: no barge-in"
            ),
            Chaos::SlowTransport => ensure!(
                min_audio_gap(run).is_some_and(|g| g >= min_gap),
                "{call}: frames were not slowed ({:?})",
                min_audio_gap(run)
            ),
            _ => {}
        }
        // Recovery: whatever failed in the first turn, the second question is answered.
        ensure!(
            audio_after(run, second_end),
            "{call}: second turn unanswered"
        );
    }
    println!("chaos mix: {counts:?}; tts cut mid-answer: {tts_cut_mid_answer}");
    ensure!(
        tts_cut_mid_answer >= 1,
        "no TTS disconnect cut an answer mid-way with text heard"
    );
    ensure!(
        counts.len() >= 9,
        "too few fault kinds exercised: {counts:?}"
    );
    drop(runs);

    // No tasks outlive their calls (hung-up ones included), and the heap is back where it was.
    tasks.wait_for_baseline(&*clock, 100 * MS).await?;
    if let (Some(read), Some(before)) = (heap, heap_before) {
        let after = read();
        let grown = after.saturating_sub(before);
        println!("heap: before {before} B, after {after} B, grown {grown} B");
        // < 40 B per call: a per-call leak of any real structure would exceed it.
        ensure!(
            grown < 4 * 1024,
            "heap grew by {grown} bytes after the round"
        );
    }
    Ok(())
}
