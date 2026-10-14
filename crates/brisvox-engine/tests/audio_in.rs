//! `AudioIn`: conversion to 16 kHz, pass-through, forwarding of everything else.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::time::Duration;

use brisvox_core::frame::{
    ControlFrame, ControlKind, DataFrame, DtmfDigit, DtmfFrame, Frame, SampleRate, SystemFrame,
};
use brisvox_core::{Clock, Direction, Links, Pipeline, RunningPipeline, SimClock, TurnEpoch};
use brisvox_engine::{AudioIn, AudioInStats, CallerAudioTap, EngineConfig};
use common::{MS, Recorded, Recorder, rms, samples, sine_frame, square_frame};
use tokio_util::sync::CancellationToken;

struct Chain {
    running: RunningPipeline,
    rec: Recorded,
    stats: AudioInStats,
}

async fn start() -> Chain {
    let config = EngineConfig::DEFAULT;
    let audio_in = AudioIn::new(&config);
    let stats = audio_in.stats();
    let rec = Recorded::default();
    let running = Pipeline::new("input")
        .then_with_queue(audio_in, config.queues.input)
        .then(Recorder(rec.clone()))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    Chain {
        running,
        rec,
        stats,
    }
}

/// Zero crossings of `pcm`.
fn crossings(pcm: &[i16]) -> usize {
    pcm.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count()
}

#[tokio::test(start_paused = true)]
async fn frames_at_the_model_rate_pass_through_without_a_copy() {
    let clock = SimClock::new();
    let origin = clock.now();
    let chain = start().await;
    let frame = square_frame(origin, 0, SampleRate::Hz16000, 1_000);
    let ptr = frame.data().as_ptr();
    chain
        .running
        .input()
        .send(frame.clone().into())
        .await
        .unwrap();
    clock.sleep(20 * MS).await;
    let audio = chain.rec.audio();
    assert_eq!(audio, [frame]);
    assert_eq!(
        audio[0].data().as_ptr(),
        ptr,
        "the received buffer, not a copy"
    );
    let stats = chain.stats.snapshot();
    assert_eq!(
        (stats.frames_in, stats.passthrough, stats.resampled),
        (1, 1, 0)
    );
    assert_eq!(stats.resampler_builds, 0);
}

#[tokio::test(start_paused = true)]
async fn everything_that_is_not_audio_is_forwarded_unchanged_and_in_order() {
    let clock = SimClock::new();
    let origin = clock.now();
    let chain = start().await;
    let dtmf: Frame = DataFrame::Dtmf(DtmfFrame {
        epoch: TurnEpoch::ZERO,
        digit: DtmfDigit::D5,
        at: origin,
    })
    .into();
    let flush: Frame = ControlFrame {
        epoch: TurnEpoch::new(3),
        kind: ControlKind::Flush,
    }
    .into();
    let audio: Frame = square_frame(origin, 0, SampleRate::Hz16000, 100).into();
    let input = chain.running.input();
    for frame in [dtmf.clone(), audio.clone(), flush.clone()] {
        input.send(frame).await.unwrap();
    }
    clock.sleep(20 * MS).await;
    let seen: Vec<Frame> = chain
        .rec
        .all()
        .into_iter()
        .filter(|(f, _)| !matches!(f, Frame::System(SystemFrame::Start)))
        .map(|(f, d)| {
            assert_eq!(d, Direction::Downstream);
            f
        })
        .collect();
    assert_eq!(seen, [dtmf, audio, flush]);
    // The Start frame went through too (and started no worker: no VAD configured).
    assert!(
        chain
            .rec
            .system()
            .iter()
            .any(|s| matches!(s, SystemFrame::Start))
    );
}

async fn convert(rate: SampleRate, frames: u64) -> Chain {
    let clock = SimClock::new();
    let origin = clock.now();
    let chain = start().await;
    for i in 0..frames {
        let frame = sine_frame(origin, i, rate, 8_000.0);
        chain.running.input().send(frame.into()).await.unwrap();
        clock.sleep(20 * MS).await;
    }
    chain
}

#[tokio::test(start_paused = true)]
async fn call_audio_at_other_rates_arrives_at_16_khz_with_its_tone_intact() {
    for rate in [
        SampleRate::Hz8000,
        SampleRate::Hz22050,
        SampleRate::Hz24000,
        SampleRate::Hz44100,
        SampleRate::Hz48000,
    ] {
        let chain = convert(rate, 50).await;
        let audio = chain.rec.audio();
        assert_eq!(
            audio.len(),
            50,
            "{rate:?}: one output frame per input frame"
        );
        for (i, frame) in audio.iter().enumerate() {
            assert_eq!(frame.sample_rate(), SampleRate::Hz16000, "{rate:?}");
            assert_eq!(frame.sample_count(), 320, "{rate:?}: 20 ms at 16 kHz");
            assert_eq!(
                frame.seq(),
                i as u64,
                "{rate:?}: sequence numbers carry over"
            );
            assert_eq!(frame.epoch(), TurnEpoch::ZERO);
        }
        // Past the filter's ramp-in the tone is a 440 Hz sine of the input level: 17.6
        // crossings per 20 ms, RMS 8000 / sqrt(2).
        for frame in &audio[5..] {
            let pcm = samples(frame.data());
            let level = rms(&pcm);
            assert!(
                (level - 5_657.0).abs() < 400.0,
                "{rate:?}: rms {level} of a converted frame"
            );
            assert!(
                (16..=19).contains(&crossings(&pcm)),
                "{rate:?}: {} zero crossings",
                crossings(&pcm)
            );
        }
        let stats = chain.stats.snapshot();
        assert_eq!((stats.resampled, stats.passthrough), (50, 0), "{rate:?}");
        assert_eq!(stats.resampler_builds, 1, "{rate:?}");
        // Timestamps are the input's.
        assert_eq!(
            audio[3].timestamp() - audio[0].timestamp(),
            Duration::from_millis(60)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn input_frames_that_do_not_align_with_the_resampler_block_lose_nothing() {
    let clock = SimClock::new();
    let origin = clock.now();
    let chain = start().await;
    // 10 ms frames at 8 kHz: 80 samples, half a 160-sample block. Every other frame produces
    // nothing yet; none is lost overall.
    for i in 0..100u64 {
        let full = sine_frame(origin, i / 2, SampleRate::Hz8000, 8_000.0);
        let half = i as usize % 2;
        let bytes = full.data().slice(half * 160..half * 160 + 160);
        let frame = brisvox_core::frame::AudioFrame::new(
            bytes,
            SampleRate::Hz8000,
            origin + 10 * MS * u32::try_from(i).unwrap(),
            i,
            TurnEpoch::ZERO,
        )
        .unwrap();
        chain.running.input().send(frame.into()).await.unwrap();
        clock.sleep(10 * MS).await;
    }
    let total: usize = chain.rec.audio().iter().map(|f| f.sample_count()).sum();
    assert_eq!(total, 16_000, "1 s in, 1 s out");
    let stats = chain.stats.snapshot();
    assert_eq!(stats.empty_outputs, 50);
    assert_eq!(stats.resampled, 50);
}

#[tokio::test(start_paused = true)]
async fn a_rate_change_builds_a_new_resampler_and_a_gap_is_counted() {
    let clock = SimClock::new();
    let origin = clock.now();
    let chain = start().await;
    let send = |frame: brisvox_core::frame::AudioFrame| {
        let input = chain.running.input().clone();
        async move { input.send(frame.into()).await.unwrap() }
    };
    send(sine_frame(origin, 0, SampleRate::Hz8000, 8_000.0)).await;
    send(sine_frame(origin, 1, SampleRate::Hz8000, 8_000.0)).await;
    send(sine_frame(origin, 2, SampleRate::Hz16000, 8_000.0)).await;
    // Frames 3..5 were lost before this stage.
    send(sine_frame(origin, 6, SampleRate::Hz48000, 8_000.0)).await;
    send(sine_frame(origin, 7, SampleRate::Hz48000, 8_000.0)).await;
    clock.sleep(20 * MS).await;
    let stats = chain.stats.snapshot();
    assert_eq!(stats.frames_in, 5);
    assert_eq!(stats.passthrough, 1);
    assert_eq!(stats.resampled, 4);
    assert_eq!(stats.resampler_builds, 2, "8 kHz, then 48 kHz");
    assert_eq!(stats.gaps, 1);
    assert_eq!(chain.rec.audio().len(), 5);
    assert!(
        chain
            .rec
            .audio()
            .iter()
            .all(|f| f.sample_rate() == SampleRate::Hz16000)
    );
}

/// The `index`-th 10 ms half of `sine_frame`-style audio at 8 kHz (80 samples).
fn half_frame(origin: tokio::time::Instant, index: u64) -> brisvox_core::frame::AudioFrame {
    let full = sine_frame(origin, index / 2, SampleRate::Hz8000, 8_000.0);
    let half = index as usize % 2;
    brisvox_core::frame::AudioFrame::new(
        full.data().slice(half * 160..half * 160 + 160),
        SampleRate::Hz8000,
        origin + 10 * MS * u32::try_from(index).unwrap(),
        index,
        TurnEpoch::ZERO,
    )
    .unwrap()
}

#[tokio::test(start_paused = true)]
async fn resampled_frames_are_stamped_with_the_time_of_their_first_sample() {
    use brisvox_audio::StreamResampler;
    let delay = StreamResampler::new(SampleRate::Hz8000, SampleRate::Hz16000)
        .unwrap()
        .delay();
    assert!(
        delay > Duration::ZERO,
        "the filter has a delay to compensate"
    );

    // 20 ms frames: the output is the input shifted by the filter delay.
    let clock = SimClock::new();
    let origin = clock.now();
    let chain = convert(SampleRate::Hz8000, 10).await;
    for (k, frame) in chain.rec.audio().iter().enumerate() {
        assert_eq!(
            frame.timestamp(),
            origin + 20 * MS * u32::try_from(k).unwrap() - delay,
            "frame {k}"
        );
    }

    // 10 ms frames: a block completes with every second frame and starts at the FIRST of the
    // two, not at the completing one.
    let clock = SimClock::new();
    let origin = clock.now();
    let chain = start().await;
    for i in 0..20u64 {
        chain
            .running
            .input()
            .send(half_frame(origin, i).into())
            .await
            .unwrap();
        clock.sleep(10 * MS).await;
    }
    let audio = chain.rec.audio();
    assert_eq!(audio.len(), 10);
    for (k, frame) in audio.iter().enumerate() {
        assert_eq!(
            frame.timestamp(),
            origin + 20 * MS * u32::try_from(k).unwrap() - delay,
            "frame {k}: the block's first sample, not the completing input frame"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn returning_to_a_rate_after_other_frames_starts_a_fresh_resampler() {
    let clock = SimClock::new();
    let origin = clock.now();
    let chain = start().await;
    let send = |frame: brisvox_core::frame::AudioFrame| {
        let input = chain.running.input().clone();
        async move { input.send(frame.into()).await.unwrap() }
    };
    // Half a block of 8 kHz audio is held by the resampler ...
    send(half_frame(origin, 0)).await;
    // ... a frame at the model rate goes by without touching it ...
    let mut pass = sine_frame(origin, 0, SampleRate::Hz16000, 8_000.0);
    pass = brisvox_core::frame::AudioFrame::new(
        pass.data().clone(),
        SampleRate::Hz16000,
        origin + 10 * MS,
        1,
        TurnEpoch::ZERO,
    )
    .unwrap();
    send(pass).await;
    // ... and 8 kHz returns. The held half block belongs to the old stretch of audio, so this
    // frame is again only the first half of a block: no output.
    let mut back = half_frame(origin, 1);
    back = brisvox_core::frame::AudioFrame::new(
        back.data().clone(),
        SampleRate::Hz8000,
        origin + 30 * MS,
        2,
        TurnEpoch::ZERO,
    )
    .unwrap();
    send(back).await;
    clock.sleep(20 * MS).await;
    let stats = chain.stats.snapshot();
    assert_eq!(stats.frames_in, 3);
    assert_eq!(stats.passthrough, 1);
    assert_eq!(
        (stats.resampled, stats.empty_outputs),
        (0, 2),
        "stale state would have completed a block here: {stats:?}"
    );
    assert_eq!(chain.rec.audio().len(), 1, "only the pass-through frame");
}

/// `half_frame(index)` with another sequence number and send time.
fn half_frame_at(
    origin: tokio::time::Instant,
    index: u64,
    seq: u64,
    at: Duration,
) -> brisvox_core::frame::AudioFrame {
    let frame = half_frame(origin, index);
    brisvox_core::frame::AudioFrame::new(
        frame.data().clone(),
        SampleRate::Hz8000,
        origin + at,
        seq,
        TurnEpoch::ZERO,
    )
    .unwrap()
}

/// A hole in the sequence numbers at a CONSTANT resampled rate (no rate change to rebuild the
/// resampler anyway) must reset the resampler: the half block it holds from before the hole
/// does not complete with audio from after it.
#[tokio::test(start_paused = true)]
async fn a_sequence_hole_at_a_constant_resampled_rate_resets_the_resampler() {
    use brisvox_audio::StreamResampler;
    let delay = StreamResampler::new(SampleRate::Hz8000, SampleRate::Hz16000)
        .unwrap()
        .delay();
    let clock = SimClock::new();
    let origin = clock.now();
    let chain = start().await;
    let send = |frame: brisvox_core::frame::AudioFrame| {
        let input = chain.running.input().clone();
        async move { input.send(frame.into()).await.unwrap() }
    };
    // Half a block is held ...
    send(half_frame_at(origin, 0, 0, Duration::ZERO)).await;
    // ... frames 1..5 are lost ... the next frame is again the first half of a block: no
    // output (a stale half block would have completed one here).
    send(half_frame_at(origin, 1, 5, 50 * MS)).await;
    clock.sleep(20 * MS).await;
    let stats = chain.stats.snapshot();
    assert_eq!(
        (stats.frames_in, stats.gaps, stats.resampler_builds),
        (2, 1, 1)
    );
    assert_eq!(
        (stats.resampled, stats.empty_outputs),
        (0, 2),
        "the held half block belongs to the stretch before the hole: {stats:?}"
    );
    assert!(chain.rec.audio().is_empty());
    // The block that completes after the hole starts at the first frame after it.
    send(half_frame_at(origin, 2, 6, 60 * MS)).await;
    clock.sleep(20 * MS).await;
    let audio = chain.rec.audio();
    assert_eq!(audio.len(), 1);
    assert_eq!(audio[0].timestamp(), origin + 50 * MS - delay);
    assert_eq!(chain.stats.snapshot().gaps, 1);
}

/// Collects what the caller-audio tap saw.
struct Collect(std::sync::Arc<std::sync::Mutex<Vec<brisvox_core::frame::AudioFrame>>>);

impl CallerAudioTap for Collect {
    fn observe(&mut self, audio: &brisvox_core::frame::AudioFrame) {
        self.0.lock().unwrap().push(audio.clone());
    }
}

#[tokio::test(start_paused = true)]
async fn the_caller_audio_tap_sees_each_frame_as_received_before_conversion() {
    let config = EngineConfig::DEFAULT;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let audio_in = AudioIn::new(&config).with_tap(Collect(seen.clone()));
    let rec = Recorded::default();
    let running = Pipeline::new("input")
        .then_with_queue(audio_in, config.queues.input)
        .then(Recorder(rec.clone()))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    let clock = SimClock::new();
    let origin = clock.now();
    let frames: Vec<_> = (0..3)
        .map(|i| sine_frame(origin, i, SampleRate::Hz8000, 8_000.0))
        .collect();
    for frame in &frames {
        running.input().send(frame.clone().into()).await.unwrap();
        clock.sleep(20 * MS).await;
    }
    // The tap got the 8 kHz frames themselves (not the 16 kHz conversion the next stage got).
    assert_eq!(*seen.lock().unwrap(), frames);
    assert!(
        rec.audio()
            .iter()
            .all(|a| a.sample_rate() == SampleRate::Hz16000),
        "the chain still converts"
    );
}
