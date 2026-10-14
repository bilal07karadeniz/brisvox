//! The recording settings, from the environment and the agent file's `[recording]` table
//! (`docs/PRIVACY.md`, `docs/CONFIG.md`).
//!
//! The environment sets the directory and the defaults of every server; the agent file may
//! override the content settings per deployment ([`RecordingSettings::with_file`]): the file wins
//! over the environment. The directory itself is environment-only (it is infrastructure, not
//! agent behaviour).

use std::path::PathBuf;
use std::time::Duration;

use super::redact::PiiKinds;
use super::sink::AgeRecipients;
use crate::config::Env;
use crate::error::{Error, Result};
use crate::settings::{count_var, flag_var, var};

/// Environment variable: the directory recordings go to. Unset: nothing is recorded.
pub const ENV_RECORDING_DIR: &str = "BRISVOX_RECORDING_DIR";
/// Environment variable: write each call's transcript (default `true` once the directory is set).
pub const ENV_RECORDING_TRANSCRIPTS: &str = "BRISVOX_RECORDING_TRANSCRIPTS";
/// Environment variable: write each call's audio (default `false`: audio is the most sensitive
/// thing a call produces and cannot be redacted).
pub const ENV_RECORDING_AUDIO: &str = "BRISVOX_RECORDING_AUDIO";
/// Environment variable: replace emails, IBANs, ID, card and phone numbers in transcripts
/// (default `true`).
pub const ENV_RECORDING_REDACT_PII: &str = "BRISVOX_RECORDING_REDACT_PII";
/// Environment variable: which kinds of identifier the redaction replaces, comma-separated
/// (`email,iban,id,card,phone,number`; default all).
pub const ENV_RECORDING_REDACT_KINDS: &str = "BRISVOX_RECORDING_REDACT_KINDS";
/// Alias of [`ENV_RECORDING_TRANSCRIPTS`] (the first name of the variable, kept working).
pub const ENV_RECORD_TRANSCRIPTS: &str = "BRISVOX_RECORD_TRANSCRIPTS";
/// Alias of [`ENV_RECORDING_AUDIO`] (the first name of the variable, kept working).
pub const ENV_RECORD_AUDIO: &str = "BRISVOX_RECORD_AUDIO";
/// Alias of [`ENV_RECORDING_REDACT_PII`] (the first name of the variable, kept working).
pub const ENV_REDACT_PII: &str = "BRISVOX_REDACT_PII";
/// Environment variable: how long recordings are kept (`30d`, `72h`, `forever`; default `30d`).
pub const ENV_RECORDING_RETENTION: &str = "BRISVOX_RECORDING_RETENTION";
/// Environment variable: how often the retention sweep runs (default `1h`).
pub const ENV_RECORDING_SWEEP_INTERVAL: &str = "BRISVOX_RECORDING_SWEEP_INTERVAL";
/// Environment variable: the longest audio recorded per call, in minutes (default 120).
pub const ENV_RECORDING_MAX_MINUTES: &str = "BRISVOX_RECORDING_MAX_MINUTES";
/// Comma-separated age X25519 recipients (`age1...`): recordings are encrypted to them and
/// written as `<name>.age` (cargo feature `recording-encryption`).
pub const ENV_RECORDING_AGE_RECIPIENTS: &str = "BRISVOX_RECORDING_AGE_RECIPIENTS";

/// Default retention.
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(30 * 86_400);
/// Default time between retention sweeps.
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(3_600);
/// Default cap of recorded audio per call.
pub const DEFAULT_MAX_AUDIO: Duration = Duration::from_secs(120 * 60);
/// Largest per-call audio cap: a 48 kHz mono PCM16 WAV of this length still fits the format's
/// 4 GiB data limit.
pub const MAX_AUDIO_MINUTES: usize = 600;
/// Shortest retention: a sweep must never take a file of a call that is still running.
pub const MIN_RETENTION: Duration = Duration::from_secs(3_600);
/// Shortest sweep interval.
pub const MIN_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// Longest retention that is not `forever` (ten years).
pub const MAX_RETENTION: Duration = Duration::from_secs(3_650 * 86_400);

/// What to record and for how long to keep it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingSettings {
    /// The directory the files are written to (created at startup).
    pub dir: PathBuf,
    /// Write a JSON-lines transcript per call.
    pub transcripts: bool,
    /// Write two mono WAV files per call (caller and bot).
    pub audio: bool,
    /// Redact PII from the transcripts ([`crate::privacy::redact_pii`]).
    pub redact_pii: bool,
    /// Which kinds of identifier the redaction replaces (all by default).
    pub redact_kinds: PiiKinds,
    /// Delete recordings older than this; `None` keeps them forever.
    pub retention: Option<Duration>,
    /// Time between retention sweeps.
    pub sweep_interval: Duration,
    /// Longest audio kept per call and track.
    pub max_audio: Duration,
    /// Encrypt every recording file to these age recipients (`.age` files); `None` writes
    /// plain files.
    pub age_recipients: Option<AgeRecipients>,
}

impl RecordingSettings {
    /// Settings for `dir` with the defaults: transcripts only, PII redacted, 30 days.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            transcripts: true,
            audio: false,
            redact_pii: true,
            redact_kinds: PiiKinds::ALL,
            retention: Some(DEFAULT_RETENTION),
            sweep_interval: DEFAULT_SWEEP_INTERVAL,
            max_audio: DEFAULT_MAX_AUDIO,
            age_recipients: None,
        }
    }

    /// The recording settings from `env`; `None` when no recording variable is set.
    ///
    /// # Errors
    /// [`Error::Config`] naming the variable when a value does not parse or is out of range,
    /// when a recording variable is set without the directory (a privacy setting must not be
    /// silently ignored), when neither transcripts nor audio would be recorded, or when the
    /// retention is shorter than the longest recorded call.
    pub fn from_env(env: &dyn Env) -> Result<Option<Self>> {
        let dir = var(env, ENV_RECORDING_DIR)?;
        let transcripts = flag_alias(env, ENV_RECORDING_TRANSCRIPTS, ENV_RECORD_TRANSCRIPTS)?;
        let audio = flag_alias(env, ENV_RECORDING_AUDIO, ENV_RECORD_AUDIO)?;
        let redact = flag_alias(env, ENV_RECORDING_REDACT_PII, ENV_REDACT_PII)?;
        let kinds = var(env, ENV_RECORDING_REDACT_KINDS)?;
        let retention = var(env, ENV_RECORDING_RETENTION)?;
        let sweep = var(env, ENV_RECORDING_SWEEP_INTERVAL)?;
        let minutes = count_var(env, ENV_RECORDING_MAX_MINUTES, 1, MAX_AUDIO_MINUTES)?;
        let age = var(env, ENV_RECORDING_AGE_RECIPIENTS)?;
        let Some(dir) = dir else {
            let stray = [
                (
                    ENV_RECORDING_TRANSCRIPTS,
                    var(env, ENV_RECORDING_TRANSCRIPTS)?.is_some(),
                ),
                (
                    ENV_RECORDING_AUDIO,
                    var(env, ENV_RECORDING_AUDIO)?.is_some(),
                ),
                (
                    ENV_RECORDING_REDACT_PII,
                    var(env, ENV_RECORDING_REDACT_PII)?.is_some(),
                ),
                (
                    ENV_RECORD_TRANSCRIPTS,
                    var(env, ENV_RECORD_TRANSCRIPTS)?.is_some(),
                ),
                (ENV_RECORD_AUDIO, var(env, ENV_RECORD_AUDIO)?.is_some()),
                (ENV_REDACT_PII, var(env, ENV_REDACT_PII)?.is_some()),
                (ENV_RECORDING_REDACT_KINDS, kinds.is_some()),
                (ENV_RECORDING_RETENTION, retention.is_some()),
                (ENV_RECORDING_SWEEP_INTERVAL, sweep.is_some()),
                (ENV_RECORDING_MAX_MINUTES, minutes.is_some()),
                (ENV_RECORDING_AGE_RECIPIENTS, age.is_some()),
            ]
            .into_iter()
            .find_map(|(name, set)| set.then_some(name));
            return match stray {
                Some(name) => Err(Error::Config(format!(
                    "{name} is set but {ENV_RECORDING_DIR} is not: nothing would be recorded"
                ))),
                None => Ok(None),
            };
        };
        let mut settings = Self::new(dir);
        if let Some(on) = transcripts {
            settings.transcripts = on;
        }
        if let Some(on) = audio {
            settings.audio = on;
        }
        if let Some(on) = redact {
            settings.redact_pii = on;
        }
        if let Some(text) = kinds {
            settings.redact_kinds = PiiKinds::parse(&text).map_err(|bad| {
                Error::Config(format!(
                    "{ENV_RECORDING_REDACT_KINDS}: {bad:?} is not one of email, iban, id, card, phone, number"
                ))
            })?;
        }
        if let Some(text) = retention {
            settings.retention = parse_retention(ENV_RECORDING_RETENTION, &text)?;
        }
        if let Some(text) = sweep {
            settings.sweep_interval = parse_span(ENV_RECORDING_SWEEP_INTERVAL, &text)?;
        }
        if let Some(text) = age {
            settings.age_recipients = Some(
                AgeRecipients::parse(&text)
                    .map_err(|e| Error::Config(format!("{ENV_RECORDING_AGE_RECIPIENTS}: {e}")))?,
            );
        }
        if let Some(minutes) = minutes {
            settings.max_audio = Duration::from_secs(minutes as u64 * 60);
        }
        settings.validate()?;
        Ok(Some(settings))
    }

    /// These settings with the agent file's `[recording]` table applied on top of them: a key
    /// the file sets wins over the environment. Validated again.
    ///
    /// # Errors
    /// [`Error::Config`] when the result is inconsistent (see [`Self::validate`]).
    pub fn with_file(mut self, file: &RecordingFileSettings) -> Result<Self> {
        if let Some(on) = file.transcripts {
            self.transcripts = on;
        }
        if let Some(on) = file.audio {
            self.audio = on;
        }
        if let Some(on) = file.redact_pii {
            self.redact_pii = on;
        }
        if let Some(kinds) = file.redact_kinds {
            self.redact_kinds = kinds;
        }
        if let Some(retention) = file.retention {
            self.retention = Some(retention);
        }
        self.validate()?;
        Ok(self)
    }

    /// Checks the settings against each other.
    ///
    /// # Errors
    /// [`Error::Config`] when nothing would be recorded, the retention is shorter than
    /// [`MIN_RETENTION`] or than the longest recorded call, or the sweep interval is shorter
    /// than [`MIN_SWEEP_INTERVAL`].
    pub fn validate(&self) -> Result<()> {
        if !self.transcripts && !self.audio {
            return Err(Error::Config(format!(
                "{ENV_RECORDING_DIR} is set but both {ENV_RECORD_TRANSCRIPTS} and {ENV_RECORD_AUDIO} are off: nothing would be recorded"
            )));
        }
        if let Some(retention) = self.retention {
            if retention < MIN_RETENTION {
                return Err(Error::Config(format!(
                    "{ENV_RECORDING_RETENTION}: must be at least 1h, so a sweep never takes the files of a call in progress"
                )));
            }
            if self.audio && retention < self.max_audio {
                return Err(Error::Config(format!(
                    "{ENV_RECORDING_RETENTION}: shorter than the longest recorded call ({ENV_RECORDING_MAX_MINUTES})"
                )));
            }
        }
        if self.redact_pii && self.redact_kinds.is_empty() {
            return Err(Error::Config(format!(
                "{ENV_RECORDING_REDACT_KINDS}: redaction is on but no kind is selected; turn redaction off or name the kinds"
            )));
        }
        if self.sweep_interval < MIN_SWEEP_INTERVAL {
            return Err(Error::Config(format!(
                "{ENV_RECORDING_SWEEP_INTERVAL}: must be at least 1m"
            )));
        }
        Ok(())
    }
}

/// What the agent file's `[recording]` table sets; `None` leaves the environment's value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecordingFileSettings {
    /// `transcript`.
    pub transcripts: Option<bool>,
    /// `audio`.
    pub audio: Option<bool>,
    /// `redact`.
    pub redact_pii: Option<bool>,
    /// `redact_kinds`.
    pub redact_kinds: Option<PiiKinds>,
    /// `retention_days`.
    pub retention: Option<Duration>,
}

impl RecordingFileSettings {
    /// Whether the table sets nothing.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The recording settings of a server: the environment's, with the agent file's `[recording]`
/// table on top.
///
/// # Errors
/// [`Error::Config`] when the file sets recording keys but `BRISVOX_RECORDING_DIR` is not set
/// (a privacy setting must not be silently ignored), or when the merged settings are
/// inconsistent.
pub fn resolve_settings(
    env: Option<RecordingSettings>,
    file: &RecordingFileSettings,
) -> Result<Option<RecordingSettings>> {
    match env {
        Some(settings) => settings.with_file(file).map(Some),
        None if file.is_empty() => Ok(None),
        None => Err(Error::Config(format!(
            "the agent file has a [recording] table but {ENV_RECORDING_DIR} is not set: nothing would be recorded"
        ))),
    }
}

/// The flag `canonical`, else its older name `alias`. Both set to different values is an error:
/// a privacy setting must not depend on which name wins.
fn flag_alias(env: &dyn Env, canonical: &str, alias: &str) -> Result<Option<bool>> {
    let new = flag_var(env, canonical)?;
    let old = flag_var(env, alias)?;
    match (new, old) {
        (Some(a), Some(b)) if a != b => Err(Error::Config(format!(
            "{canonical} and {alias} (its older name) are both set, to different values"
        ))),
        (new, old) => Ok(new.or(old)),
    }
}

/// `forever`, or a span like `30d`.
fn parse_retention(name: &str, text: &str) -> Result<Option<Duration>> {
    if text.eq_ignore_ascii_case("forever") {
        return Ok(None);
    }
    let span = parse_span(name, text)?;
    if span > MAX_RETENTION {
        return Err(Error::Config(format!(
            "{name}: {text:?} is longer than 3650d (use `forever` to keep recordings)"
        )));
    }
    Ok(Some(span))
}

/// A span like `90s`, `15m`, `36h` or `30d`; a bare number is seconds.
fn parse_span(name: &str, text: &str) -> Result<Duration> {
    let bad = || {
        Error::Config(format!(
            "{name}: {text:?} is not a span like 90s, 15m, 36h or 30d"
        ))
    };
    let (digits, unit) = match text.chars().last() {
        Some('s') => (text.strip_suffix('s'), 1),
        Some('m') => (text.strip_suffix('m'), 60),
        Some('h') => (text.strip_suffix('h'), 3_600),
        Some('d') => (text.strip_suffix('d'), 86_400),
        _ => (Some(text), 1),
    };
    let value = digits
        .and_then(|d| d.trim().parse::<u64>().ok())
        .ok_or_else(bad)?;
    let secs = value.checked_mul(unit).ok_or_else(bad)?;
    if secs == 0 {
        return Err(bad());
    }
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|&(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }

    #[test]
    fn nothing_set_means_no_recording() {
        assert_eq!(RecordingSettings::from_env(&env(&[])).unwrap(), None);
    }

    #[test]
    fn the_directory_alone_records_redacted_transcripts_for_30_days() {
        let s = RecordingSettings::from_env(&env(&[(ENV_RECORDING_DIR, "/var/rec")]))
            .unwrap()
            .unwrap();
        assert_eq!(s.dir, PathBuf::from("/var/rec"));
        assert!(s.transcripts && !s.audio && s.redact_pii);
        assert_eq!(s.retention, Some(Duration::from_secs(30 * 86_400)));
        assert_eq!(s.sweep_interval, Duration::from_secs(3_600));
    }

    #[test]
    fn every_variable_is_read() {
        let s = RecordingSettings::from_env(&env(&[
            (ENV_RECORDING_DIR, "rec"),
            (ENV_RECORDING_TRANSCRIPTS, "false"),
            (ENV_RECORDING_AUDIO, "true"),
            (ENV_RECORDING_REDACT_PII, "off"),
            (ENV_RECORDING_RETENTION, "72h"),
            (ENV_RECORDING_SWEEP_INTERVAL, "15m"),
            (ENV_RECORDING_MAX_MINUTES, "30"),
        ]))
        .unwrap()
        .unwrap();
        assert!(!s.transcripts && s.audio && !s.redact_pii);
        assert_eq!(s.retention, Some(Duration::from_secs(72 * 3_600)));
        assert_eq!(s.sweep_interval, Duration::from_secs(900));
        assert_eq!(s.max_audio, Duration::from_secs(1_800));
    }

    #[test]
    fn forever_keeps_recordings_without_a_sweep_limit() {
        let s = RecordingSettings::from_env(&env(&[
            (ENV_RECORDING_DIR, "rec"),
            (ENV_RECORDING_RETENTION, "Forever"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(s.retention, None);
    }

    #[test]
    fn a_recording_variable_without_the_directory_is_an_error() {
        for name in [
            ENV_RECORDING_TRANSCRIPTS,
            ENV_RECORDING_AUDIO,
            ENV_RECORDING_REDACT_PII,
            ENV_RECORD_TRANSCRIPTS,
            ENV_RECORD_AUDIO,
            ENV_REDACT_PII,
            ENV_RECORDING_RETENTION,
            ENV_RECORDING_SWEEP_INTERVAL,
            ENV_RECORDING_MAX_MINUTES,
            ENV_RECORDING_AGE_RECIPIENTS,
        ] {
            let value = if name == ENV_RECORDING_MAX_MINUTES || name == ENV_RECORDING_RETENTION {
                "5"
            } else if name == ENV_RECORDING_AGE_RECIPIENTS {
                "age1whatever"
            } else {
                "true"
            };
            let err = RecordingSettings::from_env(&env(&[(name, value)])).unwrap_err();
            assert!(err.to_string().contains(name), "{name}: {err}");
        }
    }

    #[cfg(not(feature = "recording-encryption"))]
    #[test]
    fn age_recipients_without_the_feature_are_a_startup_error_naming_it() {
        let err = RecordingSettings::from_env(&env(&[
            (ENV_RECORDING_DIR, "rec"),
            (ENV_RECORDING_AGE_RECIPIENTS, "age1whatever"),
        ]))
        .unwrap_err()
        .to_string();
        assert!(err.contains(ENV_RECORDING_AGE_RECIPIENTS), "{err}");
        assert!(err.contains("recording-encryption"), "{err}");
    }

    #[cfg(feature = "recording-encryption")]
    #[test]
    fn age_recipients_are_parsed_at_startup() {
        let public = age::x25519::Identity::generate().to_public().to_string();
        let s = RecordingSettings::from_env(&env(&[
            (ENV_RECORDING_DIR, "rec"),
            (ENV_RECORDING_AGE_RECIPIENTS, &public),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(s.age_recipients.unwrap().len(), 1);
        let err = RecordingSettings::from_env(&env(&[
            (ENV_RECORDING_DIR, "rec"),
            (ENV_RECORDING_AGE_RECIPIENTS, "age1bad"),
        ]))
        .unwrap_err()
        .to_string();
        assert!(err.contains(ENV_RECORDING_AGE_RECIPIENTS), "{err}");
        assert!(!err.contains("age1bad"), "{err}");
        let plain = RecordingSettings::from_env(&env(&[(ENV_RECORDING_DIR, "rec")]))
            .unwrap()
            .unwrap();
        assert!(plain.age_recipients.is_none());
    }

    #[test]
    fn bad_values_name_the_variable() {
        for (name, value) in [
            (ENV_RECORDING_AUDIO, "maybe"),
            (ENV_RECORDING_REDACT_KINDS, "email,address"),
            (ENV_RECORDING_RETENTION, "soon"),
            (ENV_RECORDING_RETENTION, "0d"),
            (ENV_RECORDING_RETENTION, "30m"),
            (ENV_RECORDING_RETENTION, "99999d"),
            (ENV_RECORDING_SWEEP_INTERVAL, "5s"),
            (ENV_RECORDING_MAX_MINUTES, "0"),
            (ENV_RECORDING_MAX_MINUTES, "601"),
        ] {
            let err =
                RecordingSettings::from_env(&env(&[(ENV_RECORDING_DIR, "rec"), (name, value)]))
                    .unwrap_err();
            assert!(err.to_string().contains(name), "{name}={value}: {err}");
        }
    }

    #[test]
    fn recording_nothing_is_an_error() {
        let err = RecordingSettings::from_env(&env(&[
            (ENV_RECORDING_DIR, "rec"),
            (ENV_RECORDING_TRANSCRIPTS, "no"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("nothing would be recorded"));
    }

    #[test]
    fn retention_must_cover_the_longest_recorded_call() {
        let err = RecordingSettings::from_env(&env(&[
            (ENV_RECORDING_DIR, "rec"),
            (ENV_RECORDING_AUDIO, "true"),
            (ENV_RECORDING_RETENTION, "2h"),
            (ENV_RECORDING_MAX_MINUTES, "180"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains(ENV_RECORDING_RETENTION));
    }

    #[test]
    fn spans_parse_each_unit() {
        assert_eq!(parse_span("x", "90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_span("x", "2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_span("x", "3h").unwrap(), Duration::from_secs(10_800));
        assert_eq!(parse_span("x", "1d").unwrap(), Duration::from_secs(86_400));
        assert_eq!(parse_span("x", "45").unwrap(), Duration::from_secs(45));
        assert!(parse_span("x", "").is_err());
        assert!(parse_span("x", "d").is_err());
        assert!(parse_span("x", "-5d").is_err());
        assert!(parse_span("x", "18446744073709551615d").is_err());
    }
    #[test]
    fn the_older_variable_names_still_work_and_must_not_disagree() {
        let s = RecordingSettings::from_env(&env(&[
            (ENV_RECORDING_DIR, "rec"),
            (ENV_RECORD_AUDIO, "true"),
            (ENV_REDACT_PII, "false"),
            (ENV_RECORD_TRANSCRIPTS, "false"),
        ]))
        .unwrap()
        .unwrap();
        assert!(s.audio && !s.redact_pii && !s.transcripts);
        // Same value under both names is fine; a different one is refused.
        assert!(
            RecordingSettings::from_env(&env(&[
                (ENV_RECORDING_DIR, "rec"),
                (ENV_RECORDING_REDACT_PII, "true"),
                (ENV_REDACT_PII, "true"),
            ]))
            .is_ok()
        );
        let err = RecordingSettings::from_env(&env(&[
            (ENV_RECORDING_DIR, "rec"),
            (ENV_RECORDING_REDACT_PII, "true"),
            (ENV_REDACT_PII, "false"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains(ENV_RECORDING_REDACT_PII), "{err}");
    }

    #[test]
    fn redact_kinds_are_read_from_a_list() {
        let s = RecordingSettings::from_env(&env(&[
            (ENV_RECORDING_DIR, "rec"),
            (ENV_RECORDING_REDACT_KINDS, "Email, phone"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            s.redact_kinds,
            PiiKinds::of([
                super::super::redact::PiiKind::Email,
                super::super::redact::PiiKind::Phone
            ])
        );
        let err = RecordingSettings::from_env(&env(&[
            (ENV_RECORDING_DIR, "rec"),
            (ENV_RECORDING_REDACT_KINDS, ","),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("no kind"), "{err}");
    }

    #[test]
    fn the_agent_file_overrides_the_environment() {
        let env_settings = RecordingSettings::from_env(&env(&[
            (ENV_RECORDING_DIR, "rec"),
            (ENV_RECORDING_AUDIO, "true"),
            (ENV_RECORDING_RETENTION, "90d"),
        ]))
        .unwrap();
        let file = RecordingFileSettings {
            audio: Some(false),
            redact_kinds: Some(PiiKinds::of([super::super::redact::PiiKind::Email])),
            retention: Some(Duration::from_secs(7 * 86_400)),
            ..RecordingFileSettings::default()
        };
        let s = resolve_settings(env_settings, &file).unwrap().unwrap();
        assert!(!s.audio && s.transcripts && s.redact_pii);
        assert_eq!(s.retention, Some(Duration::from_secs(7 * 86_400)));
        assert_eq!(s.dir, PathBuf::from("rec"));
        // Nothing in the file keeps the environment's values.
        let kept = resolve_settings(
            RecordingSettings::from_env(&env(&[(ENV_RECORDING_DIR, "rec")])).unwrap(),
            &RecordingFileSettings::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(kept, RecordingSettings::new("rec"));
    }

    #[test]
    fn a_file_table_without_the_directory_is_an_error_and_an_empty_one_is_not() {
        let file = RecordingFileSettings {
            transcripts: Some(true),
            ..RecordingFileSettings::default()
        };
        let err = resolve_settings(None, &file).unwrap_err();
        assert!(err.to_string().contains(ENV_RECORDING_DIR), "{err}");
        assert_eq!(
            resolve_settings(None, &RecordingFileSettings::default()).unwrap(),
            None
        );
        // The merged result is validated: the file cannot turn everything off either.
        let off = RecordingFileSettings {
            transcripts: Some(false),
            ..RecordingFileSettings::default()
        };
        let base = RecordingSettings::new("rec");
        assert!(resolve_settings(Some(base), &off).is_err());
    }
}
