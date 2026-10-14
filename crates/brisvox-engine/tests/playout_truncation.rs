//! `PlayoutTruncation` (`docs/PIPELINE.md` section 5) with the book driven by hand, so
//! every number is exact: the settle wait on the injected clock, `heard = min(released, total) -
//! lead`, the client's reported lead (Twilio) in place of the fixed margin, rounds played to
//! their end, and the runner applying the answer to the context (partial messages, closed tool
//! calls, `Context::validate`, `TruncationStats`). The wire-oracle tests on the whole pipeline
//! are in `playout_oracle.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_methods
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use brisvox_agent::tools::ToolKind;
use brisvox_core::{Clock, SharedClock, SimClock, TurnEpoch};
use brisvox_engine::{
    Cut, CutRequest, EngineConfig, PlayoutBook, PlayoutTruncation, PlayoutTruncationConfig,
    RoundKey, TruncationSource, clear_hook,
};
use brisvox_sim::mock_llm::LlmScript;
use brisvox_transport::ClientEvent;
use common::llm::{Rig, ToolProbe, lookup_tools};

const ANSWER: &str = "One two three four.";
/// Words of [`ANSWER`], 400 ms apart.
const WORDS: [(&str, u64); 4] = [("One", 0), ("two", 400), ("three", 800), ("four.", 1_200)];

fn ms(v: u64) -> Duration {
    Duration::from_millis(v)
}

fn epoch(e: u32) -> TurnEpoch {
    TurnEpoch::new(e)
}

fn key(e: u32, round: u32) -> RoundKey {
    RoundKey::new(epoch(e), round)
}

/// Writes one utterance the way the stages do: the sent text, the timed words and the audio.
fn speak(
    book: &PlayoutBook,
    key: RoundKey,
    utterance: u64,
    sent: &str,
    words: &[(&str, u64)],
    total: u64,
) {
    book.register_round(key);
    // The round has a context message, as every round the runner records does (a runner under
    // test has set the real index already).
    if book.snapshot(key).is_none_or(|s| s.msg_index.is_none()) {
        book.set_msg_index(key, 0);
    }
    book.register_utterance(key, utterance);
    assert!(book.push_sent(utterance, sent));
    for (word, start) in words {
        assert!(book.push_word(utterance, *word, ms(*start)));
    }
    assert!(book.add_audio(utterance, ms(total)));
}

fn config(margin: u64, settle: u64) -> PlayoutTruncationConfig {
    PlayoutTruncationConfig {
        lead_margin: ms(margin),
        settle_wait: ms(settle),
        played: brisvox_agent::PlayedConfig::default(),
    }
}

fn shared_clock() -> (SimClock, SharedClock) {
    let clock = SimClock::new();
    (clock, Arc::new(clock))
}

fn request(key: RoundKey, sent: &str, interrupted_at: Option<tokio::time::Instant>) -> CutRequest {
    CutRequest {
        key,
        sent: Arc::from(sent),
        interrupted_at,
    }
}

fn heard_text(cut: &Cut) -> &str {
    match cut {
        Cut::Heard(t) | Cut::HeardInFull(t) => &t.text,
        Cut::Keep => panic!("kept: {cut:?}"),
    }
}

// ---- the source on its own --------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn heard_is_the_released_position_minus_the_margin_capped_by_the_audio_handed_over() {
    let (clock, shared) = shared_clock();
    let book = PlayoutBook::shared(8);
    let source = PlayoutTruncation::new(Arc::clone(&book), shared, config(160, 250));
    speak(&book, key(1, 0), 1, ANSWER, &WORDS, 1_600);
    book.position(epoch(1), 1, ms(900));
    book.note_clear(epoch(2));
    book.position(epoch(1), 1, ms(900));
    let cut = source
        .cut(&request(key(1, 0), ANSWER, Some(clock.now())))
        .await;
    // 900 - 160 = 740 ms: "One" (0) and "two" (400) started, "three" (800) did not.
    assert_eq!(heard_text(&cut), "One two");
    assert!(matches!(cut, Cut::Heard(t) if t.played == ms(740) && !t.estimated));

    // Released beyond the audio handed over (an upper bound gone wild): capped by the total.
    let book = PlayoutBook::shared(8);
    let source = PlayoutTruncation::new(Arc::clone(&book), clock_of(&clock), config(160, 250));
    speak(&book, key(1, 0), 1, ANSWER, &WORDS, 1_000);
    book.position(epoch(1), 1, ms(9_000));
    book.note_clear(epoch(2));
    book.position(epoch(1), 1, ms(9_000));
    let cut = source
        .cut(&request(key(1, 0), ANSWER, Some(clock.now())))
        .await;
    assert!(
        matches!(&cut, Cut::Heard(t) if t.played == ms(840)),
        "{cut:?}"
    );
    assert_eq!(heard_text(&cut), "One two three");
}

fn clock_of(clock: &SimClock) -> SharedClock {
    Arc::new(*clock)
}

#[tokio::test(start_paused = true)]
async fn the_clients_reported_lead_replaces_the_margin() {
    let (clock, shared) = shared_clock();
    // The margin guesses 160 ms; the client says what it holds.
    for (lead, expected) in [
        (Some(50), "One two three"), // 900 - 50 = 850
        (Some(100), "One two"),      // 800: "three" starts at 800, not before it
        (Some(500), "One"),          // 400: "two" starts at 400, not before it
        (Some(2_000), ""),           // more than was released: nothing
        (None, "One two"),           // no report: the margin, 740
    ] {
        let book = PlayoutBook::shared(8);
        let source =
            PlayoutTruncation::new(Arc::clone(&book), Arc::clone(&shared), config(160, 250));
        speak(&book, key(1, 0), 1, ANSWER, &WORDS, 1_600);
        book.position(epoch(1), 1, ms(900));
        if let Some(lead) = lead {
            book.set_client_lead(ms(lead));
        }
        book.note_clear(epoch(2));
        book.position(epoch(1), 1, ms(900));
        let cut = source
            .cut(&request(key(1, 0), ANSWER, Some(clock.now())))
            .await;
        assert_eq!(heard_text(&cut), expected, "lead {lead:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn the_lead_belongs_to_the_clear_it_was_reported_before() {
    let (clock, shared) = shared_clock();
    let book = PlayoutBook::shared(8);
    let source = PlayoutTruncation::new(Arc::clone(&book), shared, config(160, 250));
    speak(&book, key(1, 0), 1, ANSWER, &WORDS, 1_600);
    book.position(epoch(1), 1, ms(900));
    book.set_client_lead(ms(500));
    book.note_clear(epoch(2));
    // The clear flushed the client: the level is gone, and a later report is for the next audio.
    assert_eq!(book.client_lead(), None);
    book.set_client_lead(ms(10));
    book.position(epoch(1), 1, ms(900));
    let cut = source
        .cut(&request(key(1, 0), ANSWER, Some(clock.now())))
        .await;
    assert_eq!(
        heard_text(&cut),
        "One",
        "the lead at the clear (500 ms) decides"
    );
}

#[tokio::test(start_paused = true)]
async fn the_lead_covers_later_utterances_first() {
    let (clock, shared) = shared_clock();
    let book = PlayoutBook::shared(8);
    let source = PlayoutTruncation::new(Arc::clone(&book), shared, config(160, 250));
    // Two utterances of one turn. The client holds 1.5 s: all of the second one (1 s released)
    // and the last 0.5 s of the first (2 s released).
    speak(&book, key(1, 0), 1, ANSWER, &WORDS, 2_000);
    speak(
        &book,
        key(1, 1),
        2,
        "Five six.",
        &[("Five", 0), ("six.", 500)],
        1_000,
    );
    book.position(epoch(1), 1, ms(2_000));
    book.position(epoch(1), 2, ms(1_000));
    book.set_client_lead(ms(1_500));
    book.note_clear(epoch(2));
    book.position(epoch(1), 2, ms(1_000));

    let second = source
        .cut(&request(key(1, 1), "Five six.", Some(clock.now())))
        .await;
    assert_eq!(
        heard_text(&second),
        "",
        "the whole second utterance is still queued"
    );
    let first = source
        .cut(&request(key(1, 0), ANSWER, Some(clock.now())))
        .await;
    // 2000 - (1500 - 1000) = 1500 ms: "four." starts at 1200.
    assert_eq!(heard_text(&first), ANSWER);
    assert!(
        matches!(&first, Cut::Heard(t) if t.played == ms(1_500)),
        "{first:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn an_utterance_played_to_its_end_is_kept_whole_and_so_is_one_followed_by_a_played_one() {
    let (clock, shared) = shared_clock();
    let book = PlayoutBook::shared(8);
    let source = PlayoutTruncation::new(Arc::clone(&book), shared, config(160, 250));
    // Round 0 has no end mark of its own (the mark ends the response); round 1 does.
    speak(&book, key(1, 0), 1, ANSWER, &WORDS, 1_600);
    speak(
        &book,
        key(1, 1),
        2,
        "Five six.",
        &[("Five", 0), ("six.", 500)],
        900,
    );
    book.position(epoch(1), 1, ms(1_600));
    book.position(epoch(1), 2, ms(900));
    book.register_mark("end", 2);
    // Before the echo, round 0 (fully released) is cut by the margin behind the later audio.
    let before = source
        .cut(&request(key(1, 0), ANSWER, Some(clock.now())))
        .await;
    assert!(matches!(before, Cut::Heard(_)), "{before:?}");
    assert_eq!(book.mark_played("end"), Some(2));
    for (round, sent) in [(0, ANSWER), (1, "Five six.")] {
        let cut = source
            .cut(&request(
                key(1, round),
                &format!("{sent}  "),
                Some(clock.now()),
            ))
            .await;
        match cut {
            Cut::HeardInFull(t) => {
                assert_eq!(&*t.text, sent, "whitespace trimmed, nothing cut");
                assert!(!t.estimated);
            }
            other => panic!("round {round}: {other:?}"),
        }
    }
    // A lost utterance is never whole, whatever echoed after it.
    book.mark_lost(1);
    let lost = source
        .cut(&request(key(1, 0), ANSWER, Some(clock.now())))
        .await;
    assert!(matches!(lost, Cut::Heard(_)), "{lost:?}");
}

#[tokio::test(start_paused = true)]
async fn the_settle_wait_runs_on_the_injected_clock_from_the_barge_in_and_a_timeout_is_counted() {
    let (clock, shared) = shared_clock();
    let book = PlayoutBook::shared(8);
    let source = PlayoutTruncation::new(Arc::clone(&book), shared, config(0, 250));
    speak(&book, key(1, 0), 1, ANSWER, &WORDS, 1_600);
    speak(
        &book,
        key(1, 1),
        2,
        "Five six.",
        &[("Five", 0), ("six.", 500)],
        900,
    );
    book.position(epoch(1), 1, ms(1_600));
    book.position(epoch(1), 2, ms(300));
    // The clear came, but no position follows it: the last known one is what there is.
    book.note_clear(epoch(2));
    let barge_in = clock.now();
    clock.sleep(ms(100)).await;

    let started = clock.now();
    let second = source
        .cut(&request(key(1, 1), "Five six.", Some(barge_in)))
        .await;
    assert_eq!(
        clock.now() - started,
        ms(150),
        "the wait is what is left of 250 ms after the barge-in"
    );
    assert_eq!(
        heard_text(&second),
        "Five",
        "the last known position, 300 ms"
    );
    assert_eq!(book.stats().settle_timeouts, 1);

    // The rounds of one pass are asked one after the other: the next question does not wait again.
    let started = clock.now();
    let first = source
        .cut(&request(key(1, 0), ANSWER, Some(barge_in)))
        .await;
    assert_eq!(clock.now(), started);
    assert_eq!(heard_text(&first), ANSWER);
    assert_eq!(book.stats().settle_timeouts, 1, "not counted twice");

    // A later pass (the memo lapsed) asked long after the barge-in does not wait at all, and
    // the unsettled utterance counts again.
    clock.sleep(ms(5_000)).await;
    let started = clock.now();
    let again = source
        .cut(&request(key(1, 1), "Five six.", Some(barge_in)))
        .await;
    assert_eq!(clock.now(), started);
    assert_eq!(heard_text(&again), "Five");
    assert_eq!(book.stats().settle_timeouts, 2);
}

#[tokio::test(start_paused = true)]
async fn the_final_position_ends_the_wait_early() {
    let (clock, shared) = shared_clock();
    let book = PlayoutBook::shared(8);
    let source = Arc::new(PlayoutTruncation::new(
        Arc::clone(&book),
        Arc::clone(&shared),
        config(0, 250),
    ));
    speak(&book, key(1, 0), 1, ANSWER, &WORDS, 1_600);
    book.position(epoch(1), 1, ms(400));
    book.note_clear(epoch(2));
    let barge_in = clock.now();
    let reporter = tokio::spawn({
        let (book, clock) = (Arc::clone(&book), clock);
        async move {
            clock.sleep(ms(60)).await;
            book.position(epoch(1), 1, ms(1_000));
        }
    });
    let cut = source
        .cut(&request(key(1, 0), ANSWER, Some(barge_in)))
        .await;
    assert_eq!(clock.now() - barge_in, ms(60), "woken by the position");
    assert_eq!(
        heard_text(&cut),
        "One two three",
        "the final position, 1000 ms, decides"
    );
    assert_eq!(book.stats().settle_timeouts, 0);
    reporter.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_response_that_ended_by_itself_is_not_waited_for_and_loses_no_margin() {
    let (clock, shared) = shared_clock();
    let book = PlayoutBook::shared(8);
    let source = PlayoutTruncation::new(Arc::clone(&book), shared, config(160, 250));
    // A client that never echoes marks: everything was released, nothing is marked played.
    speak(&book, key(1, 0), 1, ANSWER, &WORDS, 1_300);
    book.position(epoch(1), 1, ms(1_300));
    let started = clock.now();
    let cut = source.cut(&request(key(1, 0), ANSWER, None)).await;
    assert_eq!(clock.now(), started);
    assert_eq!(
        heard_text(&cut),
        ANSWER,
        "the audio has played out by the time the next turn starts"
    );
    assert_eq!(book.stats().settle_timeouts, 0);
}

#[tokio::test(start_paused = true)]
async fn a_response_that_ended_by_itself_ignores_a_reported_client_lead() {
    let (clock, shared) = shared_clock();
    let book = PlayoutBook::shared(8);
    let source = PlayoutTruncation::new(Arc::clone(&book), shared, config(160, 250));
    // Twilio-style: the last report said 600 ms were still queued at the client, then the audio
    // played out and the final mark was never echoed (dropped, or the mark_timeout fallback).
    speak(&book, key(1, 0), 1, ANSWER, &WORDS, 1_600);
    book.position(epoch(1), 1, ms(1_600));
    book.set_client_lead(ms(600));
    let started = clock.now();
    let cut = source.cut(&request(key(1, 0), ANSWER, None)).await;
    assert_eq!(clock.now(), started, "nothing is waited for");
    assert_eq!(
        heard_text(&cut),
        ANSWER,
        "the caller heard all of it; the lead of an earlier moment must not cut the last words"
    );
    // The same lead on an interrupted response is subtracted: 1600 - 600 = 1000 ms.
    book.note_clear(epoch(2));
    book.position(epoch(1), 1, ms(1_600));
    let cut = source
        .cut(&request(key(1, 0), ANSWER, Some(clock.now())))
        .await;
    assert_eq!(heard_text(&cut), "One two three");
}

#[tokio::test(start_paused = true)]
async fn a_barge_in_cut_while_the_book_never_saw_a_clear_is_flagged() {
    // A turn driver wired with `with_commands` instead of `clear_hook` never calls `note_clear`:
    // every mid-utterance cut then waits out the settle wait. The miswiring is counted.
    let (clock, shared) = shared_clock();
    let book = PlayoutBook::shared(8);
    let source = PlayoutTruncation::new(Arc::clone(&book), shared, config(160, 250));
    speak(&book, key(1, 0), 1, ANSWER, &WORDS, 1_600);
    book.position(epoch(1), 1, ms(900));
    let started = clock.now();
    let cut = source.cut(&request(key(1, 0), ANSWER, Some(started))).await;
    assert_eq!(heard_text(&cut), "One two");
    assert_eq!(clock.now() - started, ms(250), "waited out the settle wait");
    assert_eq!(source.unnoted_clear_cuts(), 1);
    assert_eq!(book.stats().clears_noted, 0);
    // An uninterrupted response does not count, and neither does a properly wired book.
    let _ = source.cut(&request(key(1, 0), ANSWER, None)).await;
    assert_eq!(source.unnoted_clear_cuts(), 1);
    let wired = PlayoutBook::shared(8);
    let source = PlayoutTruncation::new(Arc::clone(&wired), clock_of(&clock), config(160, 250));
    speak(&wired, key(1, 0), 1, ANSWER, &WORDS, 1_600);
    wired.note_clear(epoch(2));
    wired.position(epoch(1), 1, ms(900));
    let _ = source
        .cut(&request(key(1, 0), ANSWER, Some(clock.now())))
        .await;
    assert_eq!(source.unnoted_clear_cuts(), 0);
    assert_eq!(wired.stats().clears_noted, 1);
}

#[test]
fn the_settle_wait_is_clamped_to_half_the_analysis_bound() {
    let mut engine = EngineConfig::DEFAULT;
    engine.settle_wait = Duration::from_secs(3);
    let config = PlayoutTruncationConfig::from_engine(&engine);
    assert_eq!(config.settle_wait, engine.timeouts.analysis_job / 2);
    engine.settle_wait = ms(250);
    assert_eq!(
        PlayoutTruncationConfig::from_engine(&engine).settle_wait,
        ms(250)
    );
}

#[tokio::test(start_paused = true)]
async fn rounds_the_book_does_not_know_and_empty_messages_are_kept() {
    let (clock, shared) = shared_clock();
    let book = PlayoutBook::shared(8);
    let source = PlayoutTruncation::new(Arc::clone(&book), shared, config(0, 250));
    let unknown = source
        .cut(&request(key(7, 0), ANSWER, Some(clock.now())))
        .await;
    assert_eq!(unknown, Cut::Keep);
    book.register_round(key(1, 0));
    let blank = source
        .cut(&request(key(1, 0), "  \n", Some(clock.now())))
        .await;
    assert_eq!(blank, Cut::Keep);
    // A known round that never had an utterance was never heard: an empty heard prefix.
    let silent = source.cut(&request(key(1, 0), ANSWER, None)).await;
    assert_eq!(heard_text(&silent), "");
}

/// "Let me check." with its last word inside the 160 ms margin (780 ms of audio).
const CHECK: &str = "Let me check.";
const CHECK_WORDS: [(&str, u64); 3] = [("Let", 0), ("me", 180), ("check.", 700)];

#[tokio::test(start_paused = true)]
async fn the_margin_shrinks_by_the_time_since_the_last_release_and_is_gone_after_a_tool_wait() {
    let (clock, shared) = shared_clock();
    // `gap` is the time between the last audio released and the barge-in (a tool or filler wait
    // after the utterance). The 160 ms margin less the gap is what the client still holds.
    for (gap, expected) in [
        (0, "Let me"),  // 780 - 160 = 620 ms: "check." (700) not heard
        (50, "Let me"), // 780 - 110 = 670
        (80, "Let me"), // 780 - 80 = 700: a word at 700 has not started
        (100, CHECK),   // 780 - 60 = 720
        (160, CHECK),   // the buffer is empty
        (5_000, CHECK), // long after: nothing is subtracted, never more than the margin
    ] {
        let book = PlayoutBook::shared(8);
        let source =
            PlayoutTruncation::new(Arc::clone(&book), Arc::clone(&shared), config(160, 250));
        speak(&book, key(1, 0), 1, CHECK, &CHECK_WORDS, 780);
        book.position(epoch(1), 1, ms(780));
        clock.sleep(ms(gap)).await;
        let barge_in = clock.now();
        book.note_clear(epoch(2));
        // No further release: the transport had no final position to flush.
        book.position(epoch(1), 1, ms(780));
        let cut = source.cut(&request(key(1, 0), CHECK, Some(barge_in))).await;
        assert_eq!(heard_text(&cut), expected, "gap {gap} ms");
    }
}

#[tokio::test(start_paused = true)]
async fn a_reported_client_lead_is_not_faded_by_the_time_since_the_last_release() {
    // The client says what it holds at the clear: that is measured, not assumed.
    let (clock, shared) = shared_clock();
    let book = PlayoutBook::shared(8);
    let source = PlayoutTruncation::new(Arc::clone(&book), shared, config(160, 250));
    speak(&book, key(1, 0), 1, CHECK, &CHECK_WORDS, 780);
    book.position(epoch(1), 1, ms(780));
    book.set_client_lead(ms(100));
    clock.sleep(ms(5_000)).await;
    let barge_in = clock.now();
    book.note_clear(epoch(2));
    book.position(epoch(1), 1, ms(780));
    let cut = source.cut(&request(key(1, 0), CHECK, Some(barge_in))).await;
    assert_eq!(heard_text(&cut), "Let me", "780 - 100 = 680 ms");
}

#[tokio::test(start_paused = true)]
async fn a_cut_filler_whose_final_position_never_came_does_not_hold_up_later_turns() {
    // Epoch 1: a filler ("One moment.", its own round without a context message) is cut by a
    // barge-in between two position reports. The transport had no final position to flush and
    // there is no end mark, so the utterance is never settled and `mark_applied` never reaches
    // its round.
    let (clock, shared) = shared_clock();
    let book = PlayoutBook::shared(8);
    let source = PlayoutTruncation::new(Arc::clone(&book), shared, config(0, 250));
    book.register_round(key(1, 0));
    book.register_utterance(key(1, 0), 1);
    assert!(book.push_sent(1, "One moment."));
    assert!(book.add_audio(1, ms(1_000)));
    book.position(epoch(1), 1, ms(400));
    book.note_clear(epoch(2));
    assert!(!book.snapshot(key(1, 0)).unwrap().settled);

    // Epoch 2: an answer with a message, cut by the next barge-in with its final position.
    speak(&book, key(2, 0), 2, ANSWER, &WORDS, 1_600);
    book.position(epoch(2), 2, ms(500));
    book.note_clear(epoch(3));
    book.position(epoch(2), 2, ms(900));
    assert!(
        book.is_settled_through(epoch(2)),
        "the old filler must not block the wait (nor the speculation that asks the same)"
    );
    let asked = clock.now();
    let cut = source.cut(&request(key(2, 0), ANSWER, Some(asked))).await;
    assert_eq!(clock.now(), asked, "no settle wait");
    assert_eq!(heard_text(&cut), "One two three");
    assert_eq!(book.stats().settle_timeouts, 0);
}

#[tokio::test(start_paused = true)]
async fn the_clear_hook_tells_the_book_before_the_clear_reaches_the_client() {
    let clock = SimClock::new();
    let shared: SharedClock = Arc::new(clock);
    let (input, commands) = common::loopback::open(&shared, &EngineConfig::DEFAULT)
        .await
        .unwrap();
    let book = Arc::clone(&input.book);
    speak(&book, key(1, 0), 1, ANSWER, &WORDS, 1_600);
    book.position(epoch(1), 1, ms(400));
    assert!(!book.snapshot(key(1, 0)).unwrap().cleared);

    let hook = clear_hook(commands, Arc::clone(&book));
    // The book is told when the hook is called, before its future is first polled, so before
    // the command can be on its way: the transport flushes its final position right after the
    // Clear, and only a position after the note counts as the final one.
    let issued = hook(epoch(2));
    assert_eq!(
        book.stats().clears_noted,
        1,
        "told before the Clear was sent"
    );
    assert!(book.snapshot(key(1, 0)).unwrap().cleared);
    assert!(
        input.recv_client(ms(50)).await.is_none(),
        "nothing is sent before the hook's future runs"
    );
    issued.await.unwrap();
    let event = input.recv_client(ms(1_000)).await;
    assert!(
        matches!(event, Some(ClientEvent::Clear { epoch: e, .. }) if e == epoch(2)),
        "{event:?}"
    );
    assert_eq!(book.stats().clears_noted, 1, "told once");
}

// ---- the runner applying it -------------------------------------------------------------------

/// A runner with the production source over `book` and the engine config `config`.
async fn rig_with(book: &Arc<PlayoutBook>, engine: EngineConfig) -> Rig {
    let book = Arc::clone(book);
    Rig::new(engine, move |builder, _| builder.with_playout(book)).await
}

/// One barge-in on a finished or running response of epoch 1, then the next turn, until `ends`
/// responses have ended.
async fn barge_in_then_next_turn(
    rig: &Rig,
    book: &PlayoutBook,
    final_positions: &[(u64, u64)],
    ends: usize,
) {
    rig.interrupt(2).await;
    book.note_clear(epoch(2));
    for (utterance, released) in final_positions {
        book.position(epoch(1), *utterance, ms(*released));
    }
    rig.settle(ms(300)).await;
    rig.turn(2, "b").await;
    rig.until("the second answer", |r| {
        r.labels()
            .iter()
            .filter(|l| *l == "llm_response_ended")
            .count()
            >= ends
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn the_runner_cuts_the_message_to_the_heard_prefix_and_records_the_stats() {
    let book = PlayoutBook::shared(16);
    let rig = rig_with(&book, EngineConfig::DEFAULT).await;
    rig.llm.push(LlmScript::text([ANSWER]));
    rig.llm.push(LlmScript::text(["Two."]));
    rig.turn(1, "a").await;
    rig.until_label("llm_response_ended").await;
    speak(&book, key(1, 0), 1, ANSWER, &WORDS, 1_600);
    book.position(epoch(1), 1, ms(900));

    barge_in_then_next_turn(&rig, &book, &[(1, 900)], 2).await;

    // The default margin is 160 ms: 740 ms heard, "One two".
    assert_eq!(
        rig.conversation(),
        ["user:a", "assistant:One two", "user:b", "assistant:Two."]
    );
    rig.context_copy().validate().unwrap();
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.cut_requests, stats.cuts_heard, stats.cuts_shortened),
        (1, 1, 1)
    );
    assert_eq!(
        (
            stats.cut_errors,
            stats.cut_timeouts,
            stats.validate_failures
        ),
        (0, 0, 0)
    );
    let truncations = *rig.truncations.lock().unwrap();
    assert_eq!(truncations.truncations, 1);
    assert_eq!(truncations.estimated, 0);
    assert_eq!(truncations.unmatched_words, 0);
    // The round is not cut again at the next turn end.
    assert!(book.snapshot(key(1, 0)).unwrap().applied);
    assert_eq!(book.stats().settle_timeouts, 0);
}

#[tokio::test(start_paused = true)]
async fn the_runner_uses_the_lead_a_twilio_style_transport_reported() {
    let mut results = Vec::new();
    for lead in [None, Some(600)] {
        let book = PlayoutBook::shared(16);
        let rig = rig_with(&book, EngineConfig::DEFAULT).await;
        rig.llm.push(LlmScript::text([ANSWER]));
        rig.llm.push(LlmScript::text(["Two."]));
        rig.turn(1, "a").await;
        rig.until_label("llm_response_ended").await;
        speak(&book, key(1, 0), 1, ANSWER, &WORDS, 1_600);
        book.position(epoch(1), 1, ms(1_300));
        if let Some(lead) = lead {
            // Twilio: the audio sent to the client and not yet played, 600 ms.
            book.set_client_lead(ms(lead));
        }
        barge_in_then_next_turn(&rig, &book, &[(1, 1_300)], 2).await;
        results.push(rig.conversation()[1].clone());
        rig.context_copy().validate().unwrap();
    }
    // No report: 1300 - 160 = 1140 ms, "three" (800) heard, "four." (1200) not.
    assert_eq!(results[0], "assistant:One two three");
    // A lead above the margin: 1300 - 600 = 700 ms, only "two" (400) after "One".
    assert_eq!(results[1], "assistant:One two");
}

#[tokio::test(start_paused = true)]
async fn the_stop_cuts_an_interrupted_response_to_what_was_heard_before_it_is_forwarded() {
    let book = PlayoutBook::shared(16);
    let rig = rig_with(&book, EngineConfig::DEFAULT).await;
    rig.llm.push(LlmScript::text([ANSWER]));
    rig.turn(1, "a").await;
    rig.until_label("llm_response_ended").await;
    speak(&book, key(1, 0), 1, ANSWER, &WORDS, 1_600);
    book.position(epoch(1), 1, ms(900));
    // The caller barges in; the call ends before the next turn (Stop instead of UserTurnEnded).
    rig.interrupt(2).await;
    book.note_clear(epoch(2));
    book.position(epoch(1), 1, ms(900));
    rig.settle(ms(300)).await;
    rig.running
        .input()
        .send(brisvox_core::frame::SystemFrame::Stop.into())
        .await
        .unwrap();
    rig.until_label("stop").await;
    assert_eq!(rig.conversation(), ["user:a", "assistant:One two"]);
    rig.context_copy().validate().unwrap();
    let stats = rig.stats.snapshot();
    assert_eq!((stats.cut_requests, stats.cuts_heard), (1, 1));
    assert!(book.snapshot(key(1, 0)).unwrap().applied);
}

/// A summarizer that records the assistant texts it is asked to cover.
#[derive(Default)]
struct Recording(std::sync::Mutex<Vec<String>>);

impl brisvox_agent::window::Summarizer for Recording {
    fn summarize<'a>(
        &'a self,
        _previous: Option<&'a brisvox_agent::window::Summary>,
        messages: &'a [brisvox_agent::context::Message],
        _max_tokens: usize,
    ) -> brisvox_agent::window::BoxFuture<'a, Result<String, brisvox_agent::window::WindowError>>
    {
        Box::pin(async move {
            let mut seen = self.0.lock().unwrap();
            for message in messages {
                if let brisvox_agent::context::Message::Assistant { text, .. } = message {
                    seen.push(text.to_string());
                }
            }
            Ok("the caller asked something".to_owned())
        })
    }
}

#[tokio::test(start_paused = true)]
async fn the_cut_is_applied_before_the_window_summary_is_refreshed() {
    // A one-turn window: the interrupted turn is summarized when the next request is built. The
    // summarizer must see the heard text, not the text the LLM wrote.
    let book = PlayoutBook::shared(16);
    let summarizer = Arc::new(Recording::default());
    let mut engine = EngineConfig::DEFAULT;
    engine.llm.window = brisvox_agent::WindowPolicy::new(300, 1);
    let rig = Rig::new(engine, {
        let book = Arc::clone(&book);
        let summarizer = Arc::clone(&summarizer);
        move |b, _| b.with_playout(book).with_summarizer(summarizer)
    })
    .await;
    let filler = "word ".repeat(200);
    rig.llm.push(LlmScript::text([ANSWER]));
    rig.llm.push(LlmScript::text(["Two."]));
    rig.turn(1, &format!("a {filler}")).await;
    rig.until_label("llm_response_ended").await;
    speak(&book, key(1, 0), 1, ANSWER, &WORDS, 1_600);
    book.position(epoch(1), 1, ms(900));
    rig.interrupt(2).await;
    book.note_clear(epoch(2));
    book.position(epoch(1), 1, ms(900));
    rig.settle(ms(300)).await;
    rig.turn(2, &format!("b {filler}")).await;
    rig.until("the second answer", |r| {
        r.labels()
            .iter()
            .filter(|l| *l == "llm_response_ended")
            .count()
            >= 2
    })
    .await;
    let seen = summarizer.0.lock().unwrap().clone();
    assert_eq!(
        seen,
        ["One two"],
        "the summary covered the heard text of the interrupted turn, cut before it was refreshed"
    );
    assert_eq!(rig.conversation()[1], "assistant:One two");
}

#[tokio::test(start_paused = true)]
async fn the_next_turns_llm_request_does_not_wait_for_the_settle_wait() {
    // The final position never comes (an end mark is outstanding and neither echoed nor cut).
    // The wait is counted from the barge-in: the caller talks on for longer than `settle_wait`,
    // so the question costs nothing.
    let book = PlayoutBook::shared(16);
    let rig = rig_with(&book, EngineConfig::DEFAULT).await;
    rig.llm.push(LlmScript::text([ANSWER]));
    rig.llm.push(LlmScript::text(["Two."]));
    rig.turn(1, "a").await;
    rig.until_label("llm_response_ended").await;
    speak(&book, key(1, 0), 1, ANSWER, &WORDS, 1_600);
    book.register_mark("end", 1);
    book.position(epoch(1), 1, ms(900));
    rig.interrupt(2).await;
    book.note_clear(epoch(2));
    rig.settle(ms(400)).await;
    let asked = rig.clock.now();
    rig.turn(2, "b").await;
    rig.until("the second request", |r| r.llm.requests().len() == 2)
        .await;
    assert!(
        rig.clock.now() - asked <= ms(20),
        "the request left {:?} after the question",
        rig.clock.now() - asked
    );
    assert_eq!(book.stats().settle_timeouts, 1);
}

#[tokio::test(start_paused = true)]
async fn a_missing_final_position_falls_back_to_the_last_one_and_is_counted() {
    let book = PlayoutBook::shared(16);
    let rig = rig_with(&book, EngineConfig::DEFAULT).await;
    rig.llm.push(LlmScript::text([ANSWER]));
    rig.llm.push(LlmScript::text(["Two."]));
    rig.turn(1, "a").await;
    rig.until_label("llm_response_ended").await;
    speak(&book, key(1, 0), 1, ANSWER, &WORDS, 1_600);
    book.position(epoch(1), 1, ms(1_000));

    // The clear, and no position after it. The caller talks on for only 100 ms before the turn
    // ends: the runner waits the rest of the 250 ms and then uses 1000 - 160 = 840 ms.
    rig.interrupt(2).await;
    book.note_clear(epoch(2));
    rig.settle(ms(100)).await;
    let asked = rig.clock.now();
    rig.turn(2, "b").await;
    rig.until("the second answer", |r| {
        r.labels()
            .iter()
            .filter(|l| *l == "llm_response_ended")
            .count()
            >= 2
    })
    .await;

    assert_eq!(rig.conversation()[1], "assistant:One two three");
    assert_eq!(book.stats().settle_timeouts, 1);
    let stats = rig.stats.snapshot();
    assert_eq!(
        stats.cut_timeouts, 0,
        "the runner's own bound was not needed: {stats:?}"
    );
    let waited = rig.clock.now() - asked;
    assert!(
        waited >= ms(150) && waited < ms(400),
        "the answer comes after the rest of the settle wait: {waited:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_barge_in_in_round_two_keeps_round_one_whole_and_cuts_round_two() {
    let book = PlayoutBook::shared(16);
    let probe = Arc::new(ToolProbe::default());
    let rig = Rig::new(EngineConfig::DEFAULT, {
        let (book, probe) = (Arc::clone(&book), Arc::clone(&probe));
        move |builder, clock| {
            let (registry, engine) = lookup_tools(clock, &probe, ToolKind::ReadOnly, ms(100), None);
            builder.with_tools(registry, engine).with_playout(book)
        }
    })
    .await;
    rig.llm
        .push(LlmScript::text(["Let me check."]).with_tool_call(
            "call_1",
            "lookup",
            r#"{"q":"time"}"#,
        ));
    rig.llm.push(LlmScript::text([ANSWER]));
    rig.llm.push(LlmScript::text(["Two."]));
    rig.turn(1, "a").await;
    rig.until_label("llm_response_ended").await;
    assert_eq!(probe.started(), 1);

    // Round 0 spoke "Let me check." (780 ms), round 1 the answer; the caller hears the second
    // 900 ms in. "check." starts at 700 ms, inside the 160 ms margin: without the chain the
    // heard position of round 0 would be 780 - 160 = 620 ms and the word would be cut.
    speak(&book, key(1, 0), 1, CHECK, &CHECK_WORDS, 780);
    speak(&book, key(1, 1), 2, ANSWER, &WORDS, 1_600);
    book.position(epoch(1), 1, ms(780));
    book.position(epoch(1), 2, ms(900));

    barge_in_then_next_turn(&rig, &book, &[(2, 900)], 2).await;

    let conversation = rig.conversation();
    assert_eq!(conversation[0], "user:a");
    assert_eq!(conversation[1], "assistant+calls[lookup]:Let me check.");
    assert!(conversation[2].starts_with("tool:"), "{conversation:?}");
    assert_eq!(conversation[3], "assistant:One two");
    assert_eq!(conversation[4], "user:b");
    rig.context_copy().validate().unwrap();
    let stats = rig.stats.snapshot();
    assert_eq!(stats.cut_requests, 2);
    assert_eq!((stats.cut_errors, stats.validate_failures), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn a_barge_in_during_tool_execution_closes_the_call_and_keeps_what_was_spoken() {
    let book = PlayoutBook::shared(16);
    let probe = Arc::new(ToolProbe::default());
    let rig = Rig::new(EngineConfig::DEFAULT, {
        let (book, probe) = (Arc::clone(&book), Arc::clone(&probe));
        move |builder, clock| {
            let (registry, engine) =
                lookup_tools(clock, &probe, ToolKind::ReadOnly, ms(5_000), None);
            builder.with_tools(registry, engine).with_playout(book)
        }
    })
    .await;
    rig.llm
        .push(LlmScript::text(["Let me check."]).with_tool_call(
            "call_1",
            "lookup",
            r#"{"q":"time"}"#,
        ));
    rig.llm.push(LlmScript::text(["Two."]));
    rig.turn(1, "a").await;
    rig.until("the tool to start", |_| probe.started() == 1)
        .await;
    rig.settle(ms(1_000)).await;

    // "Let me check." (780 ms) was spoken in full while the tool ran, its last word inside the
    // 160 ms margin. The caller barges in 2 s after the last audio was released, while the tool
    // still runs: the client's buffer is empty, so the margin must not take the word back.
    speak(&book, key(1, 0), 1, CHECK, &CHECK_WORDS, 780);
    book.position(epoch(1), 1, ms(780));
    rig.settle(ms(2_000)).await;

    barge_in_then_next_turn(&rig, &book, &[(1, 780)], 1).await;

    let conversation = rig.conversation();
    assert_eq!(conversation[0], "user:a");
    assert_eq!(conversation[1], "assistant+calls[lookup]:Let me check.");
    assert!(
        conversation[2].starts_with("tool!"),
        "closed with an error: {conversation:?}"
    );
    assert_eq!(conversation[3], "user:b");
    assert_eq!(conversation[4], "assistant:Two.");
    rig.context_copy().validate().unwrap();
    let stats = rig.stats.snapshot();
    assert_eq!(stats.interrupted_tool_calls, 1);
    assert_eq!((stats.cut_errors, stats.validate_failures), (0, 0));
    assert_eq!(probe.finished(), 0, "the tool was cancelled, not run twice");
}

#[tokio::test(start_paused = true)]
async fn nothing_heard_of_a_message_with_a_call_empties_the_text_and_keeps_the_pair() {
    let book = PlayoutBook::shared(16);
    let probe = Arc::new(ToolProbe::default());
    let rig = Rig::new(EngineConfig::DEFAULT, {
        let (book, probe) = (Arc::clone(&book), Arc::clone(&probe));
        move |builder, clock| {
            let (registry, engine) =
                lookup_tools(clock, &probe, ToolKind::ReadOnly, ms(5_000), None);
            builder.with_tools(registry, engine).with_playout(book)
        }
    })
    .await;
    rig.llm
        .push(LlmScript::text(["Let me check."]).with_tool_call(
            "call_1",
            "lookup",
            r#"{"q":"time"}"#,
        ));
    rig.llm.push(LlmScript::text(["Two."]));
    rig.turn(1, "a").await;
    rig.until("the tool to start", |_| probe.started() == 1)
        .await;
    // The text reached the TTS stage but no audio did.
    book.register_round(key(1, 0));
    book.register_utterance(key(1, 0), 1);
    book.push_sent(1, "Let me check.");

    barge_in_then_next_turn(&rig, &book, &[], 1).await;

    let conversation = rig.conversation();
    assert_eq!(conversation[1], "assistant+calls[lookup]:");
    assert!(conversation[2].starts_with("tool!"), "{conversation:?}");
    rig.context_copy().validate().unwrap();
    assert_eq!(rig.stats.snapshot().cut_errors, 0);
}
