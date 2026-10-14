//! Tracing conventions shared by all crates (see `docs/TRACING.md`).
//!
//! Spans are created only through these constructors so field names never drift:
//! `session{id}` ⊃ `turn{epoch}` ⊃ `stage{name}`. Nothing here installs a subscriber; binaries
//! do that (`brisvox_server::telemetry::init`).

use std::fmt;

use tracing::Span;

use crate::epoch::TurnEpoch;

/// Targets of the sans-IO TURN client crates behind the server-side TURN relay.
///
/// At `trace` they log whole STUN messages (the long-term credential's USERNAME, the nonce and
/// the integrity HMAC) and their `#[instrument]` spans record arguments, and at `info` they log
/// on every poll. None of that belongs in a log.
pub const TURN_CLIENT_LOG_TARGETS: [&str; 3] = ["turn_client_proto", "stun_proto", "turn_types"];

/// Whether a `tracing` event or span may be recorded: `false` for the TURN client crates'
/// ([`TURN_CLIENT_LOG_TARGETS`]) events and spans below `WARN`, `true` for everything else.
///
/// A `RUST_LOG` directive cannot bring those back, because this is not a level directive but a
/// filter in front of the output. **Whoever installs their own subscriber must apply it too**
/// (the `brisvox-server` binary does): with `tracing_subscriber`,
/// `layer.with_filter(filter_fn(brisvox_core::telemetry::turn_client_log_allowed))`, combined
/// with the layer's own filter by `FilterExt::and`.
pub fn turn_client_log_allowed(meta: &tracing::Metadata<'_>) -> bool {
    target_allowed(meta, &TURN_CLIENT_LOG_TARGETS)
}

/// Targets of the SIP stack behind the SIP transport.
///
/// `rsipstack` logs every SIP message it receives or sends at `info`, whole and unredacted: the
/// digest responses, any extra header (an `X-Telnyx-Token`) and, on a secure call, the SDP with
/// the SDES `a=crypto` lines, which are the SRTP master keys.
pub const SIP_STACK_LOG_TARGETS: [&str; 1] = ["rsipstack"];

/// Whether a `tracing` event or span may be recorded: `false` for the SIP stack's
/// ([`SIP_STACK_LOG_TARGETS`]) events and spans below `WARN`, `true` for everything else. Applied
/// like [`turn_client_log_allowed`]: as a filter in front of the output, which `RUST_LOG` cannot
/// widen.
pub fn sip_stack_log_allowed(meta: &tracing::Metadata<'_>) -> bool {
    target_allowed(meta, &SIP_STACK_LOG_TARGETS)
}

fn target_allowed(meta: &tracing::Metadata<'_>, hidden_below_warn: &[&str]) -> bool {
    if *meta.level() <= tracing::Level::WARN {
        return true;
    }
    let target = meta.target();
    !hidden_below_warn.iter().any(|name| {
        target
            .strip_prefix(name)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    })
}

/// Span covering one session's whole lifetime. Level INFO.
pub fn session_span(id: &str) -> Span {
    tracing::info_span!("session", id = %id)
}

/// Span covering one user/agent turn, identified by its turn epoch. Level INFO.
pub fn turn_span(epoch: TurnEpoch) -> Span {
    tracing::info_span!("turn", epoch = epoch.get())
}

/// Span covering one pipeline stage inside a turn (e.g. `"stt"`, `"llm"`, `"tts"`).
/// Level DEBUG so it costs almost nothing when disabled. Never create one per audio frame.
pub fn stage_span(name: &'static str) -> Span {
    tracing::debug_span!("stage", name)
}

/// Wrapper for secrets (API keys, tokens, auth headers) that can never be logged by accident.
///
/// `Debug` and `Display` both print `[REDACTED]`, so `?secret`, `%secret` and `{:?}` on any
/// struct holding one are safe. The value is reachable only through [`Secret::expose`], which
/// makes every real use greppable. No `PartialEq`: comparing credentials with `==` is not
/// constant-time; compare via a constant-time routine on `expose()` if ever needed.
#[derive(Clone, Default)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    /// Text printed in place of the secret.
    pub const REDACTED: &'static str = "[REDACTED]";

    /// Wraps a secret value.
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// Returns the secret value. Call only where it is sent to its intended destination
    /// (e.g. an auth header), never to format it.
    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T> From<T> for Secret<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(Self::REDACTED)
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(Self::REDACTED)
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use super::*;

    const KEY: &str = "sk-live-0123456789abcdef";

    /// Collects everything a `fmt` subscriber writes.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| io::Error::other("capture lock poisoned"))?
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture(f: impl FnOnce()) -> String {
        let out = Capture::default();
        let writer = out.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = out.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[derive(Debug)]
    struct ProviderConfig {
        model: &'static str,
        api_key: Secret<String>,
    }

    #[test]
    fn secret_never_formats_its_value() {
        let s = Secret::new(KEY.to_owned());
        assert_eq!(format!("{s:?}"), "[REDACTED]");
        assert_eq!(format!("{s}"), "[REDACTED]");
        assert_eq!(s.expose(), KEY);
    }

    #[test]
    fn secret_is_redacted_in_tracing_output() {
        let cfg = ProviderConfig {
            model: "nova-3",
            api_key: Secret::from(KEY.to_owned()),
        };
        let logs = capture(|| {
            let _s = session_span("sess-1").entered();
            let _t = turn_span(TurnEpoch::new(7)).entered();
            tracing::info!(api_key = %cfg.api_key, "display field");
            tracing::info!(api_key = ?cfg.api_key, "debug field");
            tracing::info!(?cfg, "nested struct");
            tracing::error!("message {}", cfg.api_key);
        });
        assert!(!logs.contains(KEY), "secret leaked into logs:\n{logs}");
        assert_eq!(logs.matches("[REDACTED]").count(), 4, "{logs}");
        assert!(logs.contains("nova-3"), "{logs}");
        assert_eq!(cfg.model, "nova-3");
    }

    #[test]
    fn spans_use_the_documented_names_and_fields() {
        let logs = capture(|| {
            let _s = session_span("sess-42").entered();
            let _t = turn_span(TurnEpoch::new(3)).entered();
            let _g = stage_span("tts").entered();
            tracing::info!("hello");
        });
        assert!(
            logs.contains("session{id=sess-42}:turn{epoch=3}:stage{name=\"tts\"}"),
            "{logs}"
        );
    }
}
