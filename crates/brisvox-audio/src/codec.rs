//! G.711 μ-law / A-law and PCM16 codecs.
//!
//! Conversions follow ITU-T G.711 as implemented by the ITU-T G.191 reference software (STL),
//! see `docs/providers/g711.md`: μ-law works on the top 14 bits of a 16-bit sample, A-law on the
//! top 13. Negative μ-law inputs use the ones' complement magnitude like the STL (the classic
//! Sun `g711.c` negates instead and differs by one step for some negative inputs, e.g. -1 ->
//! 0x7F here, 0x7E there). Both directions are table lookups; the tables are computed at compile
//! time from the reference algorithm (decode: 256 entries; encode: one entry per 14-bit / 13-bit
//! input).
//!
//! The decoders and the segment search follow the structure of the Sun Microsystems `g711.c`
//! ("provided for unrestricted use"); the μ-law negative-input rule follows the G.191 STL.
//!
//! Buffer functions write into caller-provided buffers and never allocate. PCM is 16-bit signed
//! little-endian, as in [`brisvox_core::frame::AudioFrame`].

use crate::error::{Error, Result};

const BIAS: i32 = 0x84;
const ULAW_CLIP: i32 = 8159;
const SEG_UEND: [i32; 8] = [0x3F, 0x7F, 0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF, 0x1FFF];
const SEG_AEND: [i32; 8] = [0x1F, 0x3F, 0x7F, 0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF];

/// The first segment whose end is `>= value` (8 if none).
// Const evaluation only (tables): an out-of-bounds index would be a compile error.
#[allow(clippy::indexing_slicing)]
const fn segment(value: i32, ends: &[i32; 8]) -> i32 {
    let mut seg = 0;
    while seg < 8 {
        if value <= ends[seg] {
            break;
        }
        seg += 1;
    }
    seg as i32
}

/// Reference μ-law encoder (16-bit input; G.191 STL semantics).
const fn ulaw_encode_reference(sample: i16) -> u8 {
    let x = sample as i32;
    let (mut pcm, mask) = if x < 0 {
        ((!x) >> 2, 0x7F)
    } else {
        (x >> 2, 0xFF)
    };
    if pcm > ULAW_CLIP {
        pcm = ULAW_CLIP;
    }
    pcm += BIAS >> 2;
    let seg = segment(pcm, &SEG_UEND);
    if seg >= 8 {
        return (0x7F ^ mask) as u8;
    }
    let code = (seg << 4) | ((pcm >> (seg + 1)) & 0xF);
    (code ^ mask) as u8
}

/// Reference μ-law decoder.
const fn ulaw_decode_reference(code: u8) -> i16 {
    let u = !code as i32;
    let mut t = ((u & 0x0F) << 3) + BIAS;
    t <<= (u & 0x70) >> 4;
    if u & 0x80 != 0 {
        (BIAS - t) as i16
    } else {
        (t - BIAS) as i16
    }
}

/// Reference A-law encoder (16-bit input).
const fn alaw_encode_reference(sample: i16) -> u8 {
    let mut pcm = (sample as i32) >> 3;
    let mask;
    if pcm >= 0 {
        mask = 0xD5;
    } else {
        mask = 0x55;
        pcm = -pcm - 1;
    }
    let seg = segment(pcm, &SEG_AEND);
    if seg >= 8 {
        return (0x7F ^ mask) as u8;
    }
    let mut code = seg << 4;
    if seg < 2 {
        code |= (pcm >> 1) & 0x0F;
    } else {
        code |= (pcm >> seg) & 0x0F;
    }
    (code ^ mask) as u8
}

/// Reference A-law decoder.
const fn alaw_decode_reference(code: u8) -> i16 {
    let a = (code ^ 0x55) as i32;
    let mut t = (a & 0x0F) << 4;
    let seg = (a & 0x70) >> 4;
    match seg {
        0 => t += 8,
        1 => t += 0x108,
        _ => {
            t += 0x108;
            t <<= seg - 1;
        }
    }
    if a & 0x80 != 0 { t as i16 } else { -t as i16 }
}

#[allow(clippy::indexing_slicing)] // const evaluation: out of bounds = compile error
const ULAW_DECODE: [i16; 256] = {
    let mut table = [0i16; 256];
    let mut i = 0;
    while i < 256 {
        table[i] = ulaw_decode_reference(i as u8);
        i += 1;
    }
    table
};

#[allow(clippy::indexing_slicing)] // const evaluation: out of bounds = compile error
const ALAW_DECODE: [i16; 256] = {
    let mut table = [0i16; 256];
    let mut i = 0;
    while i < 256 {
        table[i] = alaw_decode_reference(i as u8);
        i += 1;
    }
    table
};

/// μ-law code of every 14-bit input (`(sample >> 2) + 8192`).
#[allow(clippy::indexing_slicing)] // const evaluation: out of bounds = compile error
static ULAW_ENCODE: [u8; 16_384] = {
    let mut table = [0u8; 16_384];
    let mut i = 0;
    while i < 16_384 {
        table[i] = ulaw_encode_reference(((i as i32 - 8_192) << 2) as i16);
        i += 1;
    }
    table
};

/// A-law code of every 13-bit input (`(sample >> 3) + 4096`).
#[allow(clippy::indexing_slicing)] // const evaluation: out of bounds = compile error
static ALAW_ENCODE: [u8; 8_192] = {
    let mut table = [0u8; 8_192];
    let mut i = 0;
    while i < 8_192 {
        table[i] = alaw_encode_reference(((i as i32 - 4_096) << 3) as i16);
        i += 1;
    }
    table
};

/// μ-law encodes one sample.
#[inline]
pub fn ulaw_encode(sample: i16) -> u8 {
    // In 0..16384 for every i16, so the lookup never misses.
    let index = usize::from(sample.cast_unsigned() >> 2) ^ 0x2000;
    ULAW_ENCODE.get(index).copied().unwrap_or(0xFF)
}

/// μ-law decodes one sample.
#[inline]
pub fn ulaw_decode(code: u8) -> i16 {
    ULAW_DECODE.get(usize::from(code)).copied().unwrap_or(0)
}

/// A-law encodes one sample.
#[inline]
pub fn alaw_encode(sample: i16) -> u8 {
    // In 0..8192 for every i16, so the lookup never misses.
    let index = usize::from(sample.cast_unsigned() >> 3) ^ 0x1000;
    ALAW_ENCODE.get(index).copied().unwrap_or(0xD5)
}

/// A-law decodes one sample.
#[inline]
pub fn alaw_decode(code: u8) -> i16 {
    ALAW_DECODE.get(usize::from(code)).copied().unwrap_or(0)
}

/// Wire encoding of telephony / provider audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Codec {
    /// 16-bit signed little-endian PCM (2 bytes per sample).
    Pcm16,
    /// G.711 μ-law (1 byte per sample; e.g. Twilio Media Streams).
    Ulaw,
    /// G.711 A-law (1 byte per sample).
    Alaw,
}

fn check_space(needed: usize, available: usize) -> Result<()> {
    if available < needed {
        return Err(Error::BufferTooSmall { needed, available });
    }
    Ok(())
}

/// Copies `src` to the start of `out` (callers checked that it fits).
fn copy_prefix(src: &[u8], out: &mut [u8]) {
    if let Some(dst) = out.get_mut(..src.len()) {
        dst.copy_from_slice(src);
    }
}

fn check_pcm(pcm: &[u8]) -> Result<()> {
    if !pcm.len().is_multiple_of(2) {
        return Err(Error::OddPcmLength(pcm.len()));
    }
    Ok(())
}

impl Codec {
    /// Bytes per sample on the wire.
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::Pcm16 => 2,
            Self::Ulaw | Self::Alaw => 1,
        }
    }

    /// Encoded size of `pcm_bytes` bytes of PCM16.
    pub const fn encoded_len(self, pcm_bytes: usize) -> usize {
        pcm_bytes / 2 * self.bytes_per_sample()
    }

    /// PCM16 size of `encoded_bytes` bytes of this codec (whole samples only).
    pub const fn decoded_len(self, encoded_bytes: usize) -> usize {
        encoded_bytes / self.bytes_per_sample() * 2
    }

    /// Encodes PCM16 LE `pcm` into `out`; returns the bytes written
    /// ([`Self::encoded_len`]).
    ///
    /// # Errors
    /// [`Error::OddPcmLength`] for a partial sample, [`Error::BufferTooSmall`] if `out` is
    /// shorter than the encoded size. Nothing is written on error.
    pub fn encode(self, pcm: &[u8], out: &mut [u8]) -> Result<usize> {
        check_pcm(pcm)?;
        let needed = self.encoded_len(pcm.len());
        check_space(needed, out.len())?;
        let samples = pcm
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b));
        match self {
            Self::Pcm16 => copy_prefix(pcm, out),
            Self::Ulaw => {
                for (dst, sample) in out.iter_mut().zip(samples) {
                    *dst = ulaw_encode(sample);
                }
            }
            Self::Alaw => {
                for (dst, sample) in out.iter_mut().zip(samples) {
                    *dst = alaw_encode(sample);
                }
            }
        }
        Ok(needed)
    }

    /// Decodes `encoded` into PCM16 LE `out`; returns the bytes written
    /// ([`Self::decoded_len`]).
    ///
    /// # Errors
    /// [`Error::OddPcmLength`] for a partial PCM16 sample, [`Error::BufferTooSmall`] if `out`
    /// is shorter than the decoded size. Nothing is written on error.
    pub fn decode(self, encoded: &[u8], out: &mut [u8]) -> Result<usize> {
        let needed = self.decoded_len(encoded.len());
        match self {
            Self::Pcm16 => {
                check_pcm(encoded)?;
                check_space(needed, out.len())?;
                copy_prefix(encoded, out);
            }
            Self::Ulaw | Self::Alaw => {
                check_space(needed, out.len())?;
                let table = if self == Self::Ulaw {
                    &ULAW_DECODE
                } else {
                    &ALAW_DECODE
                };
                for (dst, code) in out.as_chunks_mut::<2>().0.iter_mut().zip(encoded) {
                    let sample = table.get(usize::from(*code)).copied().unwrap_or(0);
                    *dst = sample.to_le_bytes();
                }
            }
        }
        Ok(needed)
    }

    /// Encodes samples into `out`; returns the bytes written (`samples.len()` times
    /// [`Self::bytes_per_sample`]). For DSP stages that work on `i16` (resampler, VAD).
    ///
    /// # Errors
    /// [`Error::BufferTooSmall`] if `out` is too short; nothing is written then.
    pub fn encode_samples(self, samples: &[i16], out: &mut [u8]) -> Result<usize> {
        let needed = samples.len().saturating_mul(self.bytes_per_sample());
        check_space(needed, out.len())?;
        match self {
            Self::Pcm16 => {
                for (dst, sample) in out.as_chunks_mut::<2>().0.iter_mut().zip(samples) {
                    *dst = sample.to_le_bytes();
                }
            }
            Self::Ulaw => {
                for (dst, sample) in out.iter_mut().zip(samples) {
                    *dst = ulaw_encode(*sample);
                }
            }
            Self::Alaw => {
                for (dst, sample) in out.iter_mut().zip(samples) {
                    *dst = alaw_encode(*sample);
                }
            }
        }
        Ok(needed)
    }

    /// Decodes `encoded` into samples; returns the samples written (whole samples only).
    ///
    /// # Errors
    /// [`Error::OddPcmLength`] for a partial PCM16 sample, [`Error::BufferTooSmall`] if `out`
    /// is too short; nothing is written then.
    pub fn decode_samples(self, encoded: &[u8], out: &mut [i16]) -> Result<usize> {
        let needed = encoded.len() / self.bytes_per_sample();
        if self == Self::Pcm16 {
            check_pcm(encoded)?;
        }
        if out.len() < needed {
            return Err(Error::BufferTooSmall {
                needed: needed.saturating_mul(2),
                available: out.len().saturating_mul(2),
            });
        }
        match self {
            Self::Pcm16 => {
                for (dst, bytes) in out.iter_mut().zip(encoded.as_chunks::<2>().0) {
                    *dst = i16::from_le_bytes(*bytes);
                }
            }
            Self::Ulaw => {
                for (dst, code) in out.iter_mut().zip(encoded) {
                    *dst = ulaw_decode(*code);
                }
            }
            Self::Alaw => {
                for (dst, code) in out.iter_mut().zip(encoded) {
                    *dst = alaw_decode(*code);
                }
            }
        }
        Ok(needed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The STL-formulation test codecs below were written from the algorithm summary in
    // docs/providers/g711.md (not from the STL source); their value is being a different
    // formulation from the const reference functions the tables are built from.

    /// Independent μ-law expander after the G.191 STL formulation (sign from the top bit,
    /// complemented exponent/mantissa, reconstruction at the interval midpoint).
    fn ulaw_expand_stl(code: u8) -> i16 {
        let sign = if code < 0x80 { -1 } else { 1 };
        let complemented = !i32::from(code);
        let exponent = (complemented >> 4) & 0x7;
        let mantissa = complemented & 0xF;
        let step = 4 << (exponent + 1);
        let magnitude = (0x80 << exponent) + step * mantissa + step / 2 - 4 * 33;
        i16::try_from(sign * magnitude).unwrap()
    }

    /// Independent A-law expander after the G.191 STL formulation.
    fn alaw_expand_stl(code: u8) -> i16 {
        let ix = i32::from(code ^ 0x55) & 0x7F;
        let exponent = ix >> 4;
        let mut mantissa = ix & 0xF;
        if exponent > 0 {
            mantissa += 16;
        }
        mantissa = (mantissa << 4) + 8;
        if exponent > 1 {
            mantissa <<= exponent - 1;
        }
        i16::try_from(if code > 127 { mantissa } else { -mantissa }).unwrap()
    }

    #[test]
    fn decode_tables_match_an_independent_expander_for_every_code() {
        for code in 0..=u8::MAX {
            assert_eq!(ulaw_decode(code), ulaw_expand_stl(code), "ulaw {code:#04x}");
            assert_eq!(alaw_decode(code), alaw_expand_stl(code), "alaw {code:#04x}");
        }
    }

    #[test]
    fn decoded_values_lie_inside_their_quantization_interval() {
        // For every code, the inputs that encode to it form an interval; the reconstruction
        // level must be inside it (midpoint up to the 14/13-bit truncation).
        for (encode, decode, law) in [
            (
                ulaw_encode as fn(i16) -> u8,
                ulaw_decode as fn(u8) -> i16,
                "ulaw",
            ),
            (alaw_encode, alaw_decode, "alaw"),
        ] {
            let mut lo = [i32::MAX; 256];
            let mut hi = [i32::MIN; 256];
            for s in i16::MIN..=i16::MAX {
                let c = usize::from(encode(s));
                lo[c] = lo[c].min(i32::from(s));
                hi[c] = hi[c].max(i32::from(s));
            }
            for code in 0..=u8::MAX {
                let c = usize::from(code);
                if lo[c] > hi[c] {
                    continue; // never produced
                }
                let level = i32::from(decode(code));
                if law == "ulaw" && code == 0x7F {
                    // μ-law "negative zero": inputs -4..=-1 (G.191) reconstruct to 0, one
                    // 14-bit step outside their interval, in every G.711 decoder.
                    assert_eq!((lo[c], hi[c], level), (-4, -1, 0));
                    continue;
                }
                // Clipped extremes reconstruct inside the range they absorb too.
                assert!(
                    (lo[c]..=hi[c]).contains(&level),
                    "{law} {code:#04x}: {level} not in {}..={}",
                    lo[c],
                    hi[c]
                );
            }
        }
    }

    /// Independent μ-law encoder written after the G.191 STL formulation (bit counting,
    /// complemented low nibble, sign bit for non-negative input).
    fn ulaw_stl(x: i16) -> u8 {
        let x = i32::from(x);
        let absno = (if x < 0 { (!x) >> 2 } else { x >> 2 } + 33).min(0x1FFF);
        let mut i = absno >> 6;
        let mut segno = 1;
        while i != 0 {
            segno += 1;
            i >>= 1;
        }
        let high = 0x8 - segno;
        let low = 0xF - ((absno >> segno) & 0xF);
        let code = (high << 4) | low;
        (if x >= 0 { code | 0x80 } else { code }) as u8
    }

    /// Independent A-law encoder written after the G.191 STL formulation (12 MSBs, ones'
    /// complement for negative input, normalisation loop, even bits inverted).
    fn alaw_stl(x: i16) -> u8 {
        let x = i32::from(x);
        let mut ix = if x < 0 { !x } else { x } >> 4;
        if ix > 15 {
            let mut exponent = 1;
            while ix > 16 + 15 {
                ix >>= 1;
                exponent += 1;
            }
            ix -= 16;
            ix += exponent << 4;
        }
        if x >= 0 {
            ix |= 0x80;
        }
        (ix ^ 0x55) as u8
    }

    #[test]
    fn decode_tables_match_the_reference_values() {
        // G.711 extremes and zero (docs/providers/g711.md).
        assert_eq!(ulaw_decode(0xFF), 0);
        assert_eq!(ulaw_decode(0x7F), 0);
        assert_eq!(ulaw_decode(0x80), 32_124);
        assert_eq!(ulaw_decode(0x00), -32_124);
        assert_eq!(ulaw_decode(0xFE), 8);
        assert_eq!(ulaw_decode(0x7E), -8);
        assert_eq!(ulaw_decode(0x02), -30_076);
        assert_eq!(alaw_decode(0xD5), 8);
        assert_eq!(alaw_decode(0x55), -8);
        assert_eq!(alaw_decode(0xAA), 32_256);
        assert_eq!(alaw_decode(0x2A), -32_256);
    }

    #[test]
    fn encoders_match_the_reference_for_every_sample() {
        for s in i16::MIN..=i16::MAX {
            assert_eq!(ulaw_encode(s), ulaw_encode_reference(s), "ulaw {s}");
            assert_eq!(ulaw_encode(s), ulaw_stl(s), "ulaw STL {s}");
            assert_eq!(alaw_encode(s), alaw_encode_reference(s), "alaw {s}");
            assert_eq!(alaw_encode(s), alaw_stl(s), "alaw STL {s}");
        }
        // docs/providers/g711.md: silence, the -1 case that separates STL from Sun, extremes.
        assert_eq!(ulaw_encode(0), 0xFF);
        assert_eq!(ulaw_encode(-1), 0x7F);
        assert_eq!(alaw_encode(0), 0xD5);
        assert_eq!(alaw_encode(-1), 0x55);
        assert_eq!(ulaw_encode(i16::MAX), 0x80);
        assert_eq!(ulaw_encode(i16::MIN), 0x00);
        assert_eq!(alaw_encode(i16::MAX), 0xAA);
        assert_eq!(alaw_encode(i16::MIN), 0x2A);
    }

    #[test]
    fn decode_then_encode_is_the_identity_on_codes() {
        for code in 0..=u8::MAX {
            assert_eq!(alaw_encode(alaw_decode(code)), code, "alaw {code:#04x}");
            // μ-law has two zeros (0x7F and 0xFF); both decode to 0, which encodes to 0xFF.
            let expected = if code == 0x7F { 0xFF } else { code };
            assert_eq!(ulaw_encode(ulaw_decode(code)), expected, "ulaw {code:#04x}");
        }
    }

    #[test]
    fn quantization_error_is_bounded_by_the_segment_step() {
        for s in (i16::MIN..=i16::MAX).step_by(7) {
            let u = i32::from(ulaw_decode(ulaw_encode(s)));
            let a = i32::from(alaw_decode(alaw_encode(s)));
            let x = i32::from(s);
            // Largest step: 1024 (μ-law top segment) / 1024 (A-law top segment); clipping at
            // the extremes adds up to 32767 - 32124 = 643 for μ-law.
            assert!((u - x).abs() <= 1_024, "ulaw {s} -> {u}");
            assert!((a - x).abs() <= 1_024, "alaw {s} -> {a}");
        }
    }

    #[test]
    fn decoded_values_are_monotonic_in_magnitude() {
        // Codes 0x80..=0xFF are positive for μ-law, decreasing from 32124 to 0.
        let positive: Vec<i16> = (0x80..=0xFFu8).map(ulaw_decode).collect();
        assert!(positive.windows(2).all(|w| w[0] > w[1]));
    }

    #[test]
    fn buffer_codecs_round_trip_into_caller_buffers() {
        let samples: [i16; 6] = [0, 1_000, -1_000, 32_767, -32_768, 12_345];
        let pcm: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        for codec in [Codec::Pcm16, Codec::Ulaw, Codec::Alaw] {
            let mut encoded = [0u8; 12];
            let n = codec.encode(&pcm, &mut encoded).unwrap();
            assert_eq!(n, codec.encoded_len(pcm.len()));
            let mut decoded = [0u8; 12];
            let m = codec.decode(&encoded[..n], &mut decoded).unwrap();
            assert_eq!(m, pcm.len());
            for (i, s) in samples.iter().enumerate() {
                let got = i16::from_le_bytes([decoded[2 * i], decoded[2 * i + 1]]);
                let want = match codec {
                    Codec::Pcm16 => *s,
                    Codec::Ulaw => ulaw_decode(ulaw_encode(*s)),
                    Codec::Alaw => alaw_decode(alaw_encode(*s)),
                };
                assert_eq!(got, want, "{codec:?} {s}");
            }
        }
    }

    #[test]
    fn sample_apis_match_the_byte_apis() {
        let samples: [i16; 5] = [0, -1, 1_234, i16::MIN, i16::MAX];
        let pcm: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        for codec in [Codec::Pcm16, Codec::Ulaw, Codec::Alaw] {
            let mut from_bytes = [0u8; 10];
            let mut from_samples = [0u8; 10];
            let n = codec.encode(&pcm, &mut from_bytes).unwrap();
            assert_eq!(
                codec.encode_samples(&samples, &mut from_samples).unwrap(),
                n
            );
            assert_eq!(from_bytes[..n], from_samples[..n], "{codec:?}");
            let mut decoded_bytes = [0u8; 10];
            let mut decoded_samples = [0i16; 5];
            codec.decode(&from_bytes[..n], &mut decoded_bytes).unwrap();
            assert_eq!(
                codec
                    .decode_samples(&from_bytes[..n], &mut decoded_samples)
                    .unwrap(),
                5
            );
            let expected: Vec<i16> = decoded_bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes(*b))
                .collect();
            assert_eq!(decoded_samples.to_vec(), expected, "{codec:?}");
        }
        let mut short = [0i16; 1];
        assert!(matches!(
            Codec::Ulaw.decode_samples(&[0, 0], &mut short),
            Err(Error::BufferTooSmall { .. })
        ));
        assert!(matches!(
            Codec::Pcm16.decode_samples(&[0, 0, 0], &mut [0i16; 4]),
            Err(Error::OddPcmLength(3))
        ));
        assert!(matches!(
            Codec::Alaw.encode_samples(&samples, &mut [0u8; 4]),
            Err(Error::BufferTooSmall {
                needed: 5,
                available: 4
            })
        ));
    }

    #[test]
    fn buffer_errors_write_nothing() {
        let mut out = [0xEEu8; 4];
        assert!(matches!(
            Codec::Ulaw.encode(&[0, 0, 0], &mut out),
            Err(Error::OddPcmLength(3))
        ));
        assert!(matches!(
            Codec::Ulaw.encode(&[0; 10], &mut out),
            Err(Error::BufferTooSmall {
                needed: 5,
                available: 4
            })
        ));
        assert!(matches!(
            Codec::Alaw.decode(&[0; 3], &mut out),
            Err(Error::BufferTooSmall {
                needed: 6,
                available: 4
            })
        ));
        assert!(matches!(
            Codec::Pcm16.decode(&[0; 3], &mut out),
            Err(Error::OddPcmLength(3))
        ));
        assert_eq!(out, [0xEE; 4]);
        // A larger buffer is fine; only the needed prefix is written.
        let mut big = [0xEEu8; 8];
        assert_eq!(Codec::Ulaw.decode(&[0xFF], &mut big).unwrap(), 2);
        assert_eq!(big, [0, 0, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE]);
    }
}
