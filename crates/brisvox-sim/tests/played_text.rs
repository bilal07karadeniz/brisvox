//! Played-text truncation through the reference pipeline: the caller barges in at random
//! points of a long answer, and the context must hold exactly the text that had played. The
//! bodies (and the independent oracle) live in `brisvox_sim::suite::played_text` so the
//! production pipeline runs them too; here they run against the reference chain with
//! exact positions.

use brisvox_sim::factory::ReferenceFactory;
use brisvox_sim::suite::Profile;
use brisvox_sim::suite::played_text as suite;

#[tokio::test(start_paused = true)]
async fn random_barge_ins_leave_the_context_at_exactly_the_heard_text() {
    suite::random_barge_ins_leave_the_context_at_exactly_the_heard_text(
        &ReferenceFactory,
        Profile::Exact,
    )
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_word_starting_in_the_unplayed_part_of_the_chunk_in_flight_is_not_heard() {
    suite::a_word_starting_in_the_unplayed_part_of_the_chunk_in_flight_is_not_heard(
        &ReferenceFactory,
        Profile::Exact,
    )
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_fully_played_answer_is_kept_whole() {
    suite::a_fully_played_answer_is_kept_whole(&ReferenceFactory, Profile::Exact)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_fully_played_answer_without_word_timestamps_is_not_estimated_away() {
    suite::a_fully_played_answer_without_word_timestamps_is_not_estimated_away(
        &ReferenceFactory,
        Profile::Exact,
    )
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_barge_in_is_counted_and_a_provider_without_timestamps_is_flagged_as_estimated() {
    suite::a_barge_in_is_counted_and_a_provider_without_timestamps_is_flagged_as_estimated(
        &ReferenceFactory,
        Profile::Exact,
    )
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn timed_barge_ins_are_counted_and_never_estimated() {
    suite::timed_barge_ins_are_counted_and_never_estimated(&ReferenceFactory, Profile::Exact)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_barge_in_while_the_llm_is_still_streaming_keeps_only_the_heard_part_of_what_was_sent() {
    suite::a_barge_in_while_the_llm_is_still_streaming_keeps_only_the_heard_part_of_what_was_sent(
        &ReferenceFactory,
        Profile::Exact,
    )
    .await
    .unwrap();
}
