//! Recording and privacy (`docs/PRIVACY.md`): optional transcript and audio sinks for the
//! server's calls, PII redaction of the transcripts, and a retention sweeper.
//!
//! Everything is off unless `BRISVOX_RECORDING_DIR` is set ([`RecordingSettings::from_env`]).
//! The pieces:
//!
//! - [`Recording`]: the server-level object. [`Recording::attach`] puts one call's sinks on its
//!   [`SessionBuilder`](brisvox_engine::session::SessionBuilder) and returns the
//!   [`CallRecording`] that finishes them.
//! - [`redact_pii`]: the redaction filter applied to every transcript line before it is written.
//! - [`sweep_once`] / [`Recording::spawn_sweeper`]: retention.
//!
//! The design rules, in short: a recording never slows or fails a call (bounded queues that drop
//! and count, timeouts on every disk operation, failures logged once); data that must not reach
//! the disk is redacted before it is written, not after; files are created exclusively, with
//! mode `0600` on Unix; the sweeper touches only files this module named. With the cargo
//! feature `recording-encryption` and `BRISVOX_RECORDING_AGE_RECIPIENTS` every file is written
//! as an age file (`<name>.age`) to public keys only ([`RecordingSink`]).

mod files;
mod recording;
mod redact;
mod retention;
mod settings;
mod sink;
mod track;

pub use files::{
    Kind, NAME_PREFIX, WAV_BATCH, WAV_HEADER_LEN, WavWriter, file_name, file_name_for,
    is_recording_file, wav_header,
};
pub use recording::{
    AUDIO_QUEUE, CallInfo, CallRecording, FINISH_TIMEOUT, HEARD_QUEUE, INTERRUPT_RESERVE,
    IO_TIMEOUT, MAX_BOT_TURN_BYTES, Recording, RecordingStats, Sweeper,
};
pub use redact::{PiiKind, PiiKinds, redact_pii, redact_pii_with};
pub use retention::{
    ENTRIES_PER_ROUND, MAX_ENTRIES_PER_SWEEP, SweepReport, SweepStats, run_sweeper, sweep_once,
};
pub use settings::{
    DEFAULT_MAX_AUDIO, DEFAULT_RETENTION, DEFAULT_SWEEP_INTERVAL, ENV_RECORD_AUDIO,
    ENV_RECORD_TRANSCRIPTS, ENV_RECORDING_AGE_RECIPIENTS, ENV_RECORDING_AUDIO, ENV_RECORDING_DIR,
    ENV_RECORDING_MAX_MINUTES, ENV_RECORDING_REDACT_KINDS, ENV_RECORDING_REDACT_PII,
    ENV_RECORDING_RETENTION, ENV_RECORDING_SWEEP_INTERVAL, ENV_RECORDING_TRANSCRIPTS,
    ENV_REDACT_PII, MAX_AUDIO_MINUTES, MAX_RETENTION, MIN_RETENTION, MIN_SWEEP_INTERVAL,
    RecordingFileSettings, RecordingSettings, resolve_settings,
};
pub use sink::{AGE_SUFFIX, AgeRecipients, MAX_AGE_RECIPIENTS, RecordingSink};
pub use track::{BOT_PENDING_CAP, BotTrack, CallerTrack, MAX_GAP_FILL, TrackCounters};
