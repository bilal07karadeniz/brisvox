//! A 60-minute simulated call through the context window: the LLM input stays
//! bounded, the summary covers exactly the dropped turns, and tool calls are never split.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::HashSet;
use std::future::Future;
use std::pin::pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context as TaskContext, Poll, Waker};

use brisvox_agent::context::{CharsPerToken, Context, Message, TokenEstimator, ToolCall};
use brisvox_agent::window::{BoxFuture, SUMMARY_PREFIX};
use brisvox_agent::{
    Summarizer, Summary, WindowError, WindowPolicy, build_window, needs_summary, refresh_summary,
};
use brisvox_core::TurnEpoch;
use serde_json::json;

/// Turns in 60 minutes of conversation: one turn every 10 seconds, as in the acceptance.
const TURNS: usize = 360;
/// Messages before the first turn: the prompt only.
const HEADER: usize = 1;
const SYSTEM_PROMPT: &str =
    "You are the phone assistant of a dental clinic. Keep every answer short and spoken.";
const WORDS: &[&str] = &[
    "the",
    "clinic",
    "appointment",
    "tuesday",
    "morning",
    "please",
    "can",
    "you",
    "check",
    "time",
    "for",
    "me",
    "and",
    "confirm",
    "booking",
    "price",
    "dentist",
    "hours",
    "after",
    "lunch",
];

/// `words` words of text, varied by turn and by message.
fn sentence(turn: usize, salt: usize, words: usize) -> String {
    (0..words)
        .map(|i| WORDS[(turn * 7 + salt * 3 + i) % WORDS.len()])
        .collect::<Vec<_>>()
        .join(" ")
}

/// Appends one turn: about 15 words from the caller, a tool call with its result on every tenth
/// turn, and about 40 words from the agent. Returns the caller's text.
fn push_turn(ctx: &mut Context, turn: usize) -> String {
    let user = sentence(turn, 0, 15);
    ctx.push(Message::user(user.clone()));
    if turn % 10 == 9 {
        let epoch = TurnEpoch::new(u32::try_from(turn).unwrap());
        let id = ctx.next_tool_call_id(epoch);
        let call = ToolCall {
            id: id.clone(),
            provider_id: None,
            name: "lookup_appointment".into(),
            arguments: json!({ "day": turn % 7 }),
            provider_data: None,
        };
        ctx.push(Message::assistant_with_calls("", vec![call]));
        ctx.push(Message::tool_result(id, r#"{"slot":"10:30","free":true}"#));
    }
    ctx.push(Message::assistant(sentence(turn, 1, 40)));
    user
}

/// The text of a user message.
fn user_text(message: &Message) -> Option<String> {
    match message {
        Message::User { text } => Some(text.to_string()),
        _ => None,
    }
}

/// Polls a future that completes on its first poll (the fake summarizer is immediate).
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = TaskContext::from_waker(Waker::noop());
    for _ in 0..1_000 {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
    }
    panic!("the fake summarizer did not complete");
}

/// One summarizer call, as the summarizer saw it.
#[derive(Debug, Clone, PartialEq)]
struct SummarizerCall {
    /// The summary it was asked to extend: its text and `covers_until`.
    previous: Option<(String, usize)>,
    /// Number of messages it was given.
    len: usize,
    /// The size limit it was given.
    limit: usize,
    /// The text it returned.
    returned: String,
}

/// Summarizer that records every user text it was given, in order, every call, and counts them.
#[derive(Default)]
struct RecordingSummarizer {
    calls: AtomicUsize,
    received_users: Mutex<Vec<String>>,
    log: Mutex<Vec<SummarizerCall>>,
}

impl Summarizer for RecordingSummarizer {
    fn summarize<'a>(
        &'a self,
        previous: Option<&'a Summary>,
        messages: &'a [Message],
        max_tokens: usize,
    ) -> BoxFuture<'a, Result<String, WindowError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let mut received = self.received_users.lock().unwrap();
            received.extend(messages.iter().filter_map(user_text));
            // A fixed-size summary, like a real one, so the budget is stable.
            let returned = format!(
                "The caller and the agent discussed appointments; {} earlier turns condensed.",
                received.len()
            );
            self.log.lock().unwrap().push(SummarizerCall {
                previous: previous.map(|s| (s.text.to_string(), s.covers_until)),
                len: messages.len(),
                limit: max_tokens,
                returned: returned.clone(),
            });
            Ok(returned)
        })
    }
}

/// Extends the previous summary by one short sentence per call, as a careless summarizer might.
/// Its summary outgrows the limit after some dozens of rounds.
struct GrowingSummarizer;

impl Summarizer for GrowingSummarizer {
    fn summarize<'a>(
        &'a self,
        previous: Option<&'a Summary>,
        _messages: &'a [Message],
        _max_tokens: usize,
    ) -> BoxFuture<'a, Result<String, WindowError>> {
        Box::pin(async move {
            let mut text = previous.map(|s| s.text.to_string()).unwrap_or_default();
            text.push_str(" The caller asked about the booking again.");
            Ok(text)
        })
    }
}

/// Every tool result in the window answers a call that precedes it, and every call has its
/// result inside the same turn (before the next user message).
fn assert_tool_calls_whole(messages: &[Message]) {
    for (i, message) in messages.iter().enumerate() {
        if let Message::Assistant { tool_calls, .. } = message {
            for call in tool_calls {
                let answered = messages
                    .iter()
                    .skip(i + 1)
                    .take_while(|m| !matches!(m, Message::User { .. }))
                    .any(|m| matches!(m, Message::Tool { call_id, .. } if *call_id == call.id));
                assert!(answered, "call {} has no result in its turn", call.id);
            }
        }
        if let Message::Tool { call_id, .. } = message {
            let issued = messages[..i].iter().any(|m| {
                matches!(m, Message::Assistant { tool_calls, .. }
                    if tool_calls.iter().any(|c| c.id == *call_id))
            });
            assert!(issued, "result {call_id} has no call in the window");
        }
    }
}

/// Figures from one simulated call.
#[derive(Debug)]
struct CallStats {
    max_estimated: usize,
    summarizer_calls: usize,
    windows_with_tool_calls: usize,
    final_kept_turns: usize,
    final_estimated: usize,
    final_summarized_turns: usize,
}

/// Feeds the 60-minute call turn by turn through the window with `policy`, checking the
/// acceptance after every turn.
fn simulate_call(policy: WindowPolicy) -> CallStats {
    let est = CharsPerToken;
    let summarizer = RecordingSummarizer::default();
    let mut ctx = Context::with_system(SYSTEM_PROMPT);
    let mut summary: Option<Summary> = None;
    let mut stats = CallStats {
        max_estimated: 0,
        summarizer_calls: 0,
        windows_with_tool_calls: 0,
        final_kept_turns: 0,
        final_estimated: 0,
        final_summarized_turns: 0,
    };

    for turn in 0..TURNS {
        let latest_user = push_turn(&mut ctx, turn);
        ctx.validate().unwrap();

        // The summary must cover what the window is about to drop: the range asked for starts
        // where the previous summary ends.
        let previous_end = summary.as_ref().map_or(HEADER, |s| s.covers_until);
        let start_text = summary.as_ref().map(|s| s.text.to_string());
        let asked = needs_summary(&ctx, &policy, &est, summary.as_ref()).unwrap();
        let calls = block_on(refresh_summary(
            &ctx,
            &policy,
            &est,
            &summarizer,
            &mut summary,
        ))
        .unwrap();
        if let Some(range) = asked {
            assert_eq!(
                range.start, previous_end,
                "turn {turn}: range starts at the summary"
            );
            assert!(calls >= 1);
        } else {
            assert_eq!(calls, 0);
        }
        stats.summarizer_calls += calls;

        // Every summarizer call of this turn extends the summary the previous call returned (the
        // first one extends the summary from before the turn). The stored summary is the last
        // output and covers exactly the messages the rounds gave.
        let round = std::mem::take(&mut *summarizer.log.lock().unwrap());
        assert_eq!(round.len(), calls, "turn {turn}: calls not recorded");
        let mut covers = previous_end;
        let mut text = start_text;
        for call in &round {
            assert_eq!(
                call.limit, policy.max_summary_tokens,
                "turn {turn}: the summary limit is not passed to the summarizer"
            );
            assert_eq!(
                call.previous,
                text.clone().map(|t| (t, covers)),
                "turn {turn}: the summarizer was not given the current summary"
            );
            covers += call.len;
            text = Some(call.returned.clone());
        }
        match &summary {
            None => assert!(text.is_none(), "turn {turn}: a summary was lost"),
            Some(s) => {
                assert_eq!(
                    text.as_deref(),
                    Some(&*s.text),
                    "turn {turn}: the stored summary is not the last summarizer output"
                );
                assert_eq!(
                    s.covers_until, covers,
                    "turn {turn}: covers_until is not the end of the summarized rounds"
                );
            }
        }

        let window = build_window(&ctx, &policy, &est, summary.as_ref()).unwrap();
        let messages = window.context.messages();

        // Acceptance: bounded input, valid, prompt first, latest caller message present.
        assert!(
            window.estimated_tokens <= policy.max_tokens,
            "turn {turn}: {} estimated tokens over the budget {}",
            window.estimated_tokens,
            policy.max_tokens
        );
        assert_eq!(
            window.estimated_tokens,
            window.context.estimate_tokens(&est)
        );
        assert!(
            !window.over_budget,
            "turn {turn}: nothing should be over budget"
        );
        assert!(
            window.uncovered.is_empty(),
            "turn {turn}: a dropped turn lacks a summary"
        );
        window.context.validate().unwrap();
        assert_eq!(messages.first(), Some(&Message::system(SYSTEM_PROMPT)));
        let last_user = messages.iter().rev().find_map(user_text).unwrap();
        assert_eq!(
            last_user, latest_user,
            "turn {turn}: latest caller message missing"
        );
        assert_tool_calls_whole(messages);
        if messages
            .iter()
            .any(|m| matches!(m, Message::Assistant { tool_calls, .. } if !tool_calls.is_empty()))
        {
            stats.windows_with_tool_calls += 1;
        }

        // The summary covers exactly the dropped turns: nothing is missing and nothing repeats.
        let covers = summary.as_ref().map_or(HEADER, |s| s.covers_until);
        assert_eq!(
            covers, window.kept_from,
            "turn {turn}: summary and window disagree"
        );
        match &summary {
            Some(s) => {
                assert_eq!(
                    messages.get(1),
                    Some(&Message::system(format!("{SUMMARY_PREFIX}{}", s.text))),
                    "turn {turn}: the summary text is not in the window after the prompt"
                );
                assert!(
                    est.estimate(&messages[1]) <= policy.max_summary_tokens,
                    "turn {turn}: the window's summary exceeds its limit"
                );
            }
            None => assert!(
                !messages
                    .iter()
                    .any(|m| matches!(m, Message::System { .. }) && m != &messages[0]),
                "turn {turn}: a system message without a summary"
            ),
        }
        let summarized: Vec<String> = ctx.messages()[HEADER..covers]
            .iter()
            .filter_map(user_text)
            .collect();
        let received = summarizer.received_users.lock().unwrap().clone();
        assert_eq!(
            received, summarized,
            "turn {turn}: summarized turns differ from the dropped ones"
        );
        let in_window: Vec<String> = messages.iter().filter_map(user_text).collect();
        let all: Vec<String> = ctx.messages().iter().filter_map(user_text).collect();
        let mut joined = summarized.clone();
        joined.extend(in_window.clone());
        assert_eq!(joined, all, "turn {turn}: a turn is lost or repeated");

        stats.max_estimated = stats.max_estimated.max(window.estimated_tokens);
        stats.final_kept_turns = window.kept_turns;
        stats.final_estimated = window.estimated_tokens;
        stats.final_summarized_turns = summarized.len();
    }

    // The full transcript is untouched by windowing.
    let user_turns = ctx
        .messages()
        .iter()
        .filter(|m| matches!(m, Message::User { .. }))
        .count();
    assert_eq!(user_turns, TURNS);
    let tool_calls: HashSet<String> = ctx
        .messages()
        .iter()
        .flat_map(|m| match m {
            Message::Assistant { tool_calls, .. } => {
                tool_calls.iter().map(|c| c.id.to_string()).collect()
            }
            _ => Vec::new(),
        })
        .collect();
    assert_eq!(tool_calls.len(), TURNS / 10);
    stats.summarizer_calls = summarizer.calls.load(Ordering::Relaxed);
    stats
}

#[test]
fn sixty_minutes_with_the_default_window_stays_bounded() {
    let policy = WindowPolicy::new(4000, 8);
    let stats = simulate_call(policy);
    assert!(stats.max_estimated <= 4000, "{stats:?}");
    // Eight recent turns verbatim; everything older is in the summary.
    assert_eq!(stats.final_kept_turns, 8);
    assert_eq!(stats.final_summarized_turns, TURNS - 8);
    assert!(
        stats.windows_with_tool_calls > 0,
        "the run must include tool calls in the window"
    );
    assert!(
        stats.summarizer_calls >= TURNS - 8,
        "one summary round per dropped turn"
    );
}

#[test]
fn sixty_minutes_with_a_budget_driven_window_fills_but_never_exceeds_the_budget() {
    let policy = WindowPolicy::new(4000, usize::MAX);
    let stats = simulate_call(policy);
    assert!(stats.max_estimated <= 4000, "{stats:?}");
    // Roughly 90 tokens per turn: the budget, not the turn count, decides what is kept.
    assert!(stats.final_kept_turns > 30, "{stats:?}");
    assert!(
        stats.final_estimated >= 3500,
        "window should use most of the budget: {stats:?}"
    );
    assert!(stats.windows_with_tool_calls > 0);
}

#[test]
fn the_window_is_configurable_for_a_tighter_budget() {
    let policy = WindowPolicy::new(1200, 4);
    let stats = simulate_call(policy);
    assert!(stats.max_estimated <= 1200, "{stats:?}");
    assert_eq!(stats.final_kept_turns, 4);
}

#[test]
fn a_summarizer_that_keeps_growing_cannot_grow_the_window() {
    let est = CharsPerToken;
    let policy = WindowPolicy::new(4000, 8);
    let mut ctx = Context::with_system(SYSTEM_PROMPT);
    let mut summary: Option<Summary> = None;
    let mut rejections = 0usize;
    for turn in 0..TURNS {
        push_turn(&mut ctx, turn);
        let before = summary.clone();
        match block_on(refresh_summary(
            &ctx,
            &policy,
            &est,
            &GrowingSummarizer,
            &mut summary,
        )) {
            Ok(_) => assert_eq!(
                rejections, 0,
                "turn {turn}: a summary was accepted after the growing one was rejected"
            ),
            Err(WindowError::SummaryTooLarge {
                estimated_tokens,
                limit,
            }) => {
                rejections += 1;
                assert_eq!(limit, policy.max_summary_tokens);
                assert!(estimated_tokens > limit, "turn {turn}");
                assert_eq!(
                    summary, before,
                    "turn {turn}: a rejected summary replaces nothing"
                );
            }
            Err(other) => panic!("turn {turn}: unexpected error {other:?}"),
        }

        // Whatever the summarizer does, the request stays within the budget and the summary
        // message within its own limit.
        let window = build_window(&ctx, &policy, &est, summary.as_ref()).unwrap();
        window.context.validate().unwrap();
        assert!(
            window.estimated_tokens <= policy.max_tokens,
            "turn {turn}: {} estimated tokens over the budget {}",
            window.estimated_tokens,
            policy.max_tokens
        );
        assert!(!window.over_budget, "turn {turn}: over budget");
        if summary.is_some() {
            let piece = &window.context.messages()[1];
            assert!(
                est.estimate(piece) <= policy.max_summary_tokens,
                "turn {turn}: the window's summary exceeds its limit"
            );
        }
    }
    assert!(
        rejections > 0,
        "the growing summary must outgrow its limit within the call"
    );
    // Once it is stale, the window reports the dropped turns that the summary does not cover.
    let window = build_window(&ctx, &policy, &est, summary.as_ref()).unwrap();
    assert!(!window.uncovered.is_empty());
}
