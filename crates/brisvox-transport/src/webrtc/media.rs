//! The audio path of one WebRTC call: Opus packets to engine PCM and back.
//!
//! [`MediaPath`] owns everything that costs CPU per frame: the Opus decoder and encoder and the
//! two [`StreamResampler`]s (48 kHz to the engine's input rate, the engine's output rate to
//! 48 kHz). It has no sockets, no str0m and no clock, so it is tested on its own and the call
//! driver decides where it runs (inline, or on the inference pool).
//!
//! ```text
//!   RTP/Opus --decode--> 48 kHz --resample--> input rate PCM16  (to the engine)
//!   output rate PCM16 --resample--> 48 kHz --20 ms frames--> encode --> RTP/Opus
//! ```
//!
//! Buffers are allocated once, at construction. The decoded PCM leaves as `Bytes` cut from one
//! shared chunk (no allocation per frame); an encoded packet is one `Arc<[u8]>` because str0m
//! takes ownership of the payload.

use std::sync::Arc;

use brisvox_audio::StreamResampler;
use brisvox_core::frame::SampleRate;
use bytes::{Bytes, BytesMut};

use super::opus::{
    FRAME_SAMPLES, MAX_PACKET_BYTES, MAX_PACKET_SAMPLES, OpusDecode, OpusEncode, OpusFactory,
    OpusSettings,
};
use crate::error::{Error, Result};

/// Allocation unit of the decoded-PCM chunk: one allocation serves many frames (a 20 ms frame
/// at 16 kHz is 640 bytes).
const PCM_CHUNK: usize = 32 * 1024;

fn resampler_error(error: &brisvox_audio::Error) -> Error {
    Error::WebRtc(format!("resampler: {error}"))
}

/// The per-call audio path (see the module docs).
pub struct MediaPath {
    decoder: Box<dyn OpusDecode>,
    encoder: Box<dyn OpusEncode>,
    rx_resampler: StreamResampler,
    tx_resampler: StreamResampler,
    /// Decoder output (up to 120 ms at 48 kHz).
    decoded: Vec<i16>,
    /// Resampled decoder output, at the input rate.
    rx_out: Vec<i16>,
    /// Chunk the decoded PCM bytes are cut from.
    rx_bytes: BytesMut,
    /// Engine PCM converted to samples.
    tx_in: Vec<i16>,
    /// The same at 48 kHz.
    tx_48: Vec<i16>,
    /// 48 kHz samples waiting for a whole 20 ms frame.
    tx_pending: Vec<i16>,
    packet: Vec<u8>,
}

impl std::fmt::Debug for MediaPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaPath")
            .field("rx", &self.rx_resampler)
            .field("tx", &self.tx_resampler)
            .field("tx_pending", &self.tx_pending.len())
            .finish_non_exhaustive()
    }
}

impl MediaPath {
    /// Builds the path for a call whose caller audio goes to the engine at `input` and whose
    /// bot audio arrives from the engine at `output`.
    ///
    /// Costs a few hundred microseconds (codec and FFT plan allocation): call it at session
    /// setup on a blocking thread, not per frame and not on an async worker.
    ///
    /// # Errors
    /// [`Error::WebRtc`] when a codec or a resampler cannot be created.
    pub fn new(
        factory: &dyn OpusFactory,
        settings: &OpusSettings,
        input: SampleRate,
        output: SampleRate,
    ) -> Result<Self> {
        let decoder = factory.decoder()?;
        let encoder = factory.encoder(settings)?;
        let rx_resampler =
            StreamResampler::new(SampleRate::Hz48000, input).map_err(|e| resampler_error(&e))?;
        let tx_resampler =
            StreamResampler::new(output, SampleRate::Hz48000).map_err(|e| resampler_error(&e))?;
        let rx_capacity = rx_resampler
            .max_output_len(MAX_PACKET_SAMPLES + rx_resampler.chunk_frames())
            .max(FRAME_SAMPLES);
        // A 20 ms engine frame is the common case; keep room for several.
        let tx_input_capacity = output_samples(output, 100);
        let tx_capacity = tx_resampler
            .max_output_len(tx_input_capacity + tx_resampler.chunk_frames())
            .max(FRAME_SAMPLES);
        Ok(Self {
            decoder,
            encoder,
            rx_resampler,
            tx_resampler,
            decoded: vec![0; MAX_PACKET_SAMPLES],
            rx_out: vec![0; rx_capacity],
            rx_bytes: BytesMut::with_capacity(PCM_CHUNK),
            tx_in: Vec::with_capacity(tx_input_capacity),
            tx_48: vec![0; tx_capacity],
            tx_pending: Vec::with_capacity(FRAME_SAMPLES * 8),
            packet: vec![0; MAX_PACKET_BYTES],
        })
    }

    /// Decodes `packet` after concealing `lost` missing frames, and appends the PCM16 bytes at
    /// the input rate (one `Bytes` per concealed frame and one for the packet; a call that
    /// yields no samples yet appends nothing) to `out`. Returns the 48 kHz samples the packet
    /// held, which the caller adds to the RTP timestamp to find the next expected one.
    ///
    /// # Errors
    /// [`Error::WebRtc`] for a corrupt packet or a resampler failure. The path stays usable.
    pub fn decode(&mut self, packet: &[u8], lost: u32, out: &mut Vec<Bytes>) -> Result<usize> {
        for _ in 0..lost {
            let n = self
                .decoder
                .conceal(self.decoded.get_mut(..FRAME_SAMPLES).unwrap_or_default())?;
            self.push_decoded(n, out)?;
        }
        let n = self.decoder.decode(packet, &mut self.decoded)?;
        self.push_decoded(n, out)?;
        Ok(n)
    }

    fn push_decoded(&mut self, samples: usize, out: &mut Vec<Bytes>) -> Result<()> {
        let decoded = self.decoded.get(..samples).unwrap_or_default();
        let produced = self
            .rx_resampler
            .process(decoded, &mut self.rx_out)
            .map_err(|e| resampler_error(&e))?;
        let pcm = self.rx_out.get(..produced).unwrap_or_default();
        if pcm.is_empty() {
            return Ok(());
        }
        let bytes = pcm.len() * 2;
        if self.rx_bytes.capacity() < bytes {
            self.rx_bytes.reserve(bytes.max(PCM_CHUNK));
        }
        for sample in pcm {
            self.rx_bytes.extend_from_slice(&sample.to_le_bytes());
        }
        out.push(self.rx_bytes.split().freeze());
        Ok(())
    }

    /// Takes engine audio (PCM16 LE at the output rate), and appends one Opus packet to `out`
    /// for every 20 ms that is complete. Less than 20 ms stays buffered for the next call (or
    /// for [`flush`](Self::flush)).
    ///
    /// # Errors
    /// [`Error::WebRtc`] when the encoder or the resampler fails. The audio of this call may be
    /// lost, the path stays usable.
    pub fn encode(&mut self, pcm: &[u8], out: &mut Vec<Arc<[u8]>>) -> Result<()> {
        self.tx_in.clear();
        self.tx_in.extend(
            pcm.as_chunks::<2>()
                .0
                .iter()
                .map(|pair| i16::from_le_bytes(*pair)),
        );
        let needed = self.tx_resampler.max_output_len(self.tx_in.len());
        if self.tx_48.len() < needed {
            self.tx_48.resize(needed, 0);
        }
        let produced = self
            .tx_resampler
            .process(&self.tx_in, &mut self.tx_48)
            .map_err(|e| resampler_error(&e))?;
        self.tx_pending
            .extend_from_slice(self.tx_48.get(..produced).unwrap_or_default());
        self.drain_frames(out)
    }

    fn drain_frames(&mut self, out: &mut Vec<Arc<[u8]>>) -> Result<()> {
        let mut used = 0;
        let mut result = Ok(());
        while let Some(frame) = self.tx_pending.get(used..used + FRAME_SAMPLES) {
            used += FRAME_SAMPLES;
            match self.encoder.encode(frame, &mut self.packet) {
                Ok(len) => out.push(Arc::from(self.packet.get(..len).unwrap_or_default())),
                Err(error) => {
                    result = Err(error.into());
                    break;
                }
            }
        }
        self.tx_pending.drain(..used);
        result
    }

    /// The end of an utterance: pushes the resampler's tail and the partial frame (padded with
    /// silence to 20 ms) through the encoder, so the last milliseconds are not held back until
    /// the next utterance.
    ///
    /// # Errors
    /// As [`encode`](Self::encode).
    pub fn flush(&mut self, out: &mut Vec<Arc<[u8]>>) -> Result<()> {
        let tail = self.tx_resampler.max_flush_len();
        if self.tx_48.len() < tail {
            self.tx_48.resize(tail, 0);
        }
        let produced = self
            .tx_resampler
            .flush(&mut self.tx_48)
            .map_err(|e| resampler_error(&e))?;
        self.tx_pending
            .extend_from_slice(self.tx_48.get(..produced).unwrap_or_default());
        if !self.tx_pending.is_empty() && !self.tx_pending.len().is_multiple_of(FRAME_SAMPLES) {
            let padded = self.tx_pending.len().div_ceil(FRAME_SAMPLES) * FRAME_SAMPLES;
            self.tx_pending.resize(padded, 0);
        }
        self.drain_frames(out)
    }

    /// A barge-in: drops the buffered output audio and the encoder's history.
    pub fn clear_output(&mut self) {
        self.tx_resampler.reset();
        self.tx_pending.clear();
        self.encoder.reset();
    }

    /// Samples at the output rate that [`encode`](Self::encode) currently holds back (resampler
    /// plus partial frame), as 48 kHz samples: what a mark would still have to flush.
    pub fn buffered_output(&self) -> usize {
        self.tx_pending.len()
    }
}

/// Samples of `ms` milliseconds at `rate`.
fn output_samples(rate: SampleRate, ms: u32) -> usize {
    usize::try_from(rate.hz() / 1000 * ms).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webrtc::opus::LibOpusFactory;

    fn path(input: SampleRate, output: SampleRate) -> MediaPath {
        MediaPath::new(&LibOpusFactory, &OpusSettings::default(), input, output).unwrap()
    }

    fn tone_bytes(rate: SampleRate, hz: f32, ms: u32, offset: usize) -> Vec<u8> {
        let count = output_samples(rate, ms);
        (offset..offset + count)
            .flat_map(|n| {
                let phase = 2.0 * std::f32::consts::PI * hz * n as f32 / rate.hz() as f32;
                ((phase.sin() * 8_000.0) as i16).to_le_bytes()
            })
            .collect()
    }

    fn samples(bytes: &[Bytes]) -> Vec<i16> {
        bytes
            .iter()
            .flat_map(|b| b.as_chunks::<2>().0.iter().map(|p| i16::from_le_bytes(*p)))
            .collect()
    }

    /// Rate of zero crossings per second: a cheap pitch estimate.
    fn frequency(pcm: &[i16], rate: u32) -> f32 {
        let crossings = pcm.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count();
        crossings as f32 * rate as f32 / pcm.len() as f32
    }

    #[test]
    fn engine_audio_goes_out_in_whole_20_ms_packets() {
        let mut path = path(SampleRate::Hz16000, SampleRate::Hz24000);
        let mut packets = Vec::new();
        // 10 engine frames of 20 ms: ten packets, minus what the resampler holds back.
        for n in 0..10 {
            let frame = tone_bytes(SampleRate::Hz24000, 500.0, 20, n * 480);
            path.encode(&frame, &mut packets).unwrap();
        }
        assert!(
            (9..=10).contains(&packets.len()),
            "{} packets",
            packets.len()
        );
        assert!(packets.iter().all(|p| !p.is_empty()));
        // The tail leaves with a flush, padded to a whole frame.
        path.flush(&mut packets).unwrap();
        // 200 ms of input plus the resampler's 3.3 ms delay: ten whole frames and a padded tail.
        assert!(
            (10..=11).contains(&packets.len()),
            "flush did not emit the held-back audio: {}",
            packets.len()
        );
        assert_eq!(path.buffered_output(), 0);
    }

    #[test]
    fn a_partial_frame_waits_and_is_not_lost() {
        let mut path = path(SampleRate::Hz16000, SampleRate::Hz24000);
        let mut packets = Vec::new();
        path.encode(&tone_bytes(SampleRate::Hz24000, 500.0, 5, 0), &mut packets)
            .unwrap();
        assert!(packets.is_empty(), "5 ms must not make a packet");
        path.flush(&mut packets).unwrap();
        assert_eq!(packets.len(), 1);
    }

    #[test]
    fn clear_output_drops_the_buffer() {
        let mut path = path(SampleRate::Hz16000, SampleRate::Hz24000);
        let mut packets = Vec::new();
        path.encode(&tone_bytes(SampleRate::Hz24000, 500.0, 15, 0), &mut packets)
            .unwrap();
        path.clear_output();
        path.flush(&mut packets).unwrap();
        assert!(packets.is_empty(), "audio from before the clear came out");
    }

    #[test]
    fn audio_survives_the_whole_loop_at_the_right_pitch_and_length() {
        // Out through the encoder, straight back through the decoder: 24 kHz in, 16 kHz out.
        let mut path = path(SampleRate::Hz16000, SampleRate::Hz24000);
        let mut packets = Vec::new();
        let mut pcm_out = Vec::new();
        for n in 0..50 {
            let frame = tone_bytes(SampleRate::Hz24000, 1_000.0, 20, n * 480);
            path.encode(&frame, &mut packets).unwrap();
        }
        path.flush(&mut packets).unwrap();
        for packet in &packets {
            let mut chunk = Vec::new();
            let n = path.decode(packet, 0, &mut chunk).unwrap();
            assert_eq!(n, FRAME_SAMPLES);
            pcm_out.extend(chunk);
        }
        let decoded = samples(&pcm_out);
        // One second in, one second out at 16 kHz (give or take the filter tail).
        assert!(
            (15_000..=17_000).contains(&decoded.len()),
            "{} samples",
            decoded.len()
        );
        let hz = frequency(&decoded[1_000..decoded.len() - 1_000], 16_000);
        assert!((950.0..1_050.0).contains(&hz), "pitch {hz} Hz");
        let peak = decoded.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!((5_000..12_000).contains(&peak), "peak {peak}");
    }

    #[test]
    fn lost_frames_are_concealed_and_counted_in_samples() {
        let mut path = path(SampleRate::Hz16000, SampleRate::Hz24000);
        let mut packets = Vec::new();
        for n in 0..10 {
            path.encode(
                &tone_bytes(SampleRate::Hz24000, 600.0, 20, n * 480),
                &mut packets,
            )
            .unwrap();
        }
        let mut out = Vec::new();
        for packet in packets.iter().take(5) {
            path.decode(packet, 0, &mut out).unwrap();
        }
        let before: usize = out.iter().map(Bytes::len).sum();
        let mut concealed = Vec::new();
        path.decode(&packets[5], 2, &mut concealed).unwrap();
        let after: usize = concealed.iter().map(Bytes::len).sum();
        // Two lost frames plus the packet: three frames of 20 ms at 16 kHz = 960 samples.
        assert!(after >= 3 * 600, "{after} bytes for three frames");
        assert!(before > 0);
    }

    #[test]
    fn a_corrupt_packet_is_an_error_and_the_path_keeps_working() {
        let mut path = path(SampleRate::Hz16000, SampleRate::Hz24000);
        let mut out = Vec::new();
        assert!(path.decode(&[], 0, &mut out).is_err());
        let mut packets = Vec::new();
        path.encode(&tone_bytes(SampleRate::Hz24000, 600.0, 40, 0), &mut packets)
            .unwrap();
        assert!(path.decode(&packets[0], 0, &mut out).is_ok());
    }
}
