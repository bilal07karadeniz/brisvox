//! Tests of the YAML front-end: the same definition, checks and messages as TOML (parity), and
//! positions and key paths of YAML-only problems.

use std::collections::BTreeMap;

use super::super::{AgentConfig, ConfigError, Format, Issue, error, load_with};
use super::{MAX_DEPTH, Resolved, resolve};

/// The TOML reference definition (the same as `config::tests::BASE`).
const TOML_BASE: &str = r#"
[agent]
name = "support-en"
language = "en"
system_prompt = "You are a helpful agent."
tools = ["lookup_order"]

[stt]
provider = "deepgram"
api_key_env = "DEEPGRAM_API_KEY"

[llm]
provider = "openai"
api_key_env = "OPENAI_API_KEY"

[tts]
provider = "cartesia"
api_key_env = "CARTESIA_API_KEY"
voice = "voice-123"
"#;

/// [`TOML_BASE`] in YAML.
const BASE: &str = "
agent:
  name: support-en
  language: en
  system_prompt: You are a helpful agent.
  tools: [lookup_order]

stt:
  provider: deepgram
  api_key_env: DEEPGRAM_API_KEY

llm:
  provider: openai
  api_key_env: OPENAI_API_KEY

tts:
  provider: cartesia
  api_key_env: CARTESIA_API_KEY
  voice: voice-123
";

/// An environment with the keys of the base definitions.
fn env() -> BTreeMap<String, String> {
    [
        ("DEEPGRAM_API_KEY", "dg-secret-value-0001"),
        ("OPENAI_API_KEY", "sk-secret-value-0002"),
        ("CARTESIA_API_KEY", "ca-secret-value-0003"),
        ("ELEVENLABS_API_KEY", "el-secret-value-0004"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

/// Loads `text` in `format` as if every adapter were compiled in.
fn load_as(text: &str, format: Format) -> Result<AgentConfig, ConfigError> {
    load_with(text, "agent", format, &env(), &|_| true)
}

/// Loads YAML `text`.
fn load(text: &str) -> Result<AgentConfig, ConfigError> {
    load_as(text, Format::Yaml)
}

/// The issues of a YAML definition that must fail.
fn issues(text: &str) -> Vec<Issue> {
    match load(text) {
        Ok(_) => panic!("expected an error for:\n{text}"),
        Err(e) => e.issues,
    }
}

/// The single issue of a failing YAML definition.
fn only_issue(text: &str) -> Issue {
    let found = issues(text);
    assert_eq!(found.len(), 1, "{found:#?}");
    found.into_iter().next().unwrap()
}

/// 1-based (line, column) of the first `needle` in `text`.
fn pos(text: &str, needle: &str) -> (usize, usize) {
    error::line_column(text, text.find(needle).unwrap())
}

/// `text` with `from` replaced by `to` (which must occur).
fn edit(text: &str, from: &str, to: &str) -> String {
    assert!(text.contains(from), "{from}");
    text.replacen(from, to, 1)
}

/// Loads the TOML and the YAML form of one broken definition and checks that they report the
/// same problems (key paths and messages) and that every YAML problem has a position. Returns
/// the YAML issues.
fn parity(toml: &str, yaml: &str) -> Vec<Issue> {
    let toml_issues = load_as(toml, Format::Toml).unwrap_err().issues;
    let yaml_issues = issues(yaml);
    let strip = |found: &[Issue]| {
        found
            .iter()
            .map(|i| (i.key.clone(), i.message.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        strip(&yaml_issues),
        strip(&toml_issues),
        "TOML:\n{toml}\nYAML:\n{yaml}"
    );
    for issue in &yaml_issues {
        assert!(issue.position.is_some(), "{issue}");
    }
    yaml_issues
}

#[test]
fn the_yaml_base_reads_into_the_same_definition_as_toml() {
    let toml = load_as(TOML_BASE, Format::Toml).unwrap();
    let yaml = load(BASE).unwrap();
    assert_eq!(yaml.definition(), toml.definition());
    assert_eq!(yaml.engine(), toml.engine());
    assert_eq!(yaml.name(), "support-en");
    assert_eq!(&*yaml.voice(), "voice-123");
    assert_eq!(yaml.tool_names().collect::<Vec<_>>(), ["lookup_order"]);
    // Spans point into the YAML text.
    let language = &yaml.definition().agent.language;
    assert_eq!(BASE.get(language.span()), Some("en"));
}

#[test]
fn an_unknown_key_has_the_same_message_path_and_its_own_position() {
    let toml = format!("{TOML_BASE}\n[stt.deepgram]\nmodle = \"nova-3\"\n");
    let yaml = edit(
        BASE,
        "  api_key_env: DEEPGRAM_API_KEY\n",
        "  api_key_env: DEEPGRAM_API_KEY\n  deepgram:\n    modle: nova-3\n",
    );
    let found = parity(&toml, &yaml);
    assert_eq!(found[0].key, "stt.deepgram.modle");
    assert_eq!(found[0].position, Some(pos(&yaml, "modle")));
}

#[test]
fn a_literal_api_key_gets_the_same_hint() {
    let toml = TOML_BASE.replacen(
        "api_key_env = \"OPENAI_API_KEY\"",
        "api_key = \"sk-literal\"",
        1,
    );
    let yaml = edit(BASE, "api_key_env: OPENAI_API_KEY", "api_key: sk-literal");
    let found = parity(&toml, &yaml);
    assert_eq!(found[0].key, "llm.api_key");
    assert!(found[0].message.contains("api_key_env"), "{}", found[0]);
    assert!(!found[0].message.contains("sk-literal"), "{}", found[0]);
}

#[test]
fn wrong_types_and_ranges_have_the_same_messages_at_the_value() {
    let cases = [
        (
            "endpointing_ms = \"300\"",
            "endpointing_ms: \"300\"",
            "\"300\"",
        ),
        ("endpointing_ms = 20000", "endpointing_ms: 20000", "20000"),
        ("endpointing_ms = -5", "endpointing_ms: -5", "-5"),
        ("endpointing_ms = 1.5", "endpointing_ms: 1.5", "1.5"),
        ("smart_format = 1", "smart_format: 1", "1\n"),
        ("model = 3", "model: 3", "3\n"),
        ("model = \"nova 3\"", "model: nova 3", "nova 3"),
    ];
    for (toml_line, yaml_line, needle) in cases {
        let toml = format!("{TOML_BASE}\n[stt.deepgram]\n{toml_line}\n");
        let yaml = edit(
            BASE,
            "  api_key_env: DEEPGRAM_API_KEY\n",
            &format!("  api_key_env: DEEPGRAM_API_KEY\n  deepgram:\n    {yaml_line}\n"),
        );
        let found = parity(&toml, &yaml);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].key.starts_with("stt.deepgram."), "{}", found[0]);
        assert_eq!(found[0].position, Some(pos(&yaml, needle)), "{}", found[0]);
    }
}

#[test]
fn an_unknown_variant_has_the_same_message() {
    let toml = TOML_BASE.replacen("provider = \"openai\"", "provider = \"openia\"", 1);
    let yaml = edit(BASE, "provider: openai", "provider: openia");
    let found = parity(&toml, &yaml);
    assert_eq!(found[0].key, "llm.provider");
    assert_eq!(found[0].position, Some(pos(&yaml, "openia")));
    assert!(
        found[0].message.contains("unknown variant `openia`"),
        "{}",
        found[0]
    );
}

#[test]
fn a_missing_field_and_a_misspelt_section_are_reported_alike() {
    let toml = TOML_BASE.replacen("voice = \"voice-123\"\n", "", 1);
    let yaml = edit(BASE, "  voice: voice-123\n", "");
    let found = parity(&toml, &yaml);
    assert!(
        found[0].message.contains("missing field `voice`"),
        "{}",
        found[0]
    );
    assert_eq!(found[0].position, Some(pos(&yaml, "tts:")));

    let toml = TOML_BASE.replacen("[tts]", "[tts_typo]", 1);
    let yaml = edit(BASE, "tts:", "tts_typo:");
    let found = parity(&toml, &yaml);
    assert_eq!(found[0].position, Some(pos(&yaml, "tts_typo")));
}

#[test]
fn cross_field_checks_report_the_same_problems_at_yaml_positions() {
    // Turkish on Deepgram Aura (at agent.language), a rate Aura does not deliver, and a turn
    // setting of the other policy: three checks, in file order.
    let toml = TOML_BASE
        .replacen("language = \"en\"", "language = \"tr\"", 1)
        .replacen(
            "provider = \"cartesia\"\napi_key_env = \"CARTESIA_API_KEY\"\nvoice = \"voice-123\"",
            "provider = \"deepgram_aura\"\napi_key_env = \"DEEPGRAM_API_KEY\"\nvoice = \"aura-2-thalia-en\"\nsample_rate = 44100",
            1,
        )
        + "\n[turn]\nsilence_ms = 500\n";
    let yaml = edit(BASE, "language: en", "language: tr").replacen(
        "  provider: cartesia\n  api_key_env: CARTESIA_API_KEY\n  voice: voice-123\n",
        "  provider: deepgram_aura\n  api_key_env: DEEPGRAM_API_KEY\n  voice: aura-2-thalia-en\n  sample_rate: 44100\n",
        1,
    ) + "\nturn:\n  silence_ms: 500\n";
    let found = parity(&toml, &yaml);
    let keys: Vec<_> = found.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(
        keys,
        ["agent.language", "tts.sample_rate", "turn.silence_ms"],
        "{found:#?}"
    );
    assert_eq!(found[0].position, Some(pos(&yaml, "tr\n")));
    assert_eq!(found[1].position, Some(pos(&yaml, "44100")));
    assert_eq!(found[2].position, Some(pos(&yaml, "500")));
}

#[test]
fn a_secondary_reads_and_is_checked_like_in_toml() {
    let toml = format!(
        "{TOML_BASE}\n[stt.secondary]\nprovider = \"deepgram\"\n\
         api_key_env = \"DEEPGRAM_API_KEY\"\nlanguage = \"en\"\n"
    );
    let yaml = edit(
        BASE,
        "  api_key_env: DEEPGRAM_API_KEY\n",
        "  api_key_env: DEEPGRAM_API_KEY\n  secondary:\n    provider: deepgram\n    \
         api_key_env: DEEPGRAM_API_KEY\n    language: en\n",
    );
    let found = parity(&toml, &yaml);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0].key, "stt.secondary.language");
    // At the value of the secondary's own `language` (not the agent's).
    let at = yaml.find("    language: en").unwrap() + "    language: ".len();
    assert_eq!(found[0].position, Some(error::line_column(&yaml, at)));
}

#[test]
fn an_options_table_for_another_provider_points_at_its_name() {
    let toml = format!("{TOML_BASE}\n[tts.elevenlabs]\nspeed = 1.0\n");
    let yaml = format!("{BASE}  elevenlabs:\n    speed: 1.0\n");
    let found = parity(&toml, &yaml);
    assert_eq!(found[0].key, "tts.elevenlabs");
    assert_eq!(found[0].position, Some(pos(&yaml, "elevenlabs:")));
}

#[test]
fn list_entries_are_reported_at_their_own_position() {
    let toml = format!("{TOML_BASE}\n[interruption]\nbackchannels = [\"en\", \"de\"]\n");
    let yaml = format!("{BASE}\ninterruption:\n  backchannels:\n    - en\n    - de\n");
    let found = parity(&toml, &yaml);
    assert_eq!(found[0].key, "interruption.backchannels[1]");
    assert_eq!(found[0].position, Some(pos(&yaml, "de\n")));

    let toml = TOML_BASE.replacen(
        "tools = [\"lookup_order\"]",
        "tools = [\"a\", \"b\", \"a\"]",
        1,
    );
    let yaml = edit(BASE, "tools: [lookup_order]", "tools: [a, b, a]");
    let found = parity(&toml, &yaml);
    assert_eq!(found[0].key, "agent.tools[2]");
    assert_eq!(found[0].position, Some(pos(&yaml, "a]")));
}

#[test]
fn secrets_come_only_from_the_environment() {
    let e = load_with(BASE, "agent.yaml", Format::Yaml, &BTreeMap::new(), &|_| {
        true
    })
    .unwrap_err();
    let keys: Vec<_> = e.issues.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(
        keys,
        ["stt.api_key_env", "llm.api_key_env", "tts.api_key_env"],
        "{e}"
    );
    assert_eq!(e.issues[0].position, Some(pos(BASE, "DEEPGRAM_API_KEY")));
    assert!(e.issues[0].message.contains("DEEPGRAM_API_KEY"), "{e}");

    let shown = format!("{:?}", load(BASE).unwrap());
    assert!(!shown.contains("dg-secret-value-0001"), "{shown}");
}

#[test]
fn positions_count_characters_after_multibyte_text() {
    // Two-byte characters before the bad value: the column counts characters, and the parser's
    // character index is not mistaken for a byte offset.
    let yaml = edit(
        BASE,
        "  system_prompt: You are a helpful agent.\n",
        "  system_prompt: Şöyle çalış ğüzel.\n  tools: [ğ ğ]\n",
    )
    .replacen("  tools: [lookup_order]\n", "", 1);
    let issue = only_issue(&yaml);
    assert_eq!(issue.key, "agent.tools[0]");
    assert_eq!(issue.position, Some(pos(&yaml, "ğ ğ")));

    let yaml = edit(
        BASE,
        "  system_prompt: You are a helpful agent.\n",
        "  system_prompt: \"Şöyle çalış\"\n  bilinmeyen: 1\n",
    );
    let issue = only_issue(&yaml);
    assert_eq!(issue.key, "agent.bilinmeyen");
    assert_eq!(issue.position, Some(pos(&yaml, "bilinmeyen")));
}

#[test]
fn windows_line_ends_and_a_byte_order_mark_keep_positions() {
    let yaml = format!("{BASE}  speed: 2\n").replace('\n', "\r\n");
    let issue = only_issue(&yaml);
    assert_eq!(issue.key, "tts.speed");
    assert_eq!(issue.position, Some(pos(&yaml, "speed")));
    assert_eq!(
        load(&BASE.replace('\n', "\r\n")).unwrap().definition(),
        load(BASE).unwrap().definition()
    );

    let yaml = format!("\u{feff}{BASE}  speed: 2\n");
    let issue = only_issue(&yaml);
    assert_eq!(issue.key, "tts.speed", "{issue}");
    assert_eq!(issue.position, Some(pos(&yaml, "speed")));
}

#[test]
fn a_lone_carriage_return_is_rejected_at_its_position() {
    // Classic Mac line ends: YAML reads a lone CR as a line break, positions count LF only.
    let yaml = "agent:\r  name: x\r  bogus: 1\r";
    let issue = super::parse(yaml).unwrap_err();
    assert!(issue.message.contains("carriage return"), "{issue}");
    assert_eq!(issue.position, Some((1, 7)), "{issue}");

    // After LF (and CRLF) lines, the position of the first lone CR is still exact.
    let yaml = format!("{}  speed: 2\r  pitch: 1\n", BASE.replace('\n', "\r\n"));
    let issue = only_issue(&yaml);
    assert!(issue.message.contains("carriage return"), "{issue}");
    assert_eq!(issue.position, Some(pos(&yaml, "\r  pitch")), "{issue}");
}

#[test]
#[allow(clippy::disallowed_methods)] // measures real time on purpose
fn a_long_single_line_reads_in_linear_time() {
    // 100k two-byte list items on one line, then an unknown key at its end: every event's
    // position used to be found by walking its line from the start (quadratic, minutes here).
    let items = vec!["ğ"; 100_000].join(",");
    let yaml = format!("agent: {{tools: [{items}], bogus: 1}}\n");
    let started = std::time::Instant::now();
    let issue = super::parse(&yaml).unwrap_err();
    let elapsed = started.elapsed();
    assert_eq!(issue.key, "agent.bogus", "{issue}");
    assert_eq!(issue.position, Some(pos(&yaml, "bogus")), "{issue}");
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "reading took {elapsed:?}"
    );
}

#[test]
fn a_syntax_error_has_a_position_and_the_enclosing_key() {
    let yaml = format!("{BASE}  sample_rate: [24000\n");
    let issue = only_issue(&yaml);
    assert!(issue.message.starts_with("YAML syntax error: "), "{issue}");
    assert!(issue.key.starts_with("tts.sample_rate"), "{issue}");
    assert!(issue.position.is_some(), "{issue}");

    let yaml = edit(BASE, "  voice: voice-123", "  voice: a: b");
    let issue = only_issue(&yaml);
    assert_eq!(issue.position.map(|p| p.0), Some(pos(&yaml, "a: b").0));
}

#[test]
fn duplicate_keys_are_rejected_at_the_second_one() {
    let yaml = format!("{BASE}  voice: voice-456\n");
    let issue = only_issue(&yaml);
    assert_eq!(issue.key, "tts.voice");
    assert_eq!(issue.position, Some(pos(&yaml, "voice: voice-456")));
    assert!(issue.message.contains("duplicate key `voice`"), "{issue}");
}

#[test]
fn anchors_aliases_tags_and_extra_documents_are_rejected() {
    let cases = [
        (
            edit(BASE, "voice: voice-123", "voice: &v voice-123\n  model: *v"),
            "anchors",
            "voice-123",
        ),
        (
            edit(BASE, "voice: voice-123", "voice: !!str 123"),
            "tags",
            "123",
        ),
        (
            format!("{BASE}---\nagent: {{}}\n"),
            "one YAML document",
            "---",
        ),
    ];
    for (yaml, word, needle) in cases {
        let issue = only_issue(&yaml);
        assert!(issue.message.contains(word), "{issue}");
        assert_eq!(issue.position, Some(pos(&yaml, needle)), "{issue}");
    }
    let yaml = format!("x: &a 1\n{BASE}");
    assert!(only_issue(&yaml).message.contains("anchors"));
    // An alias of an undefined anchor (a defined one is rejected first).
    let yaml = edit(BASE, "voice: voice-123", "voice: *v");
    let issue = only_issue(&yaml);
    assert!(
        issue.message.contains("alias") || issue.message.contains("anchor"),
        "{issue}"
    );
    assert_eq!(issue.position.map(|p| p.0), Some(pos(&yaml, "*v").0));
}

#[test]
fn malformed_text_is_an_error_with_a_position() {
    let nasty = [
        "agent: \"unterminated\n",
        "agent: 'x\n",
        "agent:\n\t- tab\n",
        "{",
        "[",
        "agent: [a, {b: }\n",
        "  - a\n b: c\n",
        "%YAML 1.1\n%YAML 1.2\n---\na: 1\n",
        "? a\n? a\n",
        "agent: |\n  text\n bad\n",
        "\u{0}",
        "agent: >\n\n\n",
        "a: b\n  c: d\n",
        "- [\n- ]\n",
    ];
    for text in nasty {
        let issue = super::parse(text).unwrap_err();
        assert!(issue.position.is_some(), "{text:?}: {issue}");
    }
}

#[test]
fn a_collection_as_a_key_is_rejected() {
    let yaml = format!("{BASE}? [a, b]\n: 1\n");
    let issue = only_issue(&yaml);
    assert!(issue.message.contains("a key must be a name"), "{issue}");
}

#[test]
fn nesting_is_bounded() {
    let deep = format!("{BASE}x: {}{}\n", "[".repeat(100), "]".repeat(100));
    let issue = only_issue(&deep);
    assert!(
        issue.message.contains(&format!("deeper than {MAX_DEPTH}")),
        "{issue}"
    );
}

#[test]
fn empty_and_non_mapping_documents_are_reported_like_toml() {
    for text in ["", "# only a comment\n"] {
        let toml = load_as(text, Format::Toml).unwrap_err().issues;
        let yaml = issues(text);
        assert_eq!(yaml[0].message, toml[0].message, "{text:?}");
    }
    let issue = only_issue("- a\n- b\n");
    assert!(
        issue.message.starts_with("invalid type: sequence"),
        "{issue}"
    );
    // serde struct visitors would read a sequence as fields by position; YAML rejects it like
    // TOML does.
    let issue = only_issue(&format!("{BASE}turn: [adaptive]\n"));
    assert_eq!(issue.key, "turn");
    assert!(
        issue.message.starts_with("invalid type: sequence"),
        "{issue}"
    );
}

#[test]
fn null_is_the_same_as_leaving_the_key_out() {
    let yaml = format!("{BASE}\nturn:\nspeculation: ~\ninterruption:\n  backchannels:\n");
    let agent = load(&yaml).unwrap();
    assert_eq!(agent.definition(), load(BASE).unwrap().definition());
    let issue = only_issue(&edit(BASE, "voice: voice-123", "voice:"));
    assert_eq!(issue.key, "tts.voice");
    assert!(
        issue.message.starts_with("invalid type: null, expected"),
        "{issue}"
    );
}

#[test]
fn scalars_follow_the_core_schema() {
    // YAML 1.1 booleans are strings: a boolean key rejects them like TOML rejects a string.
    let yaml = format!("{BASE}\nspeculation:\n  enabled: yes\n");
    let issue = only_issue(&yaml);
    assert_eq!(issue.key, "speculation.enabled");
    assert!(issue.message.contains("string \"yes\""), "{issue}");
    // Quoting makes any scalar a string; a block scalar too.
    let yaml = edit(
        BASE,
        "  system_prompt: You are a helpful agent.\n",
        "  system_prompt: |\n    Line one.\n    Line two.\n",
    )
    .replacen("voice: voice-123", "voice: \"123\"", 1);
    let agent = load(&yaml).unwrap();
    assert_eq!(&*agent.system_prompt(), "Line one.\nLine two.\n");
    assert_eq!(&*agent.voice(), "123");

    assert_eq!(resolve("300", true), Resolved::Int(300));
    assert_eq!(resolve("+5", true), Resolved::Int(5));
    assert_eq!(resolve("-5", true), Resolved::Int(-5));
    assert_eq!(resolve("0x1F", true), Resolved::Int(31));
    assert_eq!(resolve("0o17", true), Resolved::Int(15));
    assert_eq!(
        resolve("18446744073709551615", true),
        Resolved::UInt(u64::MAX)
    );
    assert_eq!(resolve("99999999999999999999", true), Resolved::BadInt);
    assert_eq!(resolve("0.7", true), Resolved::Float(0.7));
    assert_eq!(resolve(".5", true), Resolved::Float(0.5));
    assert_eq!(resolve("1e3", true), Resolved::Float(1000.0));
    assert_eq!(resolve("-.inf", true), Resolved::Float(f64::NEG_INFINITY));
    assert_eq!(resolve("~", true), Resolved::Null);
    assert_eq!(resolve("", true), Resolved::Null);
    assert_eq!(resolve("True", true), Resolved::Bool(true));
    for text in [
        "yes", "no", "on", "nova-3", "1.2.3", "1_000", "e3", ".", "0x", "1e",
    ] {
        assert_eq!(resolve(text, true), Resolved::Str(text), "{text}");
    }
    assert_eq!(resolve("300", false), Resolved::Str("300"));
}

#[test]
fn flow_style_reads_the_same() {
    let yaml = "{agent: {name: support-en, language: en, system_prompt: \"You are a helpful \
                agent.\", tools: [lookup_order]}, stt: {provider: deepgram, api_key_env: \
                DEEPGRAM_API_KEY}, llm: {provider: openai, api_key_env: OPENAI_API_KEY}, tts: \
                {provider: cartesia, api_key_env: CARTESIA_API_KEY, voice: voice-123}}";
    assert_eq!(
        load(yaml).unwrap().definition(),
        load(BASE).unwrap().definition()
    );
}

#[test]
fn the_format_follows_the_extension() {
    use std::path::Path;
    assert_eq!(Format::from_path(Path::new("a.yaml")), Format::Yaml);
    assert_eq!(Format::from_path(Path::new("a.YML")), Format::Yaml);
    assert_eq!(Format::from_path(Path::new("a.toml")), Format::Toml);
    assert_eq!(Format::from_path(Path::new("agent")), Format::Toml);
    assert!(Format::Yaml.compiled_in());
}

/// `load_file` reads a `.yml` file as YAML (the base definition names Deepgram, OpenAI and
/// Cartesia, so this needs their features).
#[cfg(feature = "all-providers")]
#[test]
fn load_file_reads_a_yml_file_as_yaml() {
    use super::super::load_file;
    let dir = std::env::temp_dir().join(format!("brisvox-yaml-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("agent.yml");
    std::fs::write(&path, BASE).unwrap();
    let agent = load_file(&path, &env()).unwrap();
    assert_eq!(agent.name(), "support-en");
    std::fs::write(&path, format!("{BASE}  speed: 2\n")).unwrap();
    let e = load_file(&path, &env()).unwrap_err();
    assert_eq!(e.issues[0].key, "tts.speed");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Every TOML example in `examples/agents` has a YAML twin that reads into the same definition,
/// and every YAML example loads and builds its adapters.
#[cfg(feature = "all-providers")]
#[test]
fn the_yaml_examples_are_twins_of_the_toml_ones() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/agents");
    let mut twins = 0;
    let mut yaml_files = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        match Format::from_path(&path) {
            Format::Yaml => yaml_files += 1,
            Format::Toml => {
                if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                    continue;
                }
                let toml = std::fs::read_to_string(&path).unwrap();
                let twin = path.with_extension("yaml");
                let yaml = std::fs::read_to_string(&twin)
                    .unwrap_or_else(|e| panic!("{} has no YAML twin: {e}", path.display()));
                let toml = super::super::parse_as(&toml, "toml", Format::Toml).unwrap();
                let yaml = super::super::parse_as(&yaml, "yaml", Format::Yaml)
                    .unwrap_or_else(|e| panic!("{e}"));
                assert_eq!(yaml, toml, "{}", twin.display());
                twins += 1;
            }
        }
    }
    assert!(
        twins >= 3,
        "expected at least three twins in {}",
        dir.display()
    );
    assert_eq!(yaml_files, twins, "every YAML example is a twin");
    assert!(
        dir.join("failover-en.yaml").exists(),
        "the example with secondaries is a twin too"
    );
    // `config::tests::the_examples_are_valid` loads every example file (both formats) with its
    // adapters.
}

#[test]
fn the_recording_table_reads_from_yaml_like_from_toml() {
    let toml = format!(
        "{TOML_BASE}
[recording]
audio = true
redact_kinds = [\"email\", \"iban\"]
retention_days = 7
"
    );
    let yaml = format!(
        "{BASE}
recording:
  audio: true
  redact_kinds: [email, iban]
  retention_days: 7
"
    );
    let from_toml = load_as(&toml, Format::Toml).unwrap().recording();
    let from_yaml = load(&yaml).unwrap().recording();
    assert_eq!(from_yaml, from_toml);
    assert_eq!(from_yaml.audio, Some(true));
    assert!(from_yaml.redact_kinds.is_some());

    // The same mistake reads the same in both.
    parity(
        &format!(
            "{TOML_BASE}
[recording]
retention_days = 0
"
        ),
        &format!(
            "{BASE}
recording:
  retention_days: 0
"
        ),
    );
}
