//! The frame model: everything that flows through a pipeline is a [`Frame`].
//!
//! Frames come in three levels (see the frame table in `docs/ARCHITECTURE.md`):
//! - [`SystemFrame`]: out-of-band lifecycle and urgent events (start/stop/cancel, interrupt, VAD,
//!   errors). Travel on each processor's priority queue and overtake data. Not epoch-tagged.
//! - [`ControlFrame`]: in-band boundaries (turns, LLM response, TTS, flush). Ordered with data.
//! - [`DataFrame`]: payload (audio, transcripts, LLM text and tool deltas, tool results, DTMF,
//!   metrics). Ordered, bounded queues.
//!
//! Every Control and Data frame carries the [`TurnEpoch`] it belongs to, so stale frames can be
//! dropped after an interruption ([`Frame::is_stale`]). `Frame` is at most 64 bytes (checked at
//! compile time); large, rare payloads are boxed and audio shares its buffer through
//! `bytes::Bytes` (no copy, no per-frame `Vec`).
//!
//! # Epoch ownership
//! The turn state machine owns the session's current epoch. On barge-in it bumps the epoch and
//! emits [`SystemFrame::Interrupt`] carrying the *new* epoch; every frame produced afterwards
//! (starting with the `UserTurnStarted` control frame of the new turn) is tagged with it.
//! Producers tag frames with the epoch current when they started the work, never a later one.
//!
//! # Privacy
//! `Debug` output never contains audio samples or user/LLM text, only sizes, so a stray `?frame`
//! in a log line cannot leak conversation content or cost microseconds formatting a buffer.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::time::Instant;

use crate::epoch::TurnEpoch;
use crate::error::{Error, Result};

/// Upper bound on `size_of::<Frame>()`, enforced at compile time.
pub const MAX_FRAME_SIZE: usize = 64;

const _: () = assert!(std::mem::size_of::<Frame>() <= MAX_FRAME_SIZE);

/// Routing priority of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameLevel {
    /// Overtakes everything; never dropped by overflow policies.
    System,
    /// In-band boundary; ordered with data.
    Control,
    /// Payload; bounded queues with per-kind overflow policy.
    Data,
}

/// A unit of work flowing through a pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Out-of-band lifecycle and urgent events.
    System(SystemFrame),
    /// In-band boundaries.
    Control(ControlFrame),
    /// Payload.
    Data(DataFrame),
}

impl Frame {
    /// Routing level of this frame.
    pub const fn level(&self) -> FrameLevel {
        match self {
            Self::System(_) => FrameLevel::System,
            Self::Control(_) => FrameLevel::Control,
            Self::Data(_) => FrameLevel::Data,
        }
    }

    /// The turn epoch this frame belongs to; `None` for system frames, which are never stale.
    pub fn epoch(&self) -> Option<TurnEpoch> {
        match self {
            Self::System(_) => None,
            Self::Control(c) => Some(c.epoch),
            Self::Data(d) => Some(d.epoch()),
        }
    }

    /// Whether the epoch purge must drop this frame when the session is at `current`.
    ///
    /// Never true for system frames (not epoch-tagged), tool results or word timestamps:
    /// - a result may come from a SideEffect call that already ran, and the LLM context must learn
    ///   about it on the next turn (see `docs/TOOLS.md` item 8). Dropping it could make the call
    ///   run twice.
    /// - a word timestamp describes audio of its turn that may already have played when the
    ///   interrupt arrives. The played-text tracker needs it to cut the interrupted message to
    ///   what was heard, so the purge must not drop it. Consumers filter words by their own
    ///   epoch.
    pub fn is_stale(&self, current: TurnEpoch) -> bool {
        match self {
            Self::System(_) | Self::Data(DataFrame::ToolResult(_) | DataFrame::Word(_)) => false,
            Self::Control(c) => c.epoch.is_stale(current),
            Self::Data(d) => d.epoch().is_stale(current),
        }
    }

    /// Stable variant name, for logs and metrics.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::System(s) => s.name(),
            Self::Control(c) => c.kind.name(),
            Self::Data(d) => d.name(),
        }
    }
}

impl From<SystemFrame> for Frame {
    fn from(f: SystemFrame) -> Self {
        Self::System(f)
    }
}

impl From<ControlFrame> for Frame {
    fn from(f: ControlFrame) -> Self {
        Self::Control(f)
    }
}

impl From<DataFrame> for Frame {
    fn from(f: DataFrame) -> Self {
        Self::Data(f)
    }
}

impl From<AudioFrame> for Frame {
    fn from(f: AudioFrame) -> Self {
        Self::Data(DataFrame::Audio(f))
    }
}

// ---------------------------------------------------------------------------------------------
// System

/// Out-of-band lifecycle and urgent events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemFrame {
    /// The pipeline starts; processors initialise.
    Start,
    /// Graceful stop: finish queued work in order, then end.
    Stop,
    /// Immediate stop: drop queued work and end now.
    Cancel,
    /// Barge-in: the session moved to `new_epoch`; drop everything older.
    Interrupt {
        /// The epoch now current.
        new_epoch: TurnEpoch,
    },
    /// VAD detected the start of user speech.
    UserStartedSpeaking {
        /// When speech started (monotonic, from the session `Clock`).
        at: Instant,
    },
    /// VAD detected the end of user speech.
    UserStoppedSpeaking {
        /// When speech stopped (monotonic, from the session `Clock`).
        at: Instant,
    },
    /// VAD saw the speech probability fall and stay low, but has not confirmed the stop yet
    /// (opt-in): `UserStoppedSpeaking` follows after the VAD's minimum silence unless
    /// the user speaks again first. It is a hint for stages that can start work early (a
    /// speculative end-of-turn analysis); a stage that does not know it forwards it unchanged.
    /// A VAD that is not configured to send it never does.
    UserSpeechFalling {
        /// The time the stop will carry if it is confirmed: the end of the last speech window
        /// plus the VAD's speech padding (monotonic, from the session `Clock`). The stop of the
        /// same fall has exactly this time; a stop with another time belongs to a later pause.
        at: Instant,
    },
    /// A processor reports an error.
    Error(Box<ErrorFrame>),
    /// Self-wake: a helper task a processor owns asks the processor to run again, through
    /// [`ProcessorContext::self_sender`](crate::processor::ProcessorContext::self_sender). The
    /// payload (a timer tick, a model verdict, a deadline) travels out of band in a typed
    /// bounded queue the stage owns; `token` says which one to look at.
    ///
    /// Rules: it is a system frame, so it overtakes queued data and is never purged by an epoch
    /// change; it is addressed to the processor that created it and is **never forwarded**
    /// ([`ProcessorContext::push`](crate::processor::ProcessorContext::push) discards it, so a
    /// stage that forwards unknown frames cannot leak it into a neighbour).
    Wake {
        /// Which pending event of the stage to handle (stage-defined).
        token: u32,
    },
}

impl SystemFrame {
    /// Stable variant name.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Cancel => "cancel",
            Self::Interrupt { .. } => "interrupt",
            Self::UserStartedSpeaking { .. } => "user_started_speaking",
            Self::UserStoppedSpeaking { .. } => "user_stopped_speaking",
            Self::UserSpeechFalling { .. } => "user_speech_falling",
            Self::Error(_) => "error",
            Self::Wake { .. } => "wake",
        }
    }
}

/// Error report carried by [`SystemFrame::Error`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorFrame {
    /// Stage that failed, e.g. `"stt"`.
    pub stage: &'static str,
    /// Rendered error (never contains credentials).
    pub message: String,
    /// Whether the session cannot continue.
    pub fatal: bool,
}

// ---------------------------------------------------------------------------------------------
// Control

/// An in-band boundary marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlFrame {
    /// Turn the boundary belongs to.
    pub epoch: TurnEpoch,
    /// Which boundary.
    pub kind: ControlKind,
}

/// Kinds of [`ControlFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlKind {
    /// The user's turn began.
    UserTurnStarted,
    /// The user's turn ended (end-of-turn decision made).
    UserTurnEnded,
    /// The LLM began streaming a response.
    LlmResponseStarted,
    /// The LLM finished its response.
    LlmResponseEnded,
    /// TTS began producing audio for the response.
    TtsStarted,
    /// TTS finished producing audio for the response.
    TtsEnded,
    /// Flush buffered data downstream now (e.g. force a pending TTS segment).
    Flush,
    /// STT: the final transcript of the current user utterance is complete (the provider's
    /// `speech_final`, e.g. the answer to a finalize request). Follows the utterance's final
    /// transcripts in order.
    UtteranceFinal,
    /// TTS: audio of the TTS utterance `utterance` starts here. Emitted by the TTS stage right
    /// before the first audio frame of each utterance and in order with the audio (both use the
    /// data lane), so the stage that writes audio to the transport knows which utterance every
    /// following audio frame belongs to without any side channel. `utterance` is the same
    /// session-monotonic id that [`WordTimestamp::utterance`] carries.
    UtteranceStarted {
        /// The TTS utterance whose audio follows.
        utterance: u64,
    },
}

impl ControlKind {
    /// Stable variant name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::UserTurnStarted => "user_turn_started",
            Self::UserTurnEnded => "user_turn_ended",
            Self::LlmResponseStarted => "llm_response_started",
            Self::LlmResponseEnded => "llm_response_ended",
            Self::TtsStarted => "tts_started",
            Self::TtsEnded => "tts_ended",
            Self::Flush => "flush",
            Self::UtteranceFinal => "utterance_final",
            Self::UtteranceStarted { .. } => "utterance_started",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Data

/// Payload frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataFrame {
    /// PCM16 audio (caller input or agent output).
    Audio(AudioFrame),
    /// Speech-to-text result.
    Transcript(TranscriptFrame),
    /// A text delta streamed by the LLM.
    LlmText(TextFrame),
    /// A fragment of a streamed tool call.
    ToolCallDelta(Box<ToolCallDelta>),
    /// The result of an executed tool call.
    ToolResult(Box<ToolResult>),
    /// A word of the bot's speech with its start offset on the playout timeline (boxed: the
    /// payload is rare and carries a text).
    Word(Box<WordTimestamp>),
    /// A DTMF key press.
    Dtmf(DtmfFrame),
    /// A latency or processing measurement.
    Metric(MetricFrame),
}

impl DataFrame {
    /// Turn the payload belongs to.
    pub fn epoch(&self) -> TurnEpoch {
        match self {
            Self::Audio(a) => a.epoch,
            Self::Transcript(t) => t.epoch,
            Self::LlmText(t) => t.epoch,
            Self::ToolCallDelta(d) => d.epoch,
            Self::ToolResult(r) => r.epoch,
            Self::Word(w) => w.epoch,
            Self::Dtmf(d) => d.epoch,
            Self::Metric(m) => m.epoch,
        }
    }

    /// Stable variant name.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Audio(_) => "audio",
            Self::Transcript(_) => "transcript",
            Self::LlmText(_) => "llm_text",
            Self::ToolCallDelta(_) => "tool_call_delta",
            Self::ToolResult(_) => "tool_result",
            Self::Word(_) => "word",
            Self::Dtmf(_) => "dtmf",
            Self::Metric(_) => "metric",
        }
    }
}

/// Sample rates the audio path supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SampleRate {
    /// 8 kHz (telephony μ-law/A-law).
    Hz8000,
    /// 16 kHz (VAD, most STT).
    Hz16000,
    /// 22.05 kHz.
    Hz22050,
    /// 24 kHz (common TTS output).
    Hz24000,
    /// 44.1 kHz.
    Hz44100,
    /// 48 kHz (WebRTC/Opus).
    Hz48000,
}

impl SampleRate {
    /// Samples per second.
    pub const fn hz(self) -> u32 {
        match self {
            Self::Hz8000 => 8_000,
            Self::Hz16000 => 16_000,
            Self::Hz22050 => 22_050,
            Self::Hz24000 => 24_000,
            Self::Hz44100 => 44_100,
            Self::Hz48000 => 48_000,
        }
    }
}

impl TryFrom<u32> for SampleRate {
    type Error = Error;

    fn try_from(hz: u32) -> Result<Self> {
        Ok(match hz {
            8_000 => Self::Hz8000,
            16_000 => Self::Hz16000,
            22_050 => Self::Hz22050,
            24_000 => Self::Hz24000,
            44_100 => Self::Hz44100,
            48_000 => Self::Hz48000,
            other => return Err(Error::UnsupportedSampleRate(other)),
        })
    }
}

impl fmt::Display for SampleRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} Hz", self.hz())
    }
}

/// Mono PCM16 little-endian audio sharing its buffer through [`Bytes`].
///
/// Fields are private so the buffer is always a whole number of samples. Cloning or slicing
/// never copies sample data.
#[derive(Clone, PartialEq, Eq)]
pub struct AudioFrame {
    data: Bytes,
    timestamp: Instant,
    seq: u64,
    epoch: TurnEpoch,
    sample_rate: SampleRate,
}

impl AudioFrame {
    /// Bytes per PCM16 sample.
    pub const BYTES_PER_SAMPLE: usize = 2;

    /// Wraps `data` (mono PCM16 LE) without copying.
    ///
    /// `timestamp` is the monotonic capture/production time from the session `Clock`; `seq` is
    /// the per-stream sequence number.
    ///
    /// # Errors
    /// [`Error::InvalidAudio`] if `data` is not a whole number of samples.
    pub fn new(
        data: Bytes,
        sample_rate: SampleRate,
        timestamp: Instant,
        seq: u64,
        epoch: TurnEpoch,
    ) -> Result<Self> {
        if !data.len().is_multiple_of(Self::BYTES_PER_SAMPLE) {
            return Err(Error::InvalidAudio(
                "PCM16 buffer length must be a multiple of 2",
            ));
        }
        Ok(Self {
            data,
            timestamp,
            seq,
            epoch,
            sample_rate,
        })
    }

    /// The PCM16 LE bytes.
    pub fn data(&self) -> &Bytes {
        &self.data
    }

    /// Consumes the frame, returning its buffer.
    pub fn into_data(self) -> Bytes {
        self.data
    }

    /// Sample rate.
    pub const fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Number of samples.
    pub fn sample_count(&self) -> usize {
        self.data.len() / Self::BYTES_PER_SAMPLE
    }

    /// Playback duration (truncated to whole microseconds).
    pub fn duration(&self) -> Duration {
        let samples = u64::try_from(self.sample_count()).unwrap_or(u64::MAX);
        let micros = samples.saturating_mul(1_000_000) / u64::from(self.sample_rate.hz());
        Duration::from_micros(micros)
    }

    /// Monotonic capture/production time.
    pub const fn timestamp(&self) -> Instant {
        self.timestamp
    }

    /// Per-stream sequence number.
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// Turn the audio belongs to.
    pub const fn epoch(&self) -> TurnEpoch {
        self.epoch
    }
}

impl fmt::Debug for AudioFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioFrame")
            .field("bytes", &self.data.len())
            .field("sample_rate", &self.sample_rate.hz())
            .field("seq", &self.seq)
            .field("epoch", &self.epoch.get())
            .finish_non_exhaustive()
    }
}

/// Formats a text payload as its length only (see the module's privacy note).
struct TextLen<'a>(&'a str);

impl fmt::Debug for TextLen<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} bytes>", self.0.len())
    }
}

/// Speech-to-text result.
#[derive(Clone, PartialEq, Eq)]
pub struct TranscriptFrame {
    /// Turn the speech belongs to.
    pub epoch: TurnEpoch,
    /// Recognised text.
    pub text: Arc<str>,
    /// `false` for interim hypotheses that may still change.
    pub is_final: bool,
    /// When the result was received (monotonic).
    pub received_at: Instant,
}

impl fmt::Debug for TranscriptFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TranscriptFrame")
            .field("epoch", &self.epoch.get())
            .field("text", &TextLen(&self.text))
            .field("is_final", &self.is_final)
            .finish_non_exhaustive()
    }
}

/// A text delta streamed by the LLM.
#[derive(Clone, PartialEq, Eq)]
pub struct TextFrame {
    /// Turn the response belongs to.
    pub epoch: TurnEpoch,
    /// The delta.
    pub text: Arc<str>,
}

impl fmt::Debug for TextFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextFrame")
            .field("epoch", &self.epoch.get())
            .field("text", &TextLen(&self.text))
            .finish()
    }
}

/// A fragment of a streamed tool call. Fragments with the same `index` are concatenated;
/// arguments are executed only once complete (see `docs/TOOLS.md`).
#[derive(Clone, PartialEq, Eq)]
pub struct ToolCallDelta {
    /// Turn the call belongs to.
    pub epoch: TurnEpoch,
    /// Position of the call within the LLM response.
    pub index: u32,
    /// Call id (present on the first fragment).
    pub id: Option<Arc<str>>,
    /// Tool name (present on the first fragment).
    pub name: Option<Arc<str>>,
    /// Raw JSON argument fragment.
    pub arguments: Arc<str>,
}

impl fmt::Debug for ToolCallDelta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolCallDelta")
            .field("epoch", &self.epoch.get())
            .field("index", &self.index)
            .field("id", &self.id)
            .field("name", &self.name)
            .field("arguments", &TextLen(&self.arguments))
            .finish()
    }
}

/// The result of an executed tool call. Exempt from the epoch purge ([`Frame::is_stale`]).
#[derive(Clone, PartialEq, Eq)]
pub struct ToolResult {
    /// Turn the call belongs to.
    pub epoch: TurnEpoch,
    /// Id of the call this answers.
    pub call_id: Arc<str>,
    /// Result payload (JSON text) given to the LLM.
    pub content: Arc<str>,
    /// Whether `content` describes a failure.
    pub is_error: bool,
}

impl fmt::Debug for ToolResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolResult")
            .field("epoch", &self.epoch.get())
            .field("call_id", &self.call_id)
            .field("content", &TextLen(&self.content))
            .field("is_error", &self.is_error)
            .finish()
    }
}

/// A word of the bot's speech, as the TTS timed it (played-text truncation).
///
/// `start` is the offset from the first audio sample of `utterance`, on the same timeline as the
/// playout clock. The word text is private to the conversation and never shown by `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct WordTimestamp {
    /// Turn the speech belongs to.
    pub epoch: TurnEpoch,
    /// The TTS utterance (context) the word was synthesized in.
    pub utterance: u64,
    /// The word as the provider reported it.
    pub word: Arc<str>,
    /// Offset from the utterance's first audio sample.
    pub start: Duration,
}

impl fmt::Debug for WordTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WordTimestamp")
            .field("epoch", &self.epoch.get())
            .field("utterance", &self.utterance)
            .field("word", &TextLen(&self.word))
            .field("start", &self.start)
            .finish()
    }
}

/// A DTMF key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DtmfFrame {
    /// Turn during which the key was pressed.
    pub epoch: TurnEpoch,
    /// The key.
    pub digit: DtmfDigit,
    /// When the press was received (monotonic).
    pub at: Instant,
}

/// DTMF keys (RFC 4733 events 0–15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DtmfDigit {
    /// `0`
    D0,
    /// `1`
    D1,
    /// `2`
    D2,
    /// `3`
    D3,
    /// `4`
    D4,
    /// `5`
    D5,
    /// `6`
    D6,
    /// `7`
    D7,
    /// `8`
    D8,
    /// `9`
    D9,
    /// `*`
    Star,
    /// `#`
    Pound,
    /// `A`
    A,
    /// `B`
    B,
    /// `C`
    C,
    /// `D`
    D,
}

impl DtmfDigit {
    /// The key as a character.
    pub const fn as_char(self) -> char {
        match self {
            Self::D0 => '0',
            Self::D1 => '1',
            Self::D2 => '2',
            Self::D3 => '3',
            Self::D4 => '4',
            Self::D5 => '5',
            Self::D6 => '6',
            Self::D7 => '7',
            Self::D8 => '8',
            Self::D9 => '9',
            Self::Star => '*',
            Self::Pound => '#',
            Self::A => 'A',
            Self::B => 'B',
            Self::C => 'C',
            Self::D => 'D',
        }
    }
}

impl TryFrom<char> for DtmfDigit {
    type Error = Error;

    fn try_from(c: char) -> Result<Self> {
        Ok(match c.to_ascii_uppercase() {
            '0' => Self::D0,
            '1' => Self::D1,
            '2' => Self::D2,
            '3' => Self::D3,
            '4' => Self::D4,
            '5' => Self::D5,
            '6' => Self::D6,
            '7' => Self::D7,
            '8' => Self::D8,
            '9' => Self::D9,
            '*' => Self::Star,
            '#' => Self::Pound,
            'A' => Self::A,
            'B' => Self::B,
            'C' => Self::C,
            'D' => Self::D,
            _ => return Err(Error::InvalidDtmfDigit(c)),
        })
    }
}

/// A latency or processing measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricFrame {
    /// Turn measured.
    pub epoch: TurnEpoch,
    /// Stage measured, e.g. `"tts"`.
    pub stage: &'static str,
    /// What was measured.
    pub kind: MetricKind,
    /// The measurement.
    pub value: Duration,
}

/// Kinds of [`MetricFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetricKind {
    /// Request sent → first byte/token/audio received.
    TimeToFirstByte,
    /// Total processing time of the stage for the turn.
    ProcessingTime,
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use super::*;

    fn now() -> Instant {
        #[allow(clippy::disallowed_methods)] // tests need some Instant value
        Instant::now()
    }

    fn audio(len: usize, rate: SampleRate) -> AudioFrame {
        AudioFrame::new(Bytes::from(vec![0u8; len]), rate, now(), 0, TurnEpoch::ZERO).unwrap()
    }

    #[test]
    fn frame_fits_in_64_bytes() {
        let sizes = [
            ("Frame", size_of::<Frame>()),
            ("DataFrame", size_of::<DataFrame>()),
            ("AudioFrame", size_of::<AudioFrame>()),
            ("SystemFrame", size_of::<SystemFrame>()),
            ("TranscriptFrame", size_of::<TranscriptFrame>()),
            ("WordTimestamp", size_of::<WordTimestamp>()),
        ];
        println!("{sizes:?}");
        assert!(size_of::<Frame>() <= MAX_FRAME_SIZE, "{sizes:?}");
    }

    #[test]
    fn audio_slices_share_one_buffer() {
        // 1 s of 16 kHz audio cut into 20 ms frames: no copies, no new allocations.
        let pcm = Bytes::from(vec![7u8; 32_000]);
        let base = pcm.as_ptr() as usize;
        let frames: Vec<AudioFrame> = (0..50u64)
            .map(|i| {
                let start = usize::try_from(i).unwrap() * 640;
                AudioFrame::new(
                    pcm.slice(start..start + 640),
                    SampleRate::Hz16000,
                    now(),
                    i,
                    TurnEpoch::ZERO,
                )
                .unwrap()
            })
            .collect();
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.data().as_ptr() as usize, base + i * 640);
            assert_eq!(f.sample_count(), 320);
            assert_eq!(f.duration(), Duration::from_millis(20));
            assert_eq!(f.seq() as usize, i);
        }
        let cloned = Frame::from(frames[3].clone());
        let Frame::Data(DataFrame::Audio(a)) = cloned else {
            panic!("expected audio");
        };
        assert_eq!(a.data().as_ptr(), frames[3].data().as_ptr());
    }

    #[test]
    fn debug_output_has_no_samples_or_text() {
        let pcm = Bytes::from(vec![0xABu8; 640]);
        let a = AudioFrame::new(pcm, SampleRate::Hz16000, now(), 3, TurnEpoch::new(2)).unwrap();
        let dbg = format!("{:?}", Frame::from(a));
        assert!(dbg.len() < 120, "{dbg}");
        assert!(dbg.contains("bytes: 640"), "{dbg}");

        let secret = "my card number is 4111";
        let t: Frame = DataFrame::Transcript(TranscriptFrame {
            epoch: TurnEpoch::ZERO,
            text: Arc::from(secret),
            is_final: true,
            received_at: now(),
        })
        .into();
        let r: Frame = DataFrame::ToolResult(Box::new(ToolResult {
            epoch: TurnEpoch::ZERO,
            call_id: Arc::from("call_1"),
            content: Arc::from(secret),
            is_error: false,
        }))
        .into();
        let d: Frame = DataFrame::ToolCallDelta(Box::new(ToolCallDelta {
            epoch: TurnEpoch::ZERO,
            index: 0,
            id: None,
            name: Some(Arc::from("pay")),
            arguments: Arc::from(secret),
        }))
        .into();
        let l: Frame = DataFrame::LlmText(TextFrame {
            epoch: TurnEpoch::ZERO,
            text: Arc::from(secret),
        })
        .into();
        let w: Frame = DataFrame::Word(Box::new(WordTimestamp {
            epoch: TurnEpoch::ZERO,
            utterance: 1,
            word: Arc::from(secret),
            start: Duration::from_millis(40),
        }))
        .into();
        for f in [t, r, d, l, w] {
            let dbg = format!("{f:?}");
            assert!(!dbg.contains("4111"), "{dbg}");
            assert!(dbg.contains("<22 bytes>"), "{dbg}");
        }
    }

    #[test]
    fn stale_purge_spares_system_frames_and_tool_results() {
        let (old, cur) = (TurnEpoch::new(1), TurnEpoch::new(2));
        let control = |epoch| -> Frame {
            ControlFrame {
                epoch,
                kind: ControlKind::TtsEnded,
            }
            .into()
        };
        let audio = |epoch| -> Frame {
            AudioFrame::new(Bytes::new(), SampleRate::Hz8000, now(), 0, epoch)
                .unwrap()
                .into()
        };
        let result: Frame = DataFrame::ToolResult(Box::new(ToolResult {
            epoch: old,
            call_id: Arc::from("c"),
            content: Arc::from("{}"),
            is_error: false,
        }))
        .into();
        assert!(control(old).is_stale(cur));
        assert!(!control(cur).is_stale(cur));
        assert!(audio(old).is_stale(cur));
        assert!(!audio(cur).is_stale(cur));
        assert!(!result.is_stale(cur));
        // Word timestamps of the interrupted turn survive the purge (the tracker needs them).
        let word: Frame = DataFrame::Word(Box::new(WordTimestamp {
            epoch: old,
            utterance: 1,
            word: Arc::from("merhaba"),
            start: Duration::from_millis(120),
        }))
        .into();
        assert!(!word.is_stale(cur));
        assert!(!Frame::from(SystemFrame::Interrupt { new_epoch: cur }).is_stale(cur));
    }

    #[test]
    fn audio_rejects_partial_samples() {
        let err = AudioFrame::new(
            Bytes::from_static(&[1, 2, 3]),
            SampleRate::Hz8000,
            now(),
            0,
            TurnEpoch::ZERO,
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidAudio(_)));
    }

    #[test]
    fn durations_per_rate() {
        assert_eq!(
            audio(320, SampleRate::Hz8000).duration(),
            Duration::from_millis(20)
        );
        assert_eq!(
            audio(1_764, SampleRate::Hz44100).duration(),
            Duration::from_millis(20)
        );
        assert_eq!(
            audio(1_920, SampleRate::Hz48000).duration(),
            Duration::from_millis(20)
        );
        assert_eq!(audio(0, SampleRate::Hz24000).duration(), Duration::ZERO);
    }

    #[test]
    fn sample_rate_round_trips() {
        for hz in [8_000, 16_000, 22_050, 24_000, 44_100, 48_000] {
            assert_eq!(SampleRate::try_from(hz).unwrap().hz(), hz);
        }
        assert_eq!(
            SampleRate::try_from(11_025),
            Err(Error::UnsupportedSampleRate(11_025))
        );
    }

    #[test]
    fn dtmf_round_trips() {
        for c in "0123456789*#ABCD".chars() {
            assert_eq!(DtmfDigit::try_from(c).unwrap().as_char(), c);
        }
        assert_eq!(DtmfDigit::try_from('a').unwrap(), DtmfDigit::A);
        assert_eq!(DtmfDigit::try_from('x'), Err(Error::InvalidDtmfDigit('x')));
    }

    #[test]
    fn levels_and_epochs() {
        let e = TurnEpoch::new(9);
        let t = now();
        let text: Arc<str> = Arc::from("hi");
        let cases: Vec<(Frame, FrameLevel, Option<TurnEpoch>, &str)> = vec![
            (SystemFrame::Start.into(), FrameLevel::System, None, "start"),
            (SystemFrame::Stop.into(), FrameLevel::System, None, "stop"),
            (
                SystemFrame::Cancel.into(),
                FrameLevel::System,
                None,
                "cancel",
            ),
            (
                SystemFrame::Interrupt { new_epoch: e }.into(),
                FrameLevel::System,
                None,
                "interrupt",
            ),
            (
                SystemFrame::UserStartedSpeaking { at: t }.into(),
                FrameLevel::System,
                None,
                "user_started_speaking",
            ),
            (
                SystemFrame::UserStoppedSpeaking { at: t }.into(),
                FrameLevel::System,
                None,
                "user_stopped_speaking",
            ),
            (
                SystemFrame::UserSpeechFalling { at: t }.into(),
                FrameLevel::System,
                None,
                "user_speech_falling",
            ),
            (
                SystemFrame::Error(Box::new(ErrorFrame {
                    stage: "stt",
                    message: "boom".into(),
                    fatal: false,
                }))
                .into(),
                FrameLevel::System,
                None,
                "error",
            ),
            (
                ControlFrame {
                    epoch: e,
                    kind: ControlKind::Flush,
                }
                .into(),
                FrameLevel::Control,
                Some(e),
                "flush",
            ),
            (
                AudioFrame::new(Bytes::new(), SampleRate::Hz16000, t, 1, e)
                    .unwrap()
                    .into(),
                FrameLevel::Data,
                Some(e),
                "audio",
            ),
            (
                DataFrame::Transcript(TranscriptFrame {
                    epoch: e,
                    text: text.clone(),
                    is_final: true,
                    received_at: t,
                })
                .into(),
                FrameLevel::Data,
                Some(e),
                "transcript",
            ),
            (
                DataFrame::LlmText(TextFrame {
                    epoch: e,
                    text: text.clone(),
                })
                .into(),
                FrameLevel::Data,
                Some(e),
                "llm_text",
            ),
            (
                DataFrame::ToolCallDelta(Box::new(ToolCallDelta {
                    epoch: e,
                    index: 0,
                    id: Some(text.clone()),
                    name: None,
                    arguments: text.clone(),
                }))
                .into(),
                FrameLevel::Data,
                Some(e),
                "tool_call_delta",
            ),
            (
                DataFrame::ToolResult(Box::new(ToolResult {
                    epoch: e,
                    call_id: text.clone(),
                    content: text,
                    is_error: false,
                }))
                .into(),
                FrameLevel::Data,
                Some(e),
                "tool_result",
            ),
            (
                DataFrame::Word(Box::new(WordTimestamp {
                    epoch: e,
                    utterance: 1,
                    word: Arc::from("merhaba"),
                    start: Duration::from_millis(120),
                }))
                .into(),
                FrameLevel::Data,
                Some(e),
                "word",
            ),
            (
                DataFrame::Dtmf(DtmfFrame {
                    epoch: e,
                    digit: DtmfDigit::Pound,
                    at: t,
                })
                .into(),
                FrameLevel::Data,
                Some(e),
                "dtmf",
            ),
            (
                DataFrame::Metric(MetricFrame {
                    epoch: e,
                    stage: "tts",
                    kind: MetricKind::TimeToFirstByte,
                    value: Duration::from_millis(80),
                })
                .into(),
                FrameLevel::Data,
                Some(e),
                "metric",
            ),
        ];
        for (frame, level, epoch, name) in cases {
            assert_eq!(frame.level(), level, "{name}");
            assert_eq!(frame.epoch(), epoch, "{name}");
            assert_eq!(frame.name(), name);
        }
    }
}
