//! The JSON Schema (draft 2020-12) of the agent definition file (docs/CONFIG.md
//! "Editor support"), behind the `schema` cargo feature.
//!
//! The schema is derived from the real serde types of [`schema`](super::schema) with `schemars`
//! (doc comments become descriptions; `deny_unknown_fields` becomes `additionalProperties:
//! false`, so a typo is flagged in the editor). `toml::Spanned<T>` fields are described as `T`
//! (they exist for positions, not for the format). The value types that check themselves while
//! the file is read ([`Millis`], [`Count`], [`Real`], [`Rate`], [`LanguageTag`], [`EnvVarName`],
//! [`Ident`], [`Token`] and the URL types) get a schema here that mirrors their rules as far as
//! JSON Schema can say them: bounds, enumerations and patterns. What it cannot say stays with
//! `brisvox-server check`: the cross-field rules (for example `turn.silence_ms` being required
//! with `fixed_silence`), the secrets in the environment and the tools the application
//! registers. Passing the schema is necessary, not sufficient.
//!
//! `brisvox-server --print-schema` prints [`agent_schema_json`]; `docs/agent.schema.json` is the
//! committed copy, kept equal by a test.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde_json::{Map, Value};

use super::schema::AgentFile;
use super::types::{
    Count, EnvVarName, HttpUrl, Ident, LanguageTag, LocalHttpUrl, LocalWsUrl, Millis, Rate, Real,
    Token, WsUrl,
};

/// Characters a URL may not contain (whitespace and control characters), as a character class
/// body for the patterns below. Written with `\u` escapes so ECMAScript (editors) and Rust
/// regular expression engines read them alike.
const URL_BAD: &str = r"\s\u0000-\u001f\u007f-\u009f";

/// The longest accepted URL, in characters (the reader counts bytes; the two agree for the
/// ASCII URLs that matter).
const MAX_URL: u32 = 2048;

impl<const MIN: u64, const MAX: u64> JsonSchema for Millis<MIN, MAX> {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Owned(format!("Millis_{MIN}_{MAX}"))
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "integer",
            "minimum": MIN,
            "maximum": MAX,
        })
    }
}

impl<const MIN: u32, const MAX: u32> JsonSchema for Count<MIN, MAX> {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Owned(format!("Count_{MIN}_{MAX}"))
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "integer",
            "minimum": MIN,
            "maximum": MAX,
        })
    }
}

impl<const LO: u32, const HI: u32> JsonSchema for Real<LO, HI> {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Owned(format!("Real_{LO}_{HI}"))
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        // The bounds are thousandths (const generics cannot be floats).
        json_schema!({
            "type": "number",
            "minimum": f64::from(LO) / 1000.0,
            "maximum": f64::from(HI) / 1000.0,
        })
    }
}

impl JsonSchema for Rate {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Rate")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "integer",
            "enum": [8000, 16000, 22050, 24000, 44100, 48000],
        })
    }
}

impl JsonSchema for LanguageTag {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("LanguageTag")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^[A-Za-z]{2,3}(-[A-Za-z0-9]{1,8})*$",
            "examples": ["en", "tr", "tr-TR"],
        })
    }
}

impl JsonSchema for EnvVarName {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("EnvVarName")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^[A-Za-z_][A-Za-z0-9_]{0,127}$",
            "examples": ["DEEPGRAM_API_KEY"],
        })
    }
}

impl JsonSchema for Ident {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Ident")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^[A-Za-z0-9_\\-]{1,64}$",
        })
    }
}

impl JsonSchema for Token {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Token")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": 256,
            "pattern": format!("^[^{URL_BAD}]+$"),
        })
    }
}

/// A URL of `secure://` (any host) or `plain://` to a loopback host (`localhost`, an IPv4
/// `127.*` literal or `[::1]`), without whitespace or control characters.
fn secure_or_loopback(secure: &str, plain: &str) -> Schema {
    let rest = format!("[^{URL_BAD}/?#@:][^{URL_BAD}]*");
    let tail = format!("([/?#][^{URL_BAD}]*)?");
    let loopback = r"(?:[Ll][Oo][Cc][Aa][Ll][Hh][Oo][Ss][Tt]|127(?:\.[0-9]{1,3}){3}|\[::1\])";
    json_schema!({
        "type": "string",
        "maxLength": MAX_URL,
        "pattern": format!(
            "^(?:{secure}://{rest}|{plain}://(?:[^{URL_BAD}/?#@]*@)?{loopback}(?::[0-9]+)?{tail})$"
        ),
    })
}

impl JsonSchema for WsUrl {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("WsUrl")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        secure_or_loopback("wss", "ws")
    }
}

impl JsonSchema for HttpUrl {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("HttpUrl")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        secure_or_loopback("https", "http")
    }
}

impl JsonSchema for LocalHttpUrl {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("LocalHttpUrl")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        // `http://` or `https://` to any host, without user info.
        json_schema!({
            "type": "string",
            "maxLength": MAX_URL,
            "pattern": format!(
                "^https?://[^{URL_BAD}/?#@:][^{URL_BAD}/?#@]*([/?#][^{URL_BAD}]*)?$"
            ),
            "examples": ["http://localhost:8880/v1"],
        })
    }
}

impl JsonSchema for LocalWsUrl {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("LocalWsUrl")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        // `ws://` or `wss://` to any host: scheme, host and port only.
        json_schema!({
            "type": "string",
            "maxLength": MAX_URL,
            "pattern": format!("^wss?://[^{URL_BAD}/?#@:][^{URL_BAD}/?#@]*$"),
            "examples": ["ws://127.0.0.1:6006"],
        })
    }
}

/// The backchannel phrase sets a definition may name (`interruption.backchannels`; the reader
/// compares case-insensitively). Schema only.
#[derive(JsonSchema)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)] // read by the schema derive only
pub(super) enum BackchannelSet {
    /// English phrases.
    En,
    /// Turkish phrases.
    Tr,
}

/// A copy of `value` with every object's keys in sorted order, so the printed schema does not
/// depend on whether some crate in the build enabled `serde_json`'s `preserve_order`.
fn sorted(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out = Map::new();
            for (key, value) in entries {
                out.insert(key, sorted(value));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// `value` with the "or null" alternative of every optional key removed: an optional key is
/// described as its value type, because TOML has no null and an absent key is how a definition
/// leaves a setting at its default.
fn without_null(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                let value = match (key.as_str(), value) {
                    ("type", Value::Array(types)) => {
                        let mut kept: Vec<Value> = types
                            .into_iter()
                            .filter(|t| t.as_str() != Some("null"))
                            .collect();
                        if kept.len() == 1 {
                            kept.remove(0)
                        } else {
                            Value::Array(kept)
                        }
                    }
                    (_, other) => without_null(other),
                };
                out.insert(key, value);
            }
            // `anyOf: [<schema>, {"type": "null"}]` becomes `<schema>` (merged into the key).
            let null = Value::Object(Map::from_iter([("type".to_owned(), "null".into())]));
            if let Some(Value::Array(alternatives)) = out.get("anyOf")
                && alternatives.len() == 2
                && alternatives.contains(&null)
                && let Some(Value::Object(only)) = alternatives.iter().find(|a| **a != null)
            {
                let only = only.clone();
                out.remove("anyOf");
                for (key, value) in only {
                    out.entry(key).or_insert(value);
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(without_null).collect()),
        other => other,
    }
}

/// The JSON Schema of an agent definition file.
#[must_use]
pub fn agent_schema() -> Value {
    let mut schema = SchemaGenerator::default().into_root_schema_for::<AgentFile>();
    schema.insert(
        "title".to_owned(),
        Value::String("Brisvox agent definition".to_owned()),
    );
    sorted(without_null(schema.to_value()))
}

/// [`agent_schema`] as the pretty-printed text `brisvox-server --print-schema` writes and
/// `docs/agent.schema.json` holds (two-space indentation, a trailing newline).
#[must_use]
pub fn agent_schema_json() -> String {
    // Serializing a `Value` cannot fail (string keys only).
    let mut text = serde_json::to_string_pretty(&agent_schema()).unwrap_or_default();
    text.push('\n');
    text
}
