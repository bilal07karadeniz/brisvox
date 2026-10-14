//! The sim seam: everything another crate needs to run the suite against its own
//! pipeline is public and works from outside `brisvox-sim`. This file is such a crate: it
//! implements `SessionFactory` and `InputTarget`, builds sessions, runs and reads them.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)] // test helpers outside #[test] fns

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_core::frame::{AudioFrame, Frame, SampleRate, SystemFrame};
use brisvox_core::{
    Direction, FrameSender, Inbox, QueueConfig, SharedClock, SimClock, TimelineRecorder, TurnEpoch,
};
use brisvox_sim::factory::{BuiltSession, ReferenceFactory, SessionFactory, SessionParts};
use brisvox_sim::fixture::{Fixture, fixtures_dir};
use brisvox_sim::input::{InputSink, InputTarget, SendFuture};
use brisvox_sim::leak::TaskCounter;
use brisvox_sim::mock_llm::{LlmScript, MockLlm};
use brisvox_sim::mock_stt::MockStt;
use brisvox_sim::mock_tts::MockTts;
use brisvox_sim::output::{OutputLog, OutputRecord, WireKind};
use brisvox_sim::reference::{PlayLog, SessionProbes, WordLog};
use brisvox_sim::scenario::{Scenario, play_input};
use brisvox_sim::session::{CallerInput, SessionRun, SimSession};
use brisvox_sim::suite::{self, Profile};
use tokio_util::sync::CancellationToken;

const MS: Duration = Duration::from_millis(1);

fn clock() -> SharedClock {
    Arc::new(SimClock::new())
}

/// An input target that counts frames and forwards them to a wrapped sink.
struct Counting {
    inner: InputSink,
    frames: AtomicUsize,
    binds: AtomicUsize,
    upstream_binds: AtomicUsize,
    /// Order of `bind` (1) and `bind_upstream` (2) calls, to check the runner's sequence.
    order: Mutex<Vec<u8>>,
    unbinds: AtomicUsize,
}

impl InputTarget for Counting {
    fn send(&self, frame: AudioFrame) -> SendFuture<'_> {
        self.frames.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move { self.inner.send(frame).await })
    }

    fn bind(&self, input: &FrameSender) {
        self.binds.fetch_add(1, Ordering::Relaxed);
        self.order.lock().unwrap().push(1);
        self.inner.bind(input.clone());
    }

    fn bind_upstream(&self, upstream: &FrameSender) {
        self.upstream_binds.fetch_add(1, Ordering::Relaxed);
        self.order.lock().unwrap().push(2);
        self.inner.bind_upstream(upstream.clone());
    }

    fn unbind(&self) {
        self.unbinds.fetch_add(1, Ordering::Relaxed);
        self.inner.unbind();
    }
}

/// A factory defined outside the crate: the reference chain behind a counting input target and
/// with one wire record per call.
#[derive(Clone)]
struct Tapped {
    target: Arc<Mutex<Vec<Arc<Counting>>>>,
}

impl SessionFactory for Tapped {
    async fn build(&self, parts: SessionParts) -> brisvox_sim::Result<BuiltSession> {
        let (clock, origin) = (parts.clock.clone(), parts.origin);
        let built = ReferenceFactory.build(parts).await?;
        let counting = Arc::new(Counting {
            inner: built.input.clone(),
            frames: AtomicUsize::new(0),
            binds: AtomicUsize::new(0),
            upstream_binds: AtomicUsize::new(0),
            order: Mutex::default(),
            unbinds: AtomicUsize::new(0),
        });
        self.target.lock().unwrap().push(Arc::clone(&counting));
        built
            .output
            .record_wire(WireKind::Hangup, clock.now(), None, None);
        assert_eq!(built.output.origin(), origin);
        let guard = Arc::clone(&counting);
        Ok(BuiltSession::new(
            built.pipeline,
            built.probes,
            InputSink::target(counting),
            built.output,
        )
        .with_guard(guard))
    }
}

#[tokio::test(start_paused = true)]
async fn a_factory_from_another_crate_runs_the_whole_suite() {
    let factory = Tapped {
        target: Arc::default(),
    };
    suite::scenarios::all(&factory, Profile::Exact)
        .await
        .unwrap();
    suite::basic::all(&factory, Profile::Exact).await.unwrap();
    let targets = factory.target.lock().unwrap();
    assert!(targets.len() >= 23, "{} sessions", targets.len());
    for counting in targets.iter() {
        assert!(counting.frames.load(Ordering::Relaxed) > 40);
        assert_eq!(counting.binds.load(Ordering::Relaxed), 1);
        // The runner hands a target the started pipeline's upstream input too, once, right
        // after the input (a target that owns a transport pump needs it for echoes).
        assert_eq!(counting.upstream_binds.load(Ordering::Relaxed), 1);
        assert_eq!(*counting.order.lock().unwrap(), [1, 2]);
        assert_eq!(counting.unbinds.load(Ordering::Relaxed), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn a_target_gets_the_upstream_input_and_other_sinks_ignore_it() {
    struct Upstream(Mutex<Option<FrameSender>>);
    impl InputTarget for Upstream {
        fn send(&self, _frame: AudioFrame) -> SendFuture<'_> {
            Box::pin(async { true })
        }

        fn bind_upstream(&self, upstream: &FrameSender) {
            *self.0.lock().unwrap() = Some(upstream.clone());
        }
    }
    let mut inbox = Inbox::new(QueueConfig::DEFAULT);
    let upstream = inbox.sender(Direction::Upstream);
    let target = Arc::new(Upstream(Mutex::new(None)));
    let sink = InputSink::target(target.clone());
    // The default of `InputTarget::bind_upstream` is a no-op; this target overrides it.
    sink.bind_upstream(upstream.clone());
    let got = target
        .0
        .lock()
        .unwrap()
        .clone()
        .expect("the target got the sender");
    got.send(Frame::System(SystemFrame::Start)).await.unwrap();
    assert!(
        inbox.try_recv().is_ok(),
        "it is the inbox's own upstream lane"
    );
    // A pipeline sink has no use for it; the call neither panics nor binds anything.
    let pipeline = InputSink::pipeline();
    pipeline.bind_upstream(upstream);
    assert!(!pipeline.is_bound());
}

#[tokio::test(start_paused = true)]
async fn the_same_call_through_a_wrapping_factory_is_byte_identical_to_the_reference() {
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("tell me a story", 900 * MS)
        .silence(1_800 * MS)
        .speak("okay stop", 600 * MS)
        .with_tail(500 * MS);
    let run = |factory: Option<Tapped>| {
        let scenario = scenario.clone();
        async move {
            let session = SimSession::new(clock());
            session
                .llm
                .push(LlmScript::text(["A very long story that goes on and on."]));
            session.llm.push(LlmScript::text(["Sure."]));
            match factory {
                None => session.run(&scenario, 2_000 * MS).await.unwrap(),
                Some(f) => session.run_with(&f, &scenario, 2_000 * MS).await.unwrap(),
            }
        }
    };
    let plain = run(None).await;
    let wrapped = run(Some(Tapped {
        target: Arc::default(),
    }))
    .await;
    assert_eq!(plain.fingerprint(), wrapped.fingerprint());
    assert_eq!(plain.output, wrapped.output);
    assert!(plain.wire.is_empty(), "the reference has no wire");
    assert_eq!(wrapped.wire.len(), 1);
    assert_eq!(wrapped.wire[0].kind, WireKind::Hangup);
}

#[tokio::test(start_paused = true)]
async fn the_budget_profile_accepts_the_reference_pipeline() {
    // The Budget ranges must contain the reference's exact values, or they could not be a
    // sensible budget for anything slower.
    let factory = ReferenceFactory;
    suite::basic::all(&factory, Profile::Budget).await.unwrap();
    suite::scenarios::all(&factory, Profile::Budget)
        .await
        .unwrap();
    suite::played_text::all(&factory, Profile::Budget)
        .await
        .unwrap();
    suite::fixtures::a_real_speech_call_is_answered_after_each_utterance(&factory, Profile::Budget)
        .await
        .unwrap();
    suite::chaos::a_hundred_chaotic_calls_stay_isolated_and_leave_nothing_behind(
        &factory,
        Profile::Budget,
        None,
    )
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_budget_profile_rejects_what_the_exact_profile_pins() {
    let ms = Duration::from_millis;
    // Exact is equality, Budget a range, and a miss names the check.
    assert!(
        Profile::Exact
            .duration("x", ms(1480), ms(1480), ms(1000)..=ms(2000))
            .is_ok()
    );
    assert!(
        Profile::Exact
            .duration("x", ms(1481), ms(1480), ms(1000)..=ms(2000))
            .is_err()
    );
    assert!(
        Profile::Budget
            .duration("x", ms(1481), ms(1480), ms(1000)..=ms(2000))
            .is_ok()
    );
    let miss = Profile::Budget
        .duration("first audio", ms(2001), ms(1480), ms(1000)..=ms(2000))
        .unwrap_err();
    assert!(miss.to_string().contains("first audio"), "{miss}");
    assert!(
        Profile::Budget
            .at_least("y", ms(300), ms(580), ms(300))
            .is_ok()
    );
    assert!(
        Profile::Exact
            .at_least("y", ms(300), ms(580), ms(300))
            .is_err()
    );
    // Heard text: Budget allows one word either way, only on a word prefix of the answer.
    let full = "one two three four";
    assert!(
        Profile::Budget
            .heard_text("t", "one two", "one two three", full)
            .is_ok()
    );
    assert!(
        Profile::Budget
            .heard_text("t", "one", "one two three", full)
            .is_err()
    );
    assert!(
        Profile::Budget
            .heard_text("t", "one three", "one three", full)
            .is_err()
    );
    assert!(
        Profile::Exact
            .heard_text("t", "one two", "one two three", full)
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn pipeline_sourced_interrupts_are_not_client_clears() {
    let origin = clock().now();
    let log = OutputLog::new(origin, Some(TimelineRecorder::new()));
    let at = origin + 500 * MS;
    // The pipeline sent one Interrupt; the client saw two clears (an epoch bump is not always a
    // barge-in) and audio that arrived later than it left the pipeline.
    let pcm = bytes::Bytes::from(vec![0u8; 640]);
    let audio = AudioFrame::new(pcm, SampleRate::Hz16000, at, 0, TurnEpoch::ZERO).unwrap();
    log.record_frame(&Frame::from(audio), at);
    log.record_frame(
        &Frame::System(SystemFrame::Interrupt {
            new_epoch: TurnEpoch::new(1),
        }),
        at + 100 * MS,
    );
    log.record_wire(
        WireKind::Audio,
        at + 30 * MS,
        Some(TurnEpoch::ZERO),
        Some(20 * MS),
    );
    log.record_wire(
        WireKind::Clear,
        at + 130 * MS,
        Some(TurnEpoch::new(1)),
        None,
    );
    log.record_wire(
        WireKind::Clear,
        at + 300 * MS,
        Some(TurnEpoch::new(2)),
        None,
    );
    assert_eq!(log.interrupts(), 1);
    assert_eq!(log.wire_clears(), 2);
    let records = log.records();
    assert_eq!(
        records,
        [
            OutputRecord::new(500 * MS, "audio", Some(0), Some(20 * MS)),
            OutputRecord::new(600 * MS, "interrupt", Some(1), None),
        ]
    );
    let wire = log.wire_records();
    assert_eq!(wire.len(), 3);
    // Wire arrival is later than the pipeline's hand-over of the same audio.
    assert_eq!(wire[0].at, 530 * MS);
    assert!(wire[0].at > records[0].at);
    assert_eq!(wire[0].kind.name(), "audio");
}

#[tokio::test(start_paused = true)]
async fn record_limits_count_what_they_drop() {
    let origin = clock().now();
    let log = OutputLog::new(origin, None).with_record_limit(2);
    for i in 0..5u64 {
        let at = origin + MS * u32::try_from(i).unwrap();
        log.record_frame(&Frame::System(SystemFrame::Stop), at);
        log.record_wire(WireKind::Mark, at, None, None);
    }
    assert_eq!(log.records().len(), 2);
    assert_eq!(log.dropped_records(), 3);
    assert_eq!(log.wire_records().len(), 2);
    assert_eq!(log.dropped_wire_records(), 3);
}

#[tokio::test(start_paused = true)]
async fn the_tap_records_what_it_forwards_into_its_log() {
    use brisvox_core::{Links, Pipeline};
    let clock = clock();
    let origin = clock.now();
    let log = OutputLog::new(origin, None);
    let mut running = Pipeline::new("tap")
        .then(log.tap(clock.clone()))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    let scenario = Scenario::new().silence(100 * MS);
    play_input(
        &scenario,
        clock.clone(),
        origin,
        SampleRate::Hz16000,
        running.input().clone(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    running.stop(Duration::from_secs(1)).await.unwrap();
    let kinds: Vec<_> = log.records().iter().map(|r| r.kind).collect();
    assert_eq!(kinds.iter().filter(|k| **k == "audio").count(), 5);
}

#[tokio::test(start_paused = true)]
async fn an_unbound_pipeline_sink_refuses_frames_and_binding_it_works() {
    let sink = InputSink::pipeline();
    assert!(!sink.is_bound());
    let pcm = bytes::Bytes::from(vec![0u8; 640]);
    let frame = || {
        AudioFrame::new(
            pcm.clone(),
            SampleRate::Hz16000,
            clock().now(),
            0,
            TurnEpoch::ZERO,
        )
        .unwrap()
    };
    assert!(!sink.send(frame()).await);
    // play_input on an unbound sink ends quietly, like a gone pipeline.
    let clock = clock();
    play_input(
        &Scenario::new().silence(100 * MS),
        clock.clone(),
        clock.now(),
        SampleRate::Hz16000,
        sink.clone(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(format!("{sink:?}").contains("pipeline"));
}

#[tokio::test(start_paused = true)]
async fn fixtures_play_into_any_input_target() {
    struct Count(AtomicUsize);
    impl InputTarget for Count {
        fn send(&self, _frame: AudioFrame) -> SendFuture<'_> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { true })
        }
    }
    let fixture = Fixture::load(&fixtures_dir(), "en_two_utterances").unwrap();
    let clock = clock();
    let count = Arc::new(Count(AtomicUsize::new(0)));
    fixture
        .play(
            clock.clone(),
            clock.now(),
            InputSink::target(count.clone()),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let expected = fixture.duration().as_millis() / 20;
    assert_eq!(
        u128::try_from(count.0.load(Ordering::Relaxed)).unwrap(),
        expected + u128::from(!fixture.duration().as_millis().is_multiple_of(20))
    );
}

#[tokio::test(start_paused = true)]
async fn the_seam_types_are_constructible_from_outside() {
    let clock = clock();
    let counter = TaskCounter::start().unwrap();
    let (stt, llm, tts) = (
        MockStt::new(clock.clone()),
        MockLlm::new(clock.clone()),
        MockTts::new(clock.clone()),
    );
    let _ = (stt, llm, tts);
    let probes = SessionProbes::new(
        Arc::default(),
        WordLog::default(),
        PlayLog::default(),
        Arc::default(),
    );
    let run = SessionRun::new(
        clock.now(),
        vec![OutputRecord::new(MS, "audio", Some(0), Some(MS))],
        Vec::new(),
        probes,
        Ok(()),
        0,
    );
    assert!(run.wire.is_empty());
    assert_eq!(run.audio_of(TurnEpoch::ZERO), MS);
    let scenario = Scenario::new();
    assert!(matches!(
        CallerInput::from(&scenario),
        CallerInput::Scenario(_)
    ));
    counter.wait_for_baseline(&*clock, MS).await.unwrap();
}

/// What a guarded factory saw, in order.
type Events = Arc<Mutex<Vec<&'static str>>>;

/// A guard that writes a wire record when it is dropped (as a reader task that is aborted might
/// have lost one) and notes the drop.
struct LateGuard {
    events: Events,
    output: OutputLog,
    clock: SharedClock,
}

impl Drop for LateGuard {
    fn drop(&mut self) {
        self.events.lock().unwrap().push("guard dropped");
        self.output
            .record_wire(WireKind::Clear, self.clock.now(), None, None);
    }
}

/// The reference chain with a [`LateGuard`] and a drain hook that records the last wire event.
#[derive(Clone)]
struct Guarded {
    events: Events,
}

impl SessionFactory for Guarded {
    async fn build(&self, parts: SessionParts) -> brisvox_sim::Result<BuiltSession> {
        let clock = parts.clock.clone();
        let built = ReferenceFactory.build(parts).await?;
        let (events, output) = (Arc::clone(&self.events), built.output.clone());
        let guard = LateGuard {
            events: Arc::clone(&events),
            output: output.clone(),
            clock: clock.clone(),
        };
        let drain_clock = clock.clone();
        Ok(
            BuiltSession::new(built.pipeline, built.probes, built.input, built.output)
                .with_guard(guard)
                .with_drain(move || async move {
                    events.lock().unwrap().push("drain");
                    // The reader sees the hang-up a little after the pipeline stopped.
                    drain_clock.sleep(20 * MS).await;
                    output.record_wire(WireKind::Hangup, drain_clock.now(), None, None);
                }),
        )
    }
}

#[tokio::test(start_paused = true)]
async fn the_guard_outlives_the_drain_and_the_log_read_and_is_dropped_last() {
    let events = Events::default();
    let session = SimSession::new(clock());
    session.llm.push(LlmScript::text(["Hi."]));
    let run = session
        .run_with(
            &Guarded {
                events: Arc::clone(&events),
            },
            &Scenario::new()
                .silence(100 * MS)
                .speak("hello", 400 * MS)
                .with_tail(300 * MS),
            1_500 * MS,
        )
        .await
        .unwrap();
    assert_eq!(*events.lock().unwrap(), ["drain", "guard dropped"]);
    // The event the drain waited for is in the run; the one the guard wrote on its drop, after
    // the logs were read, is not.
    let kinds: Vec<_> = run.wire.iter().map(|w| w.kind).collect();
    assert_eq!(kinds, [WireKind::Hangup]);
}

#[tokio::test(start_paused = true)]
async fn dropping_the_run_future_midway_drops_the_guard() {
    let events = Events::default();
    let session = SimSession::new(clock());
    session.llm.push(LlmScript::text(["Hi."]));
    let factory = Guarded {
        events: Arc::clone(&events),
    };
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("hello", 400 * MS)
        .with_tail(300 * MS);
    let timed = tokio::time::timeout(
        Duration::from_millis(300),
        session.run_with(&factory, &scenario, 1_500 * MS),
    )
    .await;
    assert!(timed.is_err(), "the run should still have been going");
    // The hang-up path: no drain ran, the guard is gone with the future.
    assert_eq!(*events.lock().unwrap(), ["guard dropped"]);
}

#[tokio::test(start_paused = true)]
async fn a_stuck_input_target_fails_the_run_instead_of_hanging_it() {
    struct Stuck;
    impl InputTarget for Stuck {
        fn send(&self, _frame: AudioFrame) -> SendFuture<'_> {
            Box::pin(std::future::pending())
        }
    }
    let clock = clock();
    let started = clock.now();
    let err = play_input(
        &Scenario::new().silence(200 * MS),
        clock.clone(),
        started,
        SampleRate::Hz16000,
        InputSink::target(Arc::new(Stuck)),
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, brisvox_sim::Error::Assertion(ref m) if m.contains("stuck")),
        "{err:?}"
    );
    assert_eq!(
        clock.now() - started,
        brisvox_sim::input::SEND_TIMEOUT,
        "gave up exactly at the deadline"
    );
    // The same for a fixture; and cancelling still ends the script quietly.
    let fixture = Fixture::load(&fixtures_dir(), "en_two_utterances").unwrap();
    let err = fixture
        .play(
            clock.clone(),
            clock.now(),
            InputSink::target(Arc::new(Stuck)),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, brisvox_sim::Error::Assertion(_)), "{err:?}");
    let cancel = CancellationToken::new();
    cancel.cancel();
    fixture
        .play(
            clock.clone(),
            clock.now(),
            InputSink::target(Arc::new(Stuck)),
            &cancel,
        )
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_log_without_a_timeline_leaves_the_marks_to_the_real_transport() {
    use brisvox_core::{Links, Pipeline, TurnEvent};
    for with_timeline in [false, true] {
        let clock = clock();
        let origin = clock.now();
        let timeline = TimelineRecorder::new();
        let log = OutputLog::new(origin, with_timeline.then(|| timeline.clone()));
        let mut running = Pipeline::new("tap")
            .then(log.tap(clock.clone()))
            .start(Links::default(), &CancellationToken::new())
            .await
            .unwrap();
        play_input(
            &Scenario::new().silence(100 * MS),
            clock.clone(),
            origin,
            SampleRate::Hz16000,
            running.input().clone(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        running.stop(Duration::from_secs(1)).await.unwrap();
        timeline.finish();
        let marked = timeline
            .history()
            .iter()
            .any(|t| t.at(TurnEvent::FirstAudioOut).is_some());
        assert_eq!(marked, with_timeline);
    }
}
