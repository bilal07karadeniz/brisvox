//! The load harness end to end, small and in real time (~10 s).

use std::time::Duration;

use brisvox_bench::sim_load::{self, LoadConfig};

const MS: Duration = Duration::from_millis(1);

#[test]
fn a_small_load_run_answers_every_turn_and_leaks_nothing() {
    let mut config = LoadConfig::new(4);
    config.turns = 2;
    config.utterance = 600 * MS;
    config.gap = 3_500 * MS;
    config.ramp = 200 * MS;
    config.linger = 1_000 * MS;
    config.worker_threads = Some(2);
    let report = sim_load::run(&config).unwrap();
    println!("{report:#?}");
    assert_eq!(report.worker_threads, 2);
    assert_eq!(report.failed_sessions, 0);
    assert_eq!(report.interrupts, 0);
    assert_eq!(report.turns_expected, 8);
    assert_eq!(report.turns_answered, 8);
    assert_eq!(report.overhead.count, 8);
    assert_eq!(report.response.count, 8);
    // Providers dominate: the mocks' medians add up to ~500 ms per turn.
    assert!(report.providers.p50 > 200 * MS, "{:?}", report.providers);
    assert!(report.response.p50 > report.providers.p50);
    assert!(report.problems().is_empty(), "{:?}", report.problems());
    assert!(report.wake.count > 0);
    assert!(report.cpu_cores_plateau.is_some());
    // Every pipeline task ended with its call.
    assert!(report.tasks_peak > report.tasks_before);
    assert!(report.tasks_after <= report.tasks_before, "leaked tasks");
    assert!(report.rss_peak > 0);
    let markdown = sim_load::markdown(std::slice::from_ref(&report), &["env".to_owned()]);
    assert!(markdown.contains("| 4 | 8/8 | 0 | 0 | 8 |"), "{markdown}");
}
