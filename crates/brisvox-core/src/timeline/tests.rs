//! Timeline tests: first-wins marks, turn rotation, late events, bounded history, metrics,
//! tracing output and overhead.

use super::*;

const MS: Duration = Duration::from_millis(1);

fn epoch(n: u32) -> TurnEpoch {
    TurnEpoch::new(n)
}

#[allow(clippy::disallowed_methods)] // a fixed base instant; offsets make tests deterministic
fn base() -> Instant {
    Instant::now()
}

/// A complete, uninterrupted turn with the given stage gaps (ms) after user speech end.
fn full_turn(rec: &TimelineRecorder, e: u32, t0: Instant) {
    use TurnEvent as E;
    let at = |ms: u64| t0 + Duration::from_millis(ms);
    rec.mark(epoch(e), E::UserSpeechStart, at(0));
    rec.mark(epoch(e), E::SttInterim, at(300));
    rec.mark(epoch(e), E::UserSpeechEnd, at(1_000));
    rec.mark(epoch(e), E::SttFinal, at(1_150));
    rec.mark(epoch(e), E::TurnEnd, at(1_400));
    rec.mark(epoch(e), E::LlmRequest, at(1_405));
    rec.mark(epoch(e), E::LlmFirstToken, at(1_705));
    rec.mark(epoch(e), E::LlmFirstSentence, at(1_805));
    rec.mark(epoch(e), E::TtsRequest, at(1_806));
    rec.mark(epoch(e), E::TtsFirstByte, at(1_956));
    rec.mark(epoch(e), E::FirstAudioOut, at(1_960));
}

#[test]
fn derived_metrics_follow_the_documented_definitions() {
    let rec = TimelineRecorder::new();
    full_turn(&rec, 0, base());
    let m = rec.current().unwrap().metrics();
    assert_eq!(
        m,
        TurnMetrics {
            end_of_turn_delay: Some(400 * MS),
            stt_final_delay: Some(150 * MS),
            llm_time_to_first_token: Some(300 * MS),
            llm_time_to_first_sentence: Some(400 * MS),
            tts_time_to_first_byte: Some(150 * MS),
            post_endpoint_latency: Some(560 * MS),
            response_latency: Some(960 * MS),
            time_to_silence: None,
        }
    );
}

#[test]
fn first_occurrence_wins_and_repeats_are_counted() {
    let rec = TimelineRecorder::new();
    let t0 = base();
    rec.mark(epoch(0), TurnEvent::SttInterim, t0 + MS);
    rec.mark(epoch(0), TurnEvent::SttInterim, t0 + 5 * MS);
    assert_eq!(
        rec.current().unwrap().at(TurnEvent::SttInterim),
        Some(t0 + MS)
    );
    assert_eq!(rec.stats().recorded, 1);
    assert_eq!(rec.stats().repeated, 1);
}

#[test]
fn newer_epoch_finishes_the_turn_and_older_events_are_late() {
    let rec = TimelineRecorder::new();
    let t0 = base();
    full_turn(&rec, 0, t0);
    rec.interrupt(epoch(0), epoch(1), t0 + 2_000 * MS, Some(t0 + 1_990 * MS));
    // A late TTS byte from the interrupted turn must not land in turn 1.
    rec.mark(epoch(0), TurnEvent::TtsFirstByte, t0 + 2_010 * MS);

    let history = rec.history();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].epoch(), epoch(0));
    assert!(history[0].at(TurnEvent::Interrupted).is_some());
    let current = rec.current().unwrap();
    assert_eq!(current.epoch(), epoch(1));
    assert_eq!(current.at(TurnEvent::TtsFirstByte), None);
    assert_eq!(rec.stats().late, 1);

    rec.finish();
    assert!(rec.current().is_none());
    assert_eq!(rec.history().len(), 2);
}

#[test]
fn missing_or_reversed_endpoints_give_no_metric() {
    let rec = TimelineRecorder::new();
    let t0 = base();
    rec.mark(epoch(0), TurnEvent::LlmFirstToken, t0);
    rec.mark(epoch(0), TurnEvent::LlmRequest, t0 + MS); // clock skew between stages
    let record = rec.current().unwrap();
    assert_eq!(record.metrics().llm_time_to_first_token, None);
    assert_eq!(record.metrics().tts_time_to_first_byte, None);
}

#[test]
fn history_is_bounded() {
    let rec = TimelineRecorder::new();
    let t0 = base();
    let turns = u32::try_from(HISTORY_CAPACITY).unwrap() + 10;
    for e in 0..=turns {
        rec.mark(epoch(e), TurnEvent::UserSpeechStart, t0);
    }
    let history = rec.history();
    assert_eq!(history.len(), HISTORY_CAPACITY);
    assert_eq!(rec.stats().evicted, 10);
    assert_eq!(history.last().unwrap().epoch(), epoch(turns - 1));
}

#[test]
#[allow(clippy::disallowed_methods)] // measuring real elapsed time is the point
fn overhead_per_event_is_small() {
    const TURNS: u32 = 2_000;
    let rec = TimelineRecorder::new();
    let t0 = base();
    let started = std::time::Instant::now();
    for e in 0..TURNS {
        for event in TurnEvent::ALL {
            rec.mark(epoch(e), event, t0);
        }
    }
    let elapsed = started.elapsed();
    let events = u128::from(TURNS) * TurnEvent::ALL.len() as u128;
    let per_event_ns = elapsed.as_nanos() / events;
    println!("timeline mark: {per_event_ns} ns/event over {events} events (no subscriber)");
    // Target < 1 µs/event (release). Debug builds are ~10x slower; keep the guard loose there.
    let limit_ns = if cfg!(debug_assertions) {
        10_000
    } else {
        1_000
    };
    assert!(per_event_ns < limit_ns, "{per_event_ns} ns/event");
}

#[test]
fn user_speech_end_is_the_last_stop_before_turn_end() {
    use TurnEvent as E;
    let rec = TimelineRecorder::new();
    let t0 = base();
    rec.mark(epoch(0), E::UserSpeechStart, t0);
    rec.mark(epoch(0), E::UserSpeechEnd, t0 + 500 * MS); // pause
    rec.mark(epoch(0), E::UserSpeechEnd, t0 + 1_500 * MS); // resumed, then stopped
    rec.mark(epoch(0), E::SttFinal, t0 + 600 * MS);
    rec.mark(epoch(0), E::SttFinal, t0 + 1_650 * MS);
    rec.mark(epoch(0), E::TurnEnd, t0 + 1_800 * MS);
    rec.mark(epoch(0), E::UserSpeechEnd, t0 + 1_900 * MS); // after the decision: ignored
    rec.mark(epoch(0), E::LlmRequest, t0 + 1_805 * MS);
    rec.mark(epoch(0), E::SttFinal, t0 + 1_950 * MS); // after the request: ignored
    let m = rec.current().unwrap().metrics();
    assert_eq!(m.end_of_turn_delay, Some(300 * MS));
    assert_eq!(m.stt_final_delay, Some(150 * MS));
    assert_eq!(rec.stats().repeated, 2);
}

#[test]
fn interrupt_survives_an_early_mark_of_the_next_turn() {
    let rec = TimelineRecorder::new();
    let t0 = base();
    full_turn(&rec, 0, t0);
    // VAD (another task) tags the new turn before the turn state machine records the barge-in.
    rec.mark(epoch(1), TurnEvent::SttInterim, t0 + 2_100 * MS);
    rec.interrupt(epoch(0), epoch(1), t0 + 2_050 * MS, Some(t0 + 2_000 * MS));
    let history = rec.history();
    assert!(
        history[0].at(TurnEvent::Interrupted).is_some(),
        "lost interrupt"
    );
    let current = rec.current().unwrap();
    assert_eq!(current.epoch(), epoch(1));
    assert_eq!(
        current.at(TurnEvent::Interrupted),
        None,
        "polluted the new turn"
    );
    assert_eq!(
        current.at(TurnEvent::UserSpeechStart),
        Some(t0 + 2_000 * MS)
    );
}

#[test]
fn time_to_silence_is_patched_into_the_interrupted_turn() {
    let rec = TimelineRecorder::new();
    let t0 = base();
    full_turn(&rec, 0, t0);
    rec.interrupt(epoch(0), epoch(1), t0 + 2_000 * MS, None);
    // The output stage reports silence afterwards, tagged with the interrupted turn.
    rec.mark(epoch(0), TurnEvent::AudioOutStopped, t0 + 2_030 * MS);
    assert_eq!(rec.history()[0].metrics().time_to_silence, Some(30 * MS));
    assert_eq!(rec.stats().late, 0);
    // Other events for a finished turn are still late.
    rec.mark(epoch(0), TurnEvent::TtsFirstByte, t0 + 2_040 * MS);
    assert_eq!(rec.stats().late, 1);
}

#[test]
fn finished_turns_do_not_reopen() {
    let rec = TimelineRecorder::new();
    let t0 = base();
    rec.mark(epoch(4), TurnEvent::UserSpeechStart, t0);
    rec.finish();
    rec.finish(); // idempotent
    rec.mark(epoch(4), TurnEvent::FirstAudioOut, t0);
    rec.mark(epoch(3), TurnEvent::FirstAudioOut, t0);
    assert!(rec.current().is_none());
    assert_eq!(rec.stats().late, 2);
    assert_eq!(rec.drain_finished().len(), 1);
    assert!(rec.history().is_empty());
}

#[test]
fn event_slots_match_declaration_order() {
    for (i, event) in TurnEvent::ALL.into_iter().enumerate() {
        assert_eq!(event.index(), i, "{event:?}");
    }
    let rec = TimelineRecorder::new();
    let t0 = base();
    full_turn(&rec, 0, t0);
    let names: Vec<&str> = rec
        .current()
        .unwrap()
        .events()
        .map(|(e, _)| e.name())
        .collect();
    assert_eq!(names.first(), Some(&"user_speech_start"));
    assert_eq!(names.len(), 11);
}

#[test]
fn every_llm_request_of_a_tool_loop_gets_its_own_metrics() {
    use TurnEvent as E;
    let rec = TimelineRecorder::new();
    let t0 = base();
    let e = epoch(3);
    rec.mark(e, E::UserSpeechEnd, t0);
    rec.mark(e, E::TurnEnd, t0 + 100 * MS);
    // Round 0: first token after 40 ms, complete after 90 ms.
    let r0 = rec.llm_request(e, t0 + 100 * MS).unwrap();
    rec.llm_first_token(e, r0, t0 + 140 * MS);
    rec.llm_done(e, r0, t0 + 190 * MS);
    // Round 1 (after tool results): first token after 20 ms, complete after 60 ms.
    let r1 = rec.llm_request(e, t0 + 400 * MS).unwrap();
    rec.llm_first_token(e, r1, t0 + 420 * MS);
    rec.llm_done(e, r1, t0 + 460 * MS);
    // Round 2: text only, first token and completion at the same instant.
    let r2 = rec.llm_request(e, t0 + 700 * MS).unwrap();
    rec.llm_first_token(e, r2, t0 + 730 * MS);
    rec.llm_done(e, r2, t0 + 730 * MS);
    // Repeated marks of a round are ignored (first wins).
    rec.llm_done(e, r2, t0 + 900 * MS);
    rec.finish();

    let turn = rec.history()[0];
    assert_eq!(turn.llm_round_count(), 3);
    let rounds: Vec<LlmRoundMetrics> = turn.llm_rounds().collect();
    assert_eq!(
        rounds,
        [
            LlmRoundMetrics {
                round: 0,
                time_to_first_token: Some(40 * MS),
                duration: Some(90 * MS),
            },
            LlmRoundMetrics {
                round: 1,
                time_to_first_token: Some(20 * MS),
                duration: Some(60 * MS),
            },
            LlmRoundMetrics {
                round: 2,
                time_to_first_token: Some(30 * MS),
                duration: Some(30 * MS),
            },
        ]
    );
    assert_eq!(turn.llm_round(3), None);
    // The turn-level marks keep the first request's behaviour.
    assert_eq!(turn.at(E::LlmRequest), Some(t0 + 100 * MS));
    assert_eq!(turn.at(E::LlmFirstToken), Some(t0 + 140 * MS));
    assert_eq!(turn.metrics().llm_time_to_first_token, Some(40 * MS));
}

#[test]
fn round_marks_of_a_late_turn_are_ignored_and_rounds_are_bounded() {
    let rec = TimelineRecorder::new();
    let t0 = base();
    let old = epoch(0);
    let round = rec.llm_request(old, t0).unwrap();
    rec.mark(epoch(1), TurnEvent::UserSpeechStart, t0 + MS);
    // The interrupted turn's late completion is late, not attached to turn 1.
    rec.llm_done(old, round, t0 + 2 * MS);
    assert_eq!(rec.current().unwrap().llm_round_count(), 0);
    assert_eq!(rec.llm_request(old, t0 + 3 * MS), None);
    assert_eq!(rec.stats().late, 2);

    let e = epoch(2);
    for i in 0..MAX_LLM_ROUNDS {
        let at = t0 + Duration::from_millis(10 * u64::try_from(i).unwrap());
        assert!(rec.llm_request(e, at).is_some(), "round {i}");
    }
    assert_eq!(rec.llm_request(e, t0 + 999 * MS), None, "turn is full");
    assert_eq!(rec.stats().llm_rounds_dropped, 1);
    assert_eq!(rec.current().unwrap().llm_round_count(), MAX_LLM_ROUNDS);
    // Marks for a round that was never opened change nothing.
    let repeated = rec.stats().repeated;
    rec.llm_done(e, MAX_LLM_ROUNDS + 5, t0 + 1_000 * MS);
    assert_eq!(rec.stats().repeated, repeated + 1);
    assert_eq!(rec.current().unwrap().llm_round(MAX_LLM_ROUNDS + 5), None);
}

#[test]
fn llm_calls_count_once_each_in_the_recorder_stats() {
    use TurnEvent as E;
    let rec = TimelineRecorder::new();
    let t0 = base();
    let e = epoch(4);
    rec.mark(e, E::TurnEnd, t0);
    // Round 0: the request changes the turn mark and opens the round: one recorded mark.
    let r0 = rec.llm_request(e, t0 + MS).unwrap();
    assert_eq!(rec.stats().recorded, 2, "TurnEnd and the first request");
    // First token changes the turn mark and the round: one recorded mark.
    rec.llm_first_token(e, r0, t0 + 2 * MS);
    assert_eq!(rec.stats().recorded, 3);
    // A repeated first token changes nothing: one repeated mark.
    rec.llm_first_token(e, r0, t0 + 3 * MS);
    assert_eq!(rec.stats().repeated, 1);
    // Round 1: the turn mark keeps its first time, but the round is new: one recorded mark.
    let r1 = rec.llm_request(e, t0 + 4 * MS).unwrap();
    assert_eq!(rec.stats().recorded, 4);
    assert_eq!(rec.stats().repeated, 1);
    rec.llm_done(e, r1, t0 + 5 * MS);
    assert_eq!(rec.stats().recorded, 5);
    assert_eq!(rec.stats().llm_rounds_dropped, 0);
}

#[test]
fn open_turn_marks_land_on_the_user_turn_in_progress_and_keep_the_latest_stop() {
    use TurnEvent as E;
    let rec = TimelineRecorder::new();
    let t0 = base();
    // No turn yet: late.
    rec.mark_open_turn(E::VadStop, t0);
    assert_eq!(rec.stats().late, 1);
    rec.mark(epoch(4), E::UserSpeechStart, t0);
    // The user pauses and resumes: the latest stop and `Finalize` before the end win.
    rec.mark_open_turn(E::VadStop, t0 + 500 * MS);
    rec.mark_open_turn(E::SttFinalize, t0 + 501 * MS);
    rec.mark_open_turn(E::VadStop, t0 + 1_300 * MS);
    rec.mark_open_turn(E::SttFinalize, t0 + 1_302 * MS);
    rec.mark(epoch(4), E::TurnEnd, t0 + 1_500 * MS);
    let turn = rec.current().unwrap();
    assert_eq!(turn.epoch(), epoch(4));
    assert_eq!(turn.between(E::VadStop, E::SttFinalize), Some(2 * MS));
    assert_eq!(turn.at(E::VadStop), Some(t0 + 1_300 * MS));
    // After the turn's end (the bot's part of it), a stop belongs to no open user turn.
    rec.mark_open_turn(E::VadStop, t0 + 2_000 * MS);
    assert_eq!(rec.current().unwrap().at(E::VadStop), Some(t0 + 1_300 * MS));
    assert_eq!(rec.stats().late, 2);
    // Nor after the recorder finished.
    rec.finish();
    rec.mark_open_turn(E::SttFinalize, t0 + 2_100 * MS);
    assert_eq!(rec.stats().late, 3);
    assert_eq!(rec.history()[0].at(E::SttFinalize), Some(t0 + 1_302 * MS));
}

/// Records what a [`TimelineSink`] was told.
#[derive(Default)]
struct Tap {
    finished: Mutex<Vec<TurnRecord>>,
    patched: Mutex<Vec<TurnRecord>>,
}

impl TimelineSink for Tap {
    fn turn_finished(&self, record: &TurnRecord) {
        self.finished.lock().unwrap().push(*record);
    }

    fn turn_patched(&self, record: &TurnRecord) {
        self.patched.lock().unwrap().push(*record);
    }
}

fn tapped() -> (TimelineRecorder, Arc<Tap>) {
    let tap = Arc::new(Tap::default());
    (TimelineRecorder::with_sink(tap.clone()), tap)
}

#[test]
fn sink_gets_each_turn_once_when_it_finishes() {
    let (rec, tap) = tapped();
    let t0 = base();
    full_turn(&rec, 0, t0);
    assert!(
        tap.finished.lock().unwrap().is_empty(),
        "an open turn is not finished"
    );
    full_turn(&rec, 1, t0 + 5_000 * MS);
    {
        let finished = tap.finished.lock().unwrap();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].epoch(), epoch(0));
        assert!(finished[0].at(TurnEvent::FirstAudioOut).is_some());
    }
    rec.finish();
    rec.finish();
    let finished = tap.finished.lock().unwrap();
    assert_eq!(finished.len(), 2, "finish() hands over the open turn once");
    assert_eq!(finished[1].epoch(), epoch(1));
}

#[test]
fn sink_sees_late_patches_of_finished_turns() {
    let (rec, tap) = tapped();
    let t0 = base();
    full_turn(&rec, 0, t0);
    // Barge-in: turn 0 is interrupted and turn 1 begins, then the output reports silence.
    rec.interrupt(epoch(0), epoch(1), t0 + 2_000 * MS, Some(t0 + 1_990 * MS));
    assert_eq!(tap.finished.lock().unwrap().len(), 1);
    rec.mark(epoch(0), TurnEvent::AudioOutStopped, t0 + 2_040 * MS);
    let patched = tap.patched.lock().unwrap();
    assert_eq!(patched.len(), 1);
    assert_eq!(patched[0].epoch(), epoch(0));
    assert_eq!(patched[0].metrics().time_to_silence, Some(40 * MS));
}

#[test]
fn sink_sees_an_interruption_recorded_after_the_turn_finished() {
    let (rec, tap) = tapped();
    let t0 = base();
    full_turn(&rec, 0, t0);
    // Another stage already moved to turn 1, so turn 0 is finished when the barge-in lands.
    rec.mark(epoch(1), TurnEvent::UserSpeechStart, t0 + 1_990 * MS);
    assert_eq!(tap.finished.lock().unwrap().len(), 1);
    rec.interrupt(epoch(0), epoch(1), t0 + 2_000 * MS, None);
    let patched = tap.patched.lock().unwrap();
    assert_eq!(patched.len(), 1);
    assert!(patched[0].at(TurnEvent::Interrupted).is_some());
}

#[test]
fn clones_share_one_sink_and_a_recorder_needs_none() {
    let (rec, tap) = tapped();
    let clone = rec.clone();
    let t0 = base();
    full_turn(&clone, 0, t0);
    rec.finish();
    assert_eq!(tap.finished.lock().unwrap().len(), 1);
    let plain = TimelineRecorder::new();
    full_turn(&plain, 0, t0);
    plain.finish();
}

#[test]
fn llm_round_request_instants_are_exposed() {
    let rec = TimelineRecorder::new();
    let t0 = base();
    let e = epoch(0);
    let r0 = rec.llm_request(e, t0).unwrap();
    let r1 = rec.llm_request(e, t0 + 700 * MS).unwrap();
    rec.llm_done(e, r0, t0 + 300 * MS);
    let turn = rec.current().unwrap();
    assert_eq!(turn.llm_round_request_at(r0), Some(t0));
    assert_eq!(turn.llm_round_request_at(r1), Some(t0 + 700 * MS));
    assert_eq!(turn.llm_round_request_at(2), None);
}

#[test]
fn is_marked_looks_at_the_current_turn_and_the_past_ones_without_counting_or_rotating() {
    use TurnEvent as E;
    let rec = TimelineRecorder::new();
    let t0 = base();
    assert!(!rec.is_marked(epoch(1), E::AudioOutStopped), "no turn yet");
    rec.mark(epoch(1), E::UserSpeechStart, t0);
    rec.mark(epoch(1), E::AudioOutStopped, t0 + MS);
    assert!(rec.is_marked(epoch(1), E::AudioOutStopped));
    assert!(!rec.is_marked(epoch(1), E::FirstChunk), "another event");
    assert!(
        !rec.is_marked(epoch(2), E::AudioOutStopped),
        "another epoch"
    );
    // A finished turn stays answerable.
    rec.mark(epoch(2), E::UserSpeechStart, t0 + 10 * MS);
    assert!(rec.is_marked(epoch(1), E::AudioOutStopped));
    assert!(!rec.is_marked(epoch(2), E::AudioOutStopped));
    // The query counts nothing and never starts a turn.
    let stats = rec.stats();
    assert!(!rec.is_marked(epoch(3), E::UserSpeechStart));
    assert_eq!(rec.stats(), stats);
    assert_eq!(rec.current().unwrap().epoch(), epoch(2));
}

#[test]
fn the_segmenter_and_tts_text_events_are_first_wins_slots_of_their_own() {
    use TurnEvent as E;
    assert_eq!(E::ALL.len(), E::COUNT);
    assert_eq!(E::FirstChunk.name(), "first_chunk");
    assert_eq!(E::TtsFirstText.name(), "tts_first_text");
    let rec = TimelineRecorder::new();
    let t0 = base();
    rec.mark(epoch(1), E::FirstChunk, t0 + 5 * MS);
    rec.mark(epoch(1), E::FirstChunk, t0 + 9 * MS);
    rec.mark(epoch(1), E::TtsFirstText, t0 + 6 * MS);
    let turn = rec.current().unwrap();
    assert_eq!(turn.at(E::FirstChunk), Some(t0 + 5 * MS), "first wins");
    assert_eq!(turn.between(E::FirstChunk, E::TtsFirstText), Some(MS));
    assert_eq!(rec.stats().repeated, 1);
}
