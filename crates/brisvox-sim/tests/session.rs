//! Whole simulated calls through the reference stages. The bodies live in
//! `brisvox_sim::suite::basic` (so the production pipeline runs them too); this file
//! runs them on the reference chain with exact timings (first audio at 1480 ms, interrupt at
//! 2420 ms, and so on).

use brisvox_sim::factory::ReferenceFactory;
use brisvox_sim::suite::Profile;
use brisvox_sim::suite::basic as suite;

#[tokio::test(start_paused = true)]
async fn one_full_turn_end_to_end() {
    suite::one_full_turn(&ReferenceFactory, Profile::Exact)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn barge_in_silences_the_bot_and_starts_a_new_turn() {
    suite::barge_in_silences_the_bot_and_starts_a_new_turn(&ReferenceFactory, Profile::Exact)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn llm_error_ends_the_turn_quietly_and_the_next_turn_is_not_a_barge_in() {
    suite::llm_error_ends_the_turn_quietly_and_the_next_turn_is_not_a_barge_in(
        &ReferenceFactory,
        Profile::Exact,
    )
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn barge_in_before_any_audio_drops_the_whole_response() {
    suite::barge_in_before_any_audio_drops_the_whole_response(&ReferenceFactory, Profile::Exact)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_pause_inside_the_turn_with_slow_stt_keeps_one_turn() {
    suite::a_pause_inside_the_turn_with_slow_stt_keeps_one_turn(&ReferenceFactory, Profile::Exact)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn stt_disconnect_ends_the_turn_and_the_next_one_reconnects() {
    suite::stt_disconnect_ends_the_turn_and_the_next_one_reconnects(
        &ReferenceFactory,
        Profile::Exact,
    )
    .await
    .unwrap();
}
