//! File IO of the recording sinks: bounded by a timeout, never overwriting, private by mode.
//!
//! Every await on the disk goes through [`within`]: a hung disk (a stalled network volume) turns
//! into an error after the timeout (counted on the injected clock) instead of holding a call's
//! recording task forever. WAV samples are written in batches of [`WAV_BATCH`] bytes, one disk
//! operation per batch rather than one per 20 ms frame. Files
//! are created with `create_new`, so an existing recording is never truncated, and on Unix with
//! mode `0600`: a recording is personal data.

use std::future::Future;
use std::io::{self, SeekFrom};
use std::path::Path;
use std::time::Duration;

use brisvox_core::clock::deadline_after;
use brisvox_core::{Clock, SharedClock};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use super::sink::{AGE_SUFFIX, AgeRecipients, RecordingSink};

/// Bytes of WAV samples gathered before one disk write (about 2 s of 16 kHz audio). A crash
/// loses at most this much of a track's tail.
pub const WAV_BATCH: usize = 64 * 1024;

/// Awaits `fut`, failing with [`io::ErrorKind::TimedOut`] once `timeout` has passed on `clock`.
pub async fn within<T>(
    clock: &dyn Clock,
    timeout: Duration,
    fut: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    let deadline = deadline_after(clock.now(), timeout);
    tokio::select! {
        biased;
        result = fut => result,
        () = clock.sleep_until(deadline) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "disk operation timed out",
        )),
    }
}

/// Creates the file at `path`, which must not exist yet.
pub async fn create_new(clock: &dyn Clock, path: &Path, timeout: Duration) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    within(clock, timeout, options.open(path)).await
}

/// What a recording file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The transcript, JSON lines.
    Transcript,
    /// The caller's audio, WAV.
    Caller,
    /// The bot's audio, WAV.
    Bot,
}

impl Kind {
    /// Every kind.
    pub const ALL: [Self; 3] = [Self::Transcript, Self::Caller, Self::Bot];

    /// The file name's ending.
    pub const fn suffix(self) -> &'static str {
        match self {
            Self::Transcript => ".transcript.jsonl",
            Self::Caller => ".caller.wav",
            Self::Bot => ".bot.wav",
        }
    }
}

/// Every recording file name starts with this; the sweeper touches nothing else.
pub const NAME_PREFIX: &str = "call-";

/// The file name of a call's recording: the call's start (unix seconds), its session number (a
/// counter of this process, so the start keeps names unique across restarts) and the kind.
pub fn file_name(started_unix: u64, session: u64, kind: Kind) -> String {
    format!("{NAME_PREFIX}{started_unix}-{session}{}", kind.suffix())
}

/// [`file_name`], with the `.age` suffix of an encrypted recording when `encrypted`.
pub fn file_name_for(started_unix: u64, session: u64, kind: Kind, encrypted: bool) -> String {
    let mut name = file_name(started_unix, session, kind);
    if encrypted {
        name.push_str(AGE_SUFFIX);
    }
    name
}

/// Whether `name` is one of the files this module names, plain or encrypted (`.age`).
pub fn is_recording_file(name: &str) -> bool {
    let name = name.strip_suffix(AGE_SUFFIX).unwrap_or(name);
    name.starts_with(NAME_PREFIX) && Kind::ALL.iter().any(|k| name.ends_with(k.suffix()))
}

/// Size of the canonical WAV header.
pub const WAV_HEADER_LEN: usize = 44;

/// The 44-byte header of a mono PCM16 WAV file of `data_len` bytes of samples. `data_len` of
/// `u32::MAX` is the "streaming, length unknown" form that decoders read to the end of the file.
pub fn wav_header(rate_hz: u32, data_len: u32) -> [u8; WAV_HEADER_LEN] {
    let mut h = [0u8; WAV_HEADER_LEN];
    let riff_len = data_len.saturating_add(36);
    let byte_rate = rate_hz.saturating_mul(2);
    let fields: [(usize, &[u8]); 12] = [
        (0, b"RIFF"),
        (4, &riff_len.to_le_bytes()),
        (8, b"WAVE"),
        (12, b"fmt "),
        (16, &16u32.to_le_bytes()),
        (20, &1u16.to_le_bytes()), // PCM
        (22, &1u16.to_le_bytes()), // mono
        (24, &rate_hz.to_le_bytes()),
        (28, &byte_rate.to_le_bytes()),
        (32, &2u16.to_le_bytes()),  // block align
        (34, &16u16.to_le_bytes()), // bits per sample
        (36, b"data"),
    ];
    for (at, bytes) in fields {
        if let Some(dst) = h.get_mut(at..at + bytes.len()) {
            dst.copy_from_slice(bytes);
        }
    }
    if let Some(dst) = h.get_mut(40..44) {
        dst.copy_from_slice(&data_len.to_le_bytes());
    }
    h
}

/// A mono PCM16 WAV file being written.
///
/// The header is written first with the streaming length, so a file left by a crash still plays;
/// [`Self::finalize`] seeks back and writes the true lengths. An encrypted file (an age stream)
/// cannot be rewritten: it keeps the streaming header, which decoders read to the end of the
/// file, and `finalize` only ends the stream.
#[derive(Debug)]
pub struct WavWriter {
    sink: RecordingSink,
    clock: SharedClock,
    rate_hz: u32,
    /// Samples accepted so far (written or batched).
    data_bytes: u64,
    /// Samples accepted but not yet written; flushed at [`WAV_BATCH`] and at the end.
    batch: Vec<u8>,
    timeout: Duration,
}

impl WavWriter {
    /// Creates `path` (it must not exist) and writes the provisional header.
    ///
    /// # Errors
    /// The IO error, including a timeout.
    pub async fn create(
        clock: SharedClock,
        path: &Path,
        rate_hz: u32,
        timeout: Duration,
    ) -> io::Result<Self> {
        Self::create_with(clock, path, rate_hz, timeout, None).await
    }

    /// [`Self::create`], encrypted to `recipients` when given (the caller names `path` with
    /// the `.age` suffix, [`file_name_for`]).
    ///
    /// # Errors
    /// The IO error, including a timeout.
    pub async fn create_with(
        clock: SharedClock,
        path: &Path,
        rate_hz: u32,
        timeout: Duration,
        recipients: Option<&AgeRecipients>,
    ) -> io::Result<Self> {
        let mut sink = RecordingSink::create(&*clock, path, timeout, recipients).await?;
        sink.write_all(&*clock, timeout, &wav_header(rate_hz, u32::MAX))
            .await?;
        Ok(Self {
            sink,
            clock,
            rate_hz,
            data_bytes: 0,
            batch: Vec::with_capacity(WAV_BATCH + WAV_BATCH / 4),
            timeout,
        })
    }

    /// Appends sample bytes. They are gathered and written when [`WAV_BATCH`] bytes are
    /// waiting, so most calls touch no disk.
    ///
    /// # Errors
    /// The IO error of the batch write, including a timeout.
    pub async fn append(&mut self, pcm: &[u8]) -> io::Result<()> {
        self.batch.extend_from_slice(pcm);
        self.data_bytes = self.data_bytes.saturating_add(pcm.len() as u64);
        if self.batch.len() >= WAV_BATCH {
            self.write_batch().await?;
        }
        Ok(())
    }

    async fn write_batch(&mut self) -> io::Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        self.sink
            .write_all(&*self.clock, self.timeout, &self.batch)
            .await?;
        self.batch.clear();
        Ok(())
    }

    /// Bytes of samples accepted so far (some may still be batched in memory).
    pub const fn data_bytes(&self) -> u64 {
        self.data_bytes
    }

    /// Writes the true lengths into the header and flushes (an encrypted file: writes the last
    /// batch and ends the age stream). Returns the bytes of samples.
    ///
    /// # Errors
    /// The IO error, including a timeout.
    pub async fn finalize(mut self) -> io::Result<u64> {
        let len = u32::try_from(self.data_bytes).unwrap_or(u32::MAX);
        self.write_batch().await?;
        let clock = &*self.clock;
        let timeout = self.timeout;
        if let Some(file) = self.sink.plain_file() {
            within(clock, timeout, file.flush()).await?;
            within(clock, timeout, file.seek(SeekFrom::Start(0))).await?;
            within(
                clock,
                timeout,
                file.write_all(&wav_header(self.rate_hz, len)),
            )
            .await?;
        }
        self.sink.finish(clock, timeout).await?;
        Ok(self.data_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_describes_mono_pcm16() {
        let h = wav_header(16_000, 32_000);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes([h[4], h[5], h[6], h[7]]), 32_036);
        assert_eq!(&h[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes([h[22], h[23]]), 1, "mono");
        assert_eq!(u32::from_le_bytes([h[24], h[25], h[26], h[27]]), 16_000);
        assert_eq!(u32::from_le_bytes([h[28], h[29], h[30], h[31]]), 32_000);
        assert_eq!(u16::from_le_bytes([h[34], h[35]]), 16);
        assert_eq!(&h[36..40], b"data");
        assert_eq!(u32::from_le_bytes([h[40], h[41], h[42], h[43]]), 32_000);
    }

    #[test]
    fn only_our_own_names_are_recording_files() {
        for kind in Kind::ALL {
            assert!(is_recording_file(&file_name(1_760_000_000, 7, kind)));
        }
        assert_eq!(
            file_name(1_760_000_000, 7, Kind::Caller),
            "call-1760000000-7.caller.wav"
        );
        for kind in Kind::ALL {
            let name = file_name_for(1_760_000_000, 7, kind, true);
            assert!(name.ends_with(".age"));
            assert!(is_recording_file(&name), "{name}");
            assert_eq!(
                file_name_for(1_760_000_000, 7, kind, false),
                file_name(1_760_000_000, 7, kind)
            );
        }
        for other in [
            "call-1-2.bot.wav.age.age",
            "call-1-2.age",
            ".age",
            "notes.age",
            "notes.txt",
            "call-1.txt",
            "x-call-1-2.bot.wav",
            ".caller.wav",
            "call-1-2.bot.wav.bak",
        ] {
            assert!(!is_recording_file(other), "{other}");
        }
    }

    #[test]
    fn the_streaming_header_does_not_overflow() {
        let h = wav_header(8_000, u32::MAX);
        assert_eq!(u32::from_le_bytes([h[4], h[5], h[6], h[7]]), u32::MAX);
        assert_eq!(u32::from_le_bytes([h[40], h[41], h[42], h[43]]), u32::MAX);
    }
    #[allow(clippy::disallowed_methods)]
    fn temp(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("brisvox-files-{tag}-{}.wav", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[tokio::test]
    async fn samples_are_written_in_batches_not_per_frame() {
        let path = temp("batch");
        let clock = brisvox_core::SystemClock::shared();
        let mut writer = WavWriter::create(clock, &path, 8_000, Duration::from_secs(5))
            .await
            .unwrap();
        // 100 frames of 20 ms: far less than one batch, so all of it waits in memory.
        for _ in 0..100 {
            writer.append(&[7u8; 320]).await.unwrap();
        }
        assert_eq!(writer.batch.len(), 32_000);
        assert_eq!(writer.data_bytes(), 32_000);
        // The frame that crosses the batch size sends the whole batch in one write.
        let mut sent_at = None;
        for i in 0..(WAV_BATCH / 320) {
            writer.append(&[7u8; 320]).await.unwrap();
            if writer.batch.is_empty() && sent_at.is_none() {
                sent_at = Some(i);
            }
        }
        assert!(sent_at.is_some(), "the batch was never written");
        assert!(writer.batch.len() < WAV_BATCH);
        let total = writer.finalize().await.unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len() as u64, WAV_HEADER_LEN as u64 + total);
        assert_eq!(
            u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]) as u64,
            total
        );
        assert!(bytes[WAV_HEADER_LEN..].iter().all(|&b| b == 7));
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn a_stalled_disk_operation_times_out_on_the_injected_clock() {
        let clock = brisvox_core::SystemClock::shared();
        let err = within(
            &*clock,
            Duration::from_millis(20),
            std::future::pending::<io::Result<()>>(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        let ok = within(&*clock, Duration::from_secs(5), async { Ok(3) }).await;
        assert_eq!(ok.unwrap(), 3);
    }
}
