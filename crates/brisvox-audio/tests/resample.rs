//! Streaming resampler quality and streaming behaviour.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)] // test helpers

use brisvox_audio::analysis::{rms, sine, snr_db};
use brisvox_audio::{Error, StreamResampler};
use brisvox_core::frame::SampleRate;

const RATES: [SampleRate; 6] = [
    SampleRate::Hz8000,
    SampleRate::Hz16000,
    SampleRate::Hz22050,
    SampleRate::Hz24000,
    SampleRate::Hz44100,
    SampleRate::Hz48000,
];

/// Streams `input` through `rs` in `chunk`-sized pieces; returns all output. Checks every call
/// stays within `max_output_len`.
fn stream(rs: &mut StreamResampler, input: &[i16], chunk: usize) -> Vec<i16> {
    let mut out = Vec::new();
    let mut buf = vec![0i16; rs.max_output_len(input.len()) + 16];
    for piece in input.chunks(chunk) {
        let need = rs.max_output_len(piece.len());
        let n = rs.process(piece, &mut buf[..need]).unwrap();
        assert!(n <= need);
        out.extend_from_slice(&buf[..n]);
    }
    out
}

fn frames_20ms(rate: SampleRate) -> usize {
    rate.hz() as usize / 50
}

fn gcd(a: u128, b: u128) -> u128 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// The documented filter delay for a pair: half an FFT block, where the block is 160 samples
/// of the lower rate rounded up to a whole number of the rates' exact rational steps.
fn max_delay_us(from: SampleRate, to: SampleRate) -> u128 {
    if to == SampleRate::Hz8000 && from != SampleRate::Hz8000 {
        // Sinc backend: 4.3-6 ms instead of the FFT's 10 ms.
        return 6_100;
    }
    let (f, t) = (u128::from(from.hz()), u128::from(to.hz()));
    let block_in = f * 160 / f.min(t);
    let step_in = f / gcd(f, t);
    let block_out = block_in.div_ceil(step_in) * step_in * t / f;
    (block_out / 2) * 1_000_000 / t + 100
}

#[test]
fn every_rate_pair_keeps_a_tone_clean_and_in_time() {
    println!("from -> to: chunk in, filter delay frames, delay ms, SNR dB (1 kHz)");
    for from in RATES {
        for to in RATES {
            let mut rs = StreamResampler::new(from, to).unwrap();
            let seconds = 2;
            let input = sine(1_000.0, from.hz(), from.hz() as usize * seconds, 0.5);
            let out = stream(&mut rs, &input, frames_20ms(from));
            // Length: everything but at most one buffered chunk comes out.
            let expected = to.hz() as usize * seconds;
            // One resampler call of output may still be held (rubato carries a partial block).
            let chunk_out = rs.max_output_len(rs.chunk_frames());
            assert!(
                out.len() <= expected && out.len() + chunk_out + 1 >= expected,
                "{from:?}->{to:?}: {} of {expected}",
                out.len()
            );
            // Quality after the start-up transient.
            let skip = rs.filter_delay_frames() * 2 + 64;
            let snr = snr_db(&out, 1_000.0, to.hz(), skip);
            let level = rms(&out[skip..]) / rms(&input);
            println!(
                "{:>5} -> {:>5}: {:>4} {:>4} {:>5.2} {snr:>5.1}",
                from.hz(),
                to.hz(),
                rs.chunk_frames(),
                rs.filter_delay_frames(),
                rs.delay().as_secs_f64() * 1e3
            );
            assert!(snr > 80.0, "{from:?}->{to:?}: SNR {snr:.1} dB");
            assert!(
                (0.97..1.03).contains(&level),
                "{from:?}->{to:?}: level {level}"
            );
            assert!(
                rs.delay().as_micros() <= max_delay_us(from, to),
                "{from:?}->{to:?}: {:?}",
                rs.delay()
            );
        }
    }
}

#[test]
fn a_frequency_sweep_stays_clean_below_the_shared_band_edge() {
    // Tones up to 0.85 x the lower Nyquist pass cleanly and at full level (telephony: 3.4 kHz).
    for (from, to) in [
        (SampleRate::Hz48000, SampleRate::Hz16000),
        (SampleRate::Hz16000, SampleRate::Hz48000),
        (SampleRate::Hz8000, SampleRate::Hz24000),
        (SampleRate::Hz44100, SampleRate::Hz16000),
        (SampleRate::Hz22050, SampleRate::Hz8000),
        (SampleRate::Hz16000, SampleRate::Hz8000),
        (SampleRate::Hz24000, SampleRate::Hz8000),
        (SampleRate::Hz44100, SampleRate::Hz8000),
        (SampleRate::Hz48000, SampleRate::Hz8000),
    ] {
        let nyquist = f64::from(from.hz().min(to.hz())) / 2.0;
        for fraction in [0.02, 0.1, 0.3, 0.5, 0.7, 0.8, 0.85] {
            let freq = nyquist * fraction;
            let mut rs = StreamResampler::new(from, to).unwrap();
            let input = sine(freq, from.hz(), from.hz() as usize, 0.5);
            let out = stream(&mut rs, &input, frames_20ms(from));
            let skip = rs.filter_delay_frames() * 2 + 64;
            let snr = snr_db(&out, freq, to.hz(), skip);
            // No passband droop: the tone keeps its level within 0.2 dB up to the edge.
            let gain_db = 20.0 * (rms(&out[skip..]) / rms(&input)).log10();
            assert!(
                gain_db.abs() < 0.2,
                "{from:?}->{to:?} at {freq:.0} Hz: gain {gain_db:.2} dB"
            );
            assert!(
                snr > 60.0,
                "{from:?}->{to:?} at {freq:.0} Hz: SNR {snr:.1} dB"
            );
        }
    }
}

#[test]
fn tones_above_the_output_band_are_rejected_not_aliased() {
    // Downsampling 48k -> 8k: a full-scale 6 kHz tone (above the 4 kHz output Nyquist) must
    // vanish, while a 1 kHz control tone passes through the same configuration.
    let level_after = |freq: f64| {
        let mut rs = StreamResampler::new(SampleRate::Hz48000, SampleRate::Hz8000).unwrap();
        let input = sine(freq, 48_000, 48_000, 0.99);
        let out = stream(&mut rs, &input, 960);
        let skip = rs.filter_delay_frames() * 2 + 64;
        assert!(out.len() > skip + 4_000, "{}", out.len());
        rms(&out[skip..]) / rms(&input)
    };
    assert!((0.97..1.03).contains(&level_after(1_000.0)));
    let rejected = level_after(6_000.0);
    // Nothing survives above the 16-bit floor (measured: exactly zero).
    assert!(
        rejected == 0.0 || 20.0 * rejected.log10() < -90.0,
        "{rejected}"
    );
}

#[test]
fn chunking_does_not_change_the_output() {
    // Includes a pair whose 20 ms chunk is not a multiple of the FFT block (22.05 <-> 44.1 kHz),
    // and the sinc backend (downsampling to 8 kHz), whose chunk is 5 ms.
    for (from, to) in [
        (SampleRate::Hz44100, SampleRate::Hz16000),
        (SampleRate::Hz22050, SampleRate::Hz44100),
        (SampleRate::Hz44100, SampleRate::Hz22050),
        (SampleRate::Hz48000, SampleRate::Hz8000),
        (SampleRate::Hz16000, SampleRate::Hz8000),
        (SampleRate::Hz44100, SampleRate::Hz8000),
    ] {
        let input = sine(700.0, from.hz(), from.hz() as usize, 0.4);
        let reference = stream(
            &mut StreamResampler::new(from, to).unwrap(),
            &input,
            frames_20ms(from),
        );
        for chunk in [1, 7, 160, 333, frames_20ms(from), 5_000] {
            let mut rs = StreamResampler::new(from, to).unwrap();
            assert_eq!(
                stream(&mut rs, &input, chunk),
                reference,
                "{from:?}->{to:?} chunk {chunk}"
            );
        }
    }
}

#[test]
fn equal_rates_pass_through_without_delay() {
    let mut rs = StreamResampler::new(SampleRate::Hz16000, SampleRate::Hz16000).unwrap();
    let input = sine(440.0, 16_000, 320, 0.5);
    let mut out = vec![0i16; 320];
    assert_eq!(rs.process(&input, &mut out).unwrap(), 320);
    assert_eq!(out, input);
    assert_eq!(rs.delay().as_nanos(), 0);
    assert_eq!(rs.chunk_frames(), 0);
}

#[test]
fn a_short_output_buffer_is_rejected_without_consuming_input() {
    // FFT (48 -> 16 kHz) and sinc (48 -> 8 kHz) instances alike.
    for to in [SampleRate::Hz16000, SampleRate::Hz8000] {
        let input = sine(440.0, 48_000, 48_000, 0.5);
        let fresh = || StreamResampler::new(SampleRate::Hz48000, to).unwrap();
        let reference = stream(&mut fresh(), &input, 960);
        let mut rs = fresh();
        let mut out = Vec::new();
        let mut buf = vec![0i16; 4_096];
        // A partial chunk first, so a consuming error path would shift everything after it.
        let n = rs.process(&input[..500], &mut buf).unwrap();
        out.extend_from_slice(&buf[..n]);
        let need = rs.max_output_len(960);
        assert!(need > 0, "48k->{to:?}");
        assert!(matches!(
            rs.process(&input[500..1_460], &mut buf[..need - 1]),
            Err(Error::BufferTooSmall { .. })
        ));
        // Retry the same input with room, then the rest: identical to a clean run.
        for piece in input[500..].chunks(960) {
            let n = rs.process(piece, &mut buf).unwrap();
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, reference, "48k->{to:?}");
    }
}

#[test]
fn reset_restores_a_fresh_instance() {
    // A reset in mid-stream (a partial chunk buffered, filter state dirty) must leave nothing
    // behind, for the FFT pair and for the sinc pairs (downsampling to 8 kHz).
    for (from, to) in [
        (SampleRate::Hz8000, SampleRate::Hz16000),
        (SampleRate::Hz48000, SampleRate::Hz8000),
        (SampleRate::Hz16000, SampleRate::Hz8000),
    ] {
        let fresh = || StreamResampler::new(from, to).unwrap();
        let speech = sine(300.0, from.hz(), from.hz() as usize / 5, 0.7);
        let next = sine(1_200.0, from.hz(), from.hz() as usize / 5, 0.3);
        let mut used = fresh();
        stream(&mut used, &speech[..1_234], 160);
        used.reset();
        assert_eq!(
            stream(&mut used, &next, 160),
            stream(&mut fresh(), &next, 160),
            "{from:?}->{to:?}"
        );
    }
}

#[test]
fn flush_drains_the_partial_chunk_and_the_filter_tail_exactly() {
    for (from, to) in [
        (SampleRate::Hz24000, SampleRate::Hz8000),
        (SampleRate::Hz8000, SampleRate::Hz16000),
        (SampleRate::Hz22050, SampleRate::Hz16000),
    ] {
        let mut rs = StreamResampler::new(from, to).unwrap();
        // An utterance whose length is not a whole number of chunks.
        let input = sine(500.0, from.hz(), from.hz() as usize / 3 + 7, 0.5);
        let mut out = stream(&mut rs, &input, 333);
        let mut tail = vec![0i16; rs.max_flush_len()];
        let n = rs.flush(&mut tail).unwrap();
        assert_eq!(n, tail.len());
        out.extend_from_slice(&tail[..n]);
        // All the input comes out, plus the filter delay at the start, nothing more.
        let expected = input.len() * to.hz() as usize / from.hz() as usize + rs_delay(from, to);
        assert_eq!(out.len(), expected, "{from:?}->{to:?}");
        // The end of the tone is there (not silence).
        let last = &out[out.len() - 20..out.len() - 5];
        assert!(rms(last) > 0.05, "{from:?}->{to:?}: tail {last:?}");
        // flush reset the instance: nothing left, and it behaves like a fresh one.
        assert_eq!(rs.max_flush_len(), 0);
        let again = sine(900.0, from.hz(), 4_000, 0.3);
        assert_eq!(
            stream(&mut rs, &again, 160),
            stream(&mut StreamResampler::new(from, to).unwrap(), &again, 160)
        );
    }
    // A short buffer is refused without changing anything; flushing nothing yields nothing.
    let mut rs = StreamResampler::new(SampleRate::Hz16000, SampleRate::Hz8000).unwrap();
    assert_eq!(rs.flush(&mut []).unwrap(), 0);
    stream(&mut rs, &sine(300.0, 16_000, 1_000, 0.5), 1_000);
    let need = rs.max_flush_len();
    assert!(need > 0);
    assert!(matches!(
        rs.flush(&mut vec![0i16; need - 1]),
        Err(Error::BufferTooSmall { .. })
    ));
    assert_eq!(rs.max_flush_len(), need);
}

fn rs_delay(from: SampleRate, to: SampleRate) -> usize {
    StreamResampler::new(from, to)
        .unwrap()
        .filter_delay_frames()
}

#[test]
fn reported_delay_matches_the_signal_timing() {
    // A click at input sample k must come out (as the peak) at k * to / from + delay.
    // Every sinc pair (to 8 kHz) and FFT pairs of each kind.
    for (from, to) in [
        (SampleRate::Hz8000, SampleRate::Hz16000),
        (SampleRate::Hz16000, SampleRate::Hz8000),
        (SampleRate::Hz22050, SampleRate::Hz8000),
        (SampleRate::Hz24000, SampleRate::Hz8000),
        (SampleRate::Hz44100, SampleRate::Hz8000),
        (SampleRate::Hz48000, SampleRate::Hz8000),
        (SampleRate::Hz48000, SampleRate::Hz16000),
        (SampleRate::Hz22050, SampleRate::Hz16000),
        (SampleRate::Hz44100, SampleRate::Hz24000),
    ] {
        let mut rs = StreamResampler::new(from, to).unwrap();
        let k = from.hz() as usize / 10;
        let mut input = vec![0i16; from.hz() as usize / 2];
        input[k] = 20_000;
        let mut out = stream(&mut rs, &input, frames_20ms(from));
        let mut tail = vec![0i16; rs.max_flush_len()];
        let n = rs.flush(&mut tail).unwrap();
        out.extend_from_slice(&tail[..n]);
        let peak = out
            .iter()
            .enumerate()
            .max_by_key(|(_, s)| s.unsigned_abs())
            .unwrap()
            .0;
        let delay = StreamResampler::new(from, to)
            .unwrap()
            .filter_delay_frames();
        let expected = k * to.hz() as usize / from.hz() as usize + delay;
        assert!(
            peak.abs_diff(expected) <= 1,
            "{from:?}->{to:?}: peak at {peak}, expected {expected} (delay {delay})"
        );
    }
}

#[test]
fn downsampling_to_8k_uses_the_sinc_backend_everything_else_the_fft() {
    use brisvox_audio::resample::Backend;
    for from in RATES {
        for to in RATES {
            let backend = StreamResampler::new(from, to).unwrap().backend();
            let expected = if from == to {
                "copy"
            } else if to == SampleRate::Hz8000 {
                "sinc"
            } else {
                "fft"
            };
            let actual = match backend {
                Backend::Copy => "copy",
                Backend::Fft => "fft",
                Backend::Sinc { .. } => "sinc",
            };
            assert_eq!(actual, expected, "{from:?}->{to:?}");
        }
    }
}

#[test]
fn sinc_lengths_are_the_documented_ones() {
    // The design sweep (tests/sinc_design.rs, docs/LATENCY.md) chose these; a change here must
    // come with that sweep re-run.
    use brisvox_audio::resample::Backend;
    for (from, expected) in [
        (SampleRate::Hz16000, 192),
        (SampleRate::Hz22050, 224),
        (SampleRate::Hz24000, 224),
        (SampleRate::Hz44100, 384),
        (SampleRate::Hz48000, 512),
    ] {
        let backend = StreamResampler::new(from, SampleRate::Hz8000)
            .unwrap()
            .backend();
        assert_eq!(
            backend,
            Backend::Sinc { sinc_len: expected },
            "{from:?}->8k"
        );
    }
}
