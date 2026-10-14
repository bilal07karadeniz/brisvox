//! The engine-facing vocabulary: what a transport reports ([`TransportEvent`]) and what the engine
//! asks it to do ([`TransportCommand`]).

use std::collections::BTreeMap;
use std::time::Duration;

use brisvox_core::TurnEpoch;
use brisvox_core::frame::{AudioFrame, DtmfDigit, SampleRate};
use tokio::time::Instant;

use crate::channel::{Overflow, Overflowing};
use crate::error::{Error, Result};

/// Eviction class of audio items: input audio is the only lossy [`TransportEvent`].
pub(crate) const AUDIO_CLASS: u8 = 1;

/// Identifies one utterance (a TTS response, a caller turn) so playback is reported per
/// utterance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UtteranceId(u64);

impl UtteranceId {
    /// An utterance id with the given number.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The utterance number.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Call metadata, reported once when the call starts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallInfo {
    /// Provider call identifier (or a transport-chosen one).
    pub call_id: String,
    /// Provider-specific key/value metadata (sorted for stable output).
    pub metadata: BTreeMap<String, String>,
}

impl CallInfo {
    /// The metadata key adapters use for the call's sample rate in Hz (set by the websocket
    /// adapter from the client's `start`).
    pub const SAMPLE_RATE_KEY: &'static str = "sample_rate";

    /// The metadata key for the rate of the caller's audio ([`TransportEvent::Audio`]) when it
    /// differs from the rate of the bot's audio. Overrides [`SAMPLE_RATE_KEY`](Self::SAMPLE_RATE_KEY)
    /// for the input direction (RTVI: the client records at 16 kHz and plays at 24 kHz).
    pub const INPUT_SAMPLE_RATE_KEY: &'static str = "input_sample_rate";

    /// The metadata key for the rate the transport accepts for the bot's audio
    /// ([`TransportCommand::Audio`]). Overrides [`SAMPLE_RATE_KEY`](Self::SAMPLE_RATE_KEY) for
    /// the output direction.
    pub const OUTPUT_SAMPLE_RATE_KEY: &'static str = "output_sample_rate";

    /// The call's sample rates, parsed from the metadata.
    ///
    /// `sample_rate` sets one rate for both directions (the generic websocket and Twilio carry
    /// mono PCM16 at one rate, `docs/WEBSOCKET_PROTOCOL.md`). `input_sample_rate` and
    /// `output_sample_rate` set a direction and win over `sample_rate`; a direction that is
    /// left out takes the other's rate. `Ok(None)` means the adapter reported no rate at all
    /// (the loopback does not): the caller picks its default.
    ///
    /// # Errors
    /// [`Error::Core`] with `UnsupportedSampleRate` when a value is a number the audio path
    /// does not support, and [`Error::Protocol`] when it is not a number at all. The engine
    /// treats both as fatal for the call: resampling from an unknown rate would corrupt audio.
    pub fn sample_rate(&self) -> Result<Option<CallRates>> {
        let parse = |key: &str| -> Result<Option<SampleRate>> {
            let Some(value) = self.metadata.get(key) else {
                return Ok(None);
            };
            let hz: u32 = value.trim().parse().map_err(|_| {
                Error::Protocol(format!("call metadata {key} {value:?} is not a number"))
            })?;
            Ok(Some(SampleRate::try_from(hz)?))
        };
        let both = parse(Self::SAMPLE_RATE_KEY)?;
        let input = parse(Self::INPUT_SAMPLE_RATE_KEY)?;
        let output = parse(Self::OUTPUT_SAMPLE_RATE_KEY)?;
        Ok(match (input.or(both), output.or(both)) {
            (None, None) => None,
            (Some(input), Some(output)) => Some(CallRates { input, output }),
            (Some(rate), None) | (None, Some(rate)) => Some(CallRates::symmetric(rate)),
        })
    }
}

/// The audio rates of one call: what the caller's audio arrives at, and what the transport
/// accepts for the bot's audio (`TransportCommand::Audio` must match it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallRates {
    /// Rate of [`TransportEvent::Audio`].
    pub input: SampleRate,
    /// Rate the transport accepts for [`TransportCommand::Audio`].
    pub output: SampleRate,
}

impl CallRates {
    /// The same rate in both directions.
    pub const fn symmetric(rate: SampleRate) -> Self {
        Self {
            input: rate,
            output: rate,
        }
    }
}

/// Why a session ended. Reported once, in [`TransportEvent::Stopped`], as the last event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StopReason {
    /// The engine sent [`TransportCommand::Stop`].
    Requested,
    /// The client hung up.
    PeerHangup,
    /// The client's connection went away without a hang-up.
    PeerDisconnected,
    /// The owner cancelled or closed the session.
    Cancelled,
    /// A task of the session failed; the message is the rendered error.
    Failed(String),
}

/// What the transport reports to the engine, in order.
///
/// Overflow, when the engine falls behind:
/// - [`TransportEvent::Audio`] (input audio) is dropped, oldest first.
/// - [`TransportEvent::PlaybackPosition`] is cumulative, so a new report replaces the queued
///   report of the same `(epoch, utterance)`; the output never waits for the engine to read it.
///   Only if the queue holds no such report does it evict the oldest input audio, and only if
///   that is absent too does it wait.
/// - [`TransportEvent::Stopped`] evicts the oldest input audio the same way. If the queue is full
///   of lossless events and the engine reads nothing for a second, it is dropped; the reason stays
///   available from [`TransportSession::stop_reason`](crate::TransportSession::stop_reason).
/// - Everything else (`Started`, DTMF, marks, errors) waits for space and is never lost, which
///   means the engine must keep reading events while it uses the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportEvent {
    /// The call started; always the first event.
    Started(CallInfo),
    /// Caller audio at the transport's native rate, tagged with the session's current epoch.
    Audio(AudioFrame),
    /// A DTMF key press, with the time the transport received it.
    Dtmf {
        /// The key.
        digit: DtmfDigit,
        /// When the transport received the key (monotonic, from the session clock).
        at: Instant,
    },
    /// The client confirmed that playback reached the mark `id`.
    MarkPlayed {
        /// The id passed to [`TransportCommand::Mark`].
        id: String,
    },
    /// A [`TransportCommand::Mark`] was cut by a [`TransportCommand::Clear`] before the client
    /// echoed it: its audio had not been released, or the mark had been sent and was still
    /// queued or buffered (the audio before it was dropped), or it was sent behind a clear it was
    /// not ordered after. No [`TransportEvent::MarkPlayed`] follows, and a late echo from the
    /// client is ignored: every mark the engine registered ends in exactly one of the two events
    /// (or in `Stopped`). A mark whose echo was already in flight when the clear arrived counts
    /// as cut, and a client that never echoes marks has the oldest one reported here once 256
    /// are outstanding.
    MarkCleared {
        /// The id passed to [`TransportCommand::Mark`].
        id: String,
    },
    /// How much of an utterance has been released to the transport (see the accuracy notes in
    /// `docs/ARCHITECTURE.md`). Reported per `(epoch, utterance)`: the same utterance id in
    /// another epoch counts from zero. Monotonic per `(epoch, utterance)`, also when utterances
    /// interleave; the last report for it equals the audio released for it. Reports may be
    /// coalesced when the engine reads slowly: the latest one always survives.
    PlaybackPosition {
        /// Epoch of the utterance.
        epoch: TurnEpoch,
        /// The utterance.
        utterance: UtteranceId,
        /// Audio released for the utterance so far.
        played: Duration,
    },
    /// How far the caller is behind the audio released to the wire, as the client reports it: the
    /// audio the client holds that has not been played yet. Subtract it from a
    /// [`TransportEvent::PlaybackPosition`] to estimate what the caller heard
    /// (`heard = position - lead`, never below zero). Only protocols whose client tells how far
    /// it has played send it (Twilio: the progress marks; the loopback and the generic websocket
    /// do not). A level, not a counter: the engine should use the latest one, and a new report
    /// replaces a queued one. It is measured when the client echoes a mark, so it can be a few
    /// hundred milliseconds old, and a clear starts it over (the audio it counted is gone).
    PlaybackLead {
        /// Audio sent to the client and not yet played.
        lead: Duration,
    },
    /// A non-fatal transport fault reported by the client protocol.
    Error {
        /// Rendered description (never credentials).
        message: String,
    },
    /// The session ended. The last event; the event stream closes after it. (See the overflow
    /// notes above for the one case in which it is dropped.)
    Stopped {
        /// Why the session ended.
        reason: StopReason,
    },
}

impl Overflowing for TransportEvent {
    fn overflow(&self) -> Overflow {
        match self {
            Self::Audio(_) => Overflow::DropOldest,
            Self::PlaybackPosition { .. } | Self::PlaybackLead { .. } | Self::Stopped { .. } => {
                Overflow::Displace
            }
            _ => Overflow::Backpressure,
        }
    }

    fn class(&self) -> u8 {
        match self {
            Self::Audio(_) => AUDIO_CLASS,
            _ => 0,
        }
    }

    fn supersedes(&self, queued: &Self) -> bool {
        match (self, queued) {
            (
                Self::PlaybackPosition {
                    epoch,
                    utterance,
                    played,
                },
                Self::PlaybackPosition {
                    epoch: queued_epoch,
                    utterance: queued_utterance,
                    played: queued_played,
                },
            ) => epoch == queued_epoch && utterance == queued_utterance && played >= queued_played,
            (Self::PlaybackLead { .. }, Self::PlaybackLead { .. }) => true,
            _ => false,
        }
    }
}

/// What the engine asks the transport to do. Lossless: a full command queue makes the sender
/// wait. [`Clear`](Self::Clear) never waits behind queued audio: it is applied when `send`
/// is called (see [`CommandSender`](crate::CommandSender)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportCommand {
    /// Play `frame` to the caller. Contract: `epoch` equals `frame.epoch()`, and audio of an epoch
    /// follows the [`Clear`](Self::Clear) that started it (epoch 0 needs none). Frames whose epoch
    /// is not the transport's current one are dropped and never played.
    Audio {
        /// Epoch the audio belongs to.
        epoch: TurnEpoch,
        /// Utterance the audio belongs to.
        utterance: UtteranceId,
        /// PCM16 audio at any [`SampleRate`] the transport
        /// accepts.
        frame: AudioFrame,
    },
    /// Barge-in: everything queued or buffered for older epochs is dropped now, and the
    /// transport tells the client to clear its buffers. Older than or equal to the current epoch:
    /// ignored. Out of band: it takes effect while `send` runs, not after the audio commands
    /// queued before it; at most the one frame the output task is already writing to the wire
    /// still goes out ahead of it. Audio and marks sent before the clear but still in the command
    /// queue are treated as older: the audio is dropped, the marks are reported as
    /// [`TransportEvent::MarkCleared`]. So is every other mark the client has not echoed yet,
    /// whether it is still waiting for its audio or was already sent.
    Clear {
        /// The epoch now current.
        epoch: TurnEpoch,
    },
    /// Asks the client to echo `id` once everything released before this point has played. Marks
    /// queued behind audio are sent to the client when the audio ahead of them is released; a mark
    /// cut by a clear before the client echoed it is reported as
    /// [`TransportEvent::MarkCleared`] and its echo, if one still comes, is ignored. Use a
    /// distinct id per mark: an echo is matched to the outstanding mark with that id.
    Mark {
        /// Caller-chosen id, echoed in [`TransportEvent::MarkPlayed`].
        id: String,
    },
    /// Hang up: pending audio is dropped, the client is told, and the session ends with
    /// [`StopReason::Requested`]. In order with the other commands: it takes effect once the
    /// commands queued before it were accepted.
    Stop,
}

impl Overflowing for TransportCommand {
    fn overflow(&self) -> Overflow {
        Overflow::Backpressure
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(rate: Option<&str>) -> CallInfo {
        let mut info = CallInfo::default();
        if let Some(rate) = rate {
            info.metadata
                .insert(CallInfo::SAMPLE_RATE_KEY.to_owned(), rate.to_owned());
        }
        info
    }

    #[test]
    fn sample_rate_parses_the_metadata_value() {
        assert_eq!(
            info(Some("8000")).sample_rate().unwrap(),
            Some(CallRates::symmetric(SampleRate::Hz8000))
        );
        assert_eq!(
            info(Some(" 48000 ")).sample_rate().unwrap(),
            Some(CallRates {
                input: SampleRate::Hz48000,
                output: SampleRate::Hz48000
            })
        );
    }

    #[test]
    fn the_output_rate_key_splits_the_two_directions() {
        let mut with_output = info(Some("16000"));
        with_output.metadata.insert(
            CallInfo::OUTPUT_SAMPLE_RATE_KEY.to_owned(),
            "24000".to_owned(),
        );
        assert_eq!(
            with_output.sample_rate().unwrap(),
            Some(CallRates {
                input: SampleRate::Hz16000,
                output: SampleRate::Hz24000
            })
        );
        with_output
            .metadata
            .insert(CallInfo::OUTPUT_SAMPLE_RATE_KEY.to_owned(), "x".to_owned());
        assert!(matches!(with_output.sample_rate(), Err(Error::Protocol(_))));
    }

    #[test]
    fn missing_rate_is_none_and_bad_rates_are_errors() {
        assert_eq!(info(None).sample_rate().unwrap(), None);
        assert!(matches!(
            info(Some("11025")).sample_rate(),
            Err(Error::Core(brisvox_core::Error::UnsupportedSampleRate(
                11_025
            )))
        ));
        assert!(matches!(
            info(Some("fast")).sample_rate(),
            Err(Error::Protocol(_))
        ));
        assert!(matches!(
            info(Some("-1")).sample_rate(),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn per_direction_rates_override_the_shared_one() {
        let mut info = info(Some("8000"));
        info.metadata.insert(
            CallInfo::INPUT_SAMPLE_RATE_KEY.to_owned(),
            "16000".to_owned(),
        );
        assert_eq!(
            info.sample_rate().unwrap(),
            Some(CallRates {
                input: SampleRate::Hz16000,
                output: SampleRate::Hz8000
            })
        );
        // Without the shared key: both given.
        let mut split = CallInfo::default();
        split.metadata.insert(
            CallInfo::INPUT_SAMPLE_RATE_KEY.to_owned(),
            "16000".to_owned(),
        );
        split.metadata.insert(
            CallInfo::OUTPUT_SAMPLE_RATE_KEY.to_owned(),
            "24000".to_owned(),
        );
        assert_eq!(
            split.sample_rate().unwrap(),
            Some(CallRates {
                input: SampleRate::Hz16000,
                output: SampleRate::Hz24000
            })
        );
        // One direction only: the other takes its rate.
        split.metadata.remove(CallInfo::INPUT_SAMPLE_RATE_KEY);
        assert_eq!(
            split.sample_rate().unwrap(),
            Some(CallRates::symmetric(SampleRate::Hz24000))
        );
        // A bad per-direction value is an error that names the key.
        split
            .metadata
            .insert(CallInfo::INPUT_SAMPLE_RATE_KEY.to_owned(), "x".to_owned());
        let error = split.sample_rate().unwrap_err().to_string();
        assert!(error.contains("input_sample_rate"), "{error}");
    }

    #[tokio::test]
    async fn a_new_playback_lead_replaces_a_queued_one_and_never_waits() {
        use crate::channel;
        // Capacity 2 with a lossless event queued: leads must neither pile up nor block.
        let (tx, mut rx) = channel::bounded::<TransportEvent>(2);
        tx.send(TransportEvent::Error {
            message: "x".to_owned(),
        })
        .await
        .unwrap();
        let sends = async {
            for ms in 0..10u64 {
                tx.send(TransportEvent::PlaybackLead {
                    lead: Duration::from_millis(ms),
                })
                .await
                .unwrap();
            }
        };
        tokio::time::timeout(Duration::from_secs(1), sends)
            .await
            .expect("a lead report waited for space");
        assert_eq!(tx.stats().dropped_oldest, 9);
        assert!(matches!(
            rx.recv().await,
            Some(TransportEvent::Error { .. })
        ));
        assert_eq!(
            rx.recv().await,
            Some(TransportEvent::PlaybackLead {
                lead: Duration::from_millis(9)
            })
        );
    }
}
