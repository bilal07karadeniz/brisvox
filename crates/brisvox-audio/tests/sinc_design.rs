//! The windowed-sinc backend used for downsampling to 8 kHz: the design bar that
//! `sinc_len_for` was chosen against, pinned through the public `StreamResampler`, plus the
//! ignored probes that reproduce the numbers in `docs/LATENCY.md` (`cargo test --release -p
//! brisvox-audio --test sinc_design -- --ignored --nocapture`; release, the probes time code).
#![allow(clippy::unwrap_used, clippy::indexing_slicing)] // test helpers

use std::f64::consts::TAU;
use std::time::Duration;

use audioadapter_buffers::direct::InterleavedSlice;
use brisvox_audio::StreamResampler;
use brisvox_audio::analysis::sine;
use brisvox_core::SystemClock;
use brisvox_core::frame::SampleRate;
use rubato::{
    Async, Fft, FixedAsync, FixedSync, Resampler, SincInterpolationParameters,
    SincInterpolationType, WindowFunction,
};

/// The five pairs that downsample to 8 kHz with the sinc backend.
const SINC_PAIRS: [SampleRate; 5] = [
    SampleRate::Hz16000,
    SampleRate::Hz22050,
    SampleRate::Hz24000,
    SampleRate::Hz44100,
    SampleRate::Hz48000,
];

/// Streams `input` through `rs` in 20 ms frames of the input rate; returns all output.
fn stream(rs: &mut StreamResampler, input: &[i16]) -> Vec<i16> {
    let frame = rs.from_rate().hz() as usize / 50;
    let mut out = Vec::new();
    let mut buf = vec![0i16; rs.max_output_len(frame) + rs.max_output_len(rs.chunk_frames())];
    for piece in input.chunks(frame) {
        let n = rs.process(piece, &mut buf).unwrap();
        out.extend_from_slice(&buf[..n]);
    }
    let mut tail = vec![0i16; rs.max_flush_len()];
    let n = rs.flush(&mut tail).unwrap();
    out.extend_from_slice(&tail[..n]);
    out
}

/// Amplitude of the `freq` component of `signal` (relative to full scale), by least squares
/// over the samples after `skip`. Sees residual aliases far below what `snr_db` can.
fn amplitude(signal: &[i16], freq: f64, rate: u32, skip: usize) -> f64 {
    let w = TAU * freq / f64::from(rate);
    let (mut s, mut c, mut n) = (0.0f64, 0.0f64, 0usize);
    for (i, &y) in signal.iter().enumerate().skip(skip) {
        let t = w * i as f64;
        s += f64::from(y) * t.sin();
        c += f64::from(y) * t.cos();
        n += 1;
    }
    2.0 * (s * s + c * c).sqrt() / n.max(1) as f64 / 32_768.0
}

/// Level in dB, relative to the input tone's amplitude `amp`, of the component at `measure_hz`
/// in the 8 kHz output of a `freq` tone fed at the `from` rate.
fn level_db(from: SampleRate, freq: f64, amp: f64, measure_hz: u32) -> f64 {
    let mut rs = StreamResampler::new(from, SampleRate::Hz8000).unwrap();
    let input = sine(freq, from.hz(), from.hz() as usize, amp);
    let out = stream(&mut rs, &input);
    let skip = rs.filter_delay_frames() * 2 + 64;
    20.0 * (amplitude(&out, f64::from(measure_hz), 8_000, skip) / amp)
        .max(1e-12)
        .log10()
}

#[test]
fn sinc_pairs_meet_the_passband_and_stopband_bar() {
    // The bar `sinc_len_for` was chosen against (src/resample.rs, docs/LATENCY.md): flat to
    // 3.4 kHz, at most 2 dB down at 3.6 kHz, and 55 dB rejection of 4.4 kHz (which aliases
    // into the band at 3.6 kHz) and of 6 kHz (aliasing to 2 kHz).
    for from in SINC_PAIRS {
        let pass_34 = level_db(from, 3_400.0, 0.5, 3_400);
        let pass_36 = level_db(from, 3_600.0, 0.5, 3_600);
        assert!(
            pass_34.abs() <= 0.2,
            "{from:?}->8k: 3.4 kHz gain {pass_34:.2} dB"
        );
        assert!(
            pass_36 >= -2.0,
            "{from:?}->8k: 3.6 kHz gain {pass_36:.2} dB"
        );
        let alias_44 = level_db(from, 4_400.0, 0.9, 3_600);
        let alias_6k = level_db(from, 6_000.0, 0.9, 2_000);
        assert!(
            alias_44 <= -55.0,
            "{from:?}->8k: 4.4 kHz alias at {alias_44:.1} dB"
        );
        assert!(
            alias_6k <= -55.0,
            "{from:?}->8k: 6 kHz alias at {alias_6k:.1} dB"
        );
    }
}

/// Streams a unit-amplitude `f32` tone through a rubato resampler (no i16 quantisation).
fn rubato_stream(rs: &mut dyn Resampler<f32>, freq: f64, rate: usize) -> Vec<f32> {
    let input: Vec<f32> = (0..rate)
        .map(|i| (TAU * freq / rate as f64 * i as f64).sin() as f32)
        .collect();
    let chunk = rs.input_frames_next();
    let mut out = Vec::new();
    let mut obuf = vec![0f32; rs.output_frames_max()];
    let mut ibuf = vec![0f32; chunk];
    let mut pos = 0;
    while pos + chunk <= input.len() {
        ibuf.copy_from_slice(&input[pos..pos + chunk]);
        let ia = InterleavedSlice::new(&ibuf, 1, chunk).unwrap();
        let len = obuf.len();
        let mut oa = InterleavedSlice::new_mut(&mut obuf, 1, len).unwrap();
        let (_, w) = rs.process_into_buffer(&ia, &mut oa, None).unwrap();
        out.extend_from_slice(&obuf[..w]);
        pos += chunk;
    }
    rs.reset();
    out
}

/// Level in dB of the `measure_hz` component of `sig` at `rate` (unit-amplitude input tones).
fn f32_level_db(sig: &[f32], measure_hz: f64, rate: f64) -> f64 {
    let w = TAU * measure_hz / rate;
    let (mut s, mut c, mut n) = (0f64, 0f64, 0usize);
    for (i, &y) in sig.iter().enumerate().skip(4_000) {
        let t = w * i as f64;
        s += f64::from(y) * t.sin();
        c += f64::from(y) * t.cos();
        n += 1;
    }
    20.0 * (2.0 * (s * s + c * c).sqrt() / n.max(1) as f64)
        .max(1e-12)
        .log10()
}

fn sinc_params(len: usize) -> SincInterpolationParameters {
    SincInterpolationParameters::new(len, WindowFunction::BlackmanHarris2)
        .oversampling_factor(128)
        .interpolation(SincInterpolationType::Cubic)
}

/// Sinc length sweep at 8 kHz output (rubato directly, so any length can be tried): the table
/// behind `sinc_len_for`. Prints the delay, the gain at 3.4/3.6 kHz and the alias level of a
/// 4.4 kHz tone (it lands at 3.6 kHz), plus the FFT the code used before the sinc backend.
#[test]
#[ignore = "probe: prints the sinc length sweep (docs/LATENCY.md)"]
fn probe_sinc_length_sweep() {
    for from_hz in [16_000usize, 22_050, 24_000, 44_100, 48_000] {
        for len in [48usize, 64, 96, 128, 160, 192, 224, 256, 320, 384, 512] {
            let mut rs = Async::<f32>::new_sinc(
                8_000.0 / from_hz as f64,
                1.0,
                &sinc_params(len),
                from_hz / 200,
                1,
                FixedAsync::Input,
            )
            .unwrap();
            let delay = rs.output_delay();
            let g34 = f32_level_db(&rubato_stream(&mut rs, 3_400.0, from_hz), 3_400.0, 8_000.0);
            let g36 = f32_level_db(&rubato_stream(&mut rs, 3_600.0, from_hz), 3_600.0, 8_000.0);
            let r44 = f32_level_db(&rubato_stream(&mut rs, 4_400.0, from_hz), 3_600.0, 8_000.0);
            println!(
                "sinc from {from_hz:>5} len {len:>3}: delay {:>5.2} ms, gain 3.4k {g34:>6.2} dB, 3.6k {g36:>6.2} dB, alias(4.4k) {r44:>7.1} dB",
                delay as f64 / 8.0
            );
        }
    }
    for from_hz in [16_000usize, 24_000, 48_000] {
        let block = from_hz * 160 / 8_000;
        let mut rs = Fft::<f32>::new_custom(
            from_hz,
            8_000,
            block,
            1,
            1,
            WindowFunction::BlackmanHarris2,
            FixedSync::Input,
        )
        .unwrap();
        let delay = rs.output_delay();
        let g34 = f32_level_db(&rubato_stream(&mut rs, 3_400.0, from_hz), 3_400.0, 8_000.0);
        let g36 = f32_level_db(&rubato_stream(&mut rs, 3_600.0, from_hz), 3_600.0, 8_000.0);
        let r44 = f32_level_db(&rubato_stream(&mut rs, 4_400.0, from_hz), 3_600.0, 8_000.0);
        println!(
            "fft from {from_hz:>5} block {block}: delay {:>5.2} ms, gain 3.4k {g34:>6.2} dB, 3.6k {g36:>6.2} dB, alias(4.4k) {r44:>7.1} dB",
            delay as f64 / 8.0
        );
    }
}

/// Upsampling 8 -> 16 kHz with the sinc backend (not used by the crate: the FFT stays): the
/// delay, passband gain and CPU per 20 ms frame behind the 3 ms adoption bar in
/// `docs/LATENCY.md`, next to the FFT the crate uses.
#[test]
#[ignore = "probe: upsampling sinc vs FFT (docs/LATENCY.md)"]
fn probe_upsampling_sinc_versus_fft() {
    let clock = SystemClock::shared();
    // One second of 8 kHz audio is 50 frames of 20 ms; time 200 seconds of it.
    let input: Vec<f32> = (0..8_000)
        .map(|i| (TAU * 3_400.0 / 8_000.0 * i as f64).sin() as f32)
        .collect();
    let time_per_frame = |rs: &mut dyn Resampler<f32>| -> Duration {
        let chunk = rs.input_frames_next();
        let mut obuf = vec![0f32; rs.output_frames_max()];
        let mut ibuf = vec![0f32; chunk];
        let start = clock.now();
        for _ in 0..200 {
            let mut pos = 0;
            while pos + chunk <= input.len() {
                ibuf.copy_from_slice(&input[pos..pos + chunk]);
                let ia = InterleavedSlice::new(&ibuf, 1, chunk).unwrap();
                let len = obuf.len();
                let mut oa = InterleavedSlice::new_mut(&mut obuf, 1, len).unwrap();
                rs.process_into_buffer(&ia, &mut oa, None).unwrap();
                pos += chunk;
            }
            rs.reset();
        }
        clock.now().saturating_duration_since(start) / (200 * 50)
    };
    for len in [64usize, 96, 128, 192] {
        let mut rs =
            Async::<f32>::new_sinc(2.0, 1.0, &sinc_params(len), 40, 1, FixedAsync::Input).unwrap();
        let delay = rs.output_delay();
        let g34 = f32_level_db(&rubato_stream(&mut rs, 3_400.0, 8_000), 3_400.0, 16_000.0);
        let cpu = time_per_frame(&mut rs);
        println!(
            "upsample 8k->16k sinc len {len}: delay {:.2} ms, gain 3.4k {g34:.2} dB, {cpu:?} per 20 ms",
            delay as f64 / 16.0
        );
    }
    let mut rs = Fft::<f32>::new_custom(
        8_000,
        16_000,
        160,
        1,
        1,
        WindowFunction::BlackmanHarris2,
        FixedSync::Input,
    )
    .unwrap();
    let delay = rs.output_delay();
    let g34 = f32_level_db(&rubato_stream(&mut rs, 3_400.0, 8_000), 3_400.0, 16_000.0);
    let cpu = time_per_frame(&mut rs);
    println!(
        "upsample 8k->16k fft block 160: delay {:.2} ms, gain 3.4k {g34:.2} dB, {cpu:?} per 20 ms",
        delay as f64 / 16.0
    );
}

/// Construction and per-frame cost of the backends, and the cost of cycling many instances
/// (what the sessions do). Wall-clock, release build.
#[test]
#[ignore = "probe: construction and cache costs (docs/LATENCY.md)"]
fn probe_construction_and_cache_cost() {
    let clock = SystemClock::shared();
    for (from, to) in [
        (SampleRate::Hz48000, SampleRate::Hz8000),
        (SampleRate::Hz24000, SampleRate::Hz8000),
        (SampleRate::Hz16000, SampleRate::Hz8000),
        (SampleRate::Hz48000, SampleRate::Hz16000),
        (SampleRate::Hz8000, SampleRate::Hz16000),
    ] {
        let mut times = Vec::new();
        let mut backend = None;
        for _ in 0..30 {
            let start = clock.now();
            let rs = StreamResampler::new(from, to).unwrap();
            times.push(clock.now().saturating_duration_since(start));
            backend = Some(rs.backend());
        }
        times.sort();
        println!(
            "construct {from:?}->{to:?} ({backend:?}): median {:?}, min {:?}, max {:?}",
            times[times.len() / 2],
            times[0],
            times[times.len() - 1]
        );
    }
    // Cycling 500 instances (one per session) vs one warm instance, per 20 ms frame.
    for (from, to) in [
        (SampleRate::Hz24000, SampleRate::Hz8000),
        (SampleRate::Hz48000, SampleRate::Hz16000),
    ] {
        let frame = from.hz() as usize / 50;
        let input = sine(1_000.0, from.hz(), frame, 0.5);
        let mut single = StreamResampler::new(from, to).unwrap();
        let mut buf = vec![0i16; single.max_output_len(frame) + 1_024];
        for _ in 0..50 {
            single.process(&input, &mut buf).unwrap();
        }
        let start = clock.now();
        for _ in 0..2_000 {
            single.process(&input, &mut buf).unwrap();
        }
        let warm = clock.now().saturating_duration_since(start) / 2_000;
        let mut many: Vec<StreamResampler> = (0..500)
            .map(|_| StreamResampler::new(from, to).unwrap())
            .collect();
        for rs in &mut many {
            rs.process(&input, &mut buf).unwrap();
        }
        let start = clock.now();
        for _ in 0..4 {
            for rs in &mut many {
                rs.process(&input, &mut buf).unwrap();
            }
        }
        let cycled = clock.now().saturating_duration_since(start) / (4 * 500);
        println!(
            "cycle {from:?}->{to:?}: warm {warm:?} per frame, 500 instances {cycled:?} per frame ({:.2}x)",
            cycled.as_secs_f64() / warm.as_secs_f64()
        );
    }
}
