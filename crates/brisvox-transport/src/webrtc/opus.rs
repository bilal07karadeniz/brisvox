//! The Opus codec behind small traits, and libopus as the implementation.
//!
//! WebRTC audio is Opus at an RTP clock of 48 kHz (RFC 7587). The transport needs exactly two
//! things from a codec: encode one 20 ms mono frame of 48 kHz PCM16, and decode one packet (or
//! conceal a lost one) to mono 48 kHz PCM16. [`OpusEncode`], [`OpusDecode`] and [`OpusFactory`]
//! say that and nothing more, so a pure-Rust codec can replace libopus once it passes the same
//! round-trip tests (`docs/providers/webrtc.md`, section 7).
//!
//! [`LibOpusFactory`] uses the `opus` crate (libopus 1.6.1, bundled; building it needs cmake and
//! a C compiler). Per-frame cost on the dev machine (release, 20 ms mono, VoIP, 24 kbit/s):
//! encode 90-190 us at complexity 2-5 (376 us at 9), decode 25-45 us. Both are over the 50 us
//! budget for tokio workers, so the call driver runs them on the inference pool when it is
//! given one (`WebRtcOptions::codec_pool`).

use std::fmt;

/// Samples per millisecond at the Opus RTP clock rate (48 kHz).
pub const SAMPLES_PER_MS: usize = 48;

/// Samples in one 20 ms frame at 48 kHz: the only frame size the transport encodes.
pub const FRAME_SAMPLES: usize = 20 * SAMPLES_PER_MS;

/// Most samples one Opus packet can decode to (120 ms at 48 kHz).
pub const MAX_PACKET_SAMPLES: usize = 120 * SAMPLES_PER_MS;

/// Largest Opus packet libopus produces for one frame (RFC 6716: 1275 bytes per frame).
pub const MAX_PACKET_BYTES: usize = 1275;

/// A codec failure: the rendered libopus (or other implementation) message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecError(pub String);

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "opus codec error: {}", self.0)
    }
}

impl std::error::Error for CodecError {}

impl From<CodecError> for crate::Error {
    fn from(error: CodecError) -> Self {
        Self::WebRtc(error.to_string())
    }
}

/// Encoder tuning. The defaults suit a voice agent: narrowband-to-superwideband speech at
/// 24 kbit/s with in-band FEC for the packet loss a browser path sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusSettings {
    /// Target bit rate in bits per second (RFC 7587 allows 6000 to 510000). Default 24000.
    pub bitrate_bps: i32,
    /// libopus complexity, 0 (fastest) to 10. Default 5: about 185 us per frame at
    /// superwideband on the dev machine; 2 halves that at a small quality cost.
    pub complexity: i32,
    /// In-band FEC: each packet carries a low-rate copy of the previous frame, which a decoder
    /// can use to recover a lost packet. Default on.
    pub inband_fec: bool,
    /// Packet loss the encoder should plan for, in percent, when FEC is on. Default 5.
    pub expected_loss_percent: i32,
}

impl Default for OpusSettings {
    fn default() -> Self {
        Self {
            bitrate_bps: 24_000,
            complexity: 5,
            inband_fec: true,
            expected_loss_percent: 5,
        }
    }
}

/// Encodes mono 48 kHz PCM16 frames of [`FRAME_SAMPLES`] samples to Opus packets.
pub trait OpusEncode: Send {
    /// Encodes one frame (`pcm.len() == FRAME_SAMPLES`) into `out` and returns the packet length.
    ///
    /// # Errors
    /// [`CodecError`] when the frame length is wrong, `out` is too small
    /// ([`MAX_PACKET_BYTES`] always suffices) or the codec fails.
    fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, CodecError>;

    /// Forgets the signal history (a barge-in cut the stream): the next frame starts clean.
    fn reset(&mut self);
}

/// Decodes Opus packets to mono 48 kHz PCM16.
pub trait OpusDecode: Send {
    /// Decodes `packet` into `pcm` (room for [`MAX_PACKET_SAMPLES`] samples is always enough)
    /// and returns the number of samples written.
    ///
    /// # Errors
    /// [`CodecError`] for a corrupt packet or a too small `pcm`.
    fn decode(&mut self, packet: &[u8], pcm: &mut [i16]) -> Result<usize, CodecError>;

    /// Conceals one lost frame: writes `pcm.len()` samples (a multiple of 10 ms) of
    /// extrapolated audio.
    ///
    /// # Errors
    /// [`CodecError`] when the codec fails or `pcm.len()` is not a valid frame size.
    fn conceal(&mut self, pcm: &mut [i16]) -> Result<usize, CodecError>;

    /// Forgets the signal history.
    fn reset(&mut self);
}

/// Creates codec instances, one pair per call.
pub trait OpusFactory: Send + Sync + fmt::Debug {
    /// A new encoder with `settings`.
    ///
    /// # Errors
    /// [`CodecError`] when the codec cannot be created (out of memory, bad settings).
    fn encoder(&self, settings: &OpusSettings) -> Result<Box<dyn OpusEncode>, CodecError>;

    /// A new decoder.
    ///
    /// # Errors
    /// [`CodecError`] when the codec cannot be created.
    fn decoder(&self) -> Result<Box<dyn OpusDecode>, CodecError>;
}

fn codec_error(error: &opus::Error) -> CodecError {
    CodecError(format!("{}: {}", error.function(), error.description()))
}

/// libopus through the `opus` crate.
#[derive(Debug, Clone, Copy, Default)]
pub struct LibOpusFactory;

struct LibEncoder(opus::Encoder);

struct LibDecoder(opus::Decoder);

impl OpusFactory for LibOpusFactory {
    fn encoder(&self, settings: &OpusSettings) -> Result<Box<dyn OpusEncode>, CodecError> {
        let mut encoder = opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip)
            .map_err(|e| codec_error(&e))?;
        encoder
            .set_bitrate(opus::Bitrate::Bits(settings.bitrate_bps))
            .map_err(|e| codec_error(&e))?;
        encoder
            .set_complexity(settings.complexity)
            .map_err(|e| codec_error(&e))?;
        encoder
            .set_signal(opus::Signal::Voice)
            .map_err(|e| codec_error(&e))?;
        // The engine's models and TTS run at 16-24 kHz: nothing above superwideband is worth bits.
        encoder
            .set_max_bandwidth(opus::Bandwidth::Superwideband)
            .map_err(|e| codec_error(&e))?;
        encoder
            .set_inband_fec(settings.inband_fec)
            .map_err(|e| codec_error(&e))?;
        encoder
            .set_packet_loss_perc(settings.expected_loss_percent)
            .map_err(|e| codec_error(&e))?;
        Ok(Box::new(LibEncoder(encoder)))
    }

    fn decoder(&self) -> Result<Box<dyn OpusDecode>, CodecError> {
        let decoder =
            opus::Decoder::new(48_000, opus::Channels::Mono).map_err(|e| codec_error(&e))?;
        Ok(Box::new(LibDecoder(decoder)))
    }
}

impl OpusEncode for LibEncoder {
    fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, CodecError> {
        if pcm.len() != FRAME_SAMPLES {
            return Err(CodecError(format!(
                "encoder takes {FRAME_SAMPLES}-sample frames, got {}",
                pcm.len()
            )));
        }
        self.0.encode(pcm, out).map_err(|e| codec_error(&e))
    }

    fn reset(&mut self) {
        // A failed reset leaves the previous state, which only costs a click after a barge-in.
        let _ = self.0.reset_state();
    }
}

impl OpusDecode for LibDecoder {
    fn decode(&mut self, packet: &[u8], pcm: &mut [i16]) -> Result<usize, CodecError> {
        if packet.is_empty() {
            return Err(CodecError("empty packet".to_owned()));
        }
        self.0
            .decode(packet, pcm, false)
            .map_err(|e| codec_error(&e))
    }

    fn conceal(&mut self, pcm: &mut [i16]) -> Result<usize, CodecError> {
        // An empty input asks libopus for packet loss concealment of `pcm.len()` samples.
        self.0.decode(&[], pcm, false).map_err(|e| codec_error(&e))
    }

    fn reset(&mut self) {
        let _ = self.0.reset_state();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, frames: usize, amplitude: f32) -> Vec<i16> {
        (0..frames * FRAME_SAMPLES)
            .map(|n| {
                let phase = 2.0 * std::f32::consts::PI * hz * n as f32 / 48_000.0;
                (phase.sin() * amplitude) as i16
            })
            .collect()
    }

    /// Signal-to-noise ratio in dB of `decoded` against `original`, searching the lag (codec
    /// look-ahead) that aligns them best.
    fn snr_db(original: &[i16], decoded: &[i16]) -> (f64, usize) {
        let mut best = (f64::NEG_INFINITY, 0);
        for lag in 0..600 {
            let n = original.len().saturating_sub(lag + 2_000);
            let (mut signal, mut noise) = (0.0f64, 0.0f64);
            for i in 1_000..n {
                let a = f64::from(original[i]);
                let b = f64::from(decoded[i + lag]);
                signal += a * a;
                noise += (a - b) * (a - b);
            }
            let snr = 10.0 * (signal / noise.max(1.0)).log10();
            if snr > best.0 {
                best = (snr, lag);
            }
        }
        best
    }

    fn round_trip(pcm: &[i16], settings: &OpusSettings) -> Vec<i16> {
        let factory = LibOpusFactory;
        let mut encoder = factory.encoder(settings).unwrap();
        let mut decoder = factory.decoder().unwrap();
        let mut packet = [0u8; MAX_PACKET_BYTES];
        let mut out = Vec::new();
        let mut buffer = vec![0i16; MAX_PACKET_SAMPLES];
        for frame in pcm.as_chunks::<FRAME_SAMPLES>().0 {
            let len = encoder.encode(frame, &mut packet).unwrap();
            assert!(len > 0 && len <= MAX_PACKET_BYTES);
            let samples = decoder.decode(&packet[..len], &mut buffer).unwrap();
            assert_eq!(samples, FRAME_SAMPLES);
            out.extend_from_slice(&buffer[..samples]);
        }
        out
    }

    #[test]
    fn a_1_khz_tone_survives_the_round_trip() {
        let pcm = tone(1_000.0, 100, 8_000.0);
        let decoded = round_trip(&pcm, &OpusSettings::default());
        let (snr, lag) = snr_db(&pcm, &decoded);
        println!("1 kHz tone: SNR {snr:.1} dB at a lag of {lag} samples ({lag}/48 ms)");
        assert!(snr > 20.0, "SNR {snr:.1} dB");
        // libopus' look-ahead is 6.5 ms at 48 kHz (312 samples) plus the decoder's delay.
        assert!(lag <= 400, "codec delay {lag} samples");
    }

    #[test]
    fn a_speech_fixture_survives_the_round_trip() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/en_two_utterances.wav");
        let wav = crate::wav::parse(&std::fs::read(path).unwrap()).unwrap();
        // Upsample 3x by linear interpolation: the codec test needs speech at 48 kHz, not a
        // good resampler (the transport uses StreamResampler).
        let mono: Vec<i16> = wav
            .pcm
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect();
        assert_eq!(wav.sample_rate, 16_000);
        let mut pcm = Vec::with_capacity(mono.len() * 3);
        for pair in mono.windows(2).take(16_000 * 4) {
            let (a, b) = (i32::from(pair[0]), i32::from(pair[1]));
            pcm.push(pair[0]);
            pcm.push((a + (b - a) / 3) as i16);
            pcm.push((a + 2 * (b - a) / 3) as i16);
        }
        pcm.truncate(pcm.len() / FRAME_SAMPLES * FRAME_SAMPLES);
        let decoded = round_trip(&pcm, &OpusSettings::default());
        let (snr, lag) = snr_db(&pcm, &decoded);
        println!("speech fixture: SNR {snr:.1} dB at a lag of {lag} samples");
        // Waveform SNR of a perceptual codec on speech is modest; 8 dB at 24 kbit/s is typical.
        assert!(snr > 6.0, "SNR {snr:.1} dB");
        assert!(lag <= 400, "codec delay {lag} samples");
    }

    #[test]
    fn the_encoder_rejects_a_wrong_frame_size() {
        let mut encoder = LibOpusFactory.encoder(&OpusSettings::default()).unwrap();
        let mut packet = [0u8; MAX_PACKET_BYTES];
        assert!(encoder.encode(&[0i16; 100], &mut packet).is_err());
    }

    #[test]
    fn concealment_fills_the_gap_and_a_corrupt_packet_is_an_error() {
        let pcm = tone(440.0, 10, 6_000.0);
        let factory = LibOpusFactory;
        let mut encoder = factory.encoder(&OpusSettings::default()).unwrap();
        let mut decoder = factory.decoder().unwrap();
        let mut packet = [0u8; MAX_PACKET_BYTES];
        let mut buffer = vec![0i16; MAX_PACKET_SAMPLES];
        for frame in pcm.as_chunks::<FRAME_SAMPLES>().0 {
            let len = encoder.encode(frame, &mut packet).unwrap();
            decoder.decode(&packet[..len], &mut buffer).unwrap();
        }
        let n = decoder.conceal(&mut buffer[..FRAME_SAMPLES]).unwrap();
        assert_eq!(n, FRAME_SAMPLES);
        assert!(buffer[..n].iter().any(|s| *s != 0), "PLC returned silence");
        assert!(decoder.decode(&[], &mut buffer).is_err());
    }

    #[test]
    fn encoder_settings_reach_libopus() {
        let low = OpusSettings {
            bitrate_bps: 8_000,
            ..OpusSettings::default()
        };
        let high = OpusSettings {
            bitrate_bps: 64_000,
            ..OpusSettings::default()
        };
        let pcm = tone(300.0, 1, 8_000.0);
        let size = |settings: &OpusSettings| {
            let mut encoder = LibOpusFactory.encoder(settings).unwrap();
            let mut packet = [0u8; MAX_PACKET_BYTES];
            // Let the rate control settle on a few frames.
            let mut len = 0;
            for _ in 0..10 {
                len = encoder.encode(&pcm, &mut packet).unwrap();
            }
            len
        };
        assert!(size(&low) < size(&high));
    }
}
