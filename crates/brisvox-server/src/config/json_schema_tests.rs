//! Tests of the agent file's JSON Schema: the committed `docs/agent.schema.json` is the
//! generated one, every example agent file validates against it, and a typo does not.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
// test helpers

use std::path::{Path, PathBuf};

use jsonschema::Validator;
use serde_json::{Value, json};

use super::json_schema::{agent_schema, agent_schema_json};

/// The repository root.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn validator() -> Validator {
    jsonschema::draft202012::new(&agent_schema()).expect("the schema compiles")
}

#[test]
fn the_committed_schema_is_the_generated_one() {
    let path = root().join("docs/agent.schema.json");
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .replace("\r\n", "\n");
    assert!(
        committed == agent_schema_json(),
        "docs/agent.schema.json is stale; regenerate it with\n    \
         cargo run -p brisvox-server --features schema -- --print-schema > docs/agent.schema.json\n    \
         (from bash; in PowerShell pipe to `Set-Content -Encoding utf8` instead: `>` writes UTF-16)"
    );
}

#[test]
fn the_schema_is_a_valid_draft_2020_12_schema() {
    let schema = agent_schema();
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    jsonschema::meta::validate(&schema).expect("valid against the draft 2020-12 meta-schema");
    // Every table rejects unknown keys, like the reader (`deny_unknown_fields`).
    assert_eq!(schema["additionalProperties"], false);
    for (name, def) in schema["$defs"].as_object().unwrap() {
        if def.get("properties").is_some() {
            assert_eq!(def["additionalProperties"], false, "{name}");
        }
    }
}

/// Agent definition files below `dir` (not `Cargo.toml`, no build or dependency directories).
fn definition_files(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            if !matches!(
                name.as_str(),
                "target" | "node_modules" | ".venv" | "venv" | "__pycache__"
            ) {
                definition_files(&path, found);
            }
        } else if name != "Cargo.toml"
            && matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("toml" | "yaml" | "yml")
            )
        {
            found.push(path);
        }
    }
}

/// The file as JSON data; `None` for a file that is not an agent definition.
fn as_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).unwrap();
    let value: Value = match path.extension().and_then(|e| e.to_str()) {
        Some("toml") => toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
        #[cfg(feature = "yaml")]
        Some("yaml" | "yml") => {
            super::yaml::to_json(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        }
        _ => return None,
    };
    value.get("agent").is_some().then_some(value)
}

fn errors(validator: &Validator, instance: &Value) -> Vec<String> {
    validator
        .iter_errors(instance)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect()
}

#[test]
fn every_example_agent_file_validates() {
    let validator = validator();
    let mut files = Vec::new();
    definition_files(&root().join("examples"), &mut files);
    files.sort();
    let mut checked = 0;
    for path in &files {
        let Some(value) = as_json(path) else { continue };
        let problems = errors(&validator, &value);
        assert!(
            problems.is_empty(),
            "{}: {}",
            path.display(),
            problems.join("; ")
        );
        checked += 1;
    }
    // A moved or emptied examples directory must not turn this into a test of nothing.
    let least = if cfg!(feature = "yaml") { 12 } else { 8 };
    assert!(checked >= least, "only {checked} agent files found");
}

/// A small valid definition (every required key, nothing else).
fn minimal() -> Value {
    json!({
        "agent": {"name": "demo", "language": "en", "system_prompt": "Be brief."},
        "stt": {"provider": "deepgram", "api_key_env": "DEEPGRAM_API_KEY"},
        "llm": {"provider": "openai", "api_key_env": "OPENAI_API_KEY"},
        "tts": {"provider": "cartesia", "api_key_env": "CARTESIA_API_KEY", "voice": "v1"},
    })
}

/// `minimal()` with `pointer` set to `value`.
fn with(pointer: &str, value: Value) -> Value {
    let mut doc = minimal();
    let (parent, key) = pointer.rsplit_once('/').unwrap();
    if !parent.is_empty() && doc.pointer(parent).is_none() {
        // Create the missing tables.
        let mut at = &mut doc;
        for part in parent.split('/').skip(1) {
            at = at
                .as_object_mut()
                .unwrap()
                .entry(part.to_owned())
                .or_insert_with(|| json!({}));
        }
    }
    doc.pointer_mut(parent)
        .unwrap_or_else(|| panic!("{parent}"))
        .as_object_mut()
        .unwrap()
        .insert(key.to_owned(), value);
    doc
}

#[test]
fn the_minimal_definition_validates() {
    let problems = errors(&validator(), &minimal());
    assert!(problems.is_empty(), "{problems:?}");
}

#[test]
fn a_typo_key_fails_validation() {
    let validator = validator();
    for (pointer, what) in [
        ("/agnet", "a top-level table"),
        ("/turn/sillence_ms", "a key of [turn]"),
        ("/agent/sistem_prompt", "a key of [agent]"),
        ("/llm/openai/modle", "a key of [llm.openai]"),
        ("/stt/secondary/proivder", "a key of [stt.secondary]"),
        ("/tts/cartesia/mdoel", "a key of [tts.cartesia]"),
    ] {
        let doc = with(pointer, json!(1));
        let problems = errors(&validator, &doc);
        let key = pointer.rsplit('/').next().unwrap();
        assert!(
            problems.iter().any(|p| p.contains(key)),
            "{what} ({pointer}) was accepted or not named: {problems:?}"
        );
    }
}

#[test]
fn values_the_reader_rejects_fail_validation() {
    let validator = validator();
    let bad = [
        ("/turn/policy", json!("sloppy")),
        ("/turn/silence_ms", json!(0)),
        ("/turn/silence_ms", json!(10_001)),
        ("/turn/silence_ms", json!("300")),
        ("/audio/call_sample_rate", json!(12_345)),
        ("/agent/language", json!("english")),
        ("/agent/name", json!("has space")),
        ("/agent/system_prompt", json!("")),
        ("/agent/tools", json!(["ok_tool", "not ok"])),
        ("/stt/api_key_env", json!("sk-proj-abc123")),
        ("/stt/deepgram/base_url", json!("ws://example.com")),
        ("/stt/deepgram/base_url", json!("https://example.com")),
        ("/llm/openai/base_url", json!("http://api.example.com/v1")),
        ("/tts/openai_speech/base_url", json!("ftp://gpu/v1")),
        (
            "/tts/openai_speech/base_url",
            json!("http://u:p@gpu:8880/v1"),
        ),
        ("/tts/elevenlabs/speed", json!(1.5)),
        ("/llm/gemini/temperature", json!(2.5)),
        ("/recording/retention_days", json!(0)),
        ("/interruption/backchannels", json!(["de"])),
        (
            "/telephony/destinations",
            json!([{"name": "x", "number": "5551234"}]),
        ),
        ("/mcp/servers", json!([{"name": "a", "command": "x"}])),
        (
            "/mcp/servers",
            json!([{"name": "a", "command": "x", "tools": []}]),
        ),
        (
            "/mcp/servers",
            json!([{"name": "a", "command": "x", "tools": ["t"], "prefix": 5}]),
        ),
        (
            "/mcp/servers",
            json!([{"name": "a", "command": "x", "tools": ["t"], "readonly": ["t"]}]),
        ),
        (
            "/mcp/servers",
            json!([{"name": "a", "command": "x", "tools": ["t"], "timeout_ms": 30_001}]),
        ),
        (
            "/mcp/servers",
            json!([{"name": "a", "url": "ftp://x", "tools": ["t"]}]),
        ),
        (
            "/mcp/servers",
            json!([{"name": "a", "command": "x", "tools": ["t"], "max_in_flight": 0}]),
        ),
    ];
    for (pointer, value) in bad {
        let doc = with(pointer, value.clone());
        assert!(
            !validator.is_valid(&doc),
            "{pointer} = {value} was accepted"
        );
    }
}

#[test]
fn values_the_reader_accepts_pass_validation() {
    let validator = validator();
    let good = [
        ("/turn/policy", json!("fixed_silence")),
        ("/turn/silence_ms", json!(10_000)),
        ("/audio/call_sample_rate", json!(8000)),
        ("/agent/language", json!("tr-TR")),
        ("/agent/tools", json!(["lookup_order", "book-appointment"])),
        ("/stt/deepgram/base_url", json!("ws://localhost:8080")),
        ("/stt/deepgram/base_url", json!("ws://[::1]:8080/listen")),
        ("/stt/deepgram/base_url", json!("wss://example.com/v1")),
        ("/llm/openai/base_url", json!("http://127.0.0.1:9999/v1")),
        (
            "/tts/openai_speech/base_url",
            json!("http://192.168.1.20:8880/v1"),
        ),
        ("/tts/elevenlabs/speed", json!(1)),
        ("/llm/gemini/temperature", json!(0.7)),
        ("/interruption/backchannels", json!(["en", "tr"])),
        (
            "/telephony/destinations",
            json!([{"name": "sales", "number": "+442071234567"}]),
        ),
        (
            "/mcp/servers",
            json!([
                {"name": "files", "command": "mcp-files", "args": ["--root", "/data"],
                 "env": {"LOG": "warn"}, "env_from": {"TOKEN": "FILES_TOKEN"},
                 "tools": ["list_dir", "files.read"], "read_only": ["list_dir"],
                 "confirm": ["files.read"], "prefix": true, "timeout_ms": 30_000,
                 "max_in_flight": 256},
                {"name": "crm", "url": "https://crm.example.com/mcp", "bearer_env": "CRM_TOKEN",
                 "tools": ["find"], "prefix": "crm_", "allow_insecure": false}
            ]),
        ),
    ];
    for (pointer, value) in good {
        let doc = with(pointer, value.clone());
        let problems = errors(&validator, &doc);
        assert!(problems.is_empty(), "{pointer} = {value}: {problems:?}");
    }
}
