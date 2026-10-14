//! Real-speech fixtures (FLEURS, see fixtures/NOTICE): loading, VAD accuracy against the exact
//! ground truth, and whole simulated calls driven by real speech.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)] // test helpers outside #[test] fns

use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_core::frame::{Frame, SystemFrame};
use brisvox_core::{
    Direction, EpochPolicy, Links, Pipeline, Processor, ProcessorContext, SharedClock, SimClock,
};
use brisvox_sim::factory::ReferenceFactory;
use brisvox_sim::fixture::{Fixture, fixtures_dir};
use brisvox_sim::suite::Profile;
use brisvox_sim::suite::fixtures as suite;
use brisvox_sim::vad::{EnergyVad, VadConfig};
use tokio_util::sync::CancellationToken;

const FIXTURES: [&str; 3] = ["en_two_utterances", "tr_two_utterances", "en_tr_mixed"];

fn load(name: &str) -> Fixture {
    Fixture::load(&fixtures_dir(), name).unwrap()
}

/// Records VAD events (start/stop, backdated `at` since the origin).
#[derive(Clone)]
struct VadLog {
    origin: tokio::time::Instant,
    events: Arc<Mutex<Vec<(bool, Duration)>>>,
}

impl Processor for VadLog {
    fn name(&self) -> &'static str {
        "vad_log"
    }

    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        match &frame {
            Frame::System(SystemFrame::UserStartedSpeaking { at }) => {
                self.events.lock().unwrap().push((true, *at - self.origin));
            }
            Frame::System(SystemFrame::UserStoppedSpeaking { at }) => {
                self.events.lock().unwrap().push((false, *at - self.origin));
            }
            _ => {}
        }
        ctx.forward(frame, dir).await
    }
}

/// The energy VAD's (start, stop) pairs for a fixture.
async fn vad_segments(fixture: &Fixture) -> Vec<(Duration, Duration)> {
    let clock: SharedClock = Arc::new(SimClock::new());
    let origin = clock.now();
    let log = VadLog {
        origin,
        events: Arc::default(),
    };
    let mut running = Pipeline::new("vad")
        .then(EnergyVad::new(VadConfig::default()))
        .then(log.clone())
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    fixture
        .play(
            clock.clone(),
            origin,
            running.input().clone(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    running.stop(Duration::from_secs(1)).await.unwrap();
    let events = log.events.lock().unwrap().clone();
    // Events alternate start, stop, start, ... (a stop may be missing at the very end).
    for (i, (started, _)) in events.iter().enumerate() {
        assert_eq!(*started, i % 2 == 0, "VAD events out of order: {events:?}");
    }
    events
        .chunks(2)
        .map(|pair| (pair[0].1, pair.get(1).map_or(fixture.duration(), |p| p.1)))
        .collect()
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap()
}

#[test]
fn committed_fixtures_load_and_cover_english_and_turkish() {
    let mut langs = Vec::new();
    for name in FIXTURES {
        let f = load(name);
        assert_eq!(f.segments.len(), 2, "{name}");
        langs.extend(f.segments.iter().map(|s| s.lang.clone()));
        assert!(f.segments.iter().all(|s| !s.text.is_empty()));
    }
    assert!(langs.iter().any(|l| l == "en") && langs.iter().any(|l| l == "tr"));
}

#[tokio::test(start_paused = true)]
async fn energy_vad_tracks_the_ground_truth_on_real_speech() {
    for name in FIXTURES {
        let fixture = load(name);
        let truth = fixture.speech_segments();
        let found = vad_segments(&fixture).await;
        let errors: Vec<(i64, i64)> = truth
            .iter()
            .zip(&found)
            .map(|(t, f)| (ms(f.0) - ms(t.0), ms(f.1) - ms(t.1)))
            .collect();
        println!("{name}: truth {truth:?}\n  vad {found:?}\n  errors (start, end) ms {errors:?}");
        assert_eq!(
            found.len(),
            truth.len(),
            "{name}: segments {found:?} vs {truth:?}"
        );
        for (start_err, end_err) in errors {
            // 20 ms frames: the VAD is frame-accurate (measured: within 10 ms on every fixture).
            assert!(start_err.abs() <= 20, "{name}: start error {start_err} ms");
            assert!(end_err.abs() <= 20, "{name}: end error {end_err} ms");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_real_speech_call_is_answered_after_each_utterance() {
    // The body lives in the suite so the production pipeline runs it too: every
    // committed fixture (English, Turkish, mixed).
    suite::all(&ReferenceFactory, Profile::Exact).await.unwrap();
}
