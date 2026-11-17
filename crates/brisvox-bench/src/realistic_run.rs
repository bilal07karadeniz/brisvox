//! Runs the realistic calls of [`crate::realistic`] and reports them per call and per candidate
//! (feature `denoise`).
//!
//! A *candidate* is a VAD start while the bot speaks: the engine's decision starts there. Per
//! candidate the report says whether it was a real barge-in or not, what became of it (the bot was
//! cut off for good, the bot went quiet and carried on, or the caller never noticed), how long the
//! caller heard silence, and how long it took to the final outcome. The per-call table of the
//! earlier measurements stays beside it.
//!
//! **Pause, then decide** is a [`Mode`] over a [`Config`] with a pause (`Config::pause`): the real
//! turn machine runs `[interruption] pause_then_decide` with the rule of the configuration, on the
//! events of the real VAD. On a candidate the bot's audio stops at once and the decision follows
//! within the window: *never* leaves it to the guards as today, *words* resumes when the recognizer
//! has heard no word.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use brisvox_inference::silero_tract::SileroTractModel;

use crate::BenchError;
use crate::denoise_calls::{Cand, Config, HZ, model_error, run_call};
use crate::denoise_eval::Path16;
use crate::realistic::{
    FirstTurn, Kind, PairStats, Real, Recognizer, World, assemble, pair_stats, scenarios,
};

/// One row of the report's tables: a configuration run, with or without a pause.
#[derive(Debug, Clone)]
pub struct Mode {
    /// Name in the reports.
    pub name: String,
    /// The configuration it runs (index into the configurations of the run).
    pub config: usize,
}

/// What a candidate came to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fate {
    /// The bot was cut off at this time (seconds into the call).
    Committed(f64),
    /// The bot went quiet and carried on at this time.
    Resumed(f64),
    /// The bot was never touched; the candidate ended at this time.
    Passed(f64),
}

impl Fate {
    const fn at(self) -> f64 {
        match self {
            Self::Committed(t) | Self::Resumed(t) | Self::Passed(t) => t,
        }
    }

    const fn is_cut(self) -> bool {
        matches!(self, Self::Committed(_))
    }

    const fn is_resumed(self) -> bool {
        matches!(self, Self::Resumed(_))
    }
}

/// What a candidate came to. `end_s` is the end of the call.
pub(crate) fn pause_fate(c: &Cand, end_s: f64) -> Fate {
    let ended = c.stop.unwrap_or(end_s);
    if let Some(t) = c.interrupted {
        Fate::Committed(t)
    } else if let Some(t) = c.resumed {
        Fate::Resumed(t)
    } else if c.paused.is_some() {
        // The call ended (or the bot's audio did) while the bot was paused.
        Fate::Resumed(end_s)
    } else {
        Fate::Passed(ended)
    }
}

/// What a candidate was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Label {
    /// The caller's own onset.
    Real,
    /// Started on another voice before the caller spoke and was still open when the caller joined.
    Joined,
    /// Not the caller.
    False,
}

fn label(c: &Cand, span: Option<(f64, f64)>) -> Label {
    match span {
        None => Label::False,
        Some((a, b)) => {
            if c.onset >= a - 0.3 && c.onset <= b {
                Label::Real
            } else if c.onset < a - 0.3 && c.stop.is_none_or(|s| s > a) {
                Label::Joined
            } else {
                Label::False
            }
        }
    }
}

/// One candidate under one mode.
#[derive(Debug, Clone)]
pub struct CandRow {
    /// Index into the modes.
    pub mode: usize,
    /// The path.
    pub path: Path16,
    /// Index into the recognizers.
    pub rec: usize,
    /// What else was on the line.
    pub kind: Kind,
    /// Its level below the caller's speech, dB.
    pub level_db: f32,
    /// What the candidate was.
    pub label: Label,
    /// What became of it.
    pub fate: Fate,
    /// From the speech onset to the caller hearing silence, ms; none if the bot never went quiet.
    pub silence_ms: Option<f64>,
    /// From the speech onset to the final outcome, ms.
    pub final_ms: f64,
    /// How long the bot was quiet before it resumed, ms.
    pub pause_ms: Option<f64>,
    /// The bot's playout was paused for it.
    pub paused: bool,
    /// The call was a quiet one (the caller said nothing).
    pub quiet: bool,
    /// The scenario, and where the candidate began and ended (seconds into the call): to find
    /// a row again.
    pub scenario: u64,
    /// See `scenario`.
    pub onset_s: f64,
    /// See `scenario`.
    pub stop_s: Option<f64>,
}

/// How much each kind of barge-in counts in the weighted summary: real callers mostly say a
/// phrase or a sentence; a short command ("wait a second", "stop") is rarer. This is an
/// assumption, stated here and in the report, not a measurement; the per-kind rows are always
/// shown.
pub const SAY_WEIGHTS: [f64; 3] = [0.60, 0.25, 0.15];

/// How a barge-in was said.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Say {
    /// A whole sentence.
    Sentence,
    /// A 0.9 s command (the first words of a sentence).
    Command,
    /// A 0.5 s command word.
    Word,
}

impl Say {
    /// Name in reports.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Sentence => "sentence",
            Self::Command => "0.9 s command",
            Self::Word => "0.5 s command word",
        }
    }

    /// All variants.
    pub const ALL: [Self; 3] = [Self::Sentence, Self::Command, Self::Word];
}

/// One call under one mode.
#[derive(Debug, Clone)]
pub struct CallRow {
    /// Index into the modes.
    pub mode: usize,
    /// The path.
    pub path: Path16,
    /// Index into the recognizers.
    pub rec: usize,
    /// What else was on the line.
    pub kind: Kind,
    /// Its level below the caller's speech, dB.
    pub level_db: f32,
    /// The first-turn variant.
    pub first_turn: FirstTurn,
    /// The caller said nothing.
    pub quiet: bool,
    /// How the barge-in was said.
    pub say: Say,
    /// The bot was cut off at least once.
    pub cut: bool,
    /// Barge-in call: the bot was cut off before the caller spoke.
    pub premature: bool,
    /// Barge-in call: the caller never got through.
    pub missed: bool,
    /// Barge-in call: from the caller's first word to the interruption, ms.
    pub tti_ms: Option<f64>,
    /// Barge-in call: how many VAD starts were the caller's own onset (zero: the VAD never
    /// heard the caller, which no guard can change).
    pub real_candidates: u32,
    /// The length of the call, seconds.
    pub call_s: f64,
}

/// Everything a run measured.
pub struct RealisticReport {
    /// The modes, in report order.
    pub modes: Vec<Mode>,
    /// The recognizers.
    pub recs: Vec<Recognizer>,
    /// The paths.
    pub paths: Vec<Path16>,
    /// One row per candidate and mode.
    pub cands: Vec<CandRow>,
    /// One row per call and mode.
    pub calls: Vec<CallRow>,
    /// Scenarios per path and recognizer.
    pub scenarios: usize,
    /// The pairing of the scenarios.
    pub pairs: PairStats,
}

/// Runs every configuration over the scenarios on every path and with every recognizer, and
/// derives the rows of every mode.
///
/// # Errors
/// [`BenchError`] when a model, a mix or a worker fails.
pub fn run_realistic(
    world: &World,
    configs: &[Config],
    modes: &[Mode],
    recs: &[Recognizer],
    paths: &[Path16],
    silero: &Arc<SileroTractModel>,
    threads: usize,
) -> Result<RealisticReport, BenchError> {
    let scs = scenarios(world);
    let n_sc = scs.len();
    let (n_paths, n_recs) = (paths.len(), recs.len());
    let jobs: Vec<(usize, usize, usize)> = (0..n_recs)
        .flat_map(|r| (0..n_paths).flat_map(move |p| (0..n_sc).map(move |s| (r, p, s))))
        .collect();
    let next = AtomicUsize::new(0);
    type Out = (Vec<CandRow>, Vec<CallRow>);
    let results: Vec<Result<Out, BenchError>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads.max(1))
            .map(|_| {
                scope.spawn(|| -> Result<Out, BenchError> {
                    let (mut cand_rows, mut call_rows) = (Vec::new(), Vec::new());
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(r, p, s)) = jobs.get(i) else { break };
                        let (Some(rec), Some(&path), Some(sc)) =
                            (recs.get(r), paths.get(p), scs.get(s))
                        else {
                            continue;
                        };
                        let asm = assemble(world, sc, path, rec)?;
                        #[allow(clippy::cast_precision_loss)]
                        let end_s = asm.pcm.len() as f64 / HZ;
                        let mut done: BTreeMap<usize, crate::denoise_calls::CallResult> =
                            BTreeMap::new();
                        for (mi, mode) in modes.iter().enumerate() {
                            let Some(config) = configs.get(mode.config) else {
                                continue;
                            };
                            if let std::collections::btree_map::Entry::Vacant(slot) =
                                done.entry(mode.config)
                            {
                                slot.insert(run_call(&asm, config, silero, None)?);
                            }
                            let Some(result) = done.get(&mode.config) else {
                                continue;
                            };
                            derive(
                                &mut cand_rows,
                                &mut call_rows,
                                mi,
                                (r, path, sc),
                                &asm,
                                result,
                                end_s,
                            );
                        }
                    }
                    Ok((cand_rows, call_rows))
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| {
                w.join()
                    .unwrap_or_else(|_| Err(model_error("a worker panicked")))
            })
            .collect()
    });
    let (mut cands, mut calls) = (Vec::new(), Vec::new());
    for r in results {
        let (a, b) = r?;
        cands.extend(a);
        calls.extend(b);
    }
    Ok(RealisticReport {
        modes: modes.to_vec(),
        recs: recs.to_vec(),
        paths: paths.to_vec(),
        cands,
        calls,
        scenarios: scs.len(),
        pairs: pair_stats(world, &scs),
    })
}

fn derive(
    cands: &mut Vec<CandRow>,
    calls: &mut Vec<CallRow>,
    mi: usize,
    (rec, path, sc): (usize, Path16, &Real),
    asm: &crate::denoise_calls::Assembled,
    result: &crate::denoise_calls::CallResult,
    end_s: f64,
) {
    let span = asm.caller_span;
    let mut commits: Vec<f64> = Vec::new();
    for c in &result.cands {
        let fate = pause_fate(c, end_s);
        let l = label(c, span);
        let (silence_ms, pause_ms) = match (c.paused, fate) {
            (Some(p), Fate::Resumed(t)) => (Some((p - c.onset) * 1000.0), Some((t - p) * 1000.0)),
            (Some(p), _) => (Some((p - c.onset) * 1000.0), None),
            (None, Fate::Committed(t)) => (Some((t - c.onset) * 1000.0), None),
            (None, _) => (None, None),
        };
        if let Fate::Committed(t) = fate {
            commits.push(t);
        }
        cands.push(CandRow {
            mode: mi,
            path,
            rec,
            kind: sc.kind,
            level_db: sc.level_db,
            label: l,
            fate,
            silence_ms,
            final_ms: (fate.at() - c.onset) * 1000.0,
            pause_ms,
            paused: c.paused.is_some(),
            quiet: sc.quiet,
            scenario: sc.id,
            onset_s: c.onset,
            stop_s: c.stop,
        });
    }
    commits.sort_by(f64::total_cmp);
    let first = commits.first().copied();
    let (mut premature, mut missed, mut tti_ms) = (false, false, None);
    if let Some((a, _)) = span {
        match first {
            Some(t) if t < a - 0.1 => premature = true,
            Some(t) => tti_ms = Some((t - a) * 1000.0),
            None => missed = true,
        }
    }
    let say = match (sc.trim_s, sc.word) {
        (None, _) => Say::Sentence,
        (Some(_), false) => Say::Command,
        (Some(_), true) => Say::Word,
    };
    calls.push(CallRow {
        mode: mi,
        path,
        rec,
        kind: sc.kind,
        level_db: sc.level_db,
        first_turn: sc.first_turn,
        quiet: sc.quiet,
        say,
        cut: first.is_some(),
        premature,
        missed,
        tti_ms,
        real_candidates: u32::try_from(
            result
                .cands
                .iter()
                .filter(|c| label(c, span) == Label::Real)
                .count(),
        )
        .unwrap_or(u32::MAX),
        call_s: result.call_s,
    });
}

// ---------------------------------------------------------------------------------------------
// Tables

fn pct(a: usize, b: usize) -> String {
    if b == 0 {
        return "-".to_owned();
    }
    #[allow(clippy::cast_precision_loss)]
    let p = 100.0 * a as f64 / b as f64;
    format!("{p:.1}%")
}

fn quantile(v: &mut [f64], q: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let i = ((v.len() - 1) as f64 * q).round() as usize;
    v.get(i).copied()
}

fn ms(v: &mut [f64], q: f64) -> String {
    quantile(v, q).map_or("-".to_owned(), |x| format!("{x:.0}"))
}

/// The report as markdown.
pub fn realistic_markdown(
    report: &RealisticReport,
    environment: &[String],
    world: &World,
) -> String {
    let mut out = String::from("# Barge-ins and background noise\n\n");
    for line in environment {
        let _ = writeln!(out, "- {line}");
    }
    let p = &report.pairs;
    let noise_calls = report_noise_calls(report);
    let _ = write!(
        out,
        "\n## What the calls are made of\n\n\
         - {} calls per path and recognizer, of which {} have background noise on the line ({} kinds) and {} a second voice: \
         {} callers, {} second voices; **{} distinct caller x speaker pairs** ({} of the same sex, {} of different sexes, {} where a sex is not known) \
         and {} distinct conditions (pair, recording and room).\n\
         - Second voices: {}; real noise recordings: {}; rooms: {}.\n\n",
        report.scenarios,
        noise_calls,
        Kind::NOISES.len(),
        p.scenarios,
        p.callers,
        p.speakers,
        p.pairs,
        p.same,
        p.other,
        p.unknown,
        p.conditions,
        world.interferers.len(),
        world.noises.len(),
        world
            .rirs
            .iter()
            .map(|r| format!("{} ({:.2} s)", r.name, r.rt60))
            .collect::<Vec<_>>()
            .join(", "),
    );
    for (ri, rec) in report.recs.iter().enumerate() {
        for &path in &report.paths {
            let _ = writeln!(out, "## {}, {}\n", rec.name(), path.name());
            fair_table(&mut out, report, ri, path);
            noise_table(&mut out, report, ri, path);
            noise_by_kind_table(&mut out, report, ri, path);
            noise_by_level_table(&mut out, report, ri, path);
            real_table(&mut out, report, ri, path);
            real_by_say_table(&mut out, report, ri, path);
            voices_table(&mut out, report, ri, path);
            per_call_table(&mut out, report, ri, path);
            longest_pauses(&mut out, report, ri, path);
        }
    }
    out
}

fn report_noise_calls(report: &RealisticReport) -> usize {
    let Some(mode) = report.modes.len().checked_sub(1) else {
        return 0;
    };
    let (Some(&path), rec) = (report.paths.first(), 0) else {
        return 0;
    };
    calls_of(report, mode, rec, path)
        .filter(|c| c.kind.is_noise())
        .count()
}

fn calls_of(
    report: &RealisticReport,
    mode: usize,
    rec: usize,
    path: Path16,
) -> impl Iterator<Item = &CallRow> {
    report
        .calls
        .iter()
        .filter(move |c| c.mode == mode && c.rec == rec && c.path == path)
}

fn cands_of(
    report: &RealisticReport,
    mode: usize,
    rec: usize,
    path: Path16,
) -> impl Iterator<Item = &CandRow> {
    report
        .cands
        .iter()
        .filter(move |c| c.mode == mode && c.rec == rec && c.path == path)
}

/// The noise-only candidates: the candidates of quiet calls with background noise on the line
/// (nobody speaks, so none of them is a caller).
fn noise_cands(report: &RealisticReport, mode: usize, rec: usize, path: Path16) -> Vec<&CandRow> {
    cands_of(report, mode, rec, path)
        .filter(|c| c.quiet && c.kind.is_speechless_noise() && c.label == Label::False)
        .collect()
}

/// Cut off for good / paused and resumed / the bot never stopped, as shares of `rows`.
fn fates(rows: &[&CandRow]) -> (String, String, String) {
    let cut = rows.iter().filter(|c| c.fate.is_cut()).count();
    let resumed = rows.iter().filter(|c| c.fate.is_resumed()).count();
    let n = rows.len();
    (pct(cut, n), pct(resumed, n), pct(n - cut - resumed, n))
}

/// The one table that answers the question: per mode, what noise does and what a real barge-in
/// costs, with the short commands weighted by [`SAY_WEIGHTS`] and equal denominators for every mode.
fn fair_table(out: &mut String, report: &RealisticReport, rec: usize, path: Path16) {
    let _ = writeln!(
        out,
        "**Summary.** *Noise*: candidates of quiet calls with noise without speech in it. *Pauses/100*: pauses of the bot per 100 such calls; *quiet for*: how long the bot \
         was silent before it resumed (ms, median / p90); *cut off*: share of those candidates that cut the bot off for good. *Real*: barge-in calls the VAD started on the caller in but \
         that never got a cut (*held*), as a share of all barge-in calls of that kind (the same denominators in every mode: {} sentences, 0.9 s commands and 0.5 s command words each); \
         *weighted* counts them {:.0}% sentences, {:.0}% 0.9 s commands, {:.0}% 0.5 s command words (an assumption about what callers say, not a measurement). \
         *Silence*: onset to the caller hearing the bot stop; *final cut*: onset to the cut (ms, median / p90).\n\n\
         | mode | noise pauses/100 | quiet for | noise cut off | held: sentence | 0.9 s command | 0.5 s word | **held, weighted** | silence | final cut |\n\
         |---|---|---|---|---|---|---|---|---|---|",
        calls_of(report, 0, rec, path)
            .filter(|c| !c.quiet && c.say == Say::Sentence)
            .count(),
        100.0 * SAY_WEIGHTS[0],
        100.0 * SAY_WEIGHTS[1],
        100.0 * SAY_WEIGHTS[2],
    );
    for (mi, mode) in report.modes.iter().enumerate() {
        let noise = noise_cands(report, mi, rec, path);
        let calls_noise = calls_of(report, mi, rec, path)
            .filter(|c| c.quiet && c.kind.is_speechless_noise())
            .count();
        let pauses = noise.iter().filter(|c| c.paused).count();
        #[allow(clippy::cast_precision_loss)]
        let per_100 = 100.0 * pauses as f64 / calls_noise.max(1) as f64;
        let mut q1: Vec<f64> = noise.iter().filter_map(|c| c.pause_ms).collect();
        let mut q2 = q1.clone();
        let (cut, _, _) = fates(&noise);
        let barge: Vec<&CallRow> = calls_of(report, mi, rec, path)
            .filter(|c| !c.quiet)
            .collect();
        let mut weighted = 0.0;
        let mut cells = Vec::new();
        for (k, say) in Say::ALL.iter().enumerate() {
            let of: Vec<&&CallRow> = barge.iter().filter(|c| c.say == *say).collect();
            let held = of
                .iter()
                .filter(|c| c.missed && c.real_candidates > 0)
                .count();
            #[allow(clippy::cast_precision_loss)]
            let share = held as f64 / of.len().max(1) as f64;
            weighted += SAY_WEIGHTS.get(k).copied().unwrap_or(0.0) * share;
            cells.push(format!("{held}/{} ({:.1}%)", of.len(), 100.0 * share));
        }
        let real: Vec<&CandRow> = cands_of(report, mi, rec, path)
            .filter(|c| c.label == Label::Real)
            .collect();
        let mut sil: Vec<f64> = real.iter().filter_map(|c| c.silence_ms).collect();
        let mut fin: Vec<f64> = real
            .iter()
            .filter(|c| c.fate.is_cut())
            .map(|c| c.final_ms)
            .collect();
        let mut fin2 = fin.clone();
        let _ = writeln!(
            out,
            "| {} | {:.1} | {} / {} | {} | {} | {} | {} | **{:.1}%** | {} | {} / {} |",
            mode.name,
            per_100,
            ms(&mut q1, 0.5),
            ms(&mut q2, 0.9),
            cut,
            cells.first().map_or("", String::as_str),
            cells.get(1).map_or("", String::as_str),
            cells.get(2).map_or("", String::as_str),
            100.0 * weighted,
            ms(&mut sil, 0.5),
            ms(&mut fin, 0.5),
            ms(&mut fin2, 0.9),
        );
    }
    out.push('\n');
}

fn noise_table(out: &mut String, report: &RealisticReport, rec: usize, path: Path16) {
    out.push_str(
        "**Noise only.** Every VAD start while the bot speaks in a call where the caller says nothing and some noise is on the line \
         (every noise kind without speech in it: real recordings, cough, keyboard, door, dishes, music, street; all levels; babble is with the voices below). *Cut off*: the bot was cut off for good. *Paused, resumed*: the bot went quiet and carried on. \
         *Not heard*: the bot never stopped (a guard held the candidate). *Quiet for*: how long the bot was silent before it resumed (ms). \
         *Calls cut off*: quiet noise calls in which the bot was cut off at least once.\n\n\
         | mode | candidates | cut off | paused, resumed | not heard | quiet for, median | p90 | max | calls cut off |\n\
         |---|---|---|---|---|---|---|---|---|\n",
    );
    for (mi, mode) in report.modes.iter().enumerate() {
        let rows = noise_cands(report, mi, rec, path);
        let (cut, resumed, passed) = fates(&rows);
        let mut q1: Vec<f64> = rows.iter().filter_map(|c| c.pause_ms).collect();
        let (mut q2, mut q3) = (q1.clone(), q1.clone());
        let calls: Vec<&CallRow> = calls_of(report, mi, rec, path)
            .filter(|c| c.quiet && c.kind.is_speechless_noise())
            .collect();
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {}/{} |",
            mode.name,
            rows.len(),
            cut,
            resumed,
            passed,
            ms(&mut q1, 0.5),
            ms(&mut q2, 0.9),
            ms(&mut q3, 1.0),
            calls.iter().filter(|c| c.cut).count(),
            calls.len(),
        );
    }
    out.push('\n');
}

fn noise_by_kind_table(out: &mut String, report: &RealisticReport, rec: usize, path: Path16) {
    out.push_str("Noise only, by kind: *cut off* (in brackets: paused and resumed) over the candidates of the kind.\n\n| mode |");
    for k in Kind::NOISES {
        let _ = write!(out, " {} |", k.name());
    }
    out.push_str("\n|---|");
    for _ in Kind::NOISES {
        out.push_str("---|");
    }
    out.push('\n');
    for (mi, mode) in report.modes.iter().enumerate() {
        let rows = noise_cands(report, mi, rec, path);
        let _ = write!(out, "| {} |", mode.name);
        for k in Kind::NOISES {
            let r: Vec<&CandRow> = rows.iter().copied().filter(|c| c.kind == k).collect();
            let (cut, resumed, _) = fates(&r);
            let _ = write!(out, " {cut} ({resumed}) n={} |", r.len());
        }
        out.push('\n');
    }
    out.push('\n');
}

fn noise_by_level_table(out: &mut String, report: &RealisticReport, rec: usize, path: Path16) {
    let levels = crate::realistic::QUIET_NOISE_LEVELS;
    out.push_str("Noise only, by level below the caller's speech: *cut off* (in brackets: paused and resumed).\n\n| mode |");
    for l in levels {
        let _ = write!(out, " {l:+} dB |");
    }
    out.push_str("\n|---|");
    for _ in levels {
        out.push_str("---|");
    }
    out.push('\n');
    for (mi, mode) in report.modes.iter().enumerate() {
        let rows = noise_cands(report, mi, rec, path);
        let _ = write!(out, "| {} |", mode.name);
        for l in levels {
            let r: Vec<&CandRow> = rows
                .iter()
                .copied()
                .filter(|c| (c.level_db - l).abs() < 0.01)
                .collect();
            let (cut, resumed, _) = fates(&r);
            let _ = write!(out, " {cut} ({resumed}) n={} |", r.len());
        }
        out.push('\n');
    }
    out.push('\n');
}

fn real_table(out: &mut String, report: &RealisticReport, rec: usize, path: Path16) {
    out.push_str(
        "**Real barge-ins.** The barge-in calls (the caller speaks over the bot; clean line, noise or a second voice on the line). \
         *Missed*: the caller never got through; of those, *VAD*: the VAD never started on the caller's speech (no guard can change that; the same in every mode) and *held*: the VAD started and the guards or the rule never let it through. *Too early*: the bot was cut off before the caller spoke. \
         *Real candidates*: the VAD starts that are the caller's own onset; *lost*: of those, the ones that did not end in a cut (the caller has to say it again). \
         *Silence*: from the speech onset to the caller hearing the bot stop; *final cut*: from the onset to the bot being cut off (both in ms).\n\n\
         | mode | barge-in calls | missed | of which VAD | held | too early | real candidates | lost | silence, median | p90 | final cut, median | p90 | time to interrupt (first word), median | p90 |\n\
         |---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    for (mi, mode) in report.modes.iter().enumerate() {
        let calls: Vec<&CallRow> = calls_of(report, mi, rec, path)
            .filter(|c| !c.quiet)
            .collect();
        let real: Vec<&CandRow> = cands_of(report, mi, rec, path)
            .filter(|c| c.label == Label::Real)
            .collect();
        let lost = real.iter().filter(|c| !c.fate.is_cut()).count();
        let mut sil: Vec<f64> = real.iter().filter_map(|c| c.silence_ms).collect();
        let mut sil2 = sil.clone();
        let mut fin: Vec<f64> = real
            .iter()
            .filter(|c| c.fate.is_cut())
            .map(|c| c.final_ms)
            .collect();
        let mut fin2 = fin.clone();
        let mut t: Vec<f64> = calls.iter().filter_map(|c| c.tti_ms).collect();
        let mut t2 = t.clone();
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            mode.name,
            calls.len(),
            calls.iter().filter(|c| c.missed).count(),
            calls
                .iter()
                .filter(|c| c.missed && c.real_candidates == 0)
                .count(),
            calls
                .iter()
                .filter(|c| c.missed && c.real_candidates > 0)
                .count(),
            calls.iter().filter(|c| c.premature).count(),
            real.len(),
            lost,
            ms(&mut sil, 0.5),
            ms(&mut sil2, 0.9),
            ms(&mut fin, 0.5),
            ms(&mut fin2, 0.9),
            ms(&mut t, 0.5),
            ms(&mut t2, 0.9),
        );
    }
    out.push('\n');
}

fn real_by_say_table(out: &mut String, report: &RealisticReport, rec: usize, path: Path16) {
    out.push_str(
        "Real barge-ins held (missed although the VAD started on the caller; see the table above) by how they were said, and on what line (held / barge-in calls; in brackets: the VAD never started on the caller):\n\n| mode |",
    );
    for s in Say::ALL {
        let _ = write!(out, " {} |", s.name());
    }
    out.push_str(
        " clean line | with noise | with a second voice |\n|---|---|---|---|---|---|---|\n",
    );
    for (mi, mode) in report.modes.iter().enumerate() {
        let calls: Vec<&CallRow> = calls_of(report, mi, rec, path)
            .filter(|c| !c.quiet)
            .collect();
        let cell = |f: &dyn Fn(&CallRow) -> bool| {
            let v: Vec<_> = calls.iter().filter(|c| f(c)).collect();
            format!(
                "{}/{} ({})",
                v.iter()
                    .filter(|c| c.missed && c.real_candidates > 0)
                    .count(),
                v.len(),
                v.iter()
                    .filter(|c| c.missed && c.real_candidates == 0)
                    .count()
            )
        };
        let _ = write!(out, "| {} |", mode.name);
        for s in Say::ALL {
            let _ = write!(out, " {} |", cell(&|c| c.say == s));
        }
        let _ = writeln!(
            out,
            " {} | {} | {} |",
            cell(&|c| c.kind == Kind::None),
            cell(&|c| c.kind.is_noise()),
            cell(&|c| Kind::VOICES.contains(&c.kind)),
        );
    }
    out.push('\n');
}

fn voice_kinds() -> [Kind; 5] {
    [
        Kind::Dry,
        Kind::Room,
        Kind::FarField,
        Kind::Turkish,
        Kind::Babble,
    ]
}

fn voices_table(out: &mut String, report: &RealisticReport, rec: usize, path: Path16) {
    out.push_str(
        "**Second voices** (television, a person nearby) and babble, for completeness: the false candidates of quiet calls with a second voice or babble, by kind \
         (*cut off*, in brackets: paused and resumed), and *quiet for* over all of them (ms).\n\n| mode |",
    );
    for k in voice_kinds() {
        let _ = write!(out, " {} |", k.name());
    }
    out.push_str(" quiet for, median | p90 | max |\n|---|");
    for _ in 0..8 {
        out.push_str("---|");
    }
    out.push('\n');
    for (mi, mode) in report.modes.iter().enumerate() {
        let rows: Vec<&CandRow> = cands_of(report, mi, rec, path)
            .filter(|c| c.quiet && voice_kinds().contains(&c.kind) && c.label == Label::False)
            .collect();
        let _ = write!(out, "| {} |", mode.name);
        for k in voice_kinds() {
            let r: Vec<&CandRow> = rows.iter().copied().filter(|c| c.kind == k).collect();
            let (cut, resumed, _) = fates(&r);
            let _ = write!(out, " {cut} ({resumed}) n={} |", r.len());
        }
        let mut q1: Vec<f64> = rows.iter().filter_map(|c| c.pause_ms).collect();
        let (mut q2, mut q3) = (q1.clone(), q1.clone());
        let _ = writeln!(
            out,
            " {} | {} | {} |",
            ms(&mut q1, 0.5),
            ms(&mut q2, 0.9),
            ms(&mut q3, 1.0)
        );
    }
    out.push('\n');
}

fn longest_pauses(out: &mut String, report: &RealisticReport, rec: usize, path: Path16) {
    out.push_str(
        "The five longest pauses of each mode that has pauses, to see what a long one is (scenario, what was on the line, its level, where the candidate began and stopped in the call, what became of it):

         | mode | scenario | on the line | level | onset | stop | quiet for (ms) | outcome |
|---|---|---|---|---|---|---|---|
",
    );
    for (mi, mode) in report.modes.iter().enumerate() {
        let mut rows: Vec<&CandRow> = cands_of(report, mi, rec, path)
            .filter(|c| c.pause_ms.is_some())
            .collect();
        rows.sort_by(|a, b| {
            b.pause_ms
                .partial_cmp(&a.pause_ms)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for c in rows.iter().take(5) {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {:+} dB | {:.2} s | {} | {:.0} | resumed |",
                mode.name,
                c.scenario,
                c.kind.name(),
                c.level_db,
                c.onset_s,
                c.stop_s.map_or("-".to_owned(), |s| format!("{s:.2} s")),
                c.pause_ms.unwrap_or(0.0),
            );
        }
    }
    out.push('\n');
}

fn per_call_table(out: &mut String, report: &RealisticReport, rec: usize, path: Path16) {
    out.push_str(
        "Per call. *Quiet calls cut off*: the caller says nothing and the bot was cut off for good, by what is on the line (cut off / calls).\n\n| mode |",
    );
    for k in Kind::VOICES.iter().chain(Kind::NOISES.iter()) {
        let _ = write!(out, " {} |", k.name());
    }
    out.push_str("\n|---|");
    for _ in 0..(Kind::VOICES.len() + Kind::NOISES.len()) {
        out.push_str("---|");
    }
    out.push('\n');
    for (mi, mode) in report.modes.iter().enumerate() {
        let rows: Vec<&CallRow> = calls_of(report, mi, rec, path).collect();
        let _ = write!(out, "| {} |", mode.name);
        for k in Kind::VOICES.iter().chain(Kind::NOISES.iter()) {
            let q: Vec<_> = rows.iter().filter(|c| c.quiet && c.kind == *k).collect();
            let _ = write!(out, " {}/{} |", q.iter().filter(|c| c.cut).count(), q.len());
        }
        out.push('\n');
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(onset: f64, stop: Option<f64>, int: Option<f64>, words: Option<f64>) -> Cand {
        Cand {
            onset,
            arrival: onset + 0.1,
            stop,
            interrupted: int,
            first_words: words,
            paused: None,
            resumed: None,
        }
    }

    #[test]
    fn an_unpaused_candidate_is_cut_or_passed() {
        let c = cand(10.0, Some(12.0), Some(10.7), None);
        assert_eq!(pause_fate(&c, 20.0), Fate::Committed(10.7));
        let c = cand(10.0, Some(10.3), None, None);
        assert_eq!(pause_fate(&c, 20.0), Fate::Passed(10.3));
    }

    #[test]
    fn a_paused_candidate_is_cut_resumed_or_left_paused_at_the_end_of_the_call() {
        let mut c = cand(10.0, Some(12.0), Some(10.9), Some(10.9));
        c.paused = Some(10.1);
        assert_eq!(pause_fate(&c, 20.0), Fate::Committed(10.9));
        let mut c = cand(10.0, Some(14.0), None, None);
        c.paused = Some(10.1);
        c.resumed = Some(11.5);
        assert_eq!(pause_fate(&c, 20.0), Fate::Resumed(11.5));
        let mut c = cand(10.0, None, None, None);
        c.paused = Some(10.1);
        assert_eq!(pause_fate(&c, 20.0), Fate::Resumed(20.0));
    }

    #[test]
    fn labels() {
        let span = Some((12.0, 14.0));
        assert_eq!(
            label(&cand(12.1, Some(14.0), None, None), span),
            Label::Real
        );
        assert_eq!(
            label(&cand(9.0, Some(10.0), None, None), span),
            Label::False
        );
        assert_eq!(
            label(&cand(9.0, Some(13.0), None, None), span),
            Label::Joined
        );
        assert_eq!(label(&cand(9.0, None, None, None), None), Label::False);
    }
}
