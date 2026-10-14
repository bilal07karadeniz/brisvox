//! Rust tools for a brisvox voice agent, plugged into the stock server.
//!
//! [`MyTools`] implements [`AppTools`]: the server calls [`AppTools::register`] once at startup,
//! and an agent file lists the tools it uses by name (`tools = ["get_time", "save_note"]` in
//! `agent.toml`). Two tools show the two kinds (`docs/TOOLS.md`):
//!
//! - `get_time(timezone)` is [`ToolKind::ReadOnly`]: it changes nothing, so the engine may run it
//!   speculatively (before the caller has finished) and cancels it when the caller barges in.
//! - `save_note(text)` is [`ToolKind::SideEffect`]: it changes the world, so the engine never
//!   runs it speculatively and never twice for one call. It still uses the call's idempotency key,
//!   as a tool talking to a real API must: the same key always gets the same note back.
//!
//! The tools are shared by every call of the process. Per-call data comes from the
//! [`ToolCallCtx`] a tool gets (session, turn, idempotency key); anything else a tool keeps
//! (here the notes) lives behind an `Arc` and is shared by all calls.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::UNIX_EPOCH;

use brisvox_agent::tools::{
    IdempotencyKey, RegistryError, Tool, ToolCallCtx, ToolError, ToolFuture, ToolKind, ToolOutput,
    ToolRegistry, ToolSpec,
};
use brisvox_core::SharedClock;
use brisvox_server::AppTools;
use serde_json::{Value, json};

/// Longest note the tool accepts (the schema says so too, so the model is told).
const MAX_NOTE_CHARS: usize = 500;

/// This project's tools.
#[derive(Debug, Default)]
pub struct MyTools {
    /// The notes `save_note` took, shared by every call. A real application writes to its
    /// database here instead.
    notes: Arc<NoteBook>,
}

impl MyTools {
    /// The notes saved so far, oldest first.
    pub fn notes(&self) -> Vec<String> {
        self.notes.all()
    }
}

impl AppTools for MyTools {
    fn register(
        &self,
        registry: &mut ToolRegistry,
        clock: &SharedClock,
    ) -> Result<(), RegistryError> {
        registry.register(
            ToolSpec::new(
                "get_time",
                "The current date and time in a time zone, e.g. when the caller asks what time \
                 it is somewhere.",
                json!({
                    "type": "object",
                    "properties": {
                        "timezone": {
                            "type": "string",
                            "description": "A UTC offset such as \"UTC\", \"UTC+3\", \"+05:30\" \
                                            or \"UTC-08:00\"."
                        }
                    },
                    "required": ["timezone"],
                    "additionalProperties": false
                }),
                ToolKind::ReadOnly,
            ),
            GetTime {
                clock: clock.clone(),
            },
        )?;
        registry.register(
            ToolSpec::new(
                "save_note",
                "Saves a short note for the business owner (for example a message the caller \
                 wants passed on). Say what you saved afterwards.",
                json!({
                    "type": "object",
                    "properties": {
                        "text": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": MAX_NOTE_CHARS,
                            "description": "The note, in the caller's words."
                        }
                    },
                    "required": ["text"],
                    "additionalProperties": false
                }),
                ToolKind::SideEffect,
            ),
            SaveNote {
                notes: Arc::clone(&self.notes),
            },
        )
    }
}

/// `get_time`: reads the injected clock, never the system's directly.
struct GetTime {
    clock: SharedClock,
}

impl Tool for GetTime {
    fn call<'a>(&'a self, args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async move {
            let zone = args
                .get("timezone")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::new("missing `timezone`"))?;
            // The message goes to the model, so it says what to do next.
            let offset = parse_utc_offset(zone).ok_or_else(|| {
                ToolError::new(format!(
                    "`{zone}` is not a UTC offset; use a form like \"UTC\", \"UTC+3\" or \"+05:30\""
                ))
            })?;
            let secs = match self.clock.wall().duration_since(UNIX_EPOCH) {
                Ok(since) => i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
                Err(_) => 0,
            };
            Ok(ToolOutput::Json(describe(secs, offset)))
        })
    }
}

/// `save_note`: appends to the shared notes, once per idempotency key.
struct SaveNote {
    notes: Arc<NoteBook>,
}

impl Tool for SaveNote {
    fn call<'a>(&'a self, args: Value, ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async move {
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| ToolError::new("missing `text`"))?;
            // Keyed on the whole key: two calls never share it. A real API call would send
            // `ctx.idempotency_key().digest()` as its `Idempotency-Key` header (the digest is
            // for external systems; distinct keys may share a digest).
            let (number, already_saved) = self.notes.save(&ctx.idempotency_key(), text);
            Ok(ToolOutput::Json(json!({
                "saved": true,
                "note_number": number,
                "already_saved": already_saved
            })))
        })
    }
}

/// The notes taken, and which idempotency keys took them. In memory and unbounded to keep the
/// example short: a real application writes to its database, with the key in a unique column.
#[derive(Debug, Default)]
struct NoteBook {
    inner: Mutex<NoteBookInner>,
}

#[derive(Debug, Default)]
struct NoteBookInner {
    notes: Vec<String>,
    /// Idempotency key -> note number (1-based).
    by_key: HashMap<IdempotencyKey, usize>,
}

impl NoteBook {
    /// Saves `text` under `key`: `(note number, whether this key had saved it already)`.
    fn save(&self, key: &IdempotencyKey, text: &str) -> (usize, bool) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(number) = inner.by_key.get(key) {
            return (*number, true);
        }
        inner.notes.push(text.to_owned());
        let number = inner.notes.len();
        inner.by_key.insert(key.clone(), number);
        (number, false)
    }

    fn all(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .notes
            .clone()
    }
}

/// A fixed UTC offset in minutes from `"UTC"`, `"UTC+3"`, `"GMT-08:00"`, `"+05:30"`, `"-0800"`.
/// (A real application would look the zone up in a time zone database, e.g. `chrono-tz`.)
pub fn parse_utc_offset(zone: &str) -> Option<i32> {
    let zone = zone.trim().to_ascii_uppercase();
    let (named, rest) = match zone
        .strip_prefix("UTC")
        .or_else(|| zone.strip_prefix("GMT"))
    {
        Some(rest) => (true, rest),
        None => (false, zone.as_str()),
    };
    if rest.is_empty() || rest == "Z" {
        return (named || rest == "Z").then_some(0);
    }
    let (sign, digits) = if let Some(digits) = rest.strip_prefix('+') {
        (1, digits)
    } else {
        (-1, rest.strip_prefix('-')?)
    };
    let (hours, minutes) = match digits.split_once(':') {
        Some((h, m)) => (h, m),
        None if digits.len() <= 2 => (digits, "0"),
        None if digits.len() == 4 => (digits.get(..2)?, digits.get(2..)?),
        None => return None,
    };
    let hours: i32 = hours.parse().ok()?;
    let minutes: i32 = minutes.parse().ok()?;
    ((0..=14).contains(&hours) && (0..60).contains(&minutes)).then(|| sign * (hours * 60 + minutes))
}

/// The answer of `get_time` for `unix_secs` seen from `offset_minutes` east of UTC.
pub fn describe(unix_secs: i64, offset_minutes: i32) -> Value {
    const WEEKDAYS: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    let local = unix_secs.saturating_add(i64::from(offset_minutes) * 60);
    let days = local.div_euclid(86_400);
    let secs_of_day = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    // 1970-01-01 was a Thursday (4, counting Sunday as 0).
    let weekday = usize::try_from((days + 4).rem_euclid(7)).unwrap_or(0);
    let (hour, minute, second) = (
        secs_of_day / 3600,
        secs_of_day % 3600 / 60,
        secs_of_day % 60,
    );
    let sign = if offset_minutes < 0 { '-' } else { '+' };
    let (off_h, off_m) = (offset_minutes.abs() / 60, offset_minutes.abs() % 60);
    let offset = format!("{sign}{off_h:02}:{off_m:02}");
    json!({
        "timezone": format!("UTC{offset}"),
        "date": format!("{year:04}-{month:02}-{day:02}"),
        "time": format!("{hour:02}:{minute:02}:{second:02}"),
        "weekday": WEEKDAYS.get(weekday).copied().unwrap_or("Sunday"),
        "iso8601": format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{offset}"),
    })
}

/// The proleptic Gregorian date of `days` since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use brisvox_agent::ToolCallId;
    use brisvox_agent::tools::{Confirmations, DEFAULT_CONFIRM_TTL, SessionId};
    use brisvox_core::{SystemClock, TurnEpoch};
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn registry(tools: &MyTools) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        tools
            .register(&mut registry, &SystemClock::shared())
            .unwrap();
        registry
    }

    fn ctx(call: u32) -> ToolCallCtx {
        ToolCallCtx {
            session: SessionId::new(1),
            epoch: TurnEpoch::ZERO,
            call: ToolCallId::new(TurnEpoch::ZERO, call),
            cancel: CancellationToken::new(),
            confirmations: Arc::new(Confirmations::new(
                SystemClock::shared(),
                DEFAULT_CONFIRM_TTL,
            )),
        }
    }

    #[test]
    fn both_tools_register_with_their_kinds() {
        let registry = registry(&MyTools::default());
        let kinds: Vec<(&str, ToolKind)> = registry.specs().map(|s| (&*s.name, s.kind)).collect();
        assert_eq!(
            kinds,
            [
                ("get_time", ToolKind::ReadOnly),
                ("save_note", ToolKind::SideEffect)
            ]
        );
    }

    /// The agent file next to this crate loads, and every tool it names is one of ours (the
    /// server would refuse to start otherwise).
    #[test]
    fn the_agent_file_loads_and_its_tools_are_registered() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("agent.toml");
        let keys: std::collections::BTreeMap<String, String> =
            ["DEEPGRAM_API_KEY", "OPENAI_API_KEY", "CARTESIA_API_KEY"]
                .into_iter()
                .map(|k| (k.to_owned(), "placeholder".to_owned()))
                .collect();
        let agent = brisvox_server::config::load_file(&path, &keys).unwrap();
        let offered = agent.tool_registry(&registry(&MyTools::default())).unwrap();
        assert_eq!(offered.len(), 2);
        assert_eq!(
            agent.tool_names().collect::<Vec<_>>(),
            ["get_time", "save_note"]
        );
    }

    #[test]
    fn offsets_parse() {
        assert_eq!(parse_utc_offset("UTC"), Some(0));
        assert_eq!(parse_utc_offset("utc+3"), Some(180));
        assert_eq!(parse_utc_offset("+05:30"), Some(330));
        assert_eq!(parse_utc_offset("GMT-08:00"), Some(-480));
        assert_eq!(parse_utc_offset("-0800"), Some(-480));
        assert_eq!(parse_utc_offset("Z"), Some(0));
        for bad in [
            "",
            "Europe/Istanbul",
            "UTC+15",
            "UTC+3:75",
            "+",
            "UTC+x",
            "3",
        ] {
            assert_eq!(parse_utc_offset(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn dates_are_right() {
        let t = describe(1_000_000_000, 0);
        assert_eq!(t["iso8601"], "2001-09-09T01:46:40+00:00");
        assert_eq!(t["weekday"], "Sunday");
        // Istanbul (UTC+3) is already past midnight; Los Angeles (UTC-8) is still the day before.
        assert_eq!(
            describe(1_000_000_000 + 80 * 60, 3 * 60)["date"],
            "2001-09-09"
        );
        assert_eq!(
            describe(1_000_000_000 + 80 * 60, 3 * 60)["time"],
            "06:06:40"
        );
        let la = describe(1_000_000_000, -8 * 60);
        assert_eq!(la["date"], "2001-09-08");
        assert_eq!(la["weekday"], "Saturday");
        assert_eq!(la["timezone"], "UTC-08:00");
        // 2000-02-29 existed.
        assert_eq!(describe(951_782_400, 0)["date"], "2000-02-29");
        assert_eq!(describe(0, 0)["weekday"], "Thursday");
        assert_eq!(describe(-1, 0)["iso8601"], "1969-12-31T23:59:59+00:00");
    }

    #[tokio::test]
    async fn get_time_answers_and_explains_a_bad_zone() {
        let tools = MyTools::default();
        let clock = SystemClock::shared();
        let tool = GetTime { clock };
        let ToolOutput::Json(answer) = tool
            .call(json!({"timezone": "UTC+3"}), ctx(1))
            .await
            .unwrap()
        else {
            panic!("json expected");
        };
        assert_eq!(answer["timezone"], "UTC+03:00");
        let error = tool
            .call(json!({"timezone": "Mars/Olympus"}), ctx(2))
            .await
            .unwrap_err();
        assert!(error.message().contains("UTC offset"), "{error}");
        assert!(tools.notes().is_empty());
    }

    #[tokio::test]
    async fn save_note_runs_once_per_idempotency_key() {
        let tools = MyTools::default();
        let tool = SaveNote {
            notes: Arc::clone(&tools.notes),
        };
        let answer = |out: ToolOutput| match out {
            ToolOutput::Json(v) => v,
            other => panic!("json expected, got {other:?}"),
        };
        let first = answer(
            tool.call(json!({"text": "Call Ada back"}), ctx(1))
                .await
                .unwrap(),
        );
        assert_eq!(first["note_number"], 1);
        assert_eq!(first["already_saved"], false);
        // The same call delivered again: the same note, not a second one.
        let again = answer(
            tool.call(json!({"text": "Call Ada back"}), ctx(1))
                .await
                .unwrap(),
        );
        assert_eq!(again["note_number"], 1);
        assert_eq!(again["already_saved"], true);
        // A different call is a different note.
        let second = answer(tool.call(json!({"text": "Order 7"}), ctx(2)).await.unwrap());
        assert_eq!(second["note_number"], 2);
        assert_eq!(tools.notes(), ["Call Ada back", "Order 7"]);
    }
}
