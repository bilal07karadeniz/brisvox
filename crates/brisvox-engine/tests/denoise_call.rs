//! Noise suppression in a whole call (`crate::denoise`): the VAD and the Smart Turn ring hear the
//! enhanced audio, the speech-to-text provider hears it only with `denoise_stt`, and no audio is
//! ever lost or sent twice, whatever the suppressor does.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{SharedClock, SimClock};
use brisvox_engine::sim::{LoopbackCall, energy_vad_setup, open_loopback};
use brisvox_engine::{
    Call, DenoiseSetup, EngineConfig, InterruptionConfig, Offload, SessionBuilder, run_call,
};
use brisvox_inference::Denoiser;
use brisvox_inference::smart_turn::TurnModel;
use brisvox_sim::input::{InputSink, InputTarget, SendFuture};
use brisvox_sim::mock_llm::LlmScript;
use brisvox_sim::scenario::{Scenario, play_input};
use brisvox_sim::session::SimSession;
use tokio_util::sync::CancellationToken;

const MS: Duration = Duration::from_millis(1);

/// Multiplies the audio by `gain`, with no delay (0 mutes, 1 is the identity).
struct Gain(f32);

impl Denoiser for Gain {
    fn sample_rate(&self) -> u32 {
        16_000
    }
    fn delay(&self) -> Duration {
        Duration::ZERO
    }
    fn process(&mut self, input: &[i16], output: &mut Vec<i16>) -> brisvox_inference::Result<()> {
        output.extend(input.iter().map(|s| (f32::from(*s) * self.0) as i16));
        Ok(())
    }
    fn reset(&mut self) {}
}

/// Delays the audio by `lag` samples (a model's algorithmic delay) and fails its `fail_at`-th call:
/// the stream then restarts from silence and the audio in its line is lost.
struct Delayed {
    lag: usize,
    line: std::collections::VecDeque<i16>,
    calls: usize,
    fail_at: usize,
}

impl Denoiser for Delayed {
    fn sample_rate(&self) -> u32 {
        16_000
    }
    fn delay(&self) -> Duration {
        Duration::from_micros(self.lag as u64 * 1_000_000 / 16_000)
    }
    fn process(&mut self, input: &[i16], output: &mut Vec<i16>) -> brisvox_inference::Result<()> {
        self.calls += 1;
        if self.calls == self.fail_at {
            return Err(brisvox_inference::Error::Model("scripted failure".into()));
        }
        for sample in input {
            self.line.push_back(*sample);
            output.push(if self.line.len() > self.lag {
                self.line.pop_front().unwrap_or(0)
            } else {
                0
            });
        }
        Ok(())
    }
    fn reset(&mut self) {
        self.line.clear();
    }
}

/// Mutes one 20 ms frame in eight of the audio (a change the Smart Turn features cannot normalise away).
struct Chop(usize);

impl Denoiser for Chop {
    fn sample_rate(&self) -> u32 {
        16_000
    }
    fn delay(&self) -> Duration {
        Duration::ZERO
    }
    fn process(&mut self, input: &[i16], output: &mut Vec<i16>) -> brisvox_inference::Result<()> {
        for sample in input {
            output.push(if (self.0 / 320) % 8 == 7 { 0 } else { *sample });
            self.0 += 1;
        }
        Ok(())
    }
    fn reset(&mut self) {
        self.0 = 0;
    }
}

/// A Smart Turn model that remembers the mean of the (log-mel) features it was last given: it
/// moves with the level of the audio in the ring.
struct Probe(Arc<AtomicU32>);

impl TurnModel for Probe {
    fn predict(&self, features: &[f32]) -> brisvox_inference::Result<f32> {
        let mean = features.iter().sum::<f32>() / features.len() as f32;
        self.0.store(mean.to_bits(), Ordering::Relaxed);
        Ok(0.9)
    }
}

struct Caller {
    client: tokio::sync::Mutex<brisvox_transport::LoopbackClient>,
}

impl InputTarget for Caller {
    fn send(&self, frame: brisvox_core::frame::AudioFrame) -> SendFuture<'_> {
        Box::pin(async move {
            self.client
                .lock()
                .await
                .send_audio(frame.data().clone(), frame.sample_rate())
                .await
                .is_ok()
        })
    }
}

fn config() -> EngineConfig {
    let mut config = EngineConfig::DEFAULT;
    config.lead_margin = Duration::ZERO;
    config.llm.retries = 0;
    config.interruption = InterruptionConfig::OFF;
    config.turn = brisvox_agent::TurnPolicy::Adaptive {
        fallback_silence: Duration::from_secs(3),
        require_final: false,
        final_grace: 800 * MS,
    };
    config
}

fn question() -> Scenario {
    Scenario::new()
        .silence(100 * MS)
        .speak("what time is it", 800 * MS)
        .with_tail(900 * MS)
}

struct Rig {
    rate: SampleRate,
    clock: SharedClock,
    call: Call,
    _sim: SimSession,
    caller: Arc<Caller>,
    cancel: CancellationToken,
}

async fn start(denoise: Option<DenoiseSetup>, mean: Arc<AtomicU32>) -> Rig {
    start_at(SampleRate::Hz16000, denoise, mean).await
}

async fn start_at(rate: SampleRate, denoise: Option<DenoiseSetup>, mean: Arc<AtomicU32>) -> Rig {
    let scenario = question();
    let config = config();
    let clock: SharedClock = Arc::new(SimClock::new());
    let sim = SimSession::new(clock.clone());
    for utterance in scenario.utterances() {
        sim.stt.push(utterance);
    }
    sim.llm.push(LlmScript::text(["Sure thing."]));
    let LoopbackCall {
        session,
        client,
        cancel,
    } = open_loopback(&clock, 65_536).await.unwrap();
    let offload = Offload::Inline(clock.clone());
    let mut builder = SessionBuilder::new(
        config,
        clock.clone(),
        brisvox_core::TimelineRecorder::new(),
        offload.clone(),
        Arc::new(sim.stt.clone()),
        Arc::new(sim.llm.clone()),
        Arc::new(sim.tts.clone()),
    )
    .unwrap()
    .with_vad(energy_vad_setup(offload, &sim.vad, &config))
    .with_smart_turn(Arc::new(Probe(mean)));
    if let Some(setup) = denoise {
        builder = builder.with_denoise(setup);
    }
    let call = run_call(session, builder, &cancel).await.unwrap();
    Rig {
        rate,
        clock,
        call,
        _sim: sim,
        caller: Arc::new(Caller {
            client: tokio::sync::Mutex::new(client),
        }),
        cancel,
    }
}

impl Rig {
    async fn play(&self) {
        let origin = self.clock.now();
        play_input(
            &question(),
            self.clock.clone(),
            origin,
            self.rate,
            InputSink::target(self.caller.clone()),
            &self.cancel,
        )
        .await
        .unwrap();
        self.clock.sleep(1_500 * MS).await;
    }
}

/// Runs the question through a call with `denoise`; returns the counters and the mean feature
/// the Smart Turn model last saw (NaN when it never ran).
async fn run(denoise: Option<DenoiseSetup>) -> (brisvox_engine::StageCounters, f32) {
    let mean = Arc::new(AtomicU32::new(f32::NAN.to_bits()));
    let rig = start(denoise, mean.clone()).await;
    rig.play().await;
    let stages = brisvox_engine::StageCounters::read(rig.call.handles());
    (stages, f32::from_bits(mean.load(Ordering::Relaxed)))
}

#[tokio::test(start_paused = true)]
async fn without_noise_suppression_nothing_changes() {
    let (s, mean) = run(None).await;
    assert_eq!(s.denoise, brisvox_engine::DenoiseStatsSnapshot::default());
    assert_eq!(s.vad.starts, 1);
    assert!(mean.is_finite(), "the model heard the speech");
}

#[tokio::test(start_paused = true)]
async fn the_identity_changes_nothing_the_vad_or_the_turn_detector_hears() {
    let (plain, plain_mean) = run(None).await;
    let setup = DenoiseSetup::new(|| Ok(Box::new(Gain(1.0))));
    let (s, mean) = run(Some(setup)).await;
    assert_eq!(s.vad.starts, plain.vad.starts);
    assert_eq!(s.vad.stops, plain.vad.stops);
    assert!(s.denoise.chunks > 0);
    assert_eq!(s.denoise.errors + s.denoise.feed_dropped, 0);
    // Not to the speech-to-text provider by default.
    assert_eq!(s.denoise.stt_denoised_samples, 0);
    assert_eq!(s.stt.audio_ms_sent, plain.stt.audio_ms_sent);
    assert_eq!(mean, plain_mean, "the Smart Turn ring holds the same audio");
}

#[tokio::test(start_paused = true)]
async fn the_vad_hears_the_enhanced_audio_but_the_provider_hears_the_original() {
    let (plain, _) = run(None).await;
    // A suppressor that removes everything: no speech for the VAD.
    let setup = DenoiseSetup::new(|| Ok(Box::new(Gain(0.0))));
    let (s, _) = run(Some(setup)).await;
    assert_eq!(s.vad.starts, 0, "the VAD heard silence");
    // The provider still got every millisecond of the caller's audio, as it was.
    assert_eq!(s.stt.audio_ms_sent, plain.stt.audio_ms_sent);
    assert!(s.stt.audio_ms_sent > 1_500);
}

#[tokio::test(start_paused = true)]
async fn the_smart_turn_ring_holds_the_enhanced_audio() {
    let (_, plain_mean) = run(None).await;
    // The VAD still hears the speech, the ring holds a chopped version of it.
    let setup = DenoiseSetup::new(|| Ok(Box::new(Chop(0))));
    let (s, mean) = run(Some(setup)).await;
    assert_eq!(s.vad.starts, 1);
    assert!(mean.is_finite() && plain_mean.is_finite());
    assert!(
        (mean - plain_mean).abs() > 0.01,
        "the features moved with the level: plain {plain_mean}, enhanced {mean}"
    );
}

#[tokio::test(start_paused = true)]
async fn with_denoise_stt_the_provider_hears_the_enhanced_audio_exactly_once() {
    let (plain, _) = run(None).await;
    let setup = DenoiseSetup::new(|| Ok(Box::new(Gain(1.0)))).with_stt(true);
    let (s, _) = run(Some(setup)).await;
    // Every sample of the call reached the provider once: enhanced where the enhanced audio was
    // there in time, as it was elsewhere. Nothing dropped, nothing doubled.
    assert_eq!(s.stt.audio_ms_sent, plain.stt.audio_ms_sent, "{s:?}");
    assert!(s.denoise.stt_denoised_samples > 0, "{:?}", s.denoise);
    assert_eq!(s.vad.starts, plain.vad.starts);
}

#[tokio::test(start_paused = true)]
async fn a_suppressor_that_cannot_be_built_costs_the_provider_no_audio() {
    let (plain, _) = run(None).await;
    let setup = DenoiseSetup::new(|| {
        Err(brisvox_inference::Error::Model(
            "the model file is gone".into(),
        ))
    })
    .with_stt(true);
    let (s, _) = run(Some(setup)).await;
    assert_eq!(s.stt.audio_ms_sent, plain.stt.audio_ms_sent, "{s:?}");
    assert_eq!(s.denoise.stt_denoised_samples, 0);
    assert!(s.denoise.stt_fallback_frames > 0);
    // The VAD keeps running on the plain audio; only the suppression is lost.
    assert_eq!(s.vad.build_failures, 0, "{:?}", s.vad);
    assert!(s.denoise.build_failures > 0, "{:?}", s.denoise);
    assert_eq!(s.vad.starts, plain.vad.starts);
    assert_eq!(s.vad.failed_open, plain.vad.failed_open);
}

#[tokio::test(start_paused = true)]
async fn a_call_below_the_filters_minimum_rate_runs_without_it_and_says_so() {
    let setup = || DenoiseSetup::new(|| Ok(Box::new(Gain(1.0)))).with_min_call_rate(16_000);
    // A telephone call at 8 kHz: the filter is not used, the provider hears everything, and the
    // counters say why.
    let rig = start_at(
        SampleRate::Hz8000,
        Some(setup().with_stt(true)),
        Arc::default(),
    )
    .await;
    rig.play().await;
    let narrow = brisvox_engine::StageCounters::read(rig.call.handles());
    assert!(narrow.denoise.bypassed_narrowband);
    assert_eq!(narrow.denoise.chunks, 0, "{:?}", narrow.denoise);
    assert_eq!(narrow.denoise.stt_denoised_samples, 0);
    assert!(narrow.stt.audio_ms_sent > 1_500, "{:?}", narrow.stt);
    assert!(
        narrow.vad.starts >= 1,
        "the VAD still works on the plain audio"
    );
    // The same setup on a wideband call filters.
    let rig = start_at(SampleRate::Hz16000, Some(setup()), Arc::default()).await;
    rig.play().await;
    let wide = brisvox_engine::StageCounters::read(rig.call.handles());
    assert!(!wide.denoise.bypassed_narrowband);
    assert!(wide.denoise.chunks > 0);
}

/// The audio the provider heard in a whole call, in ms, after the call stopped (so that nothing is
/// still held back waiting for its enhanced version).
async fn heard_ms(denoise: Option<DenoiseSetup>) -> (u64, brisvox_engine::DenoiseStatsSnapshot) {
    let rig = start(denoise, Arc::default()).await;
    rig.play().await;
    let handles = rig.call.handles().clone();
    let mut rig = rig;
    rig.call.stop().await.unwrap();
    let stages = brisvox_engine::StageCounters::read(&handles);
    (stages.stt.audio_ms_sent, stages.denoise)
}

fn delayed(fail_at: usize) -> DenoiseSetup {
    DenoiseSetup::new(move || {
        Ok(Box::new(Delayed {
            lag: 640,
            line: std::collections::VecDeque::new(),
            calls: 0,
            fail_at,
        }))
    })
    .with_stt(true)
}

#[tokio::test(start_paused = true)]
async fn a_delaying_suppressor_that_fails_mid_call_costs_the_provider_no_audio() {
    let (plain, _) = heard_ms(None).await;
    // A suppressor with 40 ms of delay: the provider hears the whole call, except the last
    // stretch that is still held for its enhanced version when the call is stopped.
    let (steady, stats) = heard_ms(Some(delayed(usize::MAX))).await;
    assert_eq!(stats.errors, 0, "{stats:?}");
    assert!(
        plain - steady <= 40,
        "{plain} ms plain, {steady} ms delayed"
    );
    // The 30th call fails: its stream restarts, the audio in its line is gone, and the enhanced
    // chunks resume after a stretch that was never enhanced. Every sample still goes out exactly
    // once: the same audio as without the failure, not a millisecond more or less.
    let (heard, stats) = heard_ms(Some(delayed(30))).await;
    assert_eq!(stats.errors, 1, "{stats:?}");
    assert!(stats.stt_denoised_samples > 0);
    assert_eq!(heard, steady, "{stats:?}");
}
