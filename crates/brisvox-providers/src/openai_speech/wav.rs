//! A streaming reader for the RIFF/WAVE header in front of a `wav` response.
//!
//! A server that cannot serve headerless `pcm` (Chatterbox-TTS-Server's `/v1/audio/speech`
//! accepts `wav`, `opus` and `mp3` only) sends a WAV file. The header may arrive split over
//! several body chunks, may carry chunks the adapter does not need (`LIST`), and, when the
//! server streams, has no usable sizes (`0` or `0xFFFFFFFF` in the RIFF and `data` fields): the
//! sizes are therefore never trusted; the samples are everything after the `data` chunk
//! header until the body ends.

use bytes::Bytes;

/// Most header bytes buffered before the `data` chunk must have started.
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// What the header said (and the adapter checks against the session).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WavFormat {
    pub(crate) sample_rate: u32,
    pub(crate) channels: u16,
    pub(crate) bits: u16,
}

/// Why a WAV header was refused (never contains payload bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WavError {
    /// Not a RIFF/WAVE file.
    NotWav,
    /// The `data` chunk came before `fmt `, or `fmt ` is malformed.
    BadFormat,
    /// The header is longer than [`MAX_HEADER_BYTES`] without a `data` chunk.
    TooLong,
    /// Not 16-bit PCM mono.
    Unsupported(WavFormat),
}

impl WavError {
    /// Text for an error detail.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::NotWav => "the response is not a WAV file (no RIFF/WAVE header)".to_owned(),
            Self::BadFormat => "the WAV header has no valid fmt chunk before data".to_owned(),
            Self::TooLong => "the WAV header has no data chunk within 64 KiB".to_owned(),
            Self::Unsupported(f) => format!(
                "the WAV audio is {} channel(s) of {}-bit samples at {} Hz; only 16-bit mono PCM \
                 is supported",
                f.channels, f.bits, f.sample_rate
            ),
        }
    }
}

/// Outcome of feeding a chunk.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Fed {
    /// The header is not complete yet.
    NeedMore,
    /// Samples (possibly empty when the chunk ended exactly at the header's end) and the
    /// format, reported once with the first call that completes the header.
    Samples(Bytes, Option<WavFormat>),
}

/// Header state of one response.
#[derive(Debug, Default)]
pub(crate) struct WavReader {
    head: Vec<u8>,
    done: bool,
}

impl WavReader {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feeds the next body chunk.
    ///
    /// # Errors
    /// [`WavError`] for a header that is not usable 16-bit mono PCM.
    pub(crate) fn feed(&mut self, chunk: Bytes) -> Result<Fed, WavError> {
        if self.done {
            return Ok(Fed::Samples(chunk, None));
        }
        // The usual case: the whole header is in the first chunk. Parse it in place and hand
        // out a slice of the chunk, with no copy of header or samples.
        if self.head.is_empty() {
            return match parse(&chunk)? {
                Some((format, data_at)) => {
                    self.done = true;
                    let samples = if data_at <= chunk.len() {
                        chunk.slice(data_at..)
                    } else {
                        Bytes::new()
                    };
                    Ok(Fed::Samples(samples, Some(format)))
                }
                None => {
                    self.head.extend_from_slice(&chunk);
                    self.need_more()
                }
            };
        }
        self.head.extend_from_slice(&chunk);
        match parse(&self.head)? {
            None => self.need_more(),
            Some((format, data_at)) => {
                self.done = true;
                let samples = self
                    .head
                    .get(data_at..)
                    .map(Bytes::copy_from_slice)
                    .unwrap_or_default();
                self.head = Vec::new();
                Ok(Fed::Samples(samples, Some(format)))
            }
        }
    }

    /// The header is incomplete: asks for more, unless it already grew past the limit.
    fn need_more(&self) -> Result<Fed, WavError> {
        if self.head.len() > MAX_HEADER_BYTES {
            return Err(WavError::TooLong);
        }
        Ok(Fed::NeedMore)
    }
}

fn u16_at(buf: &[u8], at: usize) -> Option<u16> {
    let bytes: [u8; 2] = buf.get(at..at.checked_add(2)?)?.try_into().ok()?;
    Some(u16::from_le_bytes(bytes))
}

fn u32_at(buf: &[u8], at: usize) -> Option<u32> {
    let bytes: [u8; 4] = buf.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

/// The format and the offset of the first sample, or `None` while `buf` ends before the
/// `data` chunk header.
fn parse(buf: &[u8]) -> Result<Option<(WavFormat, usize)>, WavError> {
    // RIFF <size> WAVE
    if buf.len() < 12 {
        return if b"RIFF".starts_with(buf.get(..buf.len().min(4)).unwrap_or_default())
            || buf.is_empty()
        {
            Ok(None)
        } else {
            Err(WavError::NotWav)
        };
    }
    if buf.get(..4) != Some(b"RIFF") || buf.get(8..12) != Some(b"WAVE") {
        return Err(WavError::NotWav);
    }
    let mut at = 12_usize;
    let mut format: Option<WavFormat> = None;
    loop {
        let (Some(id), Some(size)) = (buf.get(at..at.saturating_add(4)), u32_at(buf, at + 4))
        else {
            return Ok(None);
        };
        let body = at + 8;
        if id == b"data" {
            let format = format.ok_or(WavError::BadFormat)?;
            if format.bits != 16 || format.channels != 1 {
                return Err(WavError::Unsupported(format));
            }
            return Ok(Some((format, body)));
        }
        let size = usize::try_from(size).map_err(|_| WavError::TooLong)?;
        if size > MAX_HEADER_BYTES {
            return Err(WavError::TooLong);
        }
        if id == b"fmt " {
            if size < 16 {
                return Err(WavError::BadFormat);
            }
            if buf.len() < body + 16 {
                return Ok(None);
            }
            let tag = u16_at(buf, body).ok_or(WavError::BadFormat)?;
            // 1 = PCM, 0xFFFE = WAVE_FORMAT_EXTENSIBLE (the sub-format is not read: the bit
            // depth below decides).
            if tag != 1 && tag != 0xFFFE {
                return Err(WavError::BadFormat);
            }
            format = Some(WavFormat {
                channels: u16_at(buf, body + 2).ok_or(WavError::BadFormat)?,
                sample_rate: u32_at(buf, body + 4).ok_or(WavError::BadFormat)?,
                bits: u16_at(buf, body + 14).ok_or(WavError::BadFormat)?,
            });
        }
        // Chunks are padded to an even size.
        at = body + size + (size & 1);
        if at > MAX_HEADER_BYTES {
            return Err(WavError::TooLong);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    fn header(rate: u32, channels: u16, bits: u16, extra: &[u8], data_size: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&0xFFFF_FFFF_u32.to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(extra);
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16_u32.to_le_bytes());
        v.extend_from_slice(&1_u16.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * u32::from(channels) * u32::from(bits) / 8).to_le_bytes());
        v.extend_from_slice(&(channels * bits / 8).to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_size.to_le_bytes());
        v
    }

    #[test]
    fn whole_header_in_one_chunk() {
        let mut bytes = header(24_000, 1, 16, &[], 0xFFFF_FFFF);
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        let mut reader = WavReader::new();
        let fed = reader.feed(Bytes::from(bytes)).unwrap();
        let Fed::Samples(samples, Some(format)) = fed else {
            panic!("{fed:?}");
        };
        assert_eq!(&samples[..], &[1, 2, 3, 4]);
        assert_eq!(format.sample_rate, 24_000);
        assert_eq!(
            reader.feed(Bytes::from_static(&[9, 9])).unwrap(),
            Fed::Samples(Bytes::from_static(&[9, 9]), None)
        );
    }

    #[test]
    fn a_whole_header_in_the_first_chunk_is_sliced_not_copied() {
        let mut bytes = header(24_000, 1, 16, &[], 0xFFFF_FFFF);
        let header_len = bytes.len();
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        let chunk = Bytes::from(bytes);
        let base = chunk.as_ptr();
        let mut reader = WavReader::new();
        let Fed::Samples(samples, Some(_)) = reader.feed(chunk).unwrap() else {
            panic!("samples and a format expected");
        };
        assert_eq!(&samples[..], &[1, 2, 3, 4]);
        // The samples are a view into the chunk that came in: nothing was copied.
        assert_eq!(samples.as_ptr(), base.wrapping_add(header_len));
        // A chunk that ends exactly at the header's end yields empty samples.
        let mut reader = WavReader::new();
        let Fed::Samples(samples, Some(_)) = reader
            .feed(Bytes::from(header(24_000, 1, 16, &[], 0)))
            .unwrap()
        else {
            panic!("samples and a format expected");
        };
        assert!(samples.is_empty());
    }

    #[test]
    fn header_split_byte_by_byte_with_a_list_chunk() {
        let mut list = Vec::new();
        list.extend_from_slice(b"LIST");
        list.extend_from_slice(&5_u32.to_le_bytes());
        list.extend_from_slice(b"abcde");
        list.push(0); // padding
        let mut bytes = header(24_000, 1, 16, &list, 0);
        bytes.extend_from_slice(&[7, 8]);
        let mut reader = WavReader::new();
        let mut samples = Vec::new();
        let mut formats = 0;
        for byte in bytes {
            match reader.feed(Bytes::copy_from_slice(&[byte])).unwrap() {
                Fed::NeedMore => {}
                Fed::Samples(s, f) => {
                    samples.extend_from_slice(&s);
                    formats += usize::from(f.is_some());
                }
            }
        }
        assert_eq!(samples, [7, 8]);
        assert_eq!(formats, 1);
    }

    #[test]
    fn refuses_what_is_not_16_bit_mono_wav() {
        let mut reader = WavReader::new();
        assert_eq!(
            reader.feed(Bytes::from_static(b"<html>not audio</html>")),
            Err(WavError::NotWav)
        );
        for (channels, bits) in [(2_u16, 16_u16), (1, 8), (1, 32)] {
            let mut reader = WavReader::new();
            let err = reader
                .feed(Bytes::from(header(24_000, channels, bits, &[], 0)))
                .unwrap_err();
            assert!(matches!(err, WavError::Unsupported(_)), "{err:?}");
        }
    }

    #[test]
    fn a_header_that_never_reaches_data_is_bounded() {
        let mut reader = WavReader::new();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF\0\0\0\0WAVE");
        bytes.extend_from_slice(b"LIST");
        bytes.extend_from_slice(&0xFFFF_FFF0_u32.to_le_bytes());
        assert_eq!(reader.feed(Bytes::from(bytes)), Err(WavError::TooLong));
    }
}
