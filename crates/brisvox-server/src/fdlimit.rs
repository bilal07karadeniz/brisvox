//! The server's open-file limit (`docs/DEPLOY.md`).
//!
//! Every call holds at least one socket, and a container (Docker, systemd) usually starts a
//! process with a soft `RLIMIT_NOFILE` of 1024 and a far higher hard limit: a server that does
//! not raise its own soft limit stops accepting at exactly the soft limit whatever CPU is left
//! (the first Linux capacity run stopped at 1017 calls, `accept()` returning `EMFILE`).
//! [`raise_nofile_soft_limit`] lifts the soft limit to the hard limit at startup, which is
//! allowed to any process. It never fails startup: a limit that cannot be raised is reported in
//! the returned [`NofileOutcome`], and the binary logs it as a warning. Windows has no such
//! limit; there it does nothing.

use std::fmt;

/// `RLIM_INFINITY` as `rlimit` reports it: the hard limit is not bounded.
const INFINITY: u64 = u64::MAX;

/// What to try when the hard limit is unbounded: the kernel refuses an unbounded soft limit for
/// `RLIMIT_NOFILE` (it is capped by `fs.nr_open`, 1048576 by default), so try the usual cap and
/// then smaller ones.
const UNBOUNDED_FALLBACKS: [u64; 3] = [1_048_576, 262_144, 65_536];

/// What [`raise_nofile_soft_limit`] found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NofileOutcome {
    /// The soft limit was raised from `before` to `after`; the hard limit is `hard`
    /// (`None`: unbounded).
    Raised {
        /// The soft limit found.
        before: u64,
        /// The soft limit now.
        after: u64,
        /// The hard limit (`None`: unbounded).
        hard: Option<u64>,
    },
    /// The soft limit already was the hard limit (or the platform has no such limit when
    /// `soft` is `None`): nothing to do.
    Unchanged {
        /// The soft limit (`None`: this platform has none).
        soft: Option<u64>,
        /// The hard limit (`None`: unbounded or none).
        hard: Option<u64>,
    },
    /// The limit could not be read or raised; the process keeps the limit it had.
    Failed {
        /// The soft limit found, if it could be read.
        soft: Option<u64>,
        /// The hard limit found (`None`: unbounded or unknown).
        hard: Option<u64>,
        /// Why.
        error: String,
    },
}

impl fmt::Display for NofileOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let show = |v: &Option<u64>| v.map_or_else(|| "unlimited".to_owned(), |v| v.to_string());
        match self {
            Self::Raised {
                before,
                after,
                hard,
            } => write!(
                f,
                "open-file soft limit raised from {before} to {after} (hard limit {})",
                show(hard)
            ),
            Self::Unchanged {
                soft: Some(soft),
                hard,
            } => write!(
                f,
                "open-file soft limit stays {soft} (hard limit {})",
                show(hard)
            ),
            Self::Unchanged { soft: None, .. } => {
                f.write_str("no open-file limit on this platform")
            }
            Self::Failed { soft, hard, error } => write!(
                f,
                "cannot raise the open-file soft limit (soft {}, hard {}): {error}",
                soft.map_or_else(|| "unknown".to_owned(), |v| v.to_string()),
                show(hard)
            ),
        }
    }
}

/// The soft limits to try, best first, for a process whose limits are `soft` and `hard`
/// (`u64::MAX`: unbounded). Empty when nothing can be gained.
#[cfg_attr(not(unix), allow(dead_code))] // exercised by the tests on every platform
fn candidates(soft: u64, hard: u64) -> Vec<u64> {
    if hard == INFINITY {
        if soft == INFINITY {
            return Vec::new();
        }
        UNBOUNDED_FALLBACKS
            .iter()
            .copied()
            .filter(|&limit| limit > soft)
            .collect()
    } else if hard > soft {
        vec![hard]
    } else {
        Vec::new()
    }
}

/// Raises the open-file soft limit to the hard limit (see the [module docs](self)).
#[cfg(unix)]
pub fn raise_nofile_soft_limit() -> NofileOutcome {
    use rlimit::Resource;

    let (soft, hard) = match Resource::NOFILE.get() {
        Ok(limits) => limits,
        Err(e) => {
            return NofileOutcome::Failed {
                soft: None,
                hard: None,
                error: format!("getrlimit: {e}"),
            };
        }
    };
    let bounded = |v: u64| (v != INFINITY).then_some(v);
    let tries = candidates(soft, hard);
    if tries.is_empty() {
        return NofileOutcome::Unchanged {
            soft: bounded(soft),
            hard: bounded(hard),
        };
    }
    let mut last_error = String::new();
    for target in tries {
        match Resource::NOFILE.set(target, hard) {
            Ok(()) => {
                // Read it back: the kernel is the judge of what is in force now.
                let after = Resource::NOFILE.get().map_or(target, |(soft, _)| soft);
                return NofileOutcome::Raised {
                    before: soft,
                    after,
                    hard: bounded(hard),
                };
            }
            Err(e) => last_error = format!("setrlimit({target}): {e}"),
        }
    }
    NofileOutcome::Failed {
        soft: bounded(soft),
        hard: bounded(hard),
        error: last_error,
    }
}

/// Windows has no `RLIMIT_NOFILE`: nothing to raise.
#[cfg(not(unix))]
pub fn raise_nofile_soft_limit() -> NofileOutcome {
    NofileOutcome::Unchanged {
        soft: None,
        hard: None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_bounded_hard_limit_is_the_target() {
        assert_eq!(candidates(1024, 1_048_576), vec![1_048_576]);
        assert_eq!(candidates(1024, 4096), vec![4096]);
    }

    #[test]
    fn nothing_is_tried_when_the_soft_limit_is_the_hard_limit() {
        assert!(candidates(4096, 4096).is_empty());
        assert!(candidates(INFINITY, INFINITY).is_empty());
        assert!(candidates(INFINITY, 4096).is_empty());
    }

    #[test]
    fn an_unbounded_hard_limit_tries_the_usual_caps_above_the_soft_limit() {
        assert_eq!(candidates(1024, INFINITY), vec![1_048_576, 262_144, 65_536]);
        assert_eq!(candidates(300_000, INFINITY), vec![1_048_576]);
        assert!(candidates(2_000_000, INFINITY).is_empty());
    }

    #[test]
    fn the_outcome_reads_as_one_log_line() {
        let raised = NofileOutcome::Raised {
            before: 1024,
            after: 1_048_576,
            hard: Some(1_048_576),
        };
        assert_eq!(
            raised.to_string(),
            "open-file soft limit raised from 1024 to 1048576 (hard limit 1048576)"
        );
        let failed = NofileOutcome::Failed {
            soft: Some(1024),
            hard: None,
            error: "setrlimit(65536): Operation not permitted".to_owned(),
        };
        assert!(failed.to_string().contains("hard unlimited"), "{failed}");
    }

    /// Raising a process-wide limit to the hard limit is harmless for the test process; the
    /// second call finds it done.
    #[cfg(unix)]
    #[test]
    fn the_process_ends_with_its_soft_limit_at_the_hard_limit_or_the_cap_tried() {
        let first = raise_nofile_soft_limit();
        assert!(
            !matches!(first, NofileOutcome::Failed { .. }),
            "raising to the hard limit is allowed to any process: {first}"
        );
        let (soft, hard) = rlimit::Resource::NOFILE.get().unwrap();
        assert!(soft >= 1024.min(hard), "{soft} / {hard}");
        if let NofileOutcome::Raised { before, after, .. } = first {
            assert!(after > before);
            assert_eq!(after, soft);
        }
        // Idempotent: a second call never fails and never lowers the limit.
        let again = raise_nofile_soft_limit();
        assert!(!matches!(again, NofileOutcome::Failed { .. }), "{again}");
        assert!(rlimit::Resource::NOFILE.get().unwrap().0 >= soft);
    }

    #[cfg(not(unix))]
    #[test]
    fn windows_has_nothing_to_raise() {
        let outcome = raise_nofile_soft_limit();
        assert_eq!(
            outcome,
            NofileOutcome::Unchanged {
                soft: None,
                hard: None
            }
        );
        assert_eq!(outcome.to_string(), "no open-file limit on this platform");
    }
}
