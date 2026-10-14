//! Process-level settings of the server binary: admission limit, health listener
//! address and the shutdown timeouts. They come from environment variables (the agent
//! definition files describe agents, not the process); see `docs/OPERATIONS.md`.
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `BRISVOX_MAX_SESSIONS` | 256 | sessions live or reserved at once |
//! | `BRISVOX_HEALTH_ADDR` | `0.0.0.0:8081` | `/healthz` + `/readyz` listener; `off` disables it; `${PORT}` stands for the `PORT` variable |
//! | `BRISVOX_METRICS_TOKEN` | unset | bearer token for `/metrics` when the health address is not loopback; without it `/metrics` is off there |
//! | `BRISVOX_INSECURE_LISTENERS` | `false` | accept listeners without authentication on non-loopback addresses (logged loudly) |
//! | `BRISVOX_DRAIN_TIMEOUT` | `30s` | how long a drain waits for calls to end by themselves |
//! | `BRISVOX_CANCEL_GRACE` | `5s` | how long cancelled sessions get to return |
//! | `BRISVOX_ABORT_WAIT` | `1s` | how long aborted tasks get to clean up |
//!
//! Durations are a whole number followed by `ms`, `s` or `m`; a bare number means seconds.

use std::env::VarError;
use std::net::SocketAddr;
use std::time::Duration;

use brisvox_core::Secret;

use crate::config::Env;
use crate::session::{ManagerConfig, ShutdownPolicy};
use crate::{Error, Result};

/// Environment variable: most sessions live or reserved at once.
pub const ENV_MAX_SESSIONS: &str = "BRISVOX_MAX_SESSIONS";
/// Environment variable: health listener address, or `off`.
pub const ENV_HEALTH_ADDR: &str = "BRISVOX_HEALTH_ADDR";
/// Environment variable: bearer token that `/metrics` requires on a health address that is not
/// loopback.
pub const ENV_METRICS_TOKEN: &str = "BRISVOX_METRICS_TOKEN";
/// Environment variable: `1` lets listeners without authentication bind non-loopback addresses
/// (and `/metrics` be served without a token there). Development and private networks only.
pub const ENV_INSECURE_LISTENERS: &str = "BRISVOX_INSECURE_LISTENERS";
/// Environment variable: drain stage timeout.
pub const ENV_DRAIN_TIMEOUT: &str = "BRISVOX_DRAIN_TIMEOUT";
/// Environment variable: cancel stage grace.
pub const ENV_CANCEL_GRACE: &str = "BRISVOX_CANCEL_GRACE";
/// Environment variable: abort stage wait.
pub const ENV_ABORT_WAIT: &str = "BRISVOX_ABORT_WAIT";

/// Default health listener address: every interface (containers are probed from outside the
/// network namespace), port 8081 (kept apart from the transports' ports).
pub const DEFAULT_HEALTH_ADDR: &str = "0.0.0.0:8081";

/// Longest accepted shutdown timeout (one day): a larger value is a typo, not a policy.
pub const MAX_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(86_400);

/// Settings of the server process.
#[derive(Debug, Clone)]
pub struct ServerSettings {
    /// Admission limit of the session manager.
    pub manager: ManagerConfig,
    /// Where `/healthz` and `/readyz` are served; `None` disables the listener.
    pub health_addr: Option<SocketAddr>,
    /// Bearer token `/metrics` requires on a health address that is not loopback.
    /// Without one `/metrics` is off there (unless `insecure_listeners`). Never logged.
    pub metrics_token: Option<Secret<String>>,
    /// `BRISVOX_INSECURE_LISTENERS`: serve `/metrics` without a token on a non-loopback health
    /// address (and, in [`HostSettings`](crate::HostSettings), listeners without authentication).
    pub insecure_listeners: bool,
    /// Timeouts of the drain, cancel and abort stages run on `SIGTERM`/`SIGINT` (Ctrl-C,
    /// Ctrl-Break on Windows).
    pub shutdown: ShutdownPolicy,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            manager: ManagerConfig::default(),
            health_addr: DEFAULT_HEALTH_ADDR.parse().ok(),
            metrics_token: None,
            insecure_listeners: false,
            shutdown: ShutdownPolicy::default(),
        }
    }
}

impl ServerSettings {
    /// Reads the settings from `env`; unset variables keep their defaults.
    ///
    /// # Errors
    /// [`Error::Config`] naming the variable when a value is not UTF-8 or does not parse, a
    /// limit is zero, or a timeout exceeds [`MAX_SHUTDOWN_TIMEOUT`].
    pub fn from_env(env: &dyn Env) -> Result<Self> {
        let mut settings = Self::default();
        if let Some(v) = var(env, ENV_MAX_SESSIONS)? {
            let max = v.parse::<usize>().map_err(|_| {
                Error::Config(format!("{ENV_MAX_SESSIONS}: {v:?} is not a whole number"))
            })?;
            settings.manager = ManagerConfig::new(max)
                .map_err(|e| Error::Config(format!("{ENV_MAX_SESSIONS}: {e}")))?;
        }
        if let Some(v) = addr_var(env, ENV_HEALTH_ADDR)? {
            settings.health_addr = if v.eq_ignore_ascii_case("off") {
                None
            } else {
                Some(v.parse::<SocketAddr>().map_err(|_| {
                    Error::Config(format!(
                        "{ENV_HEALTH_ADDR}: {v:?} is not an address like 0.0.0.0:8081 (or `off`)"
                    ))
                })?)
            };
        }
        settings.metrics_token = var(env, ENV_METRICS_TOKEN)?.map(Secret::new);
        if settings
            .metrics_token
            .as_ref()
            .is_some_and(|t| t.expose().chars().any(char::is_whitespace))
        {
            return Err(Error::Config(format!(
                "{ENV_METRICS_TOKEN} must not contain whitespace"
            )));
        }
        settings.insecure_listeners = flag_var(env, ENV_INSECURE_LISTENERS)?.unwrap_or(false);
        if let Some(v) = var(env, ENV_DRAIN_TIMEOUT)? {
            settings.shutdown.drain_timeout = duration(ENV_DRAIN_TIMEOUT, &v)?;
        }
        if let Some(v) = var(env, ENV_CANCEL_GRACE)? {
            settings.shutdown.cancel_grace = duration(ENV_CANCEL_GRACE, &v)?;
        }
        if let Some(v) = var(env, ENV_ABORT_WAIT)? {
            settings.shutdown.abort_wait = duration(ENV_ABORT_WAIT, &v)?;
        }
        Ok(settings)
    }
}

/// The trimmed value of `name`, `None` when unset or blank.
pub(crate) fn var(env: &dyn Env, name: &str) -> Result<Option<String>> {
    match env.var(name) {
        Ok(v) => {
            let v = v.trim();
            Ok((!v.is_empty()).then(|| v.to_owned()))
        }
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(Error::Config(format!("{name} is not valid unicode"))),
    }
}

/// Environment variable PaaS hosts (Railway, Heroku, Fly, Cloud Run) set to the one port the
/// application must listen on; read only for the [`PORT_PLACEHOLDER`].
pub const ENV_PORT: &str = "PORT";

/// The token a listener address variable (`BRISVOX_*_ADDR`) may contain, replaced by the value of
/// the `PORT` variable: `BRISVOX_WS_ADDR=[::]:${PORT}`. Exactly this text, nothing else is
/// expanded.
pub const PORT_PLACEHOLDER: &str = "${PORT}";

/// The value of the listener address variable `name` (trimmed, `None` when unset or blank) with
/// the [`PORT_PLACEHOLDER`] replaced by `PORT`.
///
/// # Errors
/// [`Error::Config`] naming both variables when the value uses the placeholder and `PORT` is
/// unset or not a port number (1 to 65535), or when it holds another `$` (nothing else is
/// expanded).
pub(crate) fn addr_var(env: &dyn Env, name: &str) -> Result<Option<String>> {
    let Some(value) = var(env, name)? else {
        return Ok(None);
    };
    expand_port(env, name, &value).map(Some)
}

/// [`addr_var`] for a value already read.
pub(crate) fn expand_port(env: &dyn Env, name: &str, value: &str) -> Result<String> {
    if !value.contains(PORT_PLACEHOLDER) {
        return Ok(value.to_owned());
    }
    let port = match env.var(ENV_PORT) {
        Ok(port) => port,
        Err(VarError::NotPresent) => {
            return Err(Error::Config(format!(
                "{name} uses {PORT_PLACEHOLDER}, but {ENV_PORT} is not set (the host provides it, or write the port number in {name})"
            )));
        }
        Err(VarError::NotUnicode(_)) => {
            return Err(Error::Config(format!(
                "{name} uses {PORT_PLACEHOLDER}, but {ENV_PORT} is not valid unicode"
            )));
        }
    };
    let port = port.trim();
    match port.parse::<u16>() {
        Ok(n) if n != 0 => {
            let expanded = value.replace(PORT_PLACEHOLDER, &n.to_string());
            if expanded.contains('$') {
                return Err(Error::Config(format!(
                    "{name}: {value:?} has a `$` that is not {PORT_PLACEHOLDER} (only that placeholder is expanded)"
                )));
            }
            Ok(expanded)
        }
        _ => Err(Error::Config(format!(
            "{name} uses {PORT_PLACEHOLDER}, but {ENV_PORT} is {port:?}, not a port number from 1              to 65535"
        ))),
    }
}

/// `name` as a duration ([`duration`]), `None` when unset or blank.
pub(crate) fn duration_var(env: &dyn Env, name: &str) -> Result<Option<Duration>> {
    var(env, name)?.map(|v| duration(name, &v)).transpose()
}

/// `name` as `true`/`false` (also `1`/`0`, `yes`/`no`, `on`/`off`), `None` when unset or blank.
pub(crate) fn flag_var(env: &dyn Env, name: &str) -> Result<Option<bool>> {
    let Some(v) = var(env, name)? else {
        return Ok(None);
    };
    match v.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(Some(true)),
        "false" | "0" | "no" | "off" => Ok(Some(false)),
        _ => Err(Error::Config(format!("{name}: {v:?} is not true or false"))),
    }
}

/// `name` as a whole number in `min..=max`, `None` when unset or blank.
pub(crate) fn count_var(
    env: &dyn Env,
    name: &str,
    min: usize,
    max: usize,
) -> Result<Option<usize>> {
    let Some(v) = var(env, name)? else {
        return Ok(None);
    };
    match v.parse::<usize>() {
        Ok(n) if (min..=max).contains(&n) => Ok(Some(n)),
        _ => Err(Error::Config(format!(
            "{name}: {v:?} is not a whole number from {min} to {max}"
        ))),
    }
}

/// Parses `30s`, `500ms`, `2m` or a bare number of seconds, at most [`MAX_SHUTDOWN_TIMEOUT`].
pub(crate) fn duration(name: &str, text: &str) -> Result<Duration> {
    let bad = || {
        Error::Config(format!(
            "{name}: {text:?} is not a duration like 30s, 500ms or 2m"
        ))
    };
    let (digits, unit_ms) = if let Some(d) = text.strip_suffix("ms") {
        (d, 1)
    } else if let Some(d) = text.strip_suffix('s') {
        (d, 1_000)
    } else if let Some(d) = text.strip_suffix('m') {
        (d, 60_000)
    } else {
        (text, 1_000)
    };
    let value = digits.trim().parse::<u64>().map_err(|_| bad())?;
    let ms = value.checked_mul(unit_ms).ok_or_else(bad)?;
    let d = Duration::from_millis(ms);
    if d > MAX_SHUTDOWN_TIMEOUT {
        return Err(Error::Config(format!(
            "{name}: {text:?} is longer than {} s",
            MAX_SHUTDOWN_TIMEOUT.as_secs()
        )));
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn defaults_when_unset() {
        let s = ServerSettings::from_env(&env(&[])).unwrap();
        assert_eq!(format!("{s:?}"), format!("{:?}", ServerSettings::default()));
        assert!(s.metrics_token.is_none());
        assert!(!s.insecure_listeners);
        assert_eq!(s.health_addr, Some("0.0.0.0:8081".parse().unwrap()));
        assert_eq!(s.shutdown, ShutdownPolicy::default());
        assert_eq!(s.manager.max_sessions, ManagerConfig::DEFAULT_MAX_SESSIONS);
    }

    #[test]
    fn reads_every_variable() {
        let s = ServerSettings::from_env(&env(&[
            (ENV_MAX_SESSIONS, " 12 "),
            (ENV_HEALTH_ADDR, "127.0.0.1:9000"),
            (ENV_DRAIN_TIMEOUT, "2m"),
            (ENV_CANCEL_GRACE, "1500ms"),
            (ENV_ABORT_WAIT, "3"),
        ]))
        .unwrap();
        assert_eq!(s.manager.max_sessions, 12);
        assert_eq!(s.health_addr, Some("127.0.0.1:9000".parse().unwrap()));
        assert_eq!(s.shutdown.drain_timeout, Duration::from_secs(120));
        assert_eq!(s.shutdown.cancel_grace, Duration::from_millis(1500));
        assert_eq!(s.shutdown.abort_wait, Duration::from_secs(3));
    }

    #[test]
    fn reads_the_metrics_token_and_the_insecure_flag_and_never_prints_the_token() {
        let s = ServerSettings::from_env(&env(&[
            (ENV_METRICS_TOKEN, "scrape-secret-123"),
            (ENV_INSECURE_LISTENERS, "1"),
        ]))
        .unwrap();
        assert_eq!(
            s.metrics_token.as_ref().unwrap().expose(),
            "scrape-secret-123"
        );
        assert!(s.insecure_listeners);
        assert!(!format!("{s:?}").contains("scrape-secret-123"));
        for (name, value) in [
            (ENV_METRICS_TOKEN, "a b"),
            (ENV_INSECURE_LISTENERS, "maybe"),
        ] {
            let err = ServerSettings::from_env(&env(&[(name, value)])).unwrap_err();
            assert!(err.to_string().contains(name), "{err}");
            assert!(!err.to_string().contains("a b"), "{err}");
        }
    }

    #[test]
    fn health_listener_can_be_disabled_and_zero_drain_is_allowed() {
        let s =
            ServerSettings::from_env(&env(&[(ENV_HEALTH_ADDR, "OFF"), (ENV_DRAIN_TIMEOUT, "0s")]))
                .unwrap();
        assert_eq!(s.health_addr, None);
        assert_eq!(s.shutdown.drain_timeout, Duration::ZERO);
    }

    #[test]
    fn the_port_placeholder_is_replaced_by_the_port_variable() {
        let s = ServerSettings::from_env(&env(&[
            (ENV_HEALTH_ADDR, "[::]:${PORT}"),
            (ENV_PORT, " 8080 "),
        ]))
        .unwrap();
        assert_eq!(s.health_addr, Some("[::]:8080".parse().unwrap()));
        // A value without the placeholder never reads PORT, even when PORT is garbage.
        let s =
            ServerSettings::from_env(&env(&[(ENV_HEALTH_ADDR, "0.0.0.0:9000"), (ENV_PORT, "x")]))
                .unwrap();
        assert_eq!(s.health_addr, Some("0.0.0.0:9000".parse().unwrap()));
        // `off` needs no PORT either.
        let s = ServerSettings::from_env(&env(&[(ENV_HEALTH_ADDR, "off")])).unwrap();
        assert_eq!(s.health_addr, None);
    }

    #[test]
    fn an_unusable_port_variable_is_an_error_naming_both_variables() {
        for port in [
            None,
            Some(""),
            Some("abc"),
            Some("0"),
            Some("65536"),
            Some("-1"),
        ] {
            let mut pairs = vec![(ENV_HEALTH_ADDR, "0.0.0.0:${PORT}")];
            if let Some(port) = port {
                pairs.push((ENV_PORT, port));
            }
            let err = ServerSettings::from_env(&env(&pairs))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(ENV_HEALTH_ADDR) && err.contains("PORT"),
                "{port:?}: {err}"
            );
        }
    }

    #[test]
    fn only_the_exact_placeholder_is_expanded() {
        for value in [
            "0.0.0.0:$PORT",
            "0.0.0.0:${port}",
            "0.0.0.0:${PORT}${PORT}$",
        ] {
            let err =
                ServerSettings::from_env(&env(&[(ENV_HEALTH_ADDR, value), (ENV_PORT, "8080")]))
                    .unwrap_err()
                    .to_string();
            assert!(err.contains(ENV_HEALTH_ADDR), "{value}: {err}");
        }
    }

    #[test]
    fn bad_values_name_the_variable() {
        for (name, value) in [
            (ENV_MAX_SESSIONS, "0"),
            (ENV_MAX_SESSIONS, "many"),
            // Above the cap: a config error, not a panic in a semaphore at startup.
            (ENV_MAX_SESSIONS, "100001"),
            (ENV_MAX_SESSIONS, "18446744073709551615"),
            (ENV_HEALTH_ADDR, "localhost"),
            (ENV_DRAIN_TIMEOUT, "30 seconds"),
            (ENV_DRAIN_TIMEOUT, "-1s"),
            (ENV_CANCEL_GRACE, "2h"),
            (ENV_ABORT_WAIT, "86401"),
            (ENV_ABORT_WAIT, "99999999999999999999m"),
        ] {
            let err = ServerSettings::from_env(&env(&[(name, value)])).unwrap_err();
            assert!(
                err.to_string().contains(name),
                "{name}={value}: {err} does not name the variable"
            );
        }
    }
}
