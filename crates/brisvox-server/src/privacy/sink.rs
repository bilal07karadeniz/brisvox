//! Where a recording file's bytes go: the plain file, or an age stream encrypted to the
//! operator's public keys (`docs/PRIVACY.md`).
//!
//! [`RecordingSink`] is what [`WavWriter`](super::WavWriter) and the transcript writer write
//! to. In plain mode it is the file and nothing else changes. With `BRISVOX_RECORDING_AGE_RECIPIENTS`
//! set (cargo feature `recording-encryption`) it wraps age's `StreamWriter` over a shared
//! in-memory buffer: every write is encrypted into the buffer and the finished ciphertext is
//! drained to the async file straight away. The server holds only public keys, so it cannot read
//! its own recordings; `age -d -i key.txt call-....age` does.
//!
//! The encryption of a batch (a 64 KiB WAV batch is about 50-100 µs of ChaCha20-Poly1305, and
//! the header's X25519 wrap about as much per recipient) runs on tokio's blocking pool, never on
//! a worker thread, and under the same disk timeout as every other disk operation. A write that
//! times out loses the cipher state, and a failed file write may have left a hole, so the sink is
//! dead after ANY error: every later write and `finish` fails (the callers already stop
//! recording a file at its first error).
//!
//! The age format releases a chunk only once more plaintext follows it, so up to 64 KiB of
//! plaintext (and the final chunk with its end mark) is held back until [`RecordingSink::finish`]:
//! a call that crashes leaves a file the stock tools decrypt only up to the last full chunk,
//! with a truncation error. A short transcript is lost entirely in that case.

#[cfg(feature = "recording-encryption")]
use std::fmt;
use std::io;
use std::path::Path;
use std::time::Duration;

use brisvox_core::Clock;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

use super::files::{create_new, within};

/// The suffix an encrypted recording's file name gets after the plain name.
pub const AGE_SUFFIX: &str = ".age";

/// Most recipients one server encrypts to (each costs a key wrap per file).
pub const MAX_AGE_RECIPIENTS: usize = 16;

/// The X25519 recipients (`age1...`) recordings are encrypted to.
#[cfg(feature = "recording-encryption")]
#[derive(Clone)]
pub struct AgeRecipients {
    keys: std::sync::Arc<[String]>,
    list: std::sync::Arc<[age::x25519::Recipient]>,
}

/// The recipients type of a build without the `recording-encryption` feature: it has no values,
/// so nothing can be encrypted and [`AgeRecipients::parse`] always fails.
#[cfg(not(feature = "recording-encryption"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgeRecipients(std::convert::Infallible);

#[cfg(feature = "recording-encryption")]
impl fmt::Debug for AgeRecipients {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AgeRecipients({} keys)", self.keys.len())
    }
}

#[cfg(feature = "recording-encryption")]
impl PartialEq for AgeRecipients {
    fn eq(&self, other: &Self) -> bool {
        self.keys == other.keys
    }
}

#[cfg(feature = "recording-encryption")]
impl Eq for AgeRecipients {}

impl AgeRecipients {
    /// Parses a comma-separated list of age X25519 recipients. The error never repeats the
    /// text: a private key pasted here by mistake must not reach the logs.
    ///
    /// # Errors
    /// A message naming the problem (and, in a build without the feature, the feature).
    pub fn parse(text: &str) -> Result<Self, String> {
        Self::parse_impl(text)
    }

    #[cfg(not(feature = "recording-encryption"))]
    #[allow(clippy::unnecessary_wraps)]
    fn parse_impl(_text: &str) -> Result<Self, String> {
        Err(
            "this build has no recording encryption: rebuild brisvox-server with the cargo \
             feature `recording-encryption`, or unset the variable"
                .to_owned(),
        )
    }

    #[cfg(feature = "recording-encryption")]
    fn parse_impl(text: &str) -> Result<Self, String> {
        let mut keys: Vec<String> = Vec::new();
        let mut list = Vec::new();
        for (i, raw) in text.split(',').enumerate() {
            let entry = raw.trim();
            if entry.is_empty() {
                continue;
            }
            let number = i + 1;
            if entry.to_ascii_uppercase().starts_with("AGE-SECRET-KEY-") {
                return Err(format!(
                    "entry {number} is an age PRIVATE key; give only the public recipient (age1...) and keep the private key off the server"
                ));
            }
            let recipient: age::x25519::Recipient = entry.parse().map_err(|_| {
                format!(
                    "entry {number} is not an age X25519 recipient (expected age1..., as printed by `age-keygen`)"
                )
            })?;
            let canonical = recipient.to_string();
            if keys.contains(&canonical) {
                continue;
            }
            if keys.len() >= MAX_AGE_RECIPIENTS {
                return Err(format!("more than {MAX_AGE_RECIPIENTS} recipients"));
            }
            keys.push(canonical);
            list.push(recipient);
        }
        if list.is_empty() {
            return Err("no recipient given".to_owned());
        }
        Ok(Self {
            keys: keys.into(),
            list: list.into(),
        })
    }

    /// Number of distinct recipients.
    pub fn len(&self) -> usize {
        #[cfg(feature = "recording-encryption")]
        {
            self.keys.len()
        }
        #[cfg(not(feature = "recording-encryption"))]
        {
            match self.0 {}
        }
    }

    /// Always false: a value has at least one recipient.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Runs `job` on the blocking pool and awaits it under the disk timeout of `clock`. A panic in
/// the job comes back as an error.
#[cfg_attr(not(feature = "recording-encryption"), allow(dead_code))]
async fn offload<T: Send + 'static>(
    clock: &dyn Clock,
    timeout: Duration,
    job: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    let handle = tokio::task::spawn_blocking(job);
    within(clock, timeout, async {
        match handle.await {
            Ok(result) => result,
            Err(e) => Err(io::Error::other(e.to_string())),
        }
    })
    .await
}

/// The destination of one recording file.
#[derive(Debug)]
pub enum RecordingSink {
    /// The file, byte for byte what is written.
    Plain(File),
    /// An age file; see the module docs.
    #[cfg(feature = "recording-encryption")]
    Age(AgeSink),
}

impl RecordingSink {
    /// Creates `path` (it must not exist): the plain file, or an age file when `recipients`
    /// is given. The age header is written before this returns.
    ///
    /// # Errors
    /// The IO error, including a timeout.
    pub async fn create(
        clock: &dyn Clock,
        path: &Path,
        timeout: Duration,
        recipients: Option<&AgeRecipients>,
    ) -> io::Result<Self> {
        #[cfg(feature = "recording-encryption")]
        if let Some(recipients) = recipients {
            return AgeSink::create(clock, path, timeout, recipients)
                .await
                .map(Self::Age);
        }
        #[cfg(not(feature = "recording-encryption"))]
        let _ = recipients;
        create_new(clock, path, timeout).await.map(Self::Plain)
    }

    /// Whether the bytes are encrypted.
    pub const fn is_encrypted(&self) -> bool {
        match self {
            Self::Plain(_) => false,
            #[cfg(feature = "recording-encryption")]
            Self::Age(_) => true,
        }
    }

    /// The plain file, for a writer that seeks (a WAV file's final header). `None` when
    /// encrypted: an age stream cannot be rewritten.
    pub const fn plain_file(&mut self) -> Option<&mut File> {
        match self {
            Self::Plain(file) => Some(file),
            #[cfg(feature = "recording-encryption")]
            Self::Age(_) => None,
        }
    }

    /// Writes `data`.
    ///
    /// # Errors
    /// The IO error, including a timeout.
    pub async fn write_all(
        &mut self,
        clock: &dyn Clock,
        timeout: Duration,
        data: &[u8],
    ) -> io::Result<()> {
        match self {
            Self::Plain(file) => within(clock, timeout, file.write_all(data)).await,
            #[cfg(feature = "recording-encryption")]
            Self::Age(sink) => sink.write_all(clock, timeout, data).await,
        }
    }

    /// Flushes the file (an age sink holds a chunk back until [`Self::finish`]).
    ///
    /// # Errors
    /// The IO error, including a timeout.
    pub async fn flush(&mut self, clock: &dyn Clock, timeout: Duration) -> io::Result<()> {
        match self {
            Self::Plain(file) => within(clock, timeout, file.flush()).await,
            #[cfg(feature = "recording-encryption")]
            Self::Age(sink) => within(clock, timeout, sink.file.flush()).await,
        }
    }

    /// Ends the file: flushes a plain file; for an age file writes the final chunk and its end
    /// mark, then flushes. Writing after `finish` is an error on an age sink.
    ///
    /// # Errors
    /// The IO error, including a timeout.
    pub async fn finish(&mut self, clock: &dyn Clock, timeout: Duration) -> io::Result<()> {
        match self {
            Self::Plain(file) => within(clock, timeout, file.flush()).await,
            #[cfg(feature = "recording-encryption")]
            Self::Age(sink) => sink.finish(clock, timeout).await,
        }
    }
}

/// A byte buffer the age stream writes into and the sink drains.
#[cfg(feature = "recording-encryption")]
#[derive(Clone, Default)]
struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(feature = "recording-encryption")]
impl SharedBuf {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<u8>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The bytes gathered so far; the buffer is left empty.
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.lock())
    }
}

#[cfg(feature = "recording-encryption")]
impl io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// An age stream into an async file.
#[cfg(feature = "recording-encryption")]
pub struct AgeSink {
    file: File,
    /// `None` after [`Self::finish`], or after a write timed out and lost the cipher state.
    writer: Option<age::stream::StreamWriter<SharedBuf>>,
    /// Set while an operation holds the cipher state and left set by any error in it: the
    /// file may be missing ciphertext, so every later write or finish fails.
    poisoned: bool,
    buf: SharedBuf,
}

#[cfg(feature = "recording-encryption")]
impl fmt::Debug for AgeSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgeSink")
            .field("open", &self.writer.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "recording-encryption")]
impl AgeSink {
    async fn create(
        clock: &dyn Clock,
        path: &Path,
        timeout: Duration,
        recipients: &AgeRecipients,
    ) -> io::Result<Self> {
        let mut file = create_new(clock, path, timeout).await?;
        let buf = SharedBuf::default();
        let (writer, header) = {
            let recipients = recipients.list.clone();
            let buf = buf.clone();
            offload(clock, timeout, move || {
                let targets = recipients.iter().map(|r| r as &dyn age::Recipient);
                let encryptor =
                    age::Encryptor::with_recipients(targets).map_err(io::Error::other)?;
                let writer = encryptor.wrap_output(buf.clone())?;
                Ok((writer, buf.take()))
            })
            .await?
        };
        within(clock, timeout, file.write_all(&header)).await?;
        Ok(Self {
            file,
            writer: Some(writer),
            poisoned: false,
            buf,
        })
    }

    async fn write_all(
        &mut self,
        clock: &dyn Clock,
        timeout: Duration,
        data: &[u8],
    ) -> io::Result<()> {
        use std::io::Write as _;
        let mut writer = self.take_writer()?;
        let data = data.to_vec();
        let buf = self.buf.clone();
        let (writer, ciphertext) = offload(clock, timeout, move || {
            writer.write_all(&data)?;
            Ok((writer, buf.take()))
        })
        .await?;
        if !ciphertext.is_empty() {
            within(clock, timeout, self.file.write_all(&ciphertext)).await?;
        }
        // Only now, with the ciphertext on disk, may the sink go on: a failure above left the
        // writer taken and the sink poisoned, so no later write can leave a hole in the file.
        self.writer = Some(writer);
        self.poisoned = false;
        Ok(())
    }

    /// Takes the cipher state out for one operation and poisons the sink until the operation
    /// puts it back: any error in between (cipher, timeout, file) leaves it dead.
    fn take_writer(&mut self) -> io::Result<age::stream::StreamWriter<SharedBuf>> {
        match self.writer.take() {
            Some(writer) => {
                self.poisoned = true;
                Ok(writer)
            }
            None if self.poisoned => Err(io::Error::other(
                "the encrypted recording failed earlier and is closed",
            )),
            None => Err(io::Error::other("the encrypted recording is closed")),
        }
    }

    async fn finish(&mut self, clock: &dyn Clock, timeout: Duration) -> io::Result<()> {
        if self.writer.is_none() && self.poisoned {
            return Err(io::Error::other(
                "the encrypted recording failed earlier and is closed",
            ));
        }
        if self.writer.is_some() {
            let writer = self.take_writer()?;
            let buf = self.buf.clone();
            let tail = offload(clock, timeout, move || {
                writer.finish()?;
                Ok(buf.take())
            })
            .await?;
            within(clock, timeout, self.file.write_all(&tail)).await?;
            // Finished: the stream is complete (a repeated `finish` only flushes).
            self.poisoned = false;
        }
        within(clock, timeout, self.file.flush()).await
    }
}

#[cfg(test)]
mod tests;
