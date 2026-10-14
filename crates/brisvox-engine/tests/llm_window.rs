//! Every LLM request of the runner is built through `build_window` (the
//! `RequestBuilder` seam). A one-hour call keeps each request within the budget, a turn with many
//! large tool results is bounded, and a summarizer that keeps returning a summary that is too
//! large is detected.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::disallowed_methods
)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use brisvox_agent::WindowPolicy;
use brisvox_agent::context::{CharsPerToken, Message};
use brisvox_agent::tools::ToolKind;
use brisvox_agent::window::{BoxFuture, SUMMARY_PREFIX, Summarizer, Summary, WindowError};
use brisvox_core::{Clock, SimClock, TurnEpoch};
use brisvox_engine::EngineConfig;
use brisvox_engine::convert::from_provider_messages;
use brisvox_providers::{LlmRequest, Role};
use brisvox_sim::mock_llm::LlmScript;
use common::MS;
use common::llm::{Rig, ToolProbe, lookup_tools_returning};
use serde_json::json;

const SECOND: Duration = Duration::from_secs(1);

/// The estimated tokens of a request, the way the window counts them.
fn request_tokens(request: &LlmRequest) -> usize {
    from_provider_messages(&request.messages, TurnEpoch::new(1))
        .unwrap()
        .estimate_tokens(&CharsPerToken)
}

/// A summarizer that answers `text` and counts its calls.
struct Fixed {
    text: String,
    calls: AtomicUsize,
}

impl Fixed {
    fn new(text: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            text: text.into(),
            calls: AtomicUsize::new(0),
        })
    }
}

impl Summarizer for Fixed {
    fn summarize<'a>(
        &'a self,
        previous: Option<&'a Summary>,
        messages: &'a [Message],
        _max_tokens: usize,
    ) -> BoxFuture<'a, Result<String, WindowError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.text.is_empty() {
                // A compact running summary: it grows by one short line per round.
                let before = previous.map_or(0, |s| s.text.matches(';').count());
                return Ok(format!(
                    "{} earlier messages summarised;",
                    before + messages.len()
                ));
            }
            Ok(self.text.clone())
        })
    }
}

fn words(n: usize, tag: &str) -> String {
    format!("{tag} {}", "word ".repeat(n))
}

#[tokio::test(start_paused = true)]
async fn a_sixty_minute_call_keeps_every_request_within_the_budget() {
    let mut config = EngineConfig::DEFAULT;
    // The turn cap (50) is far above what the budget allows (about 15 turns of ~160 tokens), so
    // only `max_tokens` can produce the bounds asserted below.
    config.llm.window = WindowPolicy::new(4_000, 50);
    let keep_recent_turns = config.llm.window.keep_recent_turns;
    let budget = config.llm.window.max_tokens;
    let summarizer = Fixed::new("");
    let rig = Rig::new(config, {
        let summarizer = Arc::clone(&summarizer);
        move |b, _| {
            b.with_system_prompt("You are a voice agent for a clinic.")
                .with_summarizer(summarizer)
        }
    })
    .await;

    let started = rig.clock.now();
    let turns = 180usize; // one turn every 20 s: an hour
    for turn in 0..turns {
        rig.llm
            .push(LlmScript::text([words(60, &format!("answer {turn}"))]));
        rig.turn(
            u32::try_from(turn).unwrap() + 1,
            &words(60, &format!("question {turn}")),
        )
        .await;
        rig.until("the answer", |r| {
            r.labels()
                .iter()
                .filter(|l| *l == "llm_response_ended")
                .count()
                == turn + 1
        })
        .await;
        rig.settle(20 * SECOND).await;
    }
    assert!(rig.clock.now() - started >= Duration::from_secs(3_600));

    // Building a window at 180 turns of transcript is cheap (O(context), see request.rs): a
    // coarse guard against an accidental quadratic, far above the real cost.
    let context = rig.context_copy();
    let builder = brisvox_engine::RequestBuilder::new(
        config.llm.window,
        config.llm.summary_timeout,
        config.llm.tool_result_floor_tokens,
    );
    let timing = std::time::Instant::now();
    let mut kept_turns = 0;
    for _ in 0..20 {
        kept_turns = builder.build(&context, &[]).unwrap().kept_turns;
    }
    assert!(
        timing.elapsed() < Duration::from_millis(500),
        "20 builds at 180 turns took {:?}",
        timing.elapsed()
    );
    assert!(
        kept_turns > 6 && kept_turns < keep_recent_turns,
        "the budget, not the turn cap, decided the window: {kept_turns} of {keep_recent_turns} turns"
    );

    // The full transcript is all there ...
    let context = rig.context_copy();
    assert_eq!(context.len(), 1 + 2 * turns);
    context.validate().unwrap();
    // ... but no request ever exceeded the budget, and none was built without the window.
    let sent = rig.llm.requests();
    assert_eq!(sent.len(), turns);
    let sizes: Vec<usize> = sent.iter().map(request_tokens).collect();
    let largest = *sizes.iter().max().unwrap();
    assert!(
        largest <= budget,
        "largest request {largest} > {budget}: {sizes:?}"
    );
    assert!(
        largest * 10 > budget * 6,
        "the budget is what binds: the largest request is only {largest} of {budget}"
    );
    let last_len = sent.last().unwrap().messages.len();
    assert!(
        last_len > 14 && last_len < 2 * keep_recent_turns,
        "the last request holds {last_len} messages of {}",
        context.len()
    );
    // Once the window is full the summary stands in for the dropped turns, right after the prompt.
    let last = sent.last().unwrap();
    assert_eq!(last.messages[0].role, Role::System);
    assert!(
        last.messages[1].content.starts_with(SUMMARY_PREFIX),
        "{:?}",
        last.messages[1]
    );
    let stats = rig.request_stats.snapshot();
    assert_eq!(
        stats.windows, turns as u64,
        "every request went through build_window"
    );
    assert_eq!(stats.over_budget, 0);
    assert_eq!(
        stats.builds_offloaded, 0,
        "361 short messages are built inline (the offload threshold is 384)"
    );
    assert_eq!(
        stats.uncovered_requests, 0,
        "the summary was always up to date before a request"
    );
    assert_eq!((stats.summary_too_large, stats.summary_failures), (0, 0));
    assert!(stats.summaries > 0);
    assert!(stats.max_estimated_tokens <= budget);
    // The conversation did not stall: every turn was answered.
    assert_eq!(rig.stats.snapshot().turns, turns as u64);
    assert_eq!(rig.stats.snapshot().failed_rounds, 0);
}

#[tokio::test(start_paused = true)]
async fn one_turn_with_twelve_large_tool_results_stays_within_the_budget() {
    let mut config = EngineConfig::DEFAULT;
    config.llm.window = WindowPolicy::new(1_000, 4);
    config.llm.max_tool_rounds = 14;
    let budget = config.llm.window.max_tokens;
    let probe = Arc::new(ToolProbe::default());
    let rig = Rig::new(config, {
        let probe = Arc::clone(&probe);
        move |b, clock| {
            // Each result is about 20 KB of text.
            let (registry, engine) = lookup_tools_returning(
                clock,
                &probe,
                ToolKind::ReadOnly,
                5 * MS,
                None,
                json!({"rows": "r".repeat(20_000)}),
            );
            b.with_system_prompt("You are a voice agent.")
                .with_tools(registry, engine)
        }
    })
    .await;
    for round in 0..12 {
        rig.llm
            .push(LlmScript::text([format!("Round {round}.")]).with_tool_call(
                &format!("call_{round}"),
                "lookup",
                r#"{"q":"everything"}"#,
            ));
    }
    rig.llm.push(LlmScript::text(["That is everything."]));
    rig.turn(1, "look up everything").await;
    rig.until_label("llm_response_ended").await;

    assert_eq!(probe.started(), 12);
    let sent = rig.llm.requests();
    assert_eq!(sent.len(), 13);
    let sizes: Vec<usize> = sent.iter().map(request_tokens).collect();
    assert!(
        sizes.iter().all(|&t| t <= budget),
        "every request fits the budget even with 12 results in the turn: {sizes:?}"
    );
    // The requests did grow with the turn (the newest results are kept), then were bounded.
    assert!(sizes[12] > sizes[1]);
    let stats = rig.request_stats.snapshot();
    assert!(stats.clipped_tool_results > 0, "{stats:?}");
    assert_eq!(stats.over_budget, 0);
    // Once the latest turn holds more than 16 KiB of results, its requests are built on the
    // offload, not on the worker.
    assert!(
        stats.builds_offloaded >= 10 && stats.builds_offloaded < 13,
        "{stats:?}"
    );
    assert_eq!(stats.offload_fallbacks, 0);
    // The transcript still holds the full results.
    let context = rig.context_copy();
    context.validate().unwrap();
    let biggest = context
        .messages()
        .iter()
        .filter(|m| matches!(m, Message::Tool { .. }))
        .map(|m| m.text().len())
        .max()
        .unwrap();
    assert!(biggest > 20_000, "{biggest}");
    // Pairs are intact in the last request.
    let last = sent.last().unwrap();
    from_provider_messages(&last.messages, TurnEpoch::new(1))
        .unwrap()
        .validate()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_summarizer_that_keeps_returning_too_large_summaries_is_detected() {
    let mut config = EngineConfig::DEFAULT;
    config.llm.window = WindowPolicy::new(600, 50);
    // 600 tokens: summaries may be 150; this one is about 250.
    let summarizer = Fixed::new("long ".repeat(200));
    let rig = Rig::new(config, {
        let summarizer = Arc::clone(&summarizer);
        move |b, _| b.with_summarizer(summarizer)
    })
    .await;
    let turns = 12usize;
    for turn in 0..turns {
        rig.llm.push(LlmScript::text([words(20, "answer")]));
        rig.turn(u32::try_from(turn).unwrap() + 1, &words(20, "question"))
            .await;
        rig.until("the answer", |r| {
            r.labels()
                .iter()
                .filter(|l| *l == "llm_response_ended")
                .count()
                == turn + 1
        })
        .await;
        rig.settle(5 * SECOND).await;
    }
    let stats = rig.request_stats.snapshot();
    assert!(stats.summary_too_large >= 3, "{stats:?}");
    assert!(stats.summary_degraded, "{stats:?}");
    assert!(
        stats.consecutive_summary_failures >= 3,
        "never reset by a success: {stats:?}"
    );
    assert_eq!(stats.summaries, 0, "a rejected summary is never stored");
    assert_eq!(stats.summary_covers_until, 0, "no summary was stored");
    // The calls still work, within the budget; the cost is visible: context was lost.
    assert!(stats.uncovered_requests > 0);
    let sizes: Vec<usize> = rig.llm.requests().iter().map(request_tokens).collect();
    assert!(sizes.iter().all(|&t| t <= 600), "{sizes:?}");
    assert_eq!(rig.stats.snapshot().turns, turns as u64);
}

/// A summarizer that takes `delay` of simulated time (or forever) before it answers.
struct Slow {
    clock: SimClock,
    delay: Option<Duration>,
    calls: AtomicUsize,
}

impl Summarizer for Slow {
    fn summarize<'a>(
        &'a self,
        previous: Option<&'a Summary>,
        messages: &'a [Message],
        _max_tokens: usize,
    ) -> BoxFuture<'a, Result<String, WindowError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            match self.delay {
                Some(delay) => self.clock.sleep(delay).await,
                None => std::future::pending::<()>().await,
            }
            let before = previous.map_or(0, |s| s.text.matches(';').count());
            Ok(format!("{} earlier messages;", before + messages.len()))
        })
    }
}

/// Sends the turn and returns how long it took until the provider got its request.
async fn request_delay(rig: &Rig, turn: usize, text: &str) -> Duration {
    let before = rig.llm.requests().len();
    let sent = rig.clock.now();
    rig.turn(u32::try_from(turn).unwrap() + 1, text).await;
    for _ in 0..4_000 {
        if rig.llm.requests().len() > before {
            return rig.clock.now() - sent;
        }
        rig.clock.sleep(MS).await;
    }
    panic!("turn {turn} never produced a request");
}

#[tokio::test(start_paused = true)]
async fn a_slow_summarizer_never_delays_a_request() {
    // The summarizer needs a full second. A saturated window needs a new summary every turn; the
    // request must go out at once with the summary at hand (the refresh runs after the response).
    let mut config = EngineConfig::DEFAULT;
    config.llm.window = WindowPolicy::new(600, 50);
    let summarizer = Arc::new(Slow {
        clock: SimClock::new(),
        delay: Some(SECOND),
        calls: AtomicUsize::new(0),
    });
    let rig = Rig::new(config, {
        let summarizer = Arc::clone(&summarizer);
        move |b, _| b.with_summarizer(summarizer)
    })
    .await;
    let mut worst = Duration::ZERO;
    for turn in 0..14usize {
        rig.llm.push(LlmScript::text([words(20, "answer")]));
        let delay = request_delay(&rig, turn, &words(20, "question")).await;
        worst = worst.max(delay);
        rig.until("the answer", |r| {
            r.labels()
                .iter()
                .filter(|l| *l == "llm_response_ended")
                .count()
                == turn + 1
        })
        .await;
        // Turns come every 3 s, so the background summary (1 s) is ready for the next one.
        rig.settle(3 * SECOND).await;
    }
    assert!(
        worst <= 2 * MS,
        "the request waited {worst:?} for the summarizer (it takes 1 s)"
    );
    let stats = rig.request_stats.snapshot();
    assert!(
        stats.summaries > 5,
        "the summary was kept up to date: {stats:?}"
    );
    assert_eq!(
        stats.uncovered_requests, 0,
        "refreshing ahead covered what each next request drops: {stats:?}"
    );
    assert_eq!(rig.stats.snapshot().failed_rounds, 0);
}

#[tokio::test(start_paused = true)]
async fn quick_turns_do_not_wait_for_a_summary_that_is_still_running() {
    // Turns come every 300 ms, faster than the 1 s summarizer: some requests go out with a summary
    // that is behind (counted), none waits, and only one summary task runs at a time.
    let mut config = EngineConfig::DEFAULT;
    config.llm.window = WindowPolicy::new(600, 50);
    let summarizer = Arc::new(Slow {
        clock: SimClock::new(),
        delay: Some(SECOND),
        calls: AtomicUsize::new(0),
    });
    let rig = Rig::new(config, {
        let summarizer = Arc::clone(&summarizer);
        move |b, _| b.with_summarizer(summarizer)
    })
    .await;
    let mut worst = Duration::ZERO;
    for turn in 0..20usize {
        rig.llm.push(LlmScript::text([words(20, "answer")]));
        worst = worst.max(request_delay(&rig, turn, &words(20, "question")).await);
        rig.until("the answer", |r| {
            r.labels()
                .iter()
                .filter(|l| *l == "llm_response_ended")
                .count()
                == turn + 1
        })
        .await;
        rig.settle(300 * MS).await;
    }
    assert!(worst <= 2 * MS, "waited {worst:?}");
    let stats = rig.request_stats.snapshot();
    assert!(stats.summaries > 0, "{stats:?}");
    assert!(
        stats.uncovered_requests > 0,
        "the summary lagged: {stats:?}"
    );
    assert!(rig.stats.snapshot().summary_in_flight > 0);
    let sizes: Vec<usize> = rig.llm.requests().iter().map(request_tokens).collect();
    assert!(sizes.iter().all(|&t| t <= 600), "{sizes:?}");
    assert_eq!(rig.stats.snapshot().turns, 20);
}

#[tokio::test(start_paused = true)]
async fn a_summarizer_that_never_answers_costs_the_requests_nothing_and_backs_off() {
    let mut config = EngineConfig::DEFAULT;
    config.llm.window = WindowPolicy::new(600, 50);
    config.llm.summary_timeout = Duration::from_secs(2);
    let summarizer = Arc::new(Slow {
        clock: SimClock::new(),
        delay: None,
        calls: AtomicUsize::new(0),
    });
    let rig = Rig::new(config, {
        let summarizer = Arc::clone(&summarizer);
        move |b, _| b.with_summarizer(summarizer)
    })
    .await;
    let mut worst = Duration::ZERO;
    let turns = 16usize;
    for turn in 0..turns {
        rig.llm.push(LlmScript::text([words(20, "answer")]));
        worst = worst.max(request_delay(&rig, turn, &words(20, "question")).await);
        rig.until("the answer", |r| {
            r.labels()
                .iter()
                .filter(|l| *l == "llm_response_ended")
                .count()
                == turn + 1
        })
        .await;
        rig.settle(3 * SECOND).await;
    }
    assert!(worst <= 2 * MS, "the request waited {worst:?}");
    let stats = rig.request_stats.snapshot();
    assert_eq!(stats.summaries, 0);
    assert!(
        stats.summary_timeouts >= 3 && stats.summary_degraded,
        "{stats:?}"
    );
    assert!(
        stats.summary_backoffs > 0,
        "the circuit breaker skipped attempts: {stats:?}"
    );
    // Fewer summarizer calls than turns: it is not asked again every turn forever.
    let calls = summarizer.calls.load(Ordering::SeqCst);
    assert!(
        calls < turns - 4,
        "{calls} summarizer calls in {turns} turns"
    );
}

/// A turn with `rounds` tool rounds whose results are `bytes` each.
fn tool_heavy_context(rounds: usize, bytes: usize) -> brisvox_agent::context::Context {
    use brisvox_agent::context::{Context, ToolCall, ToolCallId};
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
fn building_a_tool_heavy_request_is_cheap_and_does_not_grow_with_the_size_of_the_results() {
    // 12 results in one turn, over the budget, so every build clips them all and then bounds the
    // turn. The cost must follow the window (what is sent), not the size of the results.
    let config = EngineConfig::DEFAULT;
    let builder = |window| {
        brisvox_engine::RequestBuilder::new(
            window,
            config.llm.summary_timeout,
            config.llm.tool_result_floor_tokens,
        )
    };
    let policy = WindowPolicy::new(1_000, 4);
    // Interleaved runs, the best (minimum) of each size: load on the machine can only add time,
    // and both sizes see the same load. The deterministic guards are the allocation identity in
    // `tests/request_alloc.rs` (no copy that grows with the results) and
    // `clipping_a_huge_message_costs_in_proportion_to_the_limit_not_to_the_text` in brisvox-agent
    // (no bisection of the whole text); this one only bounds the wall clock coarsely.
    let (small_ctx, large_ctx) = (
        tool_heavy_context(12, 20_000),
        tool_heavy_context(12, 60_000),
    );
    let (small_builder, large_builder) = (builder(policy), builder(policy));
    small_builder.build(&small_ctx, &[]).unwrap(); // warm up
    large_builder.build(&large_ctx, &[]).unwrap();
    let (mut small, mut large) = (Duration::MAX, Duration::MAX);
    for _ in 0..15 {
        let started = std::time::Instant::now();
        small_builder.build(&small_ctx, &[]).unwrap();
        small = small.min(started.elapsed());
        let started = std::time::Instant::now();
        large_builder.build(&large_ctx, &[]).unwrap();
        large = large.min(started.elapsed());
    }
    eprintln!("per build (best of 15): 12 x 20 KB {small:?}, 12 x 60 KB {large:?}");
    // A coarse guard, an order of magnitude above the measured cost in a debug build (see
    // docs/PIPELINE.md, section 12).
    assert!(
        large < Duration::from_millis(20),
        "a build with 12 results of 60 KB took {large:?}"
    );
    assert!(
        large < small * 3 + Duration::from_millis(2),
        "3x larger results made a build {large:?} instead of about {small:?}: the cost follows the text, not the window"
    );

    // The shapes just below the inline thresholds (what a worker still builds itself): a
    // transcript of 380 short messages, and a latest turn of 15 KB in four results.
    let long = {
        let mut ctx = brisvox_agent::context::Context::with_system("You are a voice agent.");
        for turn in 0..190 {
            ctx.push(Message::user(format!("{turn} {}", "question ".repeat(15))));
            ctx.push(Message::assistant(format!(
                "{turn} {}",
                "answer ".repeat(15)
            )));
        }
        ctx
    };
    let near_bytes = tool_heavy_context(4, 3_700);
    for (what, context) in [("380 messages", &long), ("4 x 3.7 KB", &near_bytes)] {
        assert!(!brisvox_engine::request::is_heavy(context), "{what}");
        let builder = builder(WindowPolicy::default());
        builder.build(context, &[]).unwrap();
        let started = std::time::Instant::now();
        for _ in 0..50 {
            builder.build(context, &[]).unwrap();
        }
        let per_build = started.elapsed() / 50;
        eprintln!("per build, just below the inline thresholds ({what}): {per_build:?}");
        assert!(
            per_build < Duration::from_millis(5),
            "{what}: {per_build:?}"
        );
    }
}

/// A rig whose tool answers with 20 KB (so the latest turn is heavy from the second request on)
/// and whose offload is `offload`.
async fn heavy_turn_rig(offload: brisvox_engine::Offload) -> (Rig, Arc<ToolProbe>) {
    let probe = Arc::new(ToolProbe::default());
    let rig = Rig::new(EngineConfig::DEFAULT, {
        let probe = Arc::clone(&probe);
        move |b, clock| {
            let (registry, engine) = lookup_tools_returning(
                clock,
                &probe,
                ToolKind::ReadOnly,
                5 * MS,
                None,
                json!({"rows": "r".repeat(20_000)}),
            );
            b.with_tools(registry, engine).with_offload(offload)
        }
    })
    .await;
    rig.llm
        .push(LlmScript::text(["Looking."]).with_tool_call("call_1", "lookup", r#"{"q":"x"}"#));
    rig.llm.push(LlmScript::text(["Found it."]));
    (rig, probe)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_a_real_pool_the_heavy_request_is_built_on_the_pool() {
    use brisvox_core::SystemClock;
    use brisvox_inference::{InferencePool, PoolConfig};
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 1,
            queue_capacity: 8,
            name: "llm-build-test".to_owned(),
        },
        SystemClock::shared(),
    )
    .unwrap();
    let (rig, probe) = heavy_turn_rig(brisvox_engine::Offload::Pool(pool.handle())).await;
    rig.turn(1, "look it up").await;
    rig.until("the answer", |r| {
        r.labels().contains(&"llm_response_ended".to_owned())
    })
    .await;
    assert_eq!(probe.started(), 1);
    let stats = rig.request_stats.snapshot();
    assert_eq!(
        (
            stats.windows,
            stats.builds_offloaded,
            stats.offload_fallbacks
        ),
        (2, 1, 0),
        "the first request is small (inline), the second holds a 20 KB result (on the pool)"
    );
    assert_eq!(rig.llm.requests().len(), 2);
    assert_eq!(rig.conversation().last().unwrap(), "assistant:Found it.");
    drop(rig);
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn a_timed_out_build_falls_back_off_the_worker_even_while_the_context_lock_is_held() {
    // When the pool job times out, the job may still be running and hold the
    // context lock. The fallback must not wait for it (or build the heavy request) on the tokio
    // worker. Here the pool's only thread runs a job that takes the context lock 20 ms in (after
    // the build checked the context) and holds it for 400 ms, so the build job waits behind it
    // and times out after 50 ms. On this single-worker runtime a ticker shows any stall.
    use std::sync::Mutex;

    use brisvox_core::SystemClock;
    use brisvox_inference::{InferencePool, PoolConfig};
    let clock = SystemClock::shared();
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 1,
            queue_capacity: 8,
            name: "llm-build-timeout".to_owned(),
        },
        clock.clone(),
    )
    .unwrap();
    let config = EngineConfig::DEFAULT;
    let builder = Arc::new(
        brisvox_engine::RequestBuilder::new(
            WindowPolicy::default(),
            config.llm.summary_timeout,
            config.llm.tool_result_floor_tokens,
        )
        .with_offload(
            brisvox_engine::Offload::Pool(pool.handle()),
            Duration::from_millis(50),
        ),
    );
    let context = Arc::new(Mutex::new(tool_heavy_context(2, 20_000)));
    assert!(brisvox_engine::request::is_heavy(&context.lock().unwrap()));
    let ticks = Arc::new(Mutex::new(Vec::new()));
    let ticker = {
        let (ticks, clock) = (Arc::clone(&ticks), clock.clone());
        tokio::spawn(async move {
            loop {
                ticks.lock().unwrap().push(clock.now());
                clock.sleep(Duration::from_millis(5)).await;
            }
        })
    };
    let blocker = {
        let (busy, context) = (pool.handle(), Arc::clone(&context));
        tokio::spawn(async move {
            busy.run(Duration::from_secs(5), move || {
                std::thread::sleep(Duration::from_millis(20));
                let _guard = context.lock().unwrap();
                std::thread::sleep(Duration::from_millis(400));
            })
            .await
        })
    };
    // Both spawned tasks run once: the blocker job is queued before the build's.
    tokio::task::yield_now().await;
    let built = builder.build_offloaded(&context, &[]).await.unwrap();
    // One more tick after the build, so a stall right before it shows as a gap.
    clock.sleep(Duration::from_millis(20)).await;
    ticker.abort();
    assert!(!built.request.messages.is_empty());
    let stats = builder.stats().snapshot();
    assert_eq!(
        (stats.builds_offloaded, stats.offload_fallbacks),
        (0, 1),
        "{stats:?}"
    );
    let ticks = ticks.lock().unwrap().clone();
    let worst = ticks
        .windows(2)
        .map(|w| w[1].saturating_duration_since(w[0]))
        .max()
        .unwrap_or_default();
    let spanned = ticks.last().unwrap().saturating_duration_since(ticks[0]);
    assert!(
        spanned >= Duration::from_millis(250),
        "the build did not wait for the lock ({spanned:?}): the test proves nothing"
    );
    assert!(
        worst < Duration::from_millis(150),
        "the worker stalled while the fallback waited for the lock: {} ticks, worst gap {worst:?}",
        ticks.len()
    );
    blocker.await.unwrap().unwrap();
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_pool_that_refuses_the_build_does_not_fail_the_request() {
    use brisvox_core::SystemClock;
    use brisvox_inference::{InferencePool, PoolConfig};
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 1,
            queue_capacity: 2,
            name: "llm-build-refused".to_owned(),
        },
        SystemClock::shared(),
    )
    .unwrap();
    let handle = pool.handle();
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
    let (rig, probe) = heavy_turn_rig(brisvox_engine::Offload::Pool(handle)).await;
    rig.turn(1, "look it up").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(probe.started(), 1);
    let stats = rig.request_stats.snapshot();
    assert_eq!(
        (stats.builds_offloaded, stats.offload_fallbacks),
        (0, 1),
        "{stats:?}"
    );
    assert_eq!(rig.stats.snapshot().request_build_errors, 0);
    assert_eq!(rig.llm.requests().len(), 2, "the second request went out");
    assert_eq!(rig.conversation().last().unwrap(), "assistant:Found it.");
}

#[test]
fn large_tool_results_of_older_turns_make_the_context_heavy() {
    // `is_heavy` looked at the latest turn only, so a context holding large
    // results of earlier turns was snapshotted and built on the worker.
    use brisvox_engine::request::{INLINE_MAX_CONTEXT_BYTES, INLINE_MAX_TURN_BYTES, is_heavy};
    let with_old_results = |bytes: usize| {
        let mut ctx = tool_heavy_context(4, bytes);
        ctx.push(Message::assistant("Here is what I found."));
        ctx.push(Message::user("thanks, and now?"));
        ctx
    };
    // An older turn over the per-turn limit is fine on its own ...
    let old_turn = with_old_results(INLINE_MAX_TURN_BYTES / 2);
    assert!(!is_heavy(&old_turn));
    // ... but not once the whole context passes its limit.
    let heavy = with_old_results(INLINE_MAX_CONTEXT_BYTES / 4 + 1_024);
    assert!(is_heavy(&heavy));
    let below = with_old_results(INLINE_MAX_CONTEXT_BYTES / 4 - 1_024);
    assert!(!is_heavy(&below));
}
