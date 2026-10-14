//! A minimal WAV reader and writer for mono 16-bit PCM, enough for fixtures and recordings.
//!
//! [`parse`] accepts RIFF/WAVE files with a PCM `fmt ` chunk (one channel, 16 bits) and a `data`
//! chunk; other chunks are skipped. A `data` size that overruns the file (a streamed or truncated
//! recording) is clamped to what is there. [`encode`] writes the canonical 44-byte header.
//! No panics: every read is bounds-checked.

use bytes::Bytes;

/// Bytes in the canonical header written by [`encode`].
pub const HEADER_LEN: usize = 44;

/// Why a WAV file was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WavError {
    /// Not a RIFF/WAVE file, or a chunk header is cut off.
    #[error("not a valid WAV file: {0}")]
    Malformed(&'static str),
    /// A valid WAV file in a format this reader does not handle.
    #[error("unsupported WAV format: {0}")]
    Unsupported(&'static str),
}

/// Mono PCM16 audio read from a WAV file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wav {
    /// Samples per second.
    pub sample_rate: u32,
    /// PCM16 little-endian mono samples (a whole number of samples).
    pub pcm: Bytes,
}

fn le_u16(bytes: &[u8], at: usize) -> Option<u16> {
    let raw = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([*raw.first()?, *raw.get(1)?]))
}

fn le_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let raw = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([
        *raw.first()?,
        *raw.get(1)?,
        *raw.get(2)?,
        *raw.get(3)?,
    ]))
}

/// Parses a mono PCM16 WAV file.
///
/// # Errors
/// [`WavError::Malformed`] for a damaged container, [`WavError::Unsupported`] for another sample
/// format or more than one channel.
pub fn parse(bytes: &[u8]) -> Result<Wav, WavError> {
    if bytes.get(..4) != Some(b"RIFF".as_slice()) || bytes.get(8..12) != Some(b"WAVE".as_slice()) {
        return Err(WavError::Malformed("missing RIFF/WAVE header"));
    }
    let mut at = 12usize;
    let mut format: Option<u32> = None;
    while let (Some(id), Some(size)) = (bytes.get(at..at.saturating_add(4)), le_u32(bytes, at + 4))
    {
        let body = at.saturating_add(8);
        let size = usize::try_from(size).unwrap_or(usize::MAX);
        if id == b"fmt " {
            let tag = le_u16(bytes, body).ok_or(WavError::Malformed("short fmt chunk"))?;
            let channels = le_u16(bytes, body + 2).ok_or(WavError::Malformed("short fmt chunk"))?;
            let rate = le_u32(bytes, body + 4).ok_or(WavError::Malformed("short fmt chunk"))?;
            let bits = le_u16(bytes, body + 14).ok_or(WavError::Malformed("short fmt chunk"))?;
            if tag != 1 {
                return Err(WavError::Unsupported("not integer PCM"));
            }
            if channels != 1 {
                return Err(WavError::Unsupported("not mono"));
            }
            if bits != 16 {
                return Err(WavError::Unsupported("not 16 bits per sample"));
            }
            format = Some(rate);
        } else if id == b"data" {
            let sample_rate = format.ok_or(WavError::Malformed("data chunk before fmt chunk"))?;
            let rest = bytes.get(body..).unwrap_or_default();
            let mut data = rest.get(..size.min(rest.len())).unwrap_or_default();
            if data.len() % 2 == 1 {
                data = data.get(..data.len() - 1).unwrap_or_default();
            }
            return Ok(Wav {
                sample_rate,
                pcm: Bytes::copy_from_slice(data),
            });
        }
        // Chunks are padded to an even size.
        at = body.saturating_add(size).saturating_add(size & 1);
    }
    Err(WavError::Malformed("no data chunk"))
}

/// Encodes mono PCM16 `pcm` (little-endian; an odd trailing byte is dropped) as a WAV file.
pub fn encode(sample_rate: u32, pcm: &[u8]) -> Vec<u8> {
    let pcm = pcm.get(..pcm.len() & !1).unwrap_or_default();
    let data_len = u32::try_from(pcm.len()).unwrap_or(!1u32);
    let mut out = Vec::with_capacity(HEADER_LEN + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&data_len.saturating_add(36).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&sample_rate.saturating_mul(2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let pcm: Vec<u8> = (0..200u8).collect();
        let bytes = encode(16_000, &pcm);
        assert_eq!(bytes.len(), HEADER_LEN + 200);
        let wav = parse(&bytes).unwrap();
        assert_eq!(wav.sample_rate, 16_000);
        assert_eq!(wav.pcm.as_ref(), pcm.as_slice());
    }

    #[test]
    fn odd_byte_is_dropped_and_truncated_data_is_clamped() {
        let mut bytes = encode(8_000, &[1, 2, 3, 4]);
        // A data size larger than the file, as written by a streaming recorder.
        bytes[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes.push(9);
        let wav = parse(&bytes).unwrap();
        assert_eq!(wav.pcm.as_ref(), &[1, 2, 3, 4]);
    }

    #[test]
    fn extra_chunks_are_skipped() {
        let plain = encode(16_000, &[5, 6]);
        let mut bytes = plain.get(..36).unwrap().to_vec();
        bytes.extend_from_slice(b"LIST\x03\0\0\0abc\0"); // odd size, padded
        bytes.extend_from_slice(plain.get(36..).unwrap());
        assert_eq!(parse(&bytes).unwrap().pcm.as_ref(), &[5, 6]);
    }

    #[test]
    fn rejects_other_formats_and_garbage() {
        assert!(matches!(parse(b""), Err(WavError::Malformed(_))));
        assert!(matches!(
            parse(b"RIFFxxxxWAVEfmt "),
            Err(WavError::Malformed(_))
        ));
        let mut stereo = encode(16_000, &[0, 0]);
        stereo[22] = 2;
        assert_eq!(parse(&stereo), Err(WavError::Unsupported("not mono")));
        let mut eight = encode(16_000, &[0, 0]);
        eight[34] = 8;
        assert!(matches!(parse(&eight), Err(WavError::Unsupported(_))));
        let mut data_first = encode(16_000, &[0, 0]);
        data_first[12..16].copy_from_slice(b"junk");
        assert!(parse(&data_first).is_err());
    }
}
