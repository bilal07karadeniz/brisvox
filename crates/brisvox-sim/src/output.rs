//! [`OutputLog`]: what a simulated call sent back to the caller, recorded two ways.
//!
//! - **Pipeline-sourced records** ([`OutputRecord`]): every frame that reached a recording tap
//!   ([`OutputLog::tap`], or the reference pipeline's [`SimTransportOut`]) on the pipeline's output
//!   side, with the time it got there. This is where `Interrupt` frames are seen, so interrupt
//!   counts come from here and not from the client: a client is told to clear its buffer on every
//!   epoch bump, which is not always a barge-in.
//! - **Wire-arrival records** ([`WireRecord`]): what a client on the far side of a transport
//!   received, with the time it received it (audio, marks, clears, hang-up). A production factory
//!   fills these from its loopback client; the reference pipeline has no wire and leaves them
//!   empty.
//!
//! Both share one record limit mechanism: past the limit, later items are counted, not kept.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_core::frame::{DataFrame, Frame};
use brisvox_core::{SharedClock, SystemFrame, TimelineRecorder, TurnEpoch, TurnEvent};
use tokio::time::Instant;

use crate::scenario::SimTransportOut;

/// Records kept by default per [`OutputLog`] (older ones are counted, not kept).
pub const DEFAULT_RECORD_LIMIT: usize = 100_000;

/// One frame that reached the output side of the pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputRecord {
    /// When it was handed to the transport, since the recorder's origin.
    pub at: Duration,
    /// `Frame::name()`.
    pub kind: &'static str,
    /// The frame's epoch (system frames: the interrupt's new epoch, else `None`).
    pub epoch: Option<u32>,
    /// Audio length, for audio frames.
    pub audio: Option<Duration>,
}

impl OutputRecord {
    /// A record of a frame named `kind` handed over `at` after the origin.
    pub const fn new(
        at: Duration,
        kind: &'static str,
        epoch: Option<u32>,
        audio: Option<Duration>,
    ) -> Self {
        Self {
            at,
            kind,
            epoch,
            audio,
        }
    }
}

/// What a client on the wire received.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WireKind {
    /// An audio frame.
    Audio,
    /// A playback mark.
    Mark,
    /// A clear: the client's buffered audio was dropped (an epoch bump, not necessarily a
    /// barge-in).
    Clear,
    /// The call ended.
    Hangup,
}

impl WireKind {
    /// A stable lower-case name (`"audio"`, `"mark"`, `"clear"`, `"hangup"`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Mark => "mark",
            Self::Clear => "clear",
            Self::Hangup => "hangup",
        }
    }
}

/// One thing a client received from the transport, with its arrival time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireRecord {
    /// When the client received it, since the log's origin.
    pub at: Duration,
    /// What it was.
    pub kind: WireKind,
    /// The audio's epoch, or the epoch a clear moved to.
    pub epoch: Option<u32>,
    /// Audio length, for audio.
    pub audio: Option<Duration>,
}

#[derive(Default)]
struct Recorded {
    records: Vec<OutputRecord>,
    dropped: usize,
    wire: Vec<WireRecord>,
    dropped_wire: usize,
    /// Epoch of the last audio frame handed out (for `AudioOutStopped`).
    last_audio_epoch: Option<TurnEpoch>,
}

struct Shared {
    origin: Instant,
    timeline: Option<TimelineRecorder>,
    limit: Mutex<usize>,
    recorded: Mutex<Recorded>,
}

/// The shared record of a simulated call's output. Cheap to clone; clones share the records.
#[derive(Clone)]
pub struct OutputLog {
    shared: Arc<Shared>,
}

impl OutputLog {
    /// A log measuring times from `origin`. With a `timeline`, recording a frame also marks
    /// `FirstAudioOut` per turn and, when an interrupt arrives, `AudioOutStopped` on the turn
    /// whose audio was playing.
    ///
    /// Pass a `timeline` only when the recording point is the real handoff, as in the reference
    /// pipeline. A production factory whose output transport marks those two events itself
    /// (at the actual handoff to the wire) must pass `None` here: the timeline keeps the first
    /// mark of each event, so a tap's earlier mark would pre-empt the transport's and the
    /// measured latency would silently exclude the transport hop.
    pub fn new(origin: Instant, timeline: Option<TimelineRecorder>) -> Self {
        Self {
            shared: Arc::new(Shared {
                origin,
                timeline,
                limit: Mutex::new(DEFAULT_RECORD_LIMIT),
                recorded: Mutex::new(Recorded::default()),
            }),
        }
    }

    /// Keeps at most `limit` records of each kind (load tests); later ones are only counted.
    #[must_use]
    pub fn with_record_limit(self, limit: usize) -> Self {
        *self
            .shared
            .limit
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = limit;
        self
    }

    /// The instant times are measured from.
    pub fn origin(&self) -> Instant {
        self.shared.origin
    }

    fn lock(&self) -> MutexGuard<'_, Recorded> {
        self.shared
            .recorded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn limit(&self) -> usize {
        *self
            .shared
            .limit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// A recording tap over this log: a pass-through processor that records every downstream
    /// frame at `clock.now()` and forwards it. Put it right before the output transport.
    ///
    /// The tap marks the timeline exactly as [`Self::new`] says: build the log with `None` when
    /// the transport behind the tap marks `FirstAudioOut` and `AudioOutStopped` itself.
    pub fn tap(&self, clock: SharedClock) -> SimTransportOut {
        SimTransportOut::from_log(clock, self.clone()).named("output_tap")
    }

    /// Records a frame that reached the output side at `now` (pipeline-sourced).
    ///
    /// Marks the timeline as described at [`Self::new`]. An `Interrupt` is recorded with its new
    /// epoch.
    pub fn record_frame(&self, frame: &Frame, now: Instant) {
        let epoch = match frame {
            Frame::System(SystemFrame::Interrupt { new_epoch }) => Some(new_epoch.get()),
            other => other.epoch().map(TurnEpoch::get),
        };
        let limit = self.limit();
        let mut recorded = self.lock();
        let audio = match frame {
            Frame::Data(DataFrame::Audio(a)) => {
                recorded.last_audio_epoch = Some(a.epoch());
                if let Some(timeline) = &self.shared.timeline {
                    timeline.mark(a.epoch(), TurnEvent::FirstAudioOut, now);
                }
                Some(a.duration())
            }
            Frame::System(SystemFrame::Interrupt { .. }) => {
                // Playout of the interrupted turn stops here (nothing is buffered after the
                // tap in the simulation).
                if let (Some(timeline), Some(playing)) =
                    (&self.shared.timeline, recorded.last_audio_epoch.take())
                {
                    timeline.mark(playing, TurnEvent::AudioOutStopped, now);
                }
                None
            }
            _ => None,
        };
        if recorded.records.len() < limit {
            recorded.records.push(OutputRecord {
                at: now.saturating_duration_since(self.shared.origin),
                kind: frame.name(),
                epoch,
                audio,
            });
        } else {
            recorded.dropped += 1;
        }
    }

    /// Records an audio frame of `epoch` and `duration` that reached the output at `at`, in time
    /// order: the record goes before the first record that is not earlier than `at`, so audio
    /// received at the same instant as an `Interrupt` counts as received before it.
    ///
    /// For factories whose tap sits in front of a pacer: the tap records everything but audio
    /// (`Interrupt`, words, ...) as frames pass, and the factory adds the audio afterwards at the
    /// time the client on the far side of the transport received it (see
    /// [`Self::record_wire`]), so the suite's oracles read playout time, not hand-over time.
    /// Marks the timeline like [`Self::record_frame`] does for audio only when the log has one.
    pub fn record_audio(&self, epoch: TurnEpoch, at: Instant, duration: Duration) {
        let limit = self.limit();
        let mut recorded = self.lock();
        recorded.last_audio_epoch = Some(epoch);
        if let Some(timeline) = &self.shared.timeline {
            timeline.mark(epoch, TurnEvent::FirstAudioOut, at);
        }
        if recorded.records.len() < limit {
            let at = at.saturating_duration_since(self.shared.origin);
            let index = recorded.records.partition_point(|r| r.at < at);
            recorded.records.insert(
                index,
                OutputRecord {
                    at,
                    kind: "audio",
                    epoch: Some(epoch.get()),
                    audio: Some(duration),
                },
            );
        } else {
            recorded.dropped += 1;
        }
    }

    /// Records something a client on the wire received at `at`.
    pub fn record_wire(
        &self,
        kind: WireKind,
        at: Instant,
        epoch: Option<TurnEpoch>,
        audio: Option<Duration>,
    ) {
        let limit = self.limit();
        let mut recorded = self.lock();
        if recorded.wire.len() < limit {
            recorded.wire.push(WireRecord {
                at: at.saturating_duration_since(self.shared.origin),
                kind,
                epoch: epoch.map(TurnEpoch::get),
                audio,
            });
        } else {
            recorded.dropped_wire += 1;
        }
    }

    /// Every pipeline-sourced record so far, in order.
    pub fn records(&self) -> Vec<OutputRecord> {
        self.lock().records.clone()
    }

    /// Frames received after the record limit was reached.
    pub fn dropped_records(&self) -> usize {
        self.lock().dropped
    }

    /// Every wire-arrival record so far, in arrival order.
    pub fn wire_records(&self) -> Vec<WireRecord> {
        self.lock().wire.clone()
    }

    /// Wire records dropped after the record limit was reached.
    pub fn dropped_wire_records(&self) -> usize {
        self.lock().dropped_wire
    }

    /// How many `Interrupt` frames the pipeline sent (not how many clears the client got).
    pub fn interrupts(&self) -> usize {
        self.lock()
            .records
            .iter()
            .filter(|r| r.kind == "interrupt")
            .count()
    }

    /// How many clears reached the client.
    pub fn wire_clears(&self) -> usize {
        self.lock()
            .wire
            .iter()
            .filter(|r| r.kind == WireKind::Clear)
            .count()
    }
}

impl std::fmt::Debug for OutputLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let recorded = self.lock();
        f.debug_struct("OutputLog")
            .field("records", &recorded.records.len())
            .field("wire", &recorded.wire.len())
            .finish_non_exhaustive()
    }
}
