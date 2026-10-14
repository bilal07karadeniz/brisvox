//! Turn epochs: the mechanism that makes stale audio after a barge-in structurally impossible.
//!
//! Every Control and Data frame carries the [`TurnEpoch`] it was produced in. An interruption
//! bumps the session's epoch; every processor drops frames whose epoch is older than the
//! current one.

use std::fmt;

use crate::frame::{Frame, SystemFrame};

/// Monotonically increasing turn counter of one session. `u32` keeps `AudioFrame` (and so
/// `Frame`) within 64 bytes next to a `u64` sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct TurnEpoch(u32);

impl TurnEpoch {
    /// The epoch a session starts in.
    pub const ZERO: Self = Self(0);

    /// An epoch with the given number.
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// The epoch number.
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The following epoch. Saturates at `u32::MAX`: a session would need over four billion
    /// turns or interruptions to get there.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// Whether a frame tagged with `self` must be dropped when the session is at `current`.
    pub const fn is_stale(self, current: Self) -> bool {
        self.0 < current.0
    }
}

impl fmt::Display for TurnEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What a task does with an incoming frame, given the session epoch it has seen so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// Handle the frame normally.
    Process,
    /// An `Interrupt` to a newer epoch: purge stale queued frames, then handle the frame.
    Advance(TurnEpoch),
    /// Stale (older epoch, or an interrupt that is not newer): drop without handling.
    Drop,
}

/// Per-task epoch tracking: the default interruption behaviour of every processor.
///
/// Every task starts at [`TurnEpoch::ZERO`]. An `Interrupt { new_epoch }` newer than the current
/// epoch advances it; an interrupt that is not newer is a duplicate (e.g. the same interrupt
/// arriving from both directions) and is dropped. Any other frame that [`Frame::is_stale`] is
/// dropped (tool results are exempt there).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EpochGate {
    current: TurnEpoch,
}

impl EpochGate {
    /// The epoch this task is in.
    pub const fn current(&self) -> TurnEpoch {
        self.current
    }

    /// Moves to `epoch` if it is newer (used when the task itself originated an interrupt).
    /// Returns whether the epoch advanced.
    pub fn advance_to(&mut self, epoch: TurnEpoch) -> bool {
        if epoch > self.current {
            self.current = epoch;
            true
        } else {
            false
        }
    }

    /// Classifies `frame`, advancing the current epoch on a newer interrupt.
    pub fn admit(&mut self, frame: &Frame) -> Admit {
        match frame {
            Frame::System(SystemFrame::Interrupt { new_epoch }) => {
                if *new_epoch > self.current {
                    self.current = *new_epoch;
                    Admit::Advance(*new_epoch)
                } else {
                    Admit::Drop
                }
            }
            other if other.is_stale(self.current) => Admit::Drop,
            _ => Admit::Process,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_orders_and_saturates() {
        let e = TurnEpoch::ZERO.next().next();
        assert_eq!(e.get(), 2);
        assert!(TurnEpoch::ZERO < e);
        assert_eq!(TurnEpoch::new(u32::MAX).next(), TurnEpoch::new(u32::MAX));
    }

    #[test]
    fn only_older_epochs_are_stale() {
        let current = TurnEpoch::new(5);
        assert!(TurnEpoch::new(4).is_stale(current));
        assert!(!TurnEpoch::new(5).is_stale(current));
        assert!(!TurnEpoch::new(6).is_stale(current));
    }

    #[test]
    fn gate_advances_on_newer_interrupts_and_drops_stale_frames() {
        use crate::frame::{ControlFrame, ControlKind};
        let interrupt = |e| {
            Frame::from(SystemFrame::Interrupt {
                new_epoch: TurnEpoch::new(e),
            })
        };
        let control = |e| -> Frame {
            ControlFrame {
                epoch: TurnEpoch::new(e),
                kind: ControlKind::Flush,
            }
            .into()
        };
        let mut gate = EpochGate::default();
        assert_eq!(gate.admit(&control(0)), Admit::Process);
        assert_eq!(gate.admit(&interrupt(2)), Admit::Advance(TurnEpoch::new(2)));
        assert_eq!(gate.current(), TurnEpoch::new(2));
        assert_eq!(gate.admit(&control(1)), Admit::Drop);
        assert_eq!(gate.admit(&control(2)), Admit::Process);
        assert_eq!(
            gate.admit(&interrupt(2)),
            Admit::Drop,
            "duplicate interrupt"
        );
        assert_eq!(gate.admit(&interrupt(1)), Admit::Drop, "older interrupt");
        assert_eq!(gate.admit(&Frame::from(SystemFrame::Stop)), Admit::Process);
    }
}
