//! The fixture benchmark end to end: the mixed English/Turkish recording played as concurrent
//! simulated calls in real time (~15 s).

use std::time::Duration;

use brisvox_bench::fixture_load::{self, FixtureLoadConfig};

#[test]
fn the_mixed_fixture_is_answered_in_both_languages_without_barge_ins() {
    let mut config = FixtureLoadConfig::new();
    config.fixtures = vec!["en_tr_mixed".to_owned()];
    config.sessions = 2;
    config.ramp = Duration::from_millis(100);
    config.gap = Duration::from_millis(3_500);
    config.worker_threads = Some(2);
    let reports = fixture_load::run(&config).unwrap();
    assert_eq!(reports.len(), 1);
    let r = &reports[0];
    println!("{r:#?}");
    assert_eq!(r.name, "en_tr_mixed");
    assert_eq!(r.languages, ["en", "tr"]);
    assert_eq!(r.turns_expected, 4);
    assert_eq!(r.turns_answered, 4);
    assert_eq!(r.interrupts, 0);
    assert_eq!(r.failed_sessions, 0);
    // Every call's conversation is exactly: English transcript, English reply, Turkish
    // transcript, Turkish reply, i.e. the mock STT delivered the fixture transcripts to the LLM
    // and each answer matches the language of the utterance it answers.
    assert_eq!(r.conversation_mismatches, 0);
    assert!(r.problems().is_empty(), "{:?}", r.problems());
    assert_eq!(r.response.count, 4);
    // Sanity floor derived from the configured LLM and TTS latency medians; the STT final and
    // the endpoint wait come on top.
    assert!(
        r.response.p50 > fixture_load::response_floor(),
        "{:?} vs floor {:?}",
        r.response,
        fixture_load::response_floor()
    );
    // One row per utterance language, two turns each (2 calls).
    let langs: Vec<&str> = r
        .response_by_language
        .iter()
        .map(|(l, _)| l.as_str())
        .collect();
    assert_eq!(langs, ["en", "tr"]);
    assert!(r.response_by_language.iter().all(|(_, s)| s.count == 2));
    let markdown = fixture_load::markdown(&reports, &["env".to_owned()]);
    assert!(
        markdown.contains("| en_tr_mixed | en, tr | 2 | 4/4 | 0 | 0 |"),
        "{markdown}"
    );
    assert!(markdown.contains("| en_tr_mixed | tr | 2 |"), "{markdown}");
}
