//! Simulator scenarios: whole calls through the reference stages, each run twice to
//! prove determinism, all checked with the assertion API and an independent oracle (the scripted
//! LLM text). The bodies live in `brisvox_sim::suite::scenarios` so the production pipeline runs
//! the very same scenarios; here they run against the reference chain with exact
//! timings.

use brisvox_sim::factory::ReferenceFactory;
use brisvox_sim::suite::Profile;
use brisvox_sim::suite::scenarios as suite;

#[tokio::test(start_paused = true)]
async fn s01_happy_path_with_realistic_latency() {
    suite::s01_happy_path_with_realistic_latency(&ReferenceFactory, Profile::Exact)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn s02_barge_in() {
    suite::s02_barge_in(&ReferenceFactory, Profile::Exact)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn s03_double_utterance_is_one_turn() {
    suite::s03_double_utterance_is_one_turn(&ReferenceFactory, Profile::Exact)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn s04_long_silence_stays_quiet() {
    suite::s04_long_silence_stays_quiet(&ReferenceFactory, Profile::Exact)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn s05_provider_stall_delays_but_completes() {
    suite::s05_provider_stall_delays_but_completes(&ReferenceFactory, Profile::Exact)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn s06_llm_error_then_recovery() {
    suite::s06_llm_error_then_recovery(&ReferenceFactory, Profile::Exact)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn s07_stt_disconnect_then_reconnect() {
    suite::s07_stt_disconnect_then_reconnect(&ReferenceFactory, Profile::Exact)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn s08_tool_call_with_text() {
    suite::s08_tool_call_with_text(&ReferenceFactory, Profile::Exact)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn s09_back_to_back_turns() {
    suite::s09_back_to_back_turns(&ReferenceFactory, Profile::Exact)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn s10_interrupt_during_tts_first_byte_wait() {
    suite::s10_interrupt_during_tts_first_byte_wait(&ReferenceFactory, Profile::Exact)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn s11_tts_disconnect_mid_answer_then_recovery() {
    suite::s11_tts_disconnect_mid_answer_then_recovery(&ReferenceFactory, Profile::Exact)
        .await
        .unwrap();
}
