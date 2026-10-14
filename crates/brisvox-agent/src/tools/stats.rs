//! Counters of the tool engine's interruption behaviour and the digest that recognises an
//! identical call of a later response run.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

macro_rules! counters {
    ($($(#[$doc:meta])* $name:ident),* $(,)?) => {
        #[derive(Debug, Default)]
        pub(super) struct ToolStats {
            $(pub(super) $name: AtomicU64,)*
        }

        /// A copy of the engine's interruption counters ([`super::ToolEngine::stats`]).
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct ToolStatsSnapshot {
            $($(#[$doc])* pub $name: u64,)*
        }

        impl ToolStats {
            pub(super) fn snapshot(&self) -> ToolStatsSnapshot {
                ToolStatsSnapshot {
                    $($name: self.$name.load(Ordering::Relaxed),)*
                }
            }
        }
    };
}

counters! {
    /// ReadOnly calls that were running when the turn was cancelled: their future was dropped and
    /// the slot answered `interrupted`.
    read_only_cancelled,
    /// Calls (either kind) that had not started when the token was already cancelled: answered
    /// `interrupted` without running.
    not_started_cancelled,
    /// SideEffect calls running when the turn was cancelled: the slot was answered
    /// `interrupted` and the call kept running on its own task.
    side_effect_interrupted,
    /// SideEffect results reported as a late note instead of a tool result (the slot had been
    /// answered by an interruption or a timeout, or its `execute` future was dropped).
    side_effect_late_results,
    /// SideEffect calls answered with their recorded result instead of running (same key).
    side_effect_replays,
    /// Identical calls of a later response run answered with the recorded result of an orphaned call.
    orphan_replays,
    /// Identical calls of a later turn refused while the orphaned call was still running (its
    /// result follows as a note).
    orphan_in_flight,
    /// Identical calls of a later turn refused because the orphaned call ended without a result
    /// (its outcome is unknown, so it is not run again).
    orphan_lost,
    /// Identical calls of a later response run refused because the orphaned call's result already
    /// reached the context as a late note (`already_reported`; the result is not repeated).
    orphan_reported,
    /// SideEffect calls refused because an identical call (same tool, same arguments) already ran
    /// in an earlier response of the same turn and its result reached the model (`already_done`).
    side_effect_repeats,
}

impl ToolStats {
    pub(super) fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// A 64-bit FNV-1a digest of a call's serialized arguments and their length. Two calls with the
/// same tool and the same digest are treated as the same call by the orphan check. `serde_json`
/// keeps object keys sorted, so the order the model wrote them in does not matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ArgsDigest {
    hash: u64,
    len: usize,
}

impl ArgsDigest {
    /// The digest of `value`. Serializes into a hasher: no allocation. The caller has already
    /// bounded `value` by `max_argument_bytes`.
    pub(super) fn of(value: &Value) -> Self {
        struct Hasher {
            hash: u64,
            len: usize,
        }
        impl io::Write for Hasher {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                for byte in data {
                    self.hash ^= u64::from(*byte);
                    self.hash = self.hash.wrapping_mul(0x0000_0100_0000_01b3);
                }
                self.len = self.len.saturating_add(data.len());
                Ok(data.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut hasher = Hasher {
            hash: 0xcbf2_9ce4_8422_2325,
            len: 0,
        };
        // Writing to the hasher cannot fail; a serialization error would only shorten the digest.
        let _ = serde_json::to_writer(&mut hasher, value);
        Self {
            hash: hasher.hash,
            len: hasher.len,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_digest_ignores_key_order_and_tells_values_apart() {
        let a = ArgsDigest::of(&json!({"day": "mon", "party": 4}));
        let b = ArgsDigest::of(&json!({"party": 4, "day": "mon"}));
        let c = ArgsDigest::of(&json!({"party": 5, "day": "mon"}));
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
