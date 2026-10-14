//! Scenario driver, energy VAD and output recorder: exact timing under paused time.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)] // test helpers outside #[test] fns

use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_core::frame::{AudioFrame, Frame, SampleRate, SystemFrame};
use brisvox_core::{
    Direction, EpochPolicy, Links, Pipeline, Processor, ProcessorContext, SharedClock, SimClock,
    TimelineRecorder, TurnEpoch, TurnEvent,
};
use brisvox_sim::scenario::{OutputRecord, Scenario, SimTransportOut, play_input};
use brisvox_sim::vad::{EnergyVad, VadConfig};
use bytes::Bytes;
use tokio_util::sync::CancellationToken;

const MS: Duration = Duration::from_millis(1);

fn script() -> Scenario {
    Scenario::new()
        .silence(100 * MS)
        .speak("hello there", 400 * MS)
        .silence(400 * MS)
}

/// Records VAD events (name, backdated `at` relative to origin) and counts audio frames.
#[derive(Clone)]
struct Capture {
    origin: tokio::time::Instant,
    vad: Arc<Mutex<Vec<(&'static str, Duration)>>>,
    audio: Arc<Mutex<Vec<(Duration, u64)>>>,
}

impl Processor for Capture {
    fn name(&self) -> &'static str {
        "capture"
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
            Frame::System(SystemFrame::UserStartedSpeaking { at })
            | Frame::System(SystemFrame::UserStoppedSpeaking { at }) => {
                self.vad
                    .lock()
                    .unwrap()
                    .push((frame.name(), *at - self.origin));
            }
            Frame::Data(brisvox_core::DataFrame::Audio(a)) => {
                self.audio
                    .lock()
                    .unwrap()
                    .push((a.timestamp() - self.origin, a.seq()));
            }
            _ => {}
        }
        ctx.forward(frame, dir).await
    }
}

#[test]
fn script_ground_truth() {
    let s = script();
    assert_eq!(s.duration(), 900 * MS);
    assert_eq!(s.speech_segments(), [(100 * MS, 500 * MS)]);
    let utterances = s.utterances();
    assert_eq!(utterances.len(), 1);
    assert_eq!(utterances[0].finals, ["hello there"]);
}

#[tokio::test(start_paused = true)]
async fn input_frames_are_paced_and_vad_times_are_exact() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let origin = clock.now();
    let capture = Capture {
        origin,
        vad: Arc::default(),
        audio: Arc::default(),
    };
    let mut running = Pipeline::new("input")
        .then(EnergyVad::new(VadConfig::default()))
        .then(capture.clone())
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    play_input(
        &script(),
        clock.clone(),
        origin,
        SampleRate::Hz16000,
        running.input().clone(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(clock.now() - origin, 880 * MS, "last frame sent at 880 ms");
    running.stop(Duration::from_secs(1)).await.unwrap();

    let audio = capture.audio.lock().unwrap().clone();
    assert_eq!(audio.len(), 45);
    assert!(
        audio
            .iter()
            .enumerate()
            .all(|(i, (at, seq))| *at == 20 * MS * u32::try_from(i).unwrap() && *seq == i as u64)
    );
    // Start backdated to the first speech frame; stop to the end of the last speech frame.
    assert_eq!(
        capture.vad.lock().unwrap().clone(),
        [
            ("user_started_speaking", 100 * MS),
            ("user_stopped_speaking", 500 * MS),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn output_recorder_is_epoch_strict_and_marks_first_audio() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let origin = clock.now();
    let timeline = TimelineRecorder::new();
    let out = SimTransportOut::new(clock.clone(), origin, Some(timeline.clone()));
    let mut running = Pipeline::new("output")
        .then(out.clone())
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    let audio = |epoch: u32| -> Frame {
        AudioFrame::new(
            Bytes::from(vec![0u8; 640]),
            SampleRate::Hz16000,
            origin,
            0,
            TurnEpoch::new(epoch),
        )
        .unwrap()
        .into()
    };
    clock.sleep(30 * MS).await;
    running.input().send(audio(0)).await.unwrap();
    clock.sleep(30 * MS).await;
    running
        .input()
        .send(
            SystemFrame::Interrupt {
                new_epoch: TurnEpoch::new(1),
            }
            .into(),
        )
        .await
        .unwrap();
    tokio::task::yield_now().await;
    running.input().send(audio(0)).await.unwrap(); // stale: never reaches the transport
    running.input().send(audio(1)).await.unwrap();
    running.stop(Duration::from_secs(1)).await.unwrap();

    let kinds: Vec<(&str, Option<u32>, Duration)> = out
        .records()
        .iter()
        .map(|r: &OutputRecord| (r.kind, r.epoch, r.at))
        .collect();
    assert_eq!(
        kinds,
        [
            ("start", None, Duration::ZERO),
            ("audio", Some(0), 30 * MS),
            ("interrupt", Some(1), 60 * MS),
            ("audio", Some(1), 60 * MS),
            ("stop", None, 60 * MS),
        ]
    );
    assert_eq!(out.records()[1].audio, Some(20 * MS));
    let current = timeline.current().unwrap();
    assert_eq!(current.epoch(), TurnEpoch::new(1));
    assert_eq!(current.at(TurnEvent::FirstAudioOut), Some(origin + 60 * MS));
    let interrupted = timeline.history()[0];
    assert_eq!(
        interrupted.at(TurnEvent::FirstAudioOut),
        Some(origin + 30 * MS)
    );
    // The interrupt stopped turn 0's playout at 60 ms.
    assert_eq!(
        interrupted.at(TurnEvent::AudioOutStopped),
        Some(origin + 60 * MS)
    );
}

/// A PCM16 frame of `dur` starting at `origin + start`, square wave of `amplitude` (0 = silence).
fn tone_frame(
    origin: tokio::time::Instant,
    start: Duration,
    dur: Duration,
    amplitude: i16,
) -> Frame {
    let samples = usize::try_from(dur.as_millis()).unwrap() * 16;
    let mut pcm = Vec::with_capacity(samples * 2);
    for i in 0..samples {
        let v = if (i / 20) % 2 == 0 {
            amplitude
        } else {
            -amplitude
        };
        pcm.extend_from_slice(&v.to_le_bytes());
    }
    AudioFrame::new(
        Bytes::from(pcm),
        SampleRate::Hz16000,
        origin + start,
        0,
        TurnEpoch::ZERO,
    )
    .unwrap()
    .into()
}

/// Runs `frames` (start, duration, amplitude) through an EnergyVad; returns the VAD events.
async fn vad_events(frames: &[(u64, u64, i16)]) -> Vec<(&'static str, Duration)> {
    let clock: SharedClock = Arc::new(SimClock::new());
    let origin = clock.now();
    let capture = Capture {
        origin,
        vad: Arc::default(),
        audio: Arc::default(),
    };
    let mut running = Pipeline::new("vad")
        .then(EnergyVad::new(VadConfig::default()))
        .then(capture.clone())
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    for &(start, dur, amplitude) in frames {
        running
            .input()
            .send(tone_frame(
                origin,
                Duration::from_millis(start),
                Duration::from_millis(dur),
                amplitude,
            ))
            .await
            .unwrap();
    }
    running.stop(Duration::from_secs(1)).await.unwrap();
    capture.vad.lock().unwrap().clone()
}

/// `n` frames of `len` ms from `from` ms with `amplitude`.
fn run(from: u64, n: u64, len: u64, amplitude: i16) -> Vec<(u64, u64, i16)> {
    (0..n).map(|i| (from + i * len, len, amplitude)).collect()
}

#[tokio::test(start_paused = true)]
async fn vad_rejects_blips_and_bridges_short_pauses() {
    // 20 ms blip (< 40 ms start threshold), then silence: nothing.
    let mut frames = run(0, 1, 20, 3_000);
    frames.extend(run(20, 20, 20, 0));
    assert!(vad_events(&frames).await.is_empty());

    // Speech, a 200 ms pause (< 300 ms hangover), speech again, then a long silence: one
    // utterance from 0 to 700 ms.
    let mut frames = run(0, 10, 20, 3_000);
    frames.extend(run(200, 10, 20, 0));
    frames.extend(run(400, 15, 20, 3_000));
    frames.extend(run(700, 20, 20, 0));
    assert_eq!(
        vad_events(&frames).await,
        [
            ("user_started_speaking", Duration::ZERO),
            ("user_stopped_speaking", 700 * MS)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn vad_handles_two_utterances_and_other_frame_sizes() {
    // 10 ms frames: two utterances separated by 500 ms of silence.
    let mut frames = run(0, 30, 10, 3_000);
    frames.extend(run(300, 50, 10, 0));
    frames.extend(run(800, 20, 10, 3_000));
    frames.extend(run(1_000, 40, 10, 0));
    assert_eq!(
        vad_events(&frames).await,
        [
            ("user_started_speaking", Duration::ZERO),
            ("user_stopped_speaking", 300 * MS),
            ("user_started_speaking", 800 * MS),
            ("user_stopped_speaking", 1_000 * MS),
        ]
    );
    // 30 ms frames.
    let mut frames = run(0, 4, 30, 3_000);
    frames.extend(run(120, 12, 30, 0));
    assert_eq!(
        vad_events(&frames).await,
        [
            ("user_started_speaking", Duration::ZERO),
            ("user_stopped_speaking", 120 * MS)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn vad_threshold_boundary() {
    // A square wave's RMS equals its amplitude: 500 is speech, 499 is not.
    let mut at_threshold = run(0, 5, 20, 500);
    at_threshold.extend(run(100, 20, 20, 0));
    assert_eq!(vad_events(&at_threshold).await.len(), 2);
    let mut below = run(0, 5, 20, 499);
    below.extend(run(100, 20, 20, 0));
    assert!(vad_events(&below).await.is_empty());
}

/// A DropStale pass-through: drops older-epoch frames after an Interrupt.
struct Strict;

impl Processor for Strict {
    fn name(&self) -> &'static str {
        "strict"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        ctx.forward(frame, dir).await
    }
}

/// Sends 3 speech frames, an Interrupt to epoch 1, then 3 more speech frames (all epoch 0)
/// through `pipeline`; returns how many audio frames reached the capture.
async fn audio_through(
    pipeline: Pipeline,
    capture: &Capture,
    origin: tokio::time::Instant,
) -> usize {
    let mut running = pipeline
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    for i in 0..3 {
        running
            .input()
            .send(tone_frame(origin, i * 20 * MS, 20 * MS, 3_000))
            .await
            .unwrap();
    }
    tokio::task::yield_now().await;
    running
        .input()
        .send(
            SystemFrame::Interrupt {
                new_epoch: TurnEpoch::new(1),
            }
            .into(),
        )
        .await
        .unwrap();
    tokio::task::yield_now().await;
    for i in 3..6 {
        running
            .input()
            .send(tone_frame(origin, i * 20 * MS, 20 * MS, 3_000))
            .await
            .unwrap();
    }
    running.stop(Duration::from_secs(1)).await.unwrap();
    capture.audio.lock().unwrap().len()
}

#[tokio::test(start_paused = true)]
async fn the_input_side_never_loses_user_audio_to_an_interrupt() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let origin = clock.now();
    let keep = Capture {
        origin,
        vad: Arc::default(),
        audio: Arc::default(),
    };
    let through_vad = Pipeline::new("input")
        .then(EnergyVad::new(VadConfig::default()))
        .then(keep.clone());
    assert_eq!(
        audio_through(through_vad, &keep, origin).await,
        6,
        "VAD (KeepAll) lost audio"
    );

    // Contrast: a DropStale stage in the same position drops the epoch-0 audio after it.
    let strict = Capture {
        origin,
        vad: Arc::default(),
        audio: Arc::default(),
    };
    let through_strict = Pipeline::new("input").then(Strict).then(strict.clone());
    assert!(audio_through(through_strict, &strict, origin).await < 6);
}

#[tokio::test(start_paused = true)]
async fn record_limit_and_zero_length_steps() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let origin = clock.now();
    let out = SimTransportOut::new(clock.clone(), origin, None).with_record_limit(3);
    let mut running = Pipeline::new("output")
        .then(out.clone())
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    for i in 0..5 {
        running
            .input()
            .send(tone_frame(origin, i * 20 * MS, 20 * MS, 0))
            .await
            .unwrap();
    }
    running.stop(Duration::from_secs(1)).await.unwrap();
    assert_eq!(out.records().len(), 3);
    assert_eq!(
        out.dropped_records(),
        4,
        "3 audio frames and stop beyond the limit"
    );

    let s = Scenario::new()
        .speak("too short", 10 * MS)
        .silence(20 * MS)
        .with_tail(300 * MS);
    assert!(s.utterances().is_empty());
    assert!(s.speech_segments().is_empty());
    assert_eq!(s.duration(), 320 * MS);
}
