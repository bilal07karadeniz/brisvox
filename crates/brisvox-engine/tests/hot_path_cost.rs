//! Micro-benches of the work that runs on tokio workers, against the 50 us rule of CONTRIBUTING.md
//! `Segmenter::push_spans`, `ResponseAssembler::push` / `finish`,
//! `TurnMachine::handle`, the stream resamplers (caller frames and TTS chunks through
//! `OutputFramer`), request builds (`build_window` through `RequestBuilder::build`) and context
//! copies at the inline thresholds, and `PlayedText::heard`.
//!
//! Each case times single calls (the median, p99 and worst of many, best of three runs, so one
//! scheduling hiccup on a loaded machine does not count) and prints a table. The bounds depend
//! on the build: in release the median of every case that runs on a worker must be under 50 us;
//! in debug (the quality gate) it is 10 ms (the unoptimised DSP of a debug build is about 100
//! times slower), which only catches a gross regression. The release numbers are in docs/LATENCY.md ("Hot-path micro-benches"):
//! `cargo test --release -p brisvox-engine --test hot_path_cost -- --nocapture`.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods, // wall-clock timing of CPU work
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use brisvox_agent::context::{Context, Message, ToolCall, ToolCallId};
use brisvox_agent::{
    Delta, Language, PlayedConfig, PlayedText, ResponseAssembler, Segmenter, SegmenterConfig, Span,
    TurnInput, TurnMachine, TurnPolicy, WindowPolicy,
};
use brisvox_audio::StreamResampler;
use brisvox_core::TurnEpoch;
use brisvox_core::frame::SampleRate;
use brisvox_engine::{EngineConfig, OutputFramer, RequestBuilder};
use serde_json::json;

const US: Duration = Duration::from_micros(1);

/// One case's statistics over single calls.
#[derive(Debug, Clone, Copy)]
struct Stats {
    median: Duration,
    p99: Duration,
    worst: Duration,
}

/// Times `op` `n` times (after `n / 10` warm-up calls), three runs, keeping each statistic's
/// best run.
fn measure(n: usize, mut op: impl FnMut()) -> Stats {
    // A tenth of the calls in a debug build (the gate's): its bound is only a coarse guard.
    let n = if cfg!(debug_assertions) {
        (n / 10).max(50)
    } else {
        n
    };
    for _ in 0..n / 10 {
        op();
    }
    let mut best: Option<Stats> = None;
    for _ in 0..3 {
        let mut times: Vec<Duration> = (0..n)
            .map(|_| {
                let started = Instant::now();
                op();
                started.elapsed()
            })
            .collect();
        times.sort_unstable();
        let stats = Stats {
            median: times[n / 2],
            p99: times[(n * 99 / 100).min(n - 1)],
            worst: times[n - 1],
        };
        best = Some(match best {
            None => stats,
            Some(b) => Stats {
                median: b.median.min(stats.median),
                p99: b.p99.min(stats.p99),
                worst: b.worst.min(stats.worst),
            },
        });
    }
    best.unwrap()
}

/// The median bound of a case that runs on a tokio worker: the 50 us rule in release, a coarse
/// 10 ms in debug.
fn worker_bound() -> Duration {
    if cfg!(debug_assertions) {
        10_000 * US
    } else {
        50 * US
    }
}

fn report(rows: &[(&str, Stats, bool)]) {
    println!(
        "| Case | Median | p99 | Worst | On a worker | ({}) |",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    println!("|---|---|---|---|---|---|");
    for (name, s, worker) in rows {
        println!(
            "| {name} | {:.1} us | {:.1} us | {:.1} us | {} | |",
            s.median.as_secs_f64() * 1e6,
            s.p99.as_secs_f64() * 1e6,
            s.worst.as_secs_f64() * 1e6,
            if *worker { "yes" } else { "no" }
        );
    }
}

fn check(rows: &[(&str, Stats, bool)]) {
    for (name, s, worker) in rows {
        if *worker {
            assert!(
                s.median < worker_bound(),
                "{name}: median {:?} over {:?}",
                s.median,
                worker_bound()
            );
        }
    }
}

const ANSWER: &str = "Sure, here is a fairly long answer to your question. It has a second \
    sentence, with a clause, and a third one that ends the paragraph. Then 3.14 and Dr. Smith \
    on 12.03.2026 at 14:30, for 150 TL. ";

/// The answer as 4 to 12 byte deltas (an LLM's token stream).
fn deltas(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut step = 4;
    while start < text.len() {
        let mut end = (start + step).min(text.len());
        while !text.is_char_boundary(end) {
            end += 1;
        }
        out.push(&text[start..end]);
        start = end;
        step = if step >= 12 { 4 } else { step + 4 };
    }
    out
}

#[test]
fn text_side_work_stays_within_the_worker_budget() {
    let mut rows = Vec::new();

    // Segmenter: one delta per call, the stream reset when the text is used up.
    let stream = ANSWER.repeat(20);
    let pieces = deltas(&stream);
    let mut segmenter = Segmenter::new(SegmenterConfig::new(Language::En));
    let mut out: Vec<Span> = Vec::new();
    let mut i = 0usize;
    let push = measure(20_000, || {
        if i == pieces.len() {
            segmenter = Segmenter::new(SegmenterConfig::new(Language::En));
            i = 0;
        }
        segmenter.push_spans(pieces[i], &mut out);
        out.clear();
        i += 1;
    });
    rows.push(("Segmenter::push_spans (4-12 byte delta)", push, true));

    // ResponseAssembler: pushing text deltas; finishing a 2 KB and a 4 KB (inline limit)
    // response with one tool call.
    let mut assembler = ResponseAssembler::new();
    let text: Arc<str> = Arc::from("word ");
    let mut pushed = 0usize;
    let assemble_push = measure(20_000, || {
        if pushed * 5 > 60_000 {
            assembler = ResponseAssembler::new();
            pushed = 0;
        }
        assembler.push(Delta::Text(Arc::clone(&text))).unwrap();
        pushed += 1;
    });
    rows.push(("ResponseAssembler::push (text delta)", assemble_push, true));
    let context = Context::new();
    for (name, bytes) in [
        ("ResponseAssembler::finish (2 KB, 1 call)", 2_048usize),
        (
            "ResponseAssembler::finish (4 KB = inline_finish_bytes, 1 call)",
            EngineConfig::DEFAULT.llm.inline_finish_bytes,
        ),
    ] {
        let finish = measure(2_000, || {
            let mut assembler = ResponseAssembler::new();
            for _ in 0..bytes / 64 {
                assembler
                    .push(Delta::Text(Arc::from(
                        "sixty-four bytes of answer text, streamed in one delta ........",
                    )))
                    .unwrap();
            }
            assembler
                .push(Delta::ToolCall {
                    index: 0,
                    id: Some(Arc::from("call_1")),
                    name: Some(Arc::from("lookup")),
                    arguments: Arc::from(r#"{"city":"Istanbul","when":"tomorrow"}"#),
                    provider_data: None,
                })
                .unwrap();
            std::hint::black_box(assembler.finish(TurnEpoch::new(1), &context));
        });
        rows.push((name, finish, true));
    }

    // PlayedText::heard on a 200-word utterance.
    let mut played = PlayedText::new();
    let words: Vec<String> = (0..200).map(|i| format!("word{i}")).collect();
    played.push_sent(&words.join(" "));
    for (i, word) in words.iter().enumerate() {
        played.push_word(word.as_str(), Duration::from_millis(300 * i as u64));
    }
    let config = PlayedConfig::default();
    let mut at = 0u64;
    let heard = measure(20_000, || {
        at = (at + 1_337) % 60_000;
        std::hint::black_box(played.heard(Duration::from_millis(at), &config));
    });
    rows.push(("PlayedText::heard (200 words)", heard, true));

    // TurnMachine::handle: the inputs of one turn (start, interim, stop, final, ticks, bot audio).
    let mut machine = TurnMachine::new(TurnPolicy::fixed(Duration::from_millis(450)));
    let mut actions = Vec::new();
    let origin = tokio::time::Instant::now();
    let mut t = 0u64;
    let mut step = 0u32;
    let handle = measure(20_000, || {
        t += 20;
        let at = origin + Duration::from_millis(t);
        let input = match step % 8 {
            0 => TurnInput::VadStart { at },
            1 => TurnInput::SttInterim {
                text: "what time".to_owned(),
                at,
            },
            2 => TurnInput::VadStop { at },
            3 => TurnInput::SttFinal {
                text: "what time is it".to_owned(),
                at,
            },
            4 | 5 => TurnInput::Tick {
                now: at + Duration::from_millis(500),
            },
            6 => TurnInput::BotAudioStarted {
                epoch: machine.epoch(),
                at,
            },
            _ => TurnInput::BotAudioEnded {
                epoch: machine.epoch(),
                at,
            },
        };
        step += 1;
        machine.handle(input, &mut actions);
        actions.clear();
    });
    rows.push(("TurnMachine::handle (a turn's inputs)", handle, true));

    report(&rows);
    check(&rows);
}

#[test]
fn audio_side_work_stays_within_the_worker_budget() {
    let mut rows = Vec::new();
    // Caller frames (20 ms) through AudioIn's resampler to 16 kHz.
    for (name, from) in [
        (
            "StreamResampler 8 kHz -> 16 kHz, 20 ms frame",
            SampleRate::Hz8000,
        ),
        (
            "StreamResampler 48 kHz -> 16 kHz, 20 ms frame",
            SampleRate::Hz48000,
        ),
    ] {
        let mut resampler = StreamResampler::new(from, SampleRate::Hz16000).unwrap();
        let frame = vec![1_000i16; from.hz() as usize / 50];
        let mut out = vec![0i16; resampler.max_output_len(frame.len()) + resampler.chunk_frames()];
        let stats = measure(5_000, || {
            std::hint::black_box(resampler.process(&frame, &mut out).unwrap());
        });
        rows.push((name, stats, true));
    }
    // TTS chunks (24 kHz provider audio) through OutputFramer to the call rate: one push per
    // provider chunk, on the TTS reader task.
    for (name, to, chunk_ms) in [
        (
            "OutputFramer 24 kHz -> 16 kHz, 20 ms chunk",
            SampleRate::Hz16000,
            20u64,
        ),
        (
            "OutputFramer 24 kHz -> 16 kHz, 100 ms chunk",
            SampleRate::Hz16000,
            100,
        ),
        (
            "OutputFramer 24 kHz -> 8 kHz, 100 ms chunk",
            SampleRate::Hz8000,
            100,
        ),
        (
            "OutputFramer 24 kHz -> 16 kHz, 500 ms chunk (the stage cuts it in 100 ms pieces)",
            SampleRate::Hz16000,
            500,
        ),
    ] {
        let resampler = OutputFramer::build_resampler(SampleRate::Hz24000, to).unwrap();
        let mut framer = OutputFramer::with_resampler(to, to.hz() as usize / 50, resampler);
        let chunk = vec![0x10u8; 24 * 2 * chunk_ms as usize];
        let stats = measure(2_000, || {
            framer
                .push(&chunk, &mut |frame| {
                    std::hint::black_box(frame);
                })
                .unwrap();
        });
        rows.push((name, stats, chunk_ms <= 100));
    }
    report(&rows);
    check(&rows);
}

/// A transcript of `turns` question/answer pairs of about 250 bytes each.
fn transcript(turns: usize) -> Context {
    let mut ctx = Context::with_system("You are a voice agent.");
    for turn in 0..turns {
        ctx.push(Message::user(format!("{turn} {}", "question ".repeat(15))));
        ctx.push(Message::assistant(format!(
            "{turn} {}",
            "answer ".repeat(15)
        )));
    }
    ctx
}

/// One user turn and `rounds` tool rounds with results of `bytes` each.
fn tool_turn(rounds: usize, bytes: usize) -> Context {
    let epoch = TurnEpoch::new(1);
    let mut ctx = Context::with_system("You are a voice agent.");
    ctx.push(Message::user("please look everything up"));
    for round in 0..rounds {
        let id = ToolCallId::new(epoch, u32::try_from(round).unwrap());
        ctx.push(Message::assistant_with_calls(
            "",
            vec![ToolCall {
                id: id.clone(),
                provider_id: None,
                name: Arc::from("lookup"),
                arguments: json!({"n": round}),
                provider_data: None,
            }],
        ));
        ctx.push(Message::tool_result(id, "x".repeat(bytes)));
    }
    ctx
}

#[test]
fn request_builds_below_the_inline_thresholds_stay_within_the_worker_budget() {
    use brisvox_engine::request::is_heavy;
    let config = EngineConfig::DEFAULT;
    let builder = RequestBuilder::new(
        WindowPolicy::default(),
        config.llm.summary_timeout,
        config.llm.tool_result_floor_tokens,
    );
    let mut rows = Vec::new();
    for (name, context) in [
        ("RequestBuilder::build, 20 turns", transcript(20)),
        (
            "RequestBuilder::build, 380 messages (below INLINE_MAX_MESSAGES)",
            transcript(190),
        ),
        (
            "RequestBuilder::build, 4 x 3.7 KB results (below INLINE_MAX_TURN_BYTES)",
            tool_turn(4, 3_700),
        ),
    ] {
        assert!(!is_heavy(&context), "{name}");
        let stats = measure(500, || {
            std::hint::black_box(builder.build(&context, &[]).unwrap());
        });
        rows.push((name, stats, true));
        let copy = measure(500, || {
            std::hint::black_box(context.clone());
        });
        rows.push(("  Context::clone of the same", copy, true));
        let heavy = measure(2_000, || {
            std::hint::black_box(is_heavy(&context));
        });
        rows.push(("  is_heavy of the same", heavy, true));
    }
    report(&rows);
    check(&rows);
}
