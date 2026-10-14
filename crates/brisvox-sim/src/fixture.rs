//! Real-speech fixtures with exact ground truth.
//!
//! A fixture is `fixtures/<name>.wav` (PCM16 LE mono) plus `fixtures/<name>.json`, written by
//! `scripts/fixtures/build_fixtures.py`: clips of real speech (silence trimmed) concatenated
//! with known gaps, so every speech segment's start and end are known to the sample.
//!
//! ```json
//! { "name": "en_tr_mixed", "sample_rate": 16000, "samples": 194240,
//!   "segments": [ { "start": 6400, "end": 71200, "lang": "en", "text": "...",
//!                   "source": "fleurs:en_us:dev:<clip>.wav" } ] }
//! ```
//!
//! [`Fixture::play`] feeds the audio into a pipeline exactly like
//! [`play_input`](crate::scenario::play_input) does for synthetic scripts (20 ms frames on the
//! clock; slices of one buffer, no copies).

use std::path::{Path, PathBuf};
use std::time::Duration;

use brisvox_core::clock::wait_until;
use brisvox_core::frame::{AudioFrame, SampleRate};
use brisvox_core::{SharedClock, TurnEpoch};
use bytes::Bytes;
use serde::Deserialize;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::input::InputSink;
use crate::mock_stt::SttUtterance;
use crate::scenario::FRAME;

/// One speech segment of a fixture (sample offsets into the fixture audio).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FixtureSegment {
    /// First speech sample.
    pub start: u64,
    /// One past the last speech sample.
    pub end: u64,
    /// BCP-47-ish language tag of the clip (`en`, `tr`).
    pub lang: String,
    /// Transcript of the clip.
    pub text: String,
    /// Where the clip came from (corpus, subset, id).
    pub source: String,
}

#[derive(Debug, Deserialize)]
struct Sidecar {
    name: String,
    sample_rate: u32,
    samples: u64,
    segments: Vec<FixtureSegment>,
}

/// A loaded fixture: audio and ground truth.
#[derive(Debug, Clone)]
pub struct Fixture {
    /// Fixture name.
    pub name: String,
    /// Sample rate of `pcm`.
    pub sample_rate: SampleRate,
    /// PCM16 LE mono audio.
    pub pcm: Bytes,
    /// Speech segments, in order.
    pub segments: Vec<FixtureSegment>,
}

/// The repository's `fixtures/` directory (from this crate's manifest location).
///
/// It exists in a checkout of the repository only: the speech fixtures (audio under CC BY-SA 4.0)
/// are not part of the published crate, so with the crate from crates.io pass your own directory
/// to [`Fixture::load`].
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
}

fn invalid(what: String) -> Error {
    Error::Fixture(what)
}

impl Fixture {
    /// Loads `<dir>/<name>.wav` and `<dir>/<name>.json`, checking that they agree.
    ///
    /// # Errors
    /// [`Error::Fixture`] if a file is missing, malformed, or inconsistent with the other.
    pub fn load(dir: &Path, name: &str) -> Result<Self> {
        let json = std::fs::read_to_string(dir.join(format!("{name}.json")))
            .map_err(|e| invalid(format!("{name}.json: {e}")))?;
        let sidecar: Sidecar =
            serde_json::from_str(&json).map_err(|e| invalid(format!("{name}.json: {e}")))?;
        let wav = std::fs::read(dir.join(format!("{name}.wav")))
            .map_err(|e| invalid(format!("{name}.wav: {e}")))?;
        let (rate, pcm) = parse_wav(&wav).map_err(|e| invalid(format!("{name}.wav: {e}")))?;
        let sample_rate = SampleRate::try_from(rate).map_err(Error::Core)?;
        let samples = u64::try_from(pcm.len() / 2).unwrap_or(u64::MAX);
        if rate != sidecar.sample_rate || samples != sidecar.samples || sidecar.name != name {
            return Err(invalid(format!(
                "{name}: sidecar ({} Hz, {} samples, name {}) does not match audio ({rate} Hz, \
                 {samples} samples)",
                sidecar.sample_rate, sidecar.samples, sidecar.name
            )));
        }
        let mut previous_end = 0;
        for segment in &sidecar.segments {
            if segment.start < previous_end || segment.end <= segment.start || segment.end > samples
            {
                return Err(invalid(format!("{name}: bad segment {segment:?}")));
            }
            previous_end = segment.end;
        }
        Ok(Self {
            name: sidecar.name,
            sample_rate,
            pcm: Bytes::from(pcm),
            segments: sidecar.segments,
        })
    }

    /// Converts a sample offset to time.
    pub fn time_of(&self, sample: u64) -> Duration {
        let micros = sample.saturating_mul(1_000_000) / u64::from(self.sample_rate.hz());
        Duration::from_micros(micros)
    }

    /// Total length.
    pub fn duration(&self) -> Duration {
        self.time_of(u64::try_from(self.pcm.len() / 2).unwrap_or(u64::MAX))
    }

    /// Speech segments as (start, end) times: the exact ground truth.
    pub fn speech_segments(&self) -> Vec<(Duration, Duration)> {
        self.segments
            .iter()
            .map(|s| (self.time_of(s.start), self.time_of(s.end)))
            .collect()
    }

    /// What the mock STT should recognise for each segment (the transcripts).
    pub fn utterances(&self) -> Vec<SttUtterance> {
        self.segments
            .iter()
            .map(|s| SttUtterance::words(&s.text))
            .collect()
    }

    /// Feeds the fixture into `input` as 20 ms frames on `clock` from `start` (the simulated
    /// input transport). Returns when the audio ends, `cancel` fires, or the pipeline is gone.
    ///
    /// `input` is an [`InputSink`]; a pipeline input ([`brisvox_core::FrameSender`]) converts
    /// into one.
    ///
    /// # Errors
    /// Only if a frame cannot be built.
    pub async fn play(
        &self,
        clock: SharedClock,
        start: Instant,
        input: impl Into<InputSink>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let input: InputSink = input.into();
        let rate = u64::from(self.sample_rate.hz());
        let frame_bytes = usize::try_from(rate * 2 / 50).unwrap_or(640);
        let mut offset = 0usize;
        let mut index = 0u32;
        while offset < self.pcm.len() {
            let end = (offset + frame_bytes).min(self.pcm.len());
            let at = start + FRAME * index;
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(()),
                () = wait_until(&*clock, at) => {}
            }
            let frame = AudioFrame::new(
                self.pcm.slice(offset..end),
                self.sample_rate,
                at,
                u64::from(index),
                TurnEpoch::ZERO,
            )
            .map_err(Error::Core)?;
            if !input.send_bounded(&*clock, frame, cancel).await? {
                return Ok(());
            }
            offset = end;
            index = index.saturating_add(1);
        }
        Ok(())
    }
}

/// Parses a canonical RIFF/WAVE file with PCM16 mono data: returns (sample rate, PCM bytes).
fn parse_wav(bytes: &[u8]) -> std::result::Result<(u32, Vec<u8>), String> {
    let le_u32 = |b: &[u8], at: usize| -> Option<u32> {
        b.get(at..at + 4)
            .and_then(|s| <[u8; 4]>::try_from(s).ok())
            .map(u32::from_le_bytes)
    };
    let le_u16 = |b: &[u8], at: usize| -> Option<u16> {
        b.get(at..at + 2)
            .and_then(|s| <[u8; 2]>::try_from(s).ok())
            .map(u16::from_le_bytes)
    };
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err("not a RIFF/WAVE file".into());
    }
    let mut at = 12usize;
    let mut format: Option<(u16, u16, u32, u16)> = None;
    while let (Some(id), Some(size)) = (bytes.get(at..at + 4), le_u32(bytes, at + 4)) {
        let size = usize::try_from(size).map_err(|_| "chunk too large".to_owned())?;
        let body = at + 8;
        let end = body
            .checked_add(size)
            .ok_or_else(|| "chunk size overflows".to_owned())?;
        match id {
            b"fmt " => {
                format = Some((
                    le_u16(bytes, body).ok_or("short fmt chunk")?,
                    le_u16(bytes, body + 2).ok_or("short fmt chunk")?,
                    le_u32(bytes, body + 4).ok_or("short fmt chunk")?,
                    le_u16(bytes, body + 14).ok_or("short fmt chunk")?,
                ));
            }
            b"data" => {
                let (audio_format, channels, rate, bits) =
                    format.ok_or("data chunk before fmt chunk")?;
                if audio_format != 1 || channels != 1 || bits != 16 {
                    return Err(format!(
                        "need PCM16 mono, got format {audio_format}, {channels} ch, {bits} bit"
                    ));
                }
                if size % 2 != 0 {
                    return Err("odd-length PCM16 data chunk".into());
                }
                let data = bytes.get(body..end).ok_or("data chunk runs past the end")?;
                return Ok((rate, data.to_vec()));
            }
            _ => {}
        }
        // Chunks are padded to even sizes.
        at = end.saturating_add(size % 2);
    }
    Err("no data chunk".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal PCM16 mono WAV.
    fn wav(rate: u32, samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        wav_raw(1, 1, rate, 16, &data)
    }

    /// A WAV with the given format fields and raw data bytes (data chunk after fmt and LIST).
    fn wav_raw(format: u16, channels: u16, rate: u32, bits: u16, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + u32::try_from(data.len()).unwrap()).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&format.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(b"LIST");
        out.extend_from_slice(&3u32.to_le_bytes()); // odd-sized chunk: padded
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(b"data");
        out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    /// A per-test temporary directory, removed on drop (also when the test fails).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("brisvox-fixture-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A RIFF/WAVE header followed by one chunk.
    fn riff_with_chunk(id: &[u8; 4], size: u32, body: &[u8]) -> Vec<u8> {
        let mut out = b"RIFF\0\0\0\0WAVE".to_vec();
        out.extend_from_slice(id);
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn rejects_unsupported_or_malformed_wavs() {
        let pcm = [0u8; 8];
        for (case, bytes) in [
            ("stereo", wav_raw(1, 2, 16_000, 16, &pcm)),
            ("8-bit", wav_raw(1, 1, 16_000, 8, &pcm)),
            ("float", wav_raw(3, 1, 16_000, 32, &pcm)),
            ("odd length", wav_raw(1, 1, 16_000, 16, &[0u8; 7])),
            ("data before fmt", riff_with_chunk(b"data", 2, &[0, 0])),
            ("short fmt", riff_with_chunk(b"fmt ", 4, &[1, 0, 1, 0])),
            ("overflowing size", riff_with_chunk(b"JUNK", u32::MAX, &[])),
        ] {
            assert!(parse_wav(&bytes).is_err(), "{case}");
        }
        let good = wav(16_000, &[1, 2, 3, 4]);
        assert!(parse_wav(&good).is_ok());
        assert!(
            parse_wav(&good[..good.len() - 1]).is_err(),
            "data past the end"
        );
        assert!(
            parse_wav(&good[..good.len() - 16]).is_err(),
            "no data chunk"
        );
    }

    #[test]
    fn parses_pcm16_mono_and_skips_other_chunks() {
        let (rate, pcm) = parse_wav(&wav(16_000, &[1, -2, 3])).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(pcm, [1, 0, 254, 255, 3, 0]);
        assert!(parse_wav(b"not a wav").is_err());
    }

    #[test]
    fn load_checks_the_sidecar_against_the_audio() {
        let tmp = TempDir::new("load");
        let dir = &tmp.0;
        std::fs::write(dir.join("t.wav"), wav(16_000, &[0; 32_000])).unwrap();
        let sidecar = |samples: u64, end: u64| {
            format!(
                r#"{{"name":"t","sample_rate":16000,"samples":{samples},
                    "segments":[{{"start":8000,"end":{end},"lang":"en","text":"hi there",
                    "source":"test"}}]}}"#
            )
        };
        std::fs::write(dir.join("t.json"), sidecar(32_000, 24_000)).unwrap();
        let f = Fixture::load(dir, "t").unwrap();
        assert_eq!(f.duration(), Duration::from_secs(2));
        assert_eq!(
            f.speech_segments(),
            [(Duration::from_millis(500), Duration::from_millis(1_500))]
        );
        assert_eq!(f.utterances()[0].finals, ["hi there"]);
        // Inconsistent sidecars are rejected.
        std::fs::write(dir.join("t.json"), sidecar(31_999, 24_000)).unwrap();
        assert!(matches!(Fixture::load(dir, "t"), Err(Error::Fixture(_))));
        std::fs::write(dir.join("t.json"), sidecar(32_000, 40_000)).unwrap();
        assert!(matches!(Fixture::load(dir, "t"), Err(Error::Fixture(_))));
        // Overlapping or unordered segments are rejected; touching ones are fine.
        let two = |a: (u64, u64), b: (u64, u64)| {
            format!(
                r#"{{"name":"t","sample_rate":16000,"samples":32000,"segments":[
                    {{"start":{},"end":{},"lang":"en","text":"a","source":"test"}},
                    {{"start":{},"end":{},"lang":"en","text":"b","source":"test"}}]}}"#,
                a.0, a.1, b.0, b.1
            )
        };
        std::fs::write(dir.join("t.json"), two((0, 100), (100, 200))).unwrap();
        assert!(Fixture::load(dir, "t").is_ok());
        std::fs::write(dir.join("t.json"), two((0, 100), (99, 200))).unwrap();
        assert!(matches!(Fixture::load(dir, "t"), Err(Error::Fixture(_))));
        std::fs::write(dir.join("t.json"), two((100, 200), (0, 50))).unwrap();
        assert!(matches!(Fixture::load(dir, "t"), Err(Error::Fixture(_))));
        // A sidecar naming another fixture is rejected.
        let renamed = sidecar(32_000, 24_000).replace(r#""name":"t""#, r#""name":"u""#);
        std::fs::write(dir.join("t.json"), renamed).unwrap();
        assert!(matches!(Fixture::load(dir, "t"), Err(Error::Fixture(_))));
    }
}
