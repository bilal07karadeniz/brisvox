//! Installs the process-wide tracing subscriber (conventions in `docs/TRACING.md`).

use std::str::FromStr;
use std::sync::Arc;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer as _;
use tracing_subscriber::filter::{FilterExt as _, filter_fn};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt;

use crate::observability::{RedactingMakeWriter, Redactor};
use crate::privacy::{PiiKind, PiiKinds};
use crate::{Error, Result};

/// Filter used when `RUST_LOG` is unset. The TURN client crate logs every poll at info.
///
/// This is only a convenience for the level noise: a user-set `RUST_LOG` replaces it, so the
/// security-relevant part is the unconditional [`brisvox_core::telemetry::turn_client_log_allowed`] filter that
/// [`build_subscriber`] adds in front of every output.
pub const DEFAULT_FILTER: &str =
    "info,turn_client_proto=warn,stun_proto=warn,turn_types=warn,rsipstack=warn";

/// Log output format, chosen with `BRISVOX_LOG_FORMAT` (`text` | `json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Human-readable single-line output (development).
    #[default]
    Text,
    /// One JSON object per line (production log ingestion).
    Json,
}

impl FromStr for LogFormat {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(Error::Config(format!(
                "unknown log format {other:?} (expected \"text\" or \"json\")"
            ))),
        }
    }
}

/// Reads an environment variable. Unset → `Ok(None)`; set but not valid unicode → `Config`
/// error (never silently ignored).
pub fn env_value(name: &'static str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(Error::Config(format!("{name} is not valid unicode")))
        }
    }
}

/// Builds the level filter from a `RUST_LOG`-style directive string. `None` or a blank string
/// means [`DEFAULT_FILTER`]. Invalid directives are an error rather than being silently dropped.
pub fn build_filter(directives: Option<&str>) -> Result<EnvFilter> {
    let directives = match directives.map(str::trim) {
        Some(d) if !d.is_empty() => d,
        _ => DEFAULT_FILTER,
    };
    EnvFilter::try_new(directives)
        .map_err(|e| Error::Config(format!("invalid log filter {directives:?}: {e}")))
}

/// Environment variable: let the TURN client crates' own `tracing` events below `warn` through
/// (`true`/`1`). Off by default and **not for production**: at `trace` those crates log whole
/// STUN messages (the long-term credential's USERNAME, nonce and integrity HMAC).
pub const ENV_LOG_TURN_CLIENT: &str = "BRISVOX_LOG_TURN_CLIENT";

/// Environment variable: let the SIP stack's own `tracing` events (`rsipstack`) below `warn`
/// through (`true`/`1`). Off by default and **not for production**: the stack logs whole SIP
/// messages at `info`, with digest responses, extra headers and the SDES SRTP master keys.
pub const ENV_LOG_SIP_STACK: &str = "BRISVOX_LOG_SIP_STACK";

/// Which of the always-on log filters are lifted ([`ENV_LOG_TURN_CLIENT`], [`ENV_LOG_SIP_STACK`]).
/// The default lifts none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiftedFilters {
    /// Let the TURN client crates log below `warn`.
    pub turn_client: bool,
    /// Let the SIP stack log below `warn`.
    pub sip_stack: bool,
}

/// Builds the subscriber of `format` and `filter` that writes every event, scrubbed by
/// `redactor` (`docs/TRACING.md`), to `writer`. Nothing is installed.
///
/// Whatever `filter` says (a user-set `RUST_LOG` included), the events and spans of the TURN
/// client crates and of the SIP stack below `warn` are dropped: see
/// [`brisvox_core::telemetry::turn_client_log_allowed`] and
/// [`brisvox_core::telemetry::sip_stack_log_allowed`].
pub fn build_subscriber<W>(
    format: LogFormat,
    filter: EnvFilter,
    redactor: Arc<Redactor>,
    writer: W,
) -> Box<dyn tracing::Subscriber + Send + Sync>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    build_subscriber_with(format, filter, redactor, writer, LiftedFilters::default())
}

/// [`build_subscriber`] with some of the always-on filters lifted (`lifted`; `filter` still
/// decides the levels).
pub fn build_subscriber_with<W>(
    format: LogFormat,
    filter: EnvFilter,
    redactor: Arc<Redactor>,
    writer: W,
    lifted: LiftedFilters,
) -> Box<dyn tracing::Subscriber + Send + Sync>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    // A per-layer filter: it sits in front of the output, so no directive can widen it.
    let hard_filter = filter_fn(move |meta| {
        (lifted.turn_client || brisvox_core::telemetry::turn_client_log_allowed(meta))
            && (lifted.sip_stack || brisvox_core::telemetry::sip_stack_log_allowed(meta))
    });
    let filter = filter.and(hard_filter);
    let writer = RedactingMakeWriter::new(writer, redactor);
    let layer = tracing_subscriber::fmt::layer().with_writer(writer);
    match format {
        LogFormat::Text => Box::new(tracing_subscriber::registry().with(layer.with_filter(filter))),
        LogFormat::Json => {
            Box::new(tracing_subscriber::registry().with(layer.json().with_filter(filter)))
        }
    }
}

/// Installs the global subscriber with an explicit filter. Does not read the environment: the
/// log lines are scrubbed by the shape-based rules of [`Redactor::new`] only; [`init`] also
/// scrubs the values of the process's secret environment variables.
///
/// Fails with [`Error::Telemetry`] if a global subscriber is already installed.
pub fn install(format: LogFormat, filter: EnvFilter) -> Result<()> {
    install_with(format, filter, Arc::new(Redactor::new()))
}

/// [`install`] with an explicit [`Redactor`].
///
/// Fails with [`Error::Telemetry`] if a global subscriber is already installed.
pub fn install_with(format: LogFormat, filter: EnvFilter, redactor: Arc<Redactor>) -> Result<()> {
    install_inner(format, filter, redactor, LiftedFilters::default())
}

fn install_inner(
    format: LogFormat,
    filter: EnvFilter,
    redactor: Arc<Redactor>,
    lifted: LiftedFilters,
) -> Result<()> {
    build_subscriber_with(format, filter, redactor, std::io::stdout, lifted)
        .try_init()
        .map_err(|e| Error::Telemetry(e.to_string()))
}

/// Installs the global subscriber, filtered by `RUST_LOG` (default [`DEFAULT_FILTER`]) and
/// scrubbing the values of the process's secret environment variables (`*_KEY`, `*_TOKEN`,
/// `*_SECRET`, `*_PASSWORD`, ...) from every line, besides the shape-based rules.
///
/// Fails if `RUST_LOG` is invalid or not unicode, or a global subscriber is already installed.
///
/// With `BRISVOX_LOG_REDACT_PII=true` the lines are also scrubbed of personal identifiers (emails,
/// IBANs, national IDs, card and phone numbers; `BRISVOX_LOG_REDACT_KINDS` narrows the kinds,
/// default `email,iban,id,card,phone`), see `docs/PRIVACY.md`.
pub fn init(format: LogFormat) -> Result<()> {
    let rust_log = env_value("RUST_LOG")?;
    let redactor = log_redactor(&crate::config::ProcessEnv)?;
    let lifted = LiftedFilters {
        turn_client: crate::settings::flag_var(&crate::config::ProcessEnv, ENV_LOG_TURN_CLIENT)?
            .unwrap_or(false),
        sip_stack: crate::settings::flag_var(&crate::config::ProcessEnv, ENV_LOG_SIP_STACK)?
            .unwrap_or(false),
    };
    install_inner(
        format,
        build_filter(rust_log.as_deref())?,
        Arc::new(redactor),
        lifted,
    )
}

/// Environment variable: scrub personal identifiers from log lines (default `false`).
pub const ENV_LOG_REDACT_PII: &str = "BRISVOX_LOG_REDACT_PII";
/// Environment variable: the kinds the log scrubbing replaces (default `email,iban,id,card,phone`).
pub const ENV_LOG_REDACT_KINDS: &str = "BRISVOX_LOG_REDACT_KINDS";

/// The kinds the log scrubbing replaces when none are named: the identifiers that are rarely a
/// counter or a duration (a bare `number` run of 16+ digits is left out).
fn default_log_kinds() -> PiiKinds {
    PiiKinds::of([
        PiiKind::Email,
        PiiKind::Iban,
        PiiKind::Id,
        PiiKind::Card,
        PiiKind::Phone,
    ])
}

/// The log redactor of the process: the secrets of the environment, and the personal
/// identifiers when [`ENV_LOG_REDACT_PII`] says so.
///
/// # Errors
/// [`Error::Config`] naming the variable when a value does not parse, or when
/// [`ENV_LOG_REDACT_KINDS`] is set without [`ENV_LOG_REDACT_PII`].
pub fn log_redactor(env: &dyn crate::config::Env) -> Result<Redactor> {
    let on = crate::settings::flag_var(env, ENV_LOG_REDACT_PII)?;
    let kinds = crate::settings::var(env, ENV_LOG_REDACT_KINDS)?;
    let redactor = Redactor::from_process_env();
    if on != Some(true) {
        return match kinds {
            Some(_) => Err(Error::Config(format!(
                "{ENV_LOG_REDACT_KINDS} is set but {ENV_LOG_REDACT_PII} is not true: nothing would be redacted"
            ))),
            None => Ok(redactor),
        };
    }
    let kinds = match kinds {
        Some(list) => PiiKinds::parse(&list).map_err(|bad| {
            Error::Config(format!(
                "{ENV_LOG_REDACT_KINDS}: {bad:?} is not one of email, iban, id, card, phone, number"
            ))
        })?,
        None => default_log_kinds(),
    };
    if kinds.is_empty() {
        return Err(Error::Config(format!(
            "{ENV_LOG_REDACT_KINDS}: no kind is selected"
        )));
    }
    Ok(redactor.with_pii(kinds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_format_parses_case_insensitively() {
        assert_eq!("JSON".parse::<LogFormat>().unwrap(), LogFormat::Json);
        assert_eq!(" text ".parse::<LogFormat>().unwrap(), LogFormat::Text);
        assert!(matches!("yaml".parse::<LogFormat>(), Err(Error::Config(_))));
    }

    #[test]
    fn filter_rejects_invalid_directives() {
        assert!(build_filter(None).is_ok());
        assert!(build_filter(Some("  ")).is_ok());
        assert!(build_filter(Some("warn,brisvox_core=debug")).is_ok());
        assert!(matches!(
            build_filter(Some("brisvox_core=loud")),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn blank_filter_means_default_not_silence() {
        let blank = build_filter(Some("")).unwrap().to_string();
        assert_eq!(blank, build_filter(None).unwrap().to_string());
        assert_eq!(
            blank,
            build_filter(Some(DEFAULT_FILTER)).unwrap().to_string()
        );
    }

    #[test]
    fn second_install_is_an_error_not_a_panic() {
        // The only test that touches the global subscriber; independent of RUST_LOG.
        let _first = install(LogFormat::Text, build_filter(None).unwrap());
        let second = install(LogFormat::Json, build_filter(None).unwrap());
        assert!(matches!(second, Err(Error::Telemetry(_))), "{second:?}");
    }

    /// A writer into a shared buffer.
    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Logs, as the TURN client crates and a neutral target do, through a subscriber built with
    /// `directives` (what `RUST_LOG` would hold); returns what came out.
    fn emitted(format: LogFormat, directives: &str, turn_client_logs: bool) -> String {
        let capture = Capture::default();
        let sink = capture.clone();
        let subscriber = build_subscriber_with(
            format,
            build_filter(Some(directives)).unwrap(),
            Arc::new(Redactor::new()),
            move || sink.clone(),
            LiftedFilters {
                turn_client: turn_client_logs,
                sip_stack: false,
            },
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "turn_client_proto::protocol", "client-info-event");
            tracing::trace!(target: "stun_proto::agent", "stun-trace-event USERNAME nonce");
            tracing::debug!(target: "turn_types::attribute::connection", "types-debug-event");
            let span = tracing::trace_span!(target: "turn_client_proto::tcp", "turn_span");
            let _guard = span.enter();
            tracing::trace!(target: "turn_client_proto::tcp", "event-in-turn-span");
            tracing::warn!(target: "turn_client_proto::protocol", "client-warn-event");
            tracing::info!(target: "brisvox_transport::webrtc", "ours-info-event");
            tracing::info!(target: "turn_client_proto_like", "lookalike-info-event");
        });
        let bytes = capture.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    /// What the SIP stack and a neutral target log, through a subscriber with `lifted`.
    fn emitted_sip(directives: &str, lifted: LiftedFilters) -> String {
        let capture = Capture::default();
        let sink = capture.clone();
        let subscriber = build_subscriber_with(
            LogFormat::Text,
            build_filter(Some(directives)).unwrap(),
            Arc::new(Redactor::new()),
            move || sink.clone(),
            lifted,
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "rsipstack::transport::stream", "stack-info a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:KEYKEYKEY");
            tracing::trace!(target: "rsipstack::transaction", "stack-trace-event");
            tracing::warn!(target: "rsipstack::transport::tls", "stack-warn-event");
            tracing::info!(target: "brisvox_transport::sip", "ours-info-event");
            tracing::info!(target: "rsipstack_like", "lookalike-info-event");
        });
        String::from_utf8(capture.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn a_user_set_rust_log_cannot_bring_the_sip_stacks_logs_back() {
        for directives in ["trace", "info,rsipstack=trace", "debug"] {
            let out = emitted_sip(directives, LiftedFilters::default());
            for hidden in ["stack-info", "KEYKEYKEY", "stack-trace-event"] {
                assert!(!out.contains(hidden), "{directives}: {hidden} in {out}");
            }
            for shown in [
                "stack-warn-event",
                "ours-info-event",
                "lookalike-info-event",
            ] {
                assert!(
                    out.contains(shown),
                    "{directives}: {shown} missing in {out}"
                );
            }
        }
        // The explicit debugging switch lifts it; the level directives still decide.
        let lifted = LiftedFilters {
            turn_client: false,
            sip_stack: true,
        };
        assert!(emitted_sip("info", lifted).contains("stack-info"));
        assert!(!emitted_sip("warn", lifted).contains("stack-info"));
        assert!(DEFAULT_FILTER.contains("rsipstack=warn"));
    }

    #[test]
    fn a_user_set_rust_log_cannot_bring_the_turn_client_crates_logs_back() {
        for format in [LogFormat::Text, LogFormat::Json] {
            for directives in [
                "trace",
                "info,turn_client_proto=trace,stun_proto=trace,turn_types=trace",
                "debug",
            ] {
                let out = emitted(format, directives, false);
                for hidden in [
                    "client-info-event",
                    "stun-trace-event",
                    "types-debug-event",
                    "event-in-turn-span",
                    "turn_span",
                ] {
                    assert!(
                        !out.contains(hidden),
                        "{directives} {format:?}: {hidden} in {out}"
                    );
                }
                // Warnings, other targets and look-alike names are untouched.
                for shown in [
                    "client-warn-event",
                    "ours-info-event",
                    "lookalike-info-event",
                ] {
                    assert!(
                        out.contains(shown),
                        "{directives} {format:?}: {shown} missing in {out}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_turn_client_logs_come_back_only_when_asked_for_and_the_filter_allows_them() {
        let on = emitted(LogFormat::Text, "trace", true);
        assert!(
            on.contains("client-info-event") && on.contains("stun-trace-event"),
            "{on}"
        );
        // The flag lifts the extra filter, not the level filter.
        let off_level = emitted(LogFormat::Text, DEFAULT_FILTER, true);
        assert!(!off_level.contains("client-info-event"), "{off_level}");
        assert!(off_level.contains("ours-info-event"), "{off_level}");
    }

    fn vars(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|&(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }

    #[test]
    fn log_pii_redaction_is_off_unless_asked_for() {
        let line = "caller jane@example.com paid with 4111 1111 1111 1111";
        let off = log_redactor(&vars(&[])).unwrap();
        assert_eq!(off.redact(line), line);

        let on = log_redactor(&vars(&[(ENV_LOG_REDACT_PII, "true")])).unwrap();
        assert_eq!(on.redact(line), "caller [EMAIL] paid with [CARD]");
        // The credential rules still apply on top.
        assert!(
            on.redact("Authorization: Bearer abcdefgh12345678 for jane@example.com")
                .contains("[EMAIL]")
        );

        let only_email = log_redactor(&vars(&[
            (ENV_LOG_REDACT_PII, "1"),
            (ENV_LOG_REDACT_KINDS, "email"),
        ]))
        .unwrap();
        assert_eq!(
            only_email.redact(line),
            "caller [EMAIL] paid with 4111 1111 1111 1111"
        );
    }

    #[test]
    fn log_pii_settings_that_would_do_nothing_are_errors() {
        for pairs in [
            vec![(ENV_LOG_REDACT_KINDS, "email")],
            vec![
                (ENV_LOG_REDACT_PII, "false"),
                (ENV_LOG_REDACT_KINDS, "email"),
            ],
            vec![
                (ENV_LOG_REDACT_PII, "true"),
                (ENV_LOG_REDACT_KINDS, "address"),
            ],
            vec![(ENV_LOG_REDACT_PII, "true"), (ENV_LOG_REDACT_KINDS, ",")],
            vec![(ENV_LOG_REDACT_PII, "maybe")],
        ] {
            let err = log_redactor(&vars(&pairs)).unwrap_err();
            assert!(matches!(err, Error::Config(_)), "{pairs:?}: {err}");
        }
    }
}
