//! Tests of agent definition loading: errors (positions, key paths, secrets never shown),
//! cross-field checks and the mappings.

use std::collections::BTreeMap;
use std::time::Duration;

use brisvox_agent::{Confirm, Language, TurnPolicy};
use brisvox_core::frame::SampleRate;

use super::*;

/// A complete English definition (Deepgram, OpenAI, Cartesia).
const BASE: &str = r#"
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

const STT_SECRET: &str = "dg-secret-value-0001";
const LLM_SECRET: &str = "sk-secret-value-0002";
const TTS_SECRET: &str = "ca-secret-value-0003";

/// An environment with every key the tests name.
fn env() -> BTreeMap<String, String> {
    [
        ("DEEPGRAM_API_KEY", STT_SECRET),
        ("OPENAI_API_KEY", LLM_SECRET),
        ("CARTESIA_API_KEY", TTS_SECRET),
        ("ANTHROPIC_API_KEY", LLM_SECRET),
        ("ELEVENLABS_API_KEY", TTS_SECRET),
        ("ASSEMBLYAI_API_KEY", STT_SECRET),
        ("GLADIA_API_KEY", STT_SECRET),
        ("GEMINI_API_KEY", LLM_SECRET),
        ("GROQ_API_KEY", LLM_SECRET),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

/// Loads `text` as if every adapter were compiled in.
fn load(text: &str) -> Result<AgentConfig, ConfigError> {
    load_with(text, "agent.toml", Format::Toml, &env(), &|_| true)
}

/// The issues of a definition that must fail.
fn issues(text: &str) -> Vec<Issue> {
    match load(text) {
        Ok(_) => panic!("expected an error for:\n{text}"),
        Err(e) => e.issues,
    }
}

/// `BASE` with `from` replaced by `to` (which must occur).
fn edit(from: &str, to: &str) -> String {
    assert!(BASE.contains(from), "{from}");
    BASE.replacen(from, to, 1)
}

/// 1-based (line, column) of the first `needle` in `text`.
fn pos(text: &str, needle: &str) -> (usize, usize) {
    error::line_column(text, text.find(needle).unwrap())
}

/// The single issue of a failing definition.
fn only_issue(text: &str) -> Issue {
    let found = issues(text);
    assert_eq!(found.len(), 1, "{found:#?}");
    found.into_iter().next().unwrap()
}

#[test]
fn a_complete_definition_loads_with_the_engine_defaults() {
    let agent = load(BASE).unwrap();
    assert_eq!(agent.name(), "support-en");
    assert_eq!(agent.language(), Language::En);
    assert_eq!(&*agent.system_prompt(), "You are a helpful agent.");
    assert_eq!(&*agent.voice(), "voice-123");
    assert_eq!(agent.tool_names().collect::<Vec<_>>(), ["lookup_order"]);
    assert_eq!(agent.engine(), EngineConfig::DEFAULT);
}

#[test]
fn debug_never_prints_a_key() {
    let agent = load(BASE).unwrap();
    let shown = format!("{agent:?}");
    for secret in [STT_SECRET, LLM_SECRET, TTS_SECRET] {
        assert!(!shown.contains(secret), "{shown}");
    }
}

#[test]
fn an_unknown_key_is_rejected_with_its_path_and_position() {
    let text = format!("{BASE}\n[stt.deepgram]\nmodle = \"nova-3\"\n");
    let issue = only_issue(&text);
    assert_eq!(issue.key, "stt.deepgram.modle");
    assert_eq!(issue.position, Some(pos(&text, "modle")));
    assert!(issue.message.contains("unknown field `modle`"), "{issue}");
}

#[test]
fn a_literal_api_key_gets_a_hint_to_use_the_environment() {
    let text = edit(
        "api_key_env = \"OPENAI_API_KEY\"",
        "api_key = \"sk-literal\"",
    );
    let issue = only_issue(&text);
    assert_eq!(issue.key, "llm.api_key");
    assert!(issue.message.contains("api_key_env"), "{issue}");
}

#[test]
fn a_wrong_type_names_the_key_and_the_value_position() {
    let text = format!("{BASE}\n[stt.deepgram]\nendpointing_ms = \"300\"\n");
    let issue = only_issue(&text);
    assert_eq!(issue.key, "stt.deepgram.endpointing_ms");
    assert_eq!(issue.position, Some(pos(&text, "\"300\"")));
}

#[test]
fn an_out_of_range_value_names_the_range() {
    let text = format!("{BASE}\n[stt.deepgram]\nendpointing_ms = 20000\n");
    let issue = only_issue(&text);
    assert_eq!(issue.key, "stt.deepgram.endpointing_ms");
    assert_eq!(issue.position, Some(pos(&text, "20000")));
    assert!(issue.message.contains("0..=10000 ms"), "{issue}");
}

#[test]
fn a_missing_section_is_reported() {
    let text = BASE.replace("[tts]", "[tts_typo]");
    let found = issues(&text);
    assert!(
        found
            .iter()
            .any(|i| i.message.contains("tts_typo") || i.message.contains("missing field `tts`")),
        "{found:#?}"
    );
}

#[test]
fn a_syntax_error_has_a_position() {
    let text = format!("{BASE}\n[agent\n");
    let issue = only_issue(&text);
    assert_eq!(issue.position.map(|p| p.0), Some(pos(&text, "[agent\n").0));
}

#[test]
fn a_missing_env_var_names_the_variable_and_the_key_but_no_value() {
    let mut env = env();
    env.remove("DEEPGRAM_API_KEY");
    let e = load_with(BASE, "agent.toml", Format::Toml, &env, &|_| true).unwrap_err();
    assert_eq!(e.issues.len(), 1, "{e}");
    let issue = &e.issues[0];
    assert_eq!(issue.key, "stt.api_key_env");
    assert_eq!(issue.position, Some(pos(BASE, "\"DEEPGRAM_API_KEY\"")));
    assert!(issue.message.contains("`DEEPGRAM_API_KEY`"), "{issue}");
    assert!(issue.message.contains("is not set"), "{issue}");
    let shown = format!("{e} {e:?}");
    for secret in [STT_SECRET, LLM_SECRET, TTS_SECRET] {
        assert!(!shown.contains(secret), "{shown}");
    }
}

#[test]
fn an_empty_env_var_is_an_error() {
    let mut env = env();
    env.insert("CARTESIA_API_KEY".to_owned(), "  ".to_owned());
    let e = load_with(BASE, "agent.toml", Format::Toml, &env, &|_| true).unwrap_err();
    assert_eq!(e.issues[0].key, "tts.api_key_env");
    assert!(e.issues[0].message.contains("empty"), "{e}");
}

#[test]
fn every_missing_secret_is_reported_together() {
    let e = load_with(BASE, "agent.toml", Format::Toml, &BTreeMap::new(), &|_| {
        true
    })
    .unwrap_err();
    let keys: Vec<_> = e.issues.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(
        keys,
        ["stt.api_key_env", "llm.api_key_env", "tts.api_key_env"]
    );
}

/// A realistic OpenAI-style project key (not a real one).
const PASTED_SECRET: &str = "sk-proj-Q7vXk2Lm9PzR4tYw8NcB1dF6hJ3sA0eGuIoKqVbWxZyTn5Mr";

#[test]
fn a_key_pasted_into_api_key_env_is_never_echoed() {
    let text = edit(
        "api_key_env = \"OPENAI_API_KEY\"",
        &format!("api_key_env = \"{PASTED_SECRET}\""),
    );
    let e = load(&text).unwrap_err();
    let issue = &e.issues[0];
    assert_eq!(issue.key, "llm.api_key_env");
    assert_eq!(
        issue.position,
        Some(pos(&text, &format!("\"{PASTED_SECRET}\"")))
    );
    assert!(issue.message.contains("looks like a key"), "{issue}");
    assert!(issue.message.contains("`sk-p…`"), "{issue}");
    let shown = format!("{e} {e:?}");
    // No 8-character run of the key survives (only the 4-character vendor prefix is shown).
    for window in PASTED_SECRET.as_bytes().windows(8) {
        let run = std::str::from_utf8(window).unwrap();
        assert!(!shown.contains(run), "{run} leaked: {shown}");
    }
}

#[test]
fn a_key_shaped_variable_name_is_masked_when_it_is_not_set() {
    // Passes the name syntax (letters, digits, '_'), then fails the lookup.
    let pasted = "sk_4f9a1c2e7b3d8f0a6c5e9b1d2f4a7c3e8b0d6f1a9c2e";
    let text = edit(
        "api_key_env = \"CARTESIA_API_KEY\"",
        &format!("api_key_env = \"{pasted}\""),
    );
    let e = load(&text).unwrap_err();
    assert_eq!(e.issues.len(), 1, "{e}");
    assert_eq!(e.issues[0].key, "tts.api_key_env");
    assert!(e.issues[0].message.contains("is not set"), "{e}");
    assert!(e.issues[0].message.contains("looks like a key"), "{e}");
    let shown = format!("{e} {e:?}");
    assert!(!shown.contains(&pasted[..12]), "{shown}");
    // The parsed definition's Debug masks it too.
    let file = parse(&text, "t").unwrap();
    assert!(!format!("{file:?}").contains(&pasted[..12]));
    // A conventional name is still shown in full.
    let mut env = env();
    env.remove("CARTESIA_API_KEY");
    let e = load_with(BASE, "agent.toml", Format::Toml, &env, &|_| true).unwrap_err();
    assert!(e.issues[0].message.contains("`CARTESIA_API_KEY`"), "{e}");
}

#[test]
fn surrounding_whitespace_of_a_secret_is_trimmed() {
    let mut env = env();
    env.insert("DEEPGRAM_API_KEY".to_owned(), format!("  {STT_SECRET}\r\n"));
    let agent = load_with(BASE, "agent.toml", Format::Toml, &env, &|_| true).unwrap();
    assert_eq!(
        agent.stt_key.as_ref().map(|k| k.expose().as_str()),
        Some(STT_SECRET)
    );
}

#[test]
fn whitespace_inside_a_secret_is_an_error() {
    let mut env = env();
    env.insert("DEEPGRAM_API_KEY".to_owned(), "dg-secret value".to_owned());
    let e = load_with(BASE, "agent.toml", Format::Toml, &env, &|_| true).unwrap_err();
    assert_eq!(e.issues.len(), 1, "{e}");
    assert_eq!(e.issues[0].key, "stt.api_key_env");
    assert!(e.issues[0].message.contains("spaces or control"), "{e}");
    assert!(!format!("{e} {e:?}").contains("dg-secret"));
}

#[test]
fn an_endpoint_url_must_have_the_scheme_of_its_field() {
    let cases = [
        // (options table, value, expected message part)
        (
            "[tts.cartesia]\nurl",
            "https://api.cartesia.ai/tts/websocket",
            "`https://` is the wrong scheme",
        ),
        (
            "[llm.openai]\nbase_url",
            "wss://api.openai.com/v1",
            "`wss://` is the wrong scheme",
        ),
        (
            "[stt.deepgram]\nbase_url",
            "https://api.deepgram.com",
            "`https://` is the wrong scheme",
        ),
        (
            "[tts.cartesia]\nurl",
            "ws://10.0.0.5/tts/websocket",
            "only allowed to a loopback host",
        ),
        (
            "[llm.openai]\nbase_url",
            "http://user:pw@llm.internal:8000/v1",
            "only allowed to a loopback host",
        ),
        ("[tts.cartesia]\nurl", "wss://", "has no host"),
    ];
    for (table, value, expected) in cases {
        let text = format!("{BASE}\n{table} = \"{value}\"\n");
        let issue = only_issue(&text);
        let key = table.replace("]\n", ".").replace('[', "");
        assert_eq!(issue.key, key, "{value}");
        assert_eq!(issue.position, Some(pos(&text, &format!("\"{value}\""))));
        assert!(issue.message.contains(expected), "{value}: {issue}");
        if value.len() > "wss://".len() {
            assert!(!issue.message.contains(value), "{value} echoed: {issue}");
        }
    }
    for (table, value) in [
        ("[tts.cartesia]\nurl", "wss://api.cartesia.ai/tts/websocket"),
        ("[tts.cartesia]\nurl", "ws://localhost:9000/tts/websocket"),
        ("[stt.deepgram]\nbase_url", "ws://[::1]:8080"),
        ("[llm.openai]\nbase_url", "http://127.0.0.1:8000/v1"),
        ("[llm.openai]\nbase_url", "https://api.openai.com/v1"),
    ] {
        let text = format!("{BASE}\n{table} = \"{value}\"\n");
        load(&text).unwrap_or_else(|e| panic!("{value}: {e}"));
    }
}

#[test]
fn the_session_registry_holds_only_the_listed_tools() {
    use brisvox_agent::flows::example::Ack;
    use brisvox_agent::tools::{ToolKind, ToolSpec};

    let agent = load(BASE).unwrap();
    let mut server = ToolRegistry::new();
    for (name, kind) in [
        ("lookup_order", ToolKind::ReadOnly),
        ("transfer_call", ToolKind::ReadOnly),
        ("refund_order", ToolKind::SideEffect),
    ] {
        let spec = ToolSpec::new(name, "test", serde_json::json!({ "type": "object" }), kind);
        server.register(spec, Ack).unwrap();
    }
    let own = agent.tool_registry(&server).unwrap();
    let names: Vec<_> = own.specs().map(|s| s.name.to_string()).collect();
    assert_eq!(names, ["lookup_order"]);
    assert!(own.spec("refund_order").is_none());
    assert_eq!(server.len(), 3, "the server registry is unchanged");

    // A listed tool the server does not have is an error, as in check_tools.
    let Err(e) = agent.tool_registry(&ToolRegistry::new()) else {
        panic!("an unregistered tool must be an error")
    };
    assert_eq!(e.issues[0].key, "agent.tools[0]");
}

#[test]
fn confirm_tools_hold_side_effect_tools_for_the_users_confirmation() {
    use brisvox_agent::flows::example::Ack;
    use brisvox_agent::tools::{CONFIRM_TOOL, ToolKind, ToolSpec};

    let text = BASE.replace(
        "tools = [\"lookup_order\"]",
        "tools = [\"lookup_order\", \"refund_order\"]\nconfirm_tools = [\"refund_order\"]",
    );
    let agent = load(&text).unwrap();
    let mut server = ToolRegistry::new();
    for (name, kind) in [
        ("lookup_order", ToolKind::ReadOnly),
        ("refund_order", ToolKind::SideEffect),
    ] {
        let spec = ToolSpec::new(name, "test", serde_json::json!({ "type": "object" }), kind);
        server.register(spec, Ack).unwrap();
    }
    let own = agent.tool_registry(&server).unwrap();
    assert!(own.spec("refund_order").unwrap().confirm);
    assert!(!own.spec("lookup_order").unwrap().confirm);
    assert!(
        own.spec(CONFIRM_TOOL).is_some(),
        "the model gets confirm_pending"
    );
    // The server's own registry is not changed.
    assert!(!server.spec("refund_order").unwrap().confirm);
    assert!(server.spec(CONFIRM_TOOL).is_none());

    // A definition without the list has no confirmation tool.
    let plain = load(&BASE.replace("[\"lookup_order\"]", "[\"lookup_order\", \"refund_order\"]"))
        .unwrap()
        .tool_registry(&server)
        .unwrap();
    assert!(plain.spec(CONFIRM_TOOL).is_none());

    // A read-only tool cannot be held: there is nothing to confirm.
    let readonly = BASE.replace(
        "tools = [\"lookup_order\"]",
        "tools = [\"lookup_order\"]\nconfirm_tools = [\"lookup_order\"]",
    );
    let Err(e) = load(&readonly).unwrap().tool_registry(&server) else {
        panic!("a ReadOnly tool in confirm_tools must be an error")
    };
    assert_eq!(e.issues[0].key, "agent.confirm_tools[0]");
    // A name that is not one of the definition's tools is reported at load time.
    let stray = BASE.replace(
        "tools = [\"lookup_order\"]",
        "tools = [\"lookup_order\"]\nconfirm_tools = [\"other\"]",
    );
    assert_eq!(issues(&stray)[0].key, "agent.confirm_tools[0]");
}

#[test]
fn a_provider_not_compiled_in_names_the_feature() {
    let e = load_with(BASE, "agent.toml", Format::Toml, &env(), &|p| {
        p != Provider::Cartesia
    })
    .unwrap_err();
    assert_eq!(e.issues.len(), 1, "{e}");
    let issue = &e.issues[0];
    assert_eq!(issue.key, "tts.provider");
    assert_eq!(issue.position, Some(pos(BASE, "\"cartesia\"")));
    assert!(issue.message.contains("provider-cartesia"), "{issue}");
}

#[test]
fn compiled_in_follows_the_features() {
    assert_eq!(
        Provider::Deepgram.compiled_in(),
        cfg!(feature = "provider-deepgram")
    );
    assert_eq!(
        Provider::DeepgramAura.compiled_in(),
        cfg!(feature = "provider-deepgram-aura")
    );
}

/// `BASE` with an Aura TTS in `language`, voice `voice` and optional extra lines.
fn aura(language: &str, voice: &str, extra: &str) -> String {
    edit("language = \"en\"", &format!("language = \"{language}\""))
        .replace(
            "provider = \"cartesia\"\napi_key_env = \"CARTESIA_API_KEY\"\nvoice = \"voice-123\"",
            &format!(
                "provider = \"deepgram_aura\"\napi_key_env = \"DEEPGRAM_API_KEY\"\nvoice = \"{voice}\"\n{extra}"
            ),
        )
}

#[test]
fn turkish_on_deepgram_aura_is_rejected_at_the_language() {
    let text = aura("tr", "aura-2-thalia-en", "");
    let found = issues(&text);
    let issue = found
        .iter()
        .find(|i| i.message.contains("Deepgram Aura has no voice"))
        .unwrap_or_else(|| panic!("{found:#?}"));
    assert_eq!(issue.key, "agent.language");
    assert_eq!(issue.position, Some(pos(&text, "\"tr\"")));
}

#[test]
fn an_aura_voice_of_another_language_is_rejected() {
    let text = aura("en", "aura-2-carina-de", "");
    let issue = only_issue(&text);
    assert_eq!(issue.key, "tts.voice");
    assert!(issue.message.contains("speaks `de`"), "{issue}");
}

#[test]
fn an_aura_sample_rate_it_does_not_deliver_is_rejected() {
    let text = aura("en", "aura-2-thalia-en", "sample_rate = 22050");
    let issue = only_issue(&text);
    assert_eq!(issue.key, "tts.sample_rate");
    assert_eq!(issue.position, Some(pos(&text, "22050")));
    assert!(load(&aura("en", "aura-2-thalia-en", "sample_rate = 8000")).is_ok());
}

#[test]
fn an_unsupported_sample_rate_is_rejected_while_reading() {
    let text = edit(
        "voice = \"voice-123\"",
        "voice = \"voice-123\"\nsample_rate = 11025",
    );
    let issue = only_issue(&text);
    assert_eq!(issue.key, "tts.sample_rate");
    assert!(issue.message.contains("11025 Hz"), "{issue}");
}

#[test]
fn an_options_table_for_another_provider_is_rejected() {
    let text = format!("{BASE}\n[tts.elevenlabs]\nspeed = 1.0\n");
    let issue = only_issue(&text);
    assert_eq!(issue.key, "tts.elevenlabs");
    assert!(
        issue.message.contains("tts.provider is \"cartesia\""),
        "{issue}"
    );
}

#[test]
fn turn_policy_settings_must_match_the_policy() {
    let text = format!(
        "{BASE}\n[turn]\npolicy = \"fixed_silence\"\nfallback_silence_ms = 2000\n\n[early_analysis]\nenabled = true\n"
    );
    let found = issues(&text);
    let keys: Vec<_> = found.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "turn.silence_ms",
            "turn.fallback_silence_ms",
            "early_analysis.enabled"
        ],
        "{found:#?}"
    );

    let text = format!("{BASE}\n[turn]\nsilence_ms = 700\n");
    assert_eq!(only_issue(&text).key, "turn.silence_ms");
}

#[test]
fn deadlines_must_be_ordered() {
    let text = format!("{BASE}\n[turn]\nfinal_wait_ms = 2000\nresponse_deadline_ms = 1500\n");
    let issue = only_issue(&text);
    assert_eq!(issue.key, "turn.response_deadline_ms");

    let text = format!("{BASE}\n[turn]\nfinal_wait_ms = 3000\n");
    let issue = only_issue(&text);
    assert_eq!(issue.key, "turn.final_wait_ms");
    assert!(issue.message.contains("finalize"), "{issue}");
}

#[test]
fn a_frame_must_be_whole_samples_at_the_call_rate() {
    let text = format!("{BASE}\n[audio]\nframe_ms = 5\ncall_sample_rate = 22050\n");
    let issue = only_issue(&text);
    assert_eq!(issue.key, "audio.frame_ms");
    assert!(issue.message.contains("22050 Hz"), "{issue}");
}

#[test]
fn noise_suppression_is_off_by_default() {
    assert!(load(BASE).unwrap().denoise().is_none());
    let text = format!("{BASE}\n[audio]\nnoise_suppression = \"off\"\n");
    assert!(load(&text).unwrap().denoise().is_none());
}

#[test]
fn an_unknown_noise_suppression_is_rejected_at_its_value() {
    let text = format!("{BASE}\n[audio]\nnoise_suppression = \"fastenhancer\"\n");
    let error = load(&text).unwrap_err().to_string();
    assert!(error.contains("audio.noise_suppression"), "{error}");
    assert!(
        error.contains("rnnoise"),
        "the valid values are listed: {error}"
    );
}

#[test]
fn denoise_stt_needs_a_noise_suppression() {
    let text = format!("{BASE}\n[audio]\ndenoise_stt = true\n");
    let issue = only_issue(&text);
    assert_eq!(issue.key, "audio.denoise_stt");
    // False is the default and always fine.
    let text = format!("{BASE}\n[audio]\ndenoise_stt = false\n");
    assert!(load(&text).is_ok());
}

#[cfg(feature = "denoise-rnnoise")]
#[test]
fn rnnoise_builds_a_denoise_setup_that_follows_denoise_stt() {
    let text = format!("{BASE}\n[audio]\nnoise_suppression = \"rnnoise\"\n");
    let setup = load(&text).unwrap().denoise().unwrap();
    assert!(
        !setup.to_stt(),
        "the provider hears the original by default"
    );
    let text = format!("{BASE}\n[audio]\nnoise_suppression = \"rnnoise\"\ndenoise_stt = true\n");
    assert!(load(&text).unwrap().denoise().unwrap().to_stt());
}

#[cfg(feature = "denoise-hush")]
#[test]
fn hush_builds_a_denoise_setup() {
    let text = format!("{BASE}\n[audio]\nnoise_suppression = \"hush\"\n");
    let setup = load(&text).unwrap().denoise().unwrap();
    assert!(!setup.to_stt());
}

#[cfg(feature = "denoise-hush")]
#[test]
fn hush_is_refused_for_an_8_khz_call_rate() {
    let text = format!(
        "{BASE}
[audio]
noise_suppression = \"hush\"
call_sample_rate = 8000
"
    );
    let issue = only_issue(&text);
    assert_eq!(issue.key, "audio.noise_suppression");
    assert!(issue.message.contains("8000 Hz"), "{issue}");
    // At 16 kHz it is fine.
    let text = format!(
        "{BASE}
[audio]
noise_suppression = \"hush\"
call_sample_rate = 16000
"
    );
    assert!(load(&text).is_ok());
}

#[cfg(not(feature = "denoise-hush"))]
#[test]
fn hush_without_the_feature_is_a_positioned_start_error() {
    let text = format!("{BASE}\n[audio]\nnoise_suppression = \"hush\"\n");
    let issue = only_issue(&text);
    assert_eq!(issue.key, "audio.noise_suppression");
    assert!(issue.message.contains("denoise-hush"), "{issue}");
}

#[test]
fn echo_text_match_needs_min_words() {
    let text = format!("{BASE}\n[interruption]\necho_text_match = true\n");
    let issue = only_issue(&text);
    assert_eq!(issue.key, "interruption.echo_text_match");
    assert!(issue.message.contains("min_words"), "{issue}");
    let text = format!(
        "{BASE}\n[interruption]\npreset = \"off\"\nmin_words = 1\necho_text_match = true\n"
    );
    assert_eq!(only_issue(&text).key, "interruption.echo_text_match");
    let text = format!("{BASE}\n[interruption]\nmin_words = 1\necho_text_match = true\n");
    let config = load(&text).unwrap();
    assert!(config.engine().interruption.echo_text_match);
    assert!(!load(BASE).unwrap().engine().interruption.echo_text_match);
}

#[test]
fn the_vad_level_gate_is_off_by_default_and_reads_its_offset() {
    assert!(load(BASE).unwrap().level_gate().is_none());
    let text = format!("{BASE}\n[vad]\nlevel_gate_db = 12\n");
    let gate = load(&text).unwrap().level_gate().unwrap();
    let rolling = gate.rolling.unwrap();
    assert!((rolling.offset_db + 12.0).abs() < f32::EPSILON);
    assert!(gate.onset_only);
    let text = format!("{BASE}\n[vad]\nlevel_gate_db = 2\n");
    assert!(load(&text).is_err(), "out of range");
}

#[cfg(not(feature = "denoise-rnnoise"))]
#[test]
fn rnnoise_without_the_feature_is_a_positioned_start_error() {
    let text = format!("{BASE}\n[audio]\nnoise_suppression = \"rnnoise\"\n");
    let issue = only_issue(&text);
    assert_eq!(issue.key, "audio.noise_suppression");
    assert!(issue.message.contains("denoise-rnnoise"), "{issue}");
    assert!(issue.position.is_some());
}

#[test]
fn stt_models_without_turkish_are_rejected_for_turkish() {
    let tr = edit("language = \"en\"", "language = \"tr\"");
    let text = format!("{tr}\n[stt.deepgram]\nmodel = \"enhanced\"\n");
    let issue = only_issue(&text);
    assert_eq!(issue.key, "stt.deepgram.model");
    assert!(
        load(&format!(
            "{tr}\n[stt.deepgram]\nmodel = \"nova-2-general\"\n"
        ))
        .is_ok()
    );

    let text = tr
        .replace("provider = \"deepgram\"", "provider = \"assemblyai\"")
        .replace("DEEPGRAM_API_KEY", "ASSEMBLYAI_API_KEY")
        + "\n[stt.assemblyai]\nspeech_model = \"universal-streaming-multilingual\"\n";
    let issue = only_issue(&text);
    assert_eq!(issue.key, "stt.assemblyai.speech_model");
}

#[test]
fn only_english_and_turkish_conversations_are_supported() {
    let text = edit("language = \"en\"", "language = \"de\"");
    let issue = only_issue(&text);
    assert_eq!(issue.key, "agent.language");
    let text = edit("language = \"en\"", "language = \"english\"");
    assert!(only_issue(&text).message.contains("not a language tag"));
}

#[test]
fn a_bad_list_entry_is_reported_at_its_own_position() {
    let text = format!("{BASE}\n[interruption]\nbackchannels = [\"en\", \"de\"]\n");
    let issue = only_issue(&text);
    assert_eq!(issue.key, "interruption.backchannels[1]");
    assert_eq!(issue.position, Some(pos(&text, "\"de\"")));
}

#[test]
fn agent_fields_are_checked() {
    let text = edit("\"You are a helpful agent.\"", "\"   \"");
    assert_eq!(only_issue(&text).key, "agent.system_prompt");
    let text = edit(
        "tools = [\"lookup_order\"]",
        "tools = [\"a\", \"b\", \"a\"]",
    );
    let issue = only_issue(&text);
    assert_eq!(issue.key, "agent.tools[2]");
    let text = edit("tools = [\"lookup_order\"]", "tools = [\"bad name\"]");
    assert_eq!(only_issue(&text).key, "agent.tools[0]");
}

#[test]
fn llm_keys_and_compatible_servers_are_checked() {
    let text = edit("api_key_env = \"OPENAI_API_KEY\"\n", "");
    assert_eq!(only_issue(&text).key, "llm.api_key_env");

    let compatible = edit(
        "provider = \"openai\"\napi_key_env = \"OPENAI_API_KEY\"",
        "provider = \"openai_compatible\"",
    );
    assert_eq!(only_issue(&compatible).key, "llm.openai_compatible");
    let text = format!("{compatible}\n[llm.openai_compatible]\nmodel = \"m\"\n");
    assert_eq!(only_issue(&text).key, "llm.openai_compatible");
    let text = format!("{compatible}\n[llm.openai_compatible]\npreset = \"ollama\"\n");
    assert!(load(&text).is_ok());
}

/// `preset = "openrouter"` with the key from the environment and a model: the OpenRouter endpoint
/// and name, the model and `reasoning_effort` as written.
#[cfg(feature = "provider-openai-compatible")]
#[test]
fn the_openrouter_preset_fills_in_the_endpoint() {
    let compatible = edit(
        "provider = \"openai\"\napi_key_env = \"OPENAI_API_KEY\"",
        "provider = \"openai_compatible\"\napi_key_env = \"OPENAI_API_KEY\"",
    );
    let text = format!(
        "{compatible}\n[llm.openai_compatible]\npreset = \"openrouter\"\n\
         model = \"vendor/some-model\"\nreasoning_effort = \"none\"\n"
    );
    let file = parse(&text, "t").unwrap();
    let oc = map::openai_compatible(
        file.llm.openai_compatible.as_ref().map(Spanned::get_ref),
        None,
    );
    assert_eq!(oc.name, "openrouter");
    assert_eq!(oc.base_url, "https://openrouter.ai/api/v1");
    assert_eq!(oc.model, "vendor/some-model");
    assert_eq!(oc.extra_body.len(), 1);
    assert!(load(&text).is_ok(), "{:?}", load(&text).err());
}

/// `openrouter_order` / `openrouter_allow_fallbacks` become the request body's `provider` object.
#[cfg(feature = "provider-openai-compatible")]
#[test]
fn openrouter_routing_is_sent_as_the_provider_object() {
    let compatible = edit(
        "provider = \"openai\"
api_key_env = \"OPENAI_API_KEY\"",
        "provider = \"openai_compatible\"
api_key_env = \"OPENAI_API_KEY\"",
    );
    let text = format!(
        "{compatible}
[llm.openai_compatible]
preset = \"openrouter\"
model = \"vendor/m\"
         openrouter_order = [\"openai\"]
openrouter_allow_fallbacks = false
"
    );
    let file = parse(&text, "t").unwrap();
    let oc = map::openai_compatible(
        file.llm.openai_compatible.as_ref().map(Spanned::get_ref),
        None,
    );
    assert_eq!(
        oc.extra_body,
        [(
            "provider".to_owned(),
            serde_json::json!({"order": ["openai"], "allow_fallbacks": false})
        )]
    );
    assert!(load(&text).is_ok(), "{:?}", load(&text).err());
    // The fallbacks flag alone is a mistake, an empty slug too.
    let alone = format!(
        "{compatible}
[llm.openai_compatible]
preset = \"openrouter\"
model = \"m\"
         openrouter_allow_fallbacks = false
"
    );
    assert_eq!(
        only_issue(&alone).key,
        "llm.openai_compatible.openrouter_allow_fallbacks"
    );
    let empty = format!(
        "{compatible}
[llm.openai_compatible]
preset = \"openrouter\"
model = \"m\"
         openrouter_order = [\"\"]
"
    );
    assert_eq!(
        only_issue(&empty).key,
        "llm.openai_compatible.openrouter_order"
    );
}

/// `openrouter_order` / `openrouter_allow_fallbacks` are OpenRouter-only: an error
/// unless the table has `preset = "openrouter"` or a `base_url` on openrouter.ai.
#[cfg(feature = "provider-openai-compatible")]
#[test]
fn openrouter_routing_needs_the_openrouter_preset_or_base_url() {
    let compatible = edit(
        "provider = \"openai\"
api_key_env = \"OPENAI_API_KEY\"",
        "provider = \"openai_compatible\"
api_key_env = \"OPENAI_API_KEY\"",
    );
    let load_table = |table: &str| {
        load(&format!(
            "{compatible}
[llm.openai_compatible]
{table}
openrouter_order = [\"openai\"]
             openrouter_allow_fallbacks = false
"
        ))
    };
    // Another server, another preset, or no endpoint hint at all: one error per key.
    for table in [
        "preset = \"groq\"
model = \"m\"",
        "base_url = \"https://api.example.com/v1\"
model = \"m\"",
        "base_url = \"https://notopenrouter.ai/api/v1\"
model = \"m\"",
        "base_url = \"https://openrouter.ai.evil.example/api/v1\"
model = \"m\"",
    ] {
        let found = load_table(table).unwrap_err().issues;
        let mut keys: Vec<_> = found.iter().map(|i| i.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "llm.openai_compatible.openrouter_allow_fallbacks",
                "llm.openai_compatible.openrouter_order"
            ],
            "{table}: {found:#?}"
        );
        assert!(
            found
                .iter()
                .all(|i| i.message.contains("OpenRouter-only") && i.position.is_some()),
            "{found:#?}"
        );
    }
    // The preset, or the OpenRouter host (also a subdomain, with a port, any case).
    for table in [
        "preset = \"openrouter\"
model = \"a/m\"",
        "base_url = \"https://openrouter.ai/api/v1\"
model = \"a/m\"",
        "base_url = \"https://User@EU.OpenRouter.AI:443/api/v1\"
model = \"a/m\"",
    ] {
        assert!(
            load_table(table).is_ok(),
            "{table}: {:?}",
            load_table(table).err()
        );
    }
    // Neither key set: nothing to complain about on any server.
    let plain = format!(
        "{compatible}
[llm.openai_compatible]
base_url = \"https://api.example.com/v1\"
         model = \"m\"
"
    );
    assert!(load(&plain).is_ok());
}

#[test]
fn problems_are_reported_together_in_file_order() {
    let text =
        aura("tr", "aura-2-thalia-en", "sample_rate = 44100") + "\n[turn]\nsilence_ms = 500\n";
    let found = issues(&text);
    let keys: Vec<_> = found.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(
        keys,
        ["agent.language", "tts.sample_rate", "turn.silence_ms"],
        "{found:#?}"
    );
    let shown = ConfigError {
        source_name: "agent.toml".to_owned(),
        issues: found,
    }
    .to_string();
    assert!(shown.starts_with("invalid agent definition agent.toml (3 problems)"));
    assert!(
        shown.contains("\n  agent.toml:4:12: `agent.language`: "),
        "{shown}"
    );
}

#[test]
fn tools_are_checked_against_the_registry() {
    use brisvox_agent::flows::example::Ack;
    use brisvox_agent::tools::{ToolKind, ToolSpec};

    let agent = load(&edit(
        "tools = [\"lookup_order\"]",
        "tools = [\"lookup_order\", \"transfer\"]",
    ))
    .unwrap();
    let mut registry = ToolRegistry::new();
    let spec = |name: &str| {
        ToolSpec::new(
            name,
            "test",
            serde_json::json!({ "type": "object" }),
            ToolKind::ReadOnly,
        )
    };
    registry.register(spec("lookup_order"), Ack).unwrap();
    let e = agent.check_tools(&registry).unwrap_err();
    assert_eq!(e.issues.len(), 1);
    assert_eq!(e.issues[0].key, "agent.tools[1]");
    assert!(e.issues[0].message.contains("`transfer`"));
    registry.register(spec("transfer"), Ack).unwrap();
    agent.check_tools(&registry).unwrap();
}

#[test]
fn engine_mapping_applies_every_section() {
    let text = format!(
        "{BASE}
[turn]
policy = \"fixed_silence\"
silence_ms = 650
final_wait_ms = 1200
response_deadline_ms = 15000

[interruption]
preset = \"off\"
min_words = 2
confirm = \"any\"
backchannels = [\"tr\"]

[speculation]
enabled = true
on_partial = false
partial_stable_ms = 150
max_per_turn = 2
max_age_ms = 3000
max_buffered_events = 128

[audio]
frame_ms = 10
call_sample_rate = 8000
"
    )
    .replace(
        "voice = \"voice-123\"",
        "voice = \"voice-123\"\nsample_rate = 16000",
    )
    .replace(
        "api_key_env = \"OPENAI_API_KEY\"",
        "api_key_env = \"OPENAI_API_KEY\"\nmax_tool_rounds = 3\ncontext_window_tokens = 8000",
    );
    let engine = load(&text).unwrap().engine();
    assert_eq!(engine.turn, TurnPolicy::fixed(Duration::from_millis(650)));
    assert_eq!(engine.final_wait, Duration::from_millis(1200));
    assert_eq!(engine.response_deadline, Duration::from_secs(15));
    let i = engine.interruption;
    assert_eq!(i.min_speech, Duration::ZERO, "the off preset");
    assert_eq!((i.min_words, i.confirm), (2, Confirm::Any));
    assert!(i.turkish && !i.english);
    let s = engine.speculation;
    assert!(s.enabled && !s.on_partial);
    assert_eq!((s.max_per_turn, s.max_buffered_events), (2, 128));
    assert_eq!(s.max_age, Duration::from_secs(3));
    assert_eq!(s.partial_stable_for, Duration::from_millis(150));
    assert_eq!(engine.rates.frame, Duration::from_millis(10));
    assert_eq!(engine.rates.fallback_call.input, SampleRate::Hz8000);
    assert_eq!(engine.rates.fallback_call.output, SampleRate::Hz8000);
    assert_eq!(engine.rates.tts, SampleRate::Hz16000);
    assert_eq!(engine.llm.max_tool_rounds, 3);
    assert_eq!(engine.llm.window.max_tokens, 8000);
    assert!(!engine.early_analysis.enabled);
}

#[test]
fn adaptive_turn_and_early_analysis_mapping() {
    let text = format!(
        "{BASE}
[turn]
fallback_silence_ms = 2500
require_final = true

[early_analysis]
enabled = true
max_tail_ms = 200
max_per_turn = 2
"
    );
    let engine = load(&text).unwrap().engine();
    let TurnPolicy::Adaptive {
        fallback_silence,
        require_final,
        final_grace,
    } = engine.turn
    else {
        panic!("{:?}", engine.turn);
    };
    assert_eq!(fallback_silence, Duration::from_millis(2500));
    assert!(require_final);
    let TurnPolicy::Adaptive {
        final_grace: default_grace,
        ..
    } = TurnPolicy::adaptive()
    else {
        panic!("the default policy is adaptive");
    };
    assert_eq!(final_grace, default_grace);
    let early = engine.early_analysis;
    assert!(early.enabled);
    assert_eq!(early.max_tail, Duration::from_millis(200));
    assert_eq!(early.max_per_turn, 2);
    assert_eq!(
        early.fall_delay,
        brisvox_engine::EarlyAnalysisConfig::OFF.fall_delay
    );
}

#[test]
fn turkish_maps_to_the_turkish_engine_language() {
    let text = edit("language = \"en\"", "language = \"tr-TR\"");
    let agent = load(&text).unwrap();
    assert_eq!(agent.language(), Language::Tr);
    assert_eq!(agent.stt_language().as_str(), "tr-TR");
}

#[test]
fn load_file_reads_and_bounds_the_file() {
    let dir = std::env::temp_dir().join(format!("brisvox-config-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("agent.toml");
    std::fs::write(&path, BASE).unwrap();
    // The process environment has no keys: the error names them, which proves the file was read.
    let e = load_file(&path, &BTreeMap::new()).unwrap_err();
    assert_eq!(e.source_name, path.display().to_string());
    // (Without the provider features there are also "not compiled in" issues.)
    let env_keys: Vec<_> = e
        .issues
        .iter()
        .map(|i| i.key.as_str())
        .filter(|k| k.ends_with("api_key_env"))
        .collect();
    assert_eq!(
        env_keys,
        ["stt.api_key_env", "llm.api_key_env", "tts.api_key_env"]
    );

    let big = dir.join("big.toml");
    let filler = "#".repeat(usize::try_from(MAX_FILE_BYTES).unwrap() + 10);
    std::fs::write(&big, filler).unwrap();
    let e = load_file(&big, &env()).unwrap_err();
    assert!(e.issues[0].message.contains("larger than"), "{e}");

    let e = load_file(&dir.join("missing.toml"), &env()).unwrap_err();
    assert!(e.issues[0].message.contains("cannot open"), "{e}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(all(
    feature = "provider-deepgram",
    feature = "provider-openai",
    feature = "provider-cartesia"
))]
#[test]
fn provider_mapping_for_the_base_definition() {
    use brisvox_core::SystemClock;
    use tokio_util::sync::CancellationToken;

    let text = format!(
        "{BASE}
[stt.deepgram]
model = \"nova-2\"
endpointing_ms = 0
keyterms = [\"brisvox\"]
region = \"eu\"

[llm.openai]
model = \"gpt-test\"
reasoning_effort = \"low\"
max_output_tokens = 300

[tts.cartesia]
model = \"sonic-test\"
max_buffer_delay_ms = 100
"
    );
    let agent = load(&text).unwrap();
    let Ok(SttProviderConfig::Deepgram(stt)) = agent.stt() else {
        panic!("deepgram expected");
    };
    assert_eq!(stt.api_key.expose(), STT_SECRET);
    assert_eq!(stt.model, "nova-2");
    assert_eq!(stt.language.as_deref(), Some("en"));
    assert_eq!(
        stt.endpointing,
        brisvox_providers::deepgram::Endpointing::Off
    );
    assert_eq!(stt.keyterms, ["brisvox"]);
    assert_eq!(stt.region, brisvox_providers::deepgram::Region::Eu);

    let Ok(LlmProviderConfig::OpenAi(llm)) = agent.llm() else {
        panic!("openai expected");
    };
    assert_eq!(llm.api_key.expose(), LLM_SECRET);
    assert_eq!(llm.model, "gpt-test");
    assert_eq!(
        llm.reasoning_effort,
        Some(brisvox_providers::openai::ReasoningEffort::Low)
    );
    assert_eq!(llm.max_output_tokens, Some(300));

    let Ok(TtsProviderConfig::Cartesia(tts)) =
        agent.tts(SystemClock::shared(), CancellationToken::new())
    else {
        panic!("cartesia expected");
    };
    assert_eq!(tts.api_key.expose(), TTS_SECRET);
    assert_eq!(tts.model_id, "sonic-test");
    assert_eq!(tts.max_buffer_delay_ms, 100);
    assert_eq!(tts.language.as_deref(), Some("en"));
}

#[cfg(all(
    feature = "provider-assemblyai",
    feature = "provider-gladia",
    feature = "provider-anthropic",
    feature = "provider-gemini",
    feature = "provider-openai-compatible",
    feature = "provider-elevenlabs",
    feature = "provider-deepgram-aura"
))]
#[test]
fn provider_mapping_for_the_other_adapters() {
    use brisvox_core::Secret;

    let tag = |t: &str| types::LanguageTag::parse(t).unwrap();
    let key = || Secret::new("k".to_owned());
    let file = parse(
        &format!(
            "{BASE}
[stt.assemblyai]
speech_model = \"universal-3-5-pro\"
region = \"eu\"
min_turn_silence_ms = 100
max_turn_silence_ms = 1200

[stt.gladia]
region = \"eu_west\"
endpointing_ms = 300
vocabulary = [\"brisvox\"]

[llm.anthropic]
max_tokens = 512
effort = \"low\"
thinking = \"disabled\"

[llm.gemini]
thinking_level = \"minimal\"
temperature = 0.5

[llm.openai_compatible]
preset = \"groq\"
model = \"llama-test\"
max_tokens = 200
reasoning_effort = \"none\"

[tts.elevenlabs]
model = \"eleven_flash_v2_5\"
speed = 1.1

[tts.deepgram_aura]
speed = 0.9
"
        ),
        "t",
    )
    .unwrap();
    fn span<T>(o: &Option<Spanned<T>>) -> Option<&T> {
        o.as_ref().map(Spanned::get_ref)
    }

    let a = map::assemblyai(span(&file.stt.assemblyai), key(), &tag("tr-TR"));
    assert_eq!(a.speech_model, "universal-3-5-pro");
    assert_eq!(a.language.as_deref(), Some("tr"));
    assert_eq!(a.region, brisvox_providers::assemblyai::Region::Eu);
    assert_eq!(
        (a.min_turn_silence_ms, a.max_turn_silence_ms),
        (Some(100), Some(1200))
    );

    let g = map::gladia(span(&file.stt.gladia), key(), &tag("tr"));
    assert!((g.endpointing - 0.3).abs() < 1e-9);
    assert_eq!(g.region, Some(brisvox_providers::gladia::Region::EuWest));
    assert_eq!(g.vocabulary, ["brisvox"]);
    assert_eq!(g.language.as_deref(), Some("tr"));

    let an = map::anthropic(span(&file.llm.anthropic), key());
    assert_eq!(an.max_tokens, 512);
    assert_eq!(an.effort, Some(brisvox_providers::anthropic::Effort::Low));
    assert_eq!(
        an.thinking,
        brisvox_providers::anthropic::ThinkingMode::Disabled
    );
    assert_eq!(an.model, brisvox_providers::anthropic::DEFAULT_MODEL);

    let ge = map::gemini(span(&file.llm.gemini), key());
    assert_eq!(ge.temperature, Some(0.5));
    assert_eq!(
        ge.thinking_level,
        Some(brisvox_providers::gemini::ThinkingLevel::Minimal)
    );

    let oc = map::openai_compatible(span(&file.llm.openai_compatible), None);
    assert_eq!(oc.name, "groq");
    assert_eq!(oc.model, "llama-test");
    assert_eq!(oc.max_tokens, Some(200));
    assert_eq!(
        oc.extra_body,
        [(
            "reasoning_effort".to_owned(),
            serde_json::Value::String("none".to_owned())
        )]
    );
    assert!(oc.api_key.is_none());
    assert_eq!(
        oc.base_url,
        brisvox_providers::openai_compatible::Preset::Groq.base_url()
    );

    let clock = brisvox_core::SystemClock::shared();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let el = map::elevenlabs(
        span(&file.tts.elevenlabs),
        key(),
        &tag("tr-TR"),
        clock.clone(),
        shutdown.clone(),
    );
    assert_eq!(el.language_code.as_deref(), Some("tr"));
    assert_eq!(el.voice_settings.as_ref().and_then(|v| v.speed), Some(1.1));
    assert_eq!(el.voice_settings.as_ref().and_then(|v| v.stability), None);
    let plain = map::elevenlabs(None, key(), &tag("en"), clock.clone(), shutdown.clone());
    assert!(plain.voice_settings.is_none());

    let au = map::deepgram_aura(
        span(&file.tts.deepgram_aura),
        key(),
        &tag("en"),
        clock,
        shutdown,
    );
    assert_eq!(au.speed, Some(0.9));
    assert_eq!(au.language.as_deref(), Some("en"));
}

/// Every example in `examples/agents` and every shipped agent in `deploy/agents` (the files the
/// image bakes in and compose mounts; TOML, and YAML when the `yaml` feature is on) and in
/// `deploy/local/agents` (the keyless local stack) loads (with placeholder keys) and builds its
/// adapter configurations, so they cannot rot.
#[cfg(feature = "all-providers")]
#[test]
fn the_examples_are_valid() {
    for (rel, min) in [
        ("../../examples/agents", 3),
        ("../../deploy/agents", 2),
        ("../../deploy/local/agents", 2),
    ] {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
        check_agent_dir(&dir, min);
    }
}

/// Loads every agent file in `dir` and asserts there are at least `min` of them.
#[cfg(feature = "all-providers")]
fn check_agent_dir(dir: &std::path::Path, min: usize) {
    let mut seen = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let format = Format::from_path(&path);
        let known = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("toml" | "yaml" | "yml")
        );
        if !known || !format.compiled_in() {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let file = parse_as(&text, "x", format).unwrap_or_else(|e| panic!("{e}"));
        let mut env = BTreeMap::new();
        let secondary_keys = [
            file.stt
                .secondary
                .as_ref()
                .and_then(|s| s.get_ref().api_key_env.as_ref()),
            file.llm
                .secondary
                .as_ref()
                .and_then(|s| s.get_ref().api_key_env.as_ref()),
            file.tts
                .secondary
                .as_ref()
                .and_then(|s| s.get_ref().api_key_env.as_ref()),
        ];
        for name in [
            file.stt.api_key_env.as_ref(),
            file.llm.api_key_env.as_ref(),
            file.tts.api_key_env.as_ref(),
        ]
        .into_iter()
        .chain(secondary_keys)
        .flatten()
        {
            env.insert(name.get_ref().as_str().to_owned(), "placeholder".to_owned());
        }
        let agent = load_file(&path, &env).unwrap_or_else(|e| panic!("{e}"));
        agent.stt().unwrap();
        agent.llm().unwrap();
        let (clock, cancel) = (
            brisvox_core::SystemClock::shared(),
            tokio_util::sync::CancellationToken::new(),
        );
        agent.tts(clock.clone(), cancel.clone()).unwrap();
        // A secondary maps like a primary.
        assert_eq!(
            agent.stt_secondary().unwrap().is_some(),
            file.stt.secondary.is_some()
        );
        assert_eq!(
            agent.llm_secondary().unwrap().is_some(),
            file.llm.secondary.is_some()
        );
        assert_eq!(
            agent.tts_secondary(clock, cancel).unwrap().is_some(),
            file.tts.secondary.is_some()
        );
        seen += 1;
    }
    assert!(
        seen >= min,
        "expected at least {min} agent files in {}",
        dir.display()
    );
}

/// A build without the `yaml` feature names the feature instead of misreading a YAML file.
#[cfg(not(feature = "yaml"))]
#[test]
fn yaml_without_the_feature_names_the_feature() {
    assert!(!Format::Yaml.compiled_in());
    let e = parse_as("agent:\n  name: a\n", "agent.yaml", Format::Yaml).unwrap_err();
    assert_eq!(e.issues.len(), 1, "{e}");
    assert!(e.issues[0].message.contains("`yaml` feature"), "{e}");
}

#[test]
fn the_filler_section_maps_onto_the_runner_filler_config() {
    use brisvox_agent::tools::{DEFAULT_FILLER_DELAY, FillerConfig};
    // Absent: the session default, a fixed 1.2 s for every tool with a filler.
    assert_eq!(
        load(BASE).unwrap().engine().llm.filler,
        FillerConfig::DEFAULT
    );
    assert_eq!(FillerConfig::DEFAULT.delay, Some(DEFAULT_FILLER_DELAY));

    let text = format!(
        "{BASE}
[filler]
delay_ms = 900
"
    );
    let filler = load(&text).unwrap().engine().llm.filler;
    assert_eq!(
        (filler.enabled, filler.delay),
        (true, Some(Duration::from_millis(900)))
    );

    let text = format!(
        "{BASE}
[filler]
per_tool = true
"
    );
    assert_eq!(
        load(&text).unwrap().engine().llm.filler,
        FillerConfig::PER_TOOL
    );

    let text = format!(
        "{BASE}
[filler]
enabled = false
"
    );
    assert!(!load(&text).unwrap().engine().llm.filler.enabled);
}

#[test]
fn a_filler_delay_with_per_tool_delays_is_rejected() {
    let text = format!(
        "{BASE}
[filler]
per_tool = true
delay_ms = 900
"
    );
    let issue = only_issue(&text);
    assert_eq!(issue.key, "filler.delay_ms");
    assert_eq!(issue.position, Some(pos(&text, "900")));
    assert!(
        issue.message.contains(
            "filler.per_tool = true takes each tool's own delay, so filler.delay_ms has no effect; remove one of them"
        ),
        "{issue}"
    );
    assert!(!issue.message.contains("  "), "{issue}");

    let text = format!(
        "{BASE}
[filler]
delay_ms = 50
"
    );
    let issue = only_issue(&text);
    assert_eq!(issue.key, "filler.delay_ms");
    assert!(issue.message.contains("100..=10000 ms"), "{issue}");

    let text = format!(
        "{BASE}
[filler]
phrase = \"Hm.\"
"
    );
    assert!(only_issue(&text).message.contains("phrase"));
}

#[test]
fn telephony_destinations_are_checked_and_become_the_policy() {
    // Absent: no destination, DTMF on, warm transfers off.
    let policy = load(BASE).unwrap().telephony_policy();
    assert!(policy.destinations().is_empty());
    assert!(policy.allow_dtmf);
    assert!(!policy.allow_warm);

    let text = format!(
        "{BASE}
[telephony]
dtmf = false
destinations = [
  {{ name = \"sales\", number = \"+15555550100\", description = \"the sales team\" }},
  {{ name = \"support\", number = \"+15555550101\" }},
]
"
    );
    let policy = load(&text).unwrap().telephony_policy();
    let names: Vec<&str> = policy
        .destinations()
        .iter()
        .map(|d| d.name.as_str())
        .collect();
    assert_eq!(names, ["sales", "support"]);
    assert!(!policy.allow_dtmf);

    // A bad number and a repeated name, each at its own position.
    let text = format!(
        "{BASE}
[telephony]
destinations = [
  {{ name = \"sales\", number = \"555-0100\" }},
  {{ name = \"sales\", number = \"+15555550101\" }},
  {{ name = \"sales\", number = \"+15555550102\" }},
]
"
    );
    let found = issues(&text);
    let keys: Vec<&str> = found.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "telephony.destinations[0].number",
            "telephony.destinations[2].name"
        ],
        "{found:#?}"
    );
    assert_eq!(found[0].position, Some(pos(&text, "\"555-0100\"")));
    assert!(found[1].message.contains("twice"), "{}", found[1].message);
}

#[test]
fn warm_transfers_and_voicemail_are_opt_in_and_checked() {
    // Absent: no voicemail handling.
    assert!(load(BASE).unwrap().voicemail().is_none());

    let text = format!(
        "{BASE}
[telephony]
warm = true

[telephony.voicemail]
message = \"  Please call us back.  \"
on_unknown = \"hang_up\"
detect_only = true
"
    );
    let agent = load(&text).unwrap();
    assert!(agent.telephony_policy().allow_warm);
    let voicemail = agent.voicemail().unwrap();
    assert_eq!(voicemail.message, "Please call us back.", "trimmed");
    assert_eq!(voicemail.on_unknown, VoicemailUnknown::HangUp);
    assert!(voicemail.detect_only);

    // Defaults: a person on an unknown verdict, a detector that reports the greeting's end.
    let text = format!("{BASE}\n[telephony.voicemail]\nmessage = \"Hi\"\n");
    let voicemail = load(&text).unwrap().voicemail().unwrap();
    assert_eq!(voicemail.on_unknown, VoicemailUnknown::TreatAsHuman);
    assert!(!voicemail.detect_only);

    // An empty message, one that is too long, a misspelt policy, an unknown key: each at its
    // own position.
    let text = format!("{BASE}\n[telephony.voicemail]\nmessage = \"   \"\n");
    let found = issues(&text);
    assert_eq!(found[0].key, "telephony.voicemail.message", "{found:#?}");
    assert!(found[0].message.contains("empty"), "{}", found[0].message);
    let long = "x".repeat(crate::config::MAX_VOICEMAIL_CHARS + 1);
    let text = format!("{BASE}\n[telephony.voicemail]\nmessage = \"{long}\"\n");
    let found = issues(&text);
    assert_eq!(found[0].key, "telephony.voicemail.message", "{found:#?}");
    let text =
        format!("{BASE}\n[telephony.voicemail]\nmessage = \"Hi\"\non_unknown = \"ignore\"\n");
    assert!(load(&text).is_err());
    let text = format!("{BASE}\n[telephony.voicemail]\nmessage = \"Hi\"\nvoice = \"x\"\n");
    assert!(load(&text).is_err());
    let text = format!("{BASE}\n[telephony.voicemail]\non_unknown = \"hang_up\"\n");
    assert!(load(&text).is_err(), "the message is required");
}

/// Every `provider` value of a definition maps to the name an operator limits it by
/// (`BRISVOX_PROVIDER_LIMITS`): `agent_providers` and `KNOWN_PROVIDERS` agree with the file
/// format, and a call's agent reports exactly the providers it runs on.
#[test]
fn every_provider_kind_maps_to_a_name_the_provider_limits_know() {
    use crate::admission::{KNOWN_PROVIDERS, agent_providers};

    let stt = |name: &str, key: &str| {
        edit(
            "provider = \"deepgram\"\napi_key_env = \"DEEPGRAM_API_KEY\"",
            &format!("provider = \"{name}\"\napi_key_env = \"{key}\""),
        )
    };
    let llm = |name: &str, key: &str, extra: &str| {
        let text = edit(
            "provider = \"openai\"\napi_key_env = \"OPENAI_API_KEY\"",
            &format!("provider = \"{name}\"\napi_key_env = \"{key}\""),
        );
        format!("{text}{extra}")
    };
    let tts = |name: &str, key: &str| {
        edit(
            "provider = \"cartesia\"\napi_key_env = \"CARTESIA_API_KEY\"",
            &format!("provider = \"{name}\"\napi_key_env = \"{key}\""),
        )
    };
    let definitions = [
        ("deepgram", stt("deepgram", "DEEPGRAM_API_KEY")),
        ("assemblyai", stt("assemblyai", "ASSEMBLYAI_API_KEY")),
        ("gladia", stt("gladia", "GLADIA_API_KEY")),
        ("sherpa_onnx", stt("sherpa_onnx", "DEEPGRAM_API_KEY")),
        (
            "openai_realtime_stt",
            stt("openai_realtime_stt", "OPENAI_API_KEY"),
        ),
        ("openai", llm("openai", "OPENAI_API_KEY", "")),
        ("anthropic", llm("anthropic", "ANTHROPIC_API_KEY", "")),
        ("gemini", llm("gemini", "GEMINI_API_KEY", "")),
        (
            "openai_compatible",
            llm(
                "openai_compatible",
                "GROQ_API_KEY",
                "\n[llm.openai_compatible]\npreset = \"groq\"\n",
            ),
        ),
        ("cartesia", tts("cartesia", "CARTESIA_API_KEY")),
        ("elevenlabs", tts("elevenlabs", "ELEVENLABS_API_KEY")),
        ("deepgram_aura", aura("en", "aura-2-thalia-en", "")),
        (
            "openai_speech",
            edit(
                "provider = \"cartesia\"
api_key_env = \"CARTESIA_API_KEY\"",
                "provider = \"openai_speech\"",
            ) + "
[tts.openai_speech]
preset = \"kokoro\"
base_url = \"http://localhost:8880/v1\"
",
        ),
    ];

    let mut seen: Vec<&str> = Vec::new();
    for (name, text) in &definitions {
        let agent = load(text).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        let used = agent_providers(&agent);
        assert!(used.contains(name), "{name} not in {used:?}");
        assert_eq!(used.len(), 3, "an STT, an LLM and a TTS: {used:?}");
        assert!(
            KNOWN_PROVIDERS.contains(name),
            "{name} is not in KNOWN_PROVIDERS"
        );
        seen.push(name);
    }
    // KNOWN_PROVIDERS has no name no provider kind maps to.
    let mut known = KNOWN_PROVIDERS.to_vec();
    known.sort_unstable();
    seen.sort_unstable();
    assert_eq!(seen, known);
}

#[test]
fn the_recording_table_becomes_overrides_of_the_environment() {
    use crate::privacy::{PiiKind, PiiKinds};

    // Absent: nothing is overridden.
    assert!(load(BASE).unwrap().recording().is_empty());

    let text = format!(
        "{BASE}
[recording]
transcript = true
audio = false
redact = true
redact_kinds = [\"email\", \"phone\"]
retention_days = 14
"
    );
    let recording = load(&text).unwrap().recording();
    assert_eq!(recording.transcripts, Some(true));
    assert_eq!(recording.audio, Some(false));
    assert_eq!(recording.redact_pii, Some(true));
    assert_eq!(
        recording.redact_kinds,
        Some(PiiKinds::of([PiiKind::Email, PiiKind::Phone]))
    );
    assert_eq!(recording.retention, Some(Duration::from_secs(14 * 86_400)));
}

#[test]
fn bad_recording_values_are_reported_at_their_position() {
    for (line, key) in [
        ("retention_days = 0", "recording.retention_days"),
        ("retention_days = 4000", "recording.retention_days"),
        ("redact_kinds = []", "recording.redact_kinds"),
        (
            "redact = false
redact_kinds = [\"email\"]",
            "recording.redact_kinds",
        ),
    ] {
        let text = format!(
            "{BASE}
[recording]
{line}
"
        );
        let issue = only_issue(&text);
        assert_eq!(issue.key, key, "{line}");
        assert!(issue.position.is_some(), "{line}");
    }
    // A kind that does not exist and a key that does not exist are rejected by the reader.
    let text = format!(
        "{BASE}
[recording]
redact_kinds = [\"name\"]
"
    );
    assert!(load(&text).is_err());
    let text = format!(
        "{BASE}
[recording]
dir = \"/tmp\"
"
    );
    assert!(load(&text).is_err(), "the directory is environment-only");
}

// ---------------------------------------------------------------------------------------------
// openai_speech: a local TTS server, no key
// ---------------------------------------------------------------------------------------------

const SPEECH: &str = r#"
[agent]
name = "local"
language = "en"
system_prompt = "x"

[stt]
provider = "deepgram"
api_key_env = "DEEPGRAM_API_KEY"

[llm]
provider = "openai_compatible"

[llm.openai_compatible]
preset = "ollama"

[tts]
provider = "openai_speech"
voice = "af_heart"

[tts.openai_speech]
preset = "kokoro"
base_url = "http://192.168.1.20:8880/v1"
"#;

#[cfg(feature = "provider-openai-speech")]
fn speech_config(
    config: TtsProviderConfig,
) -> brisvox_providers::openai_speech::OpenAiSpeechConfig {
    match config {
        TtsProviderConfig::OpenAiSpeech(c) => c,
        #[allow(unreachable_patterns)]
        _ => panic!("expected the openai_speech configuration"),
    }
}

#[cfg(feature = "provider-openai-speech")]
#[test]
fn a_local_speech_server_needs_no_tts_key_and_maps_its_options() {
    let agent = load(SPEECH).unwrap();
    let (clock, cancel) = (
        brisvox_core::SystemClock::shared(),
        tokio_util::sync::CancellationToken::new(),
    );
    let c = speech_config(agent.tts(clock.clone(), cancel.clone()).unwrap());
    assert_eq!(c.base_url, "http://192.168.1.20:8880/v1");
    assert_eq!(c.model, "kokoro");
    assert_eq!(c.stream, Some(true));
    assert_eq!(c.warm_text, "Hi.", "the kokoro preset warms the model");
    assert_eq!(c.warm_interval, Duration::from_secs(60));
    assert!(c.api_key.is_none());
    assert!(
        agent
            .tts_secondary(clock.clone(), cancel.clone())
            .unwrap()
            .is_none()
    );

    // Overrides, and the optional key when the definition names one (https: it travels).
    let text = SPEECH
        .replace("[tts]\n", "[tts]\napi_key_env = \"SPEECH_KEY\"\n")
        .replace(
            "http://192.168.1.20:8880/v1\"",
            "https://tts.example.net/v1\"\nmodel = \"my-kokoro\"\nspeed = 1.25\nformat = \"wav\"\n\
         first_audio_timeout_ms = 30000\nwarm_text = \"\"\nwarm_interval_ms = 5000",
        );
    let mut env = env();
    env.insert("SPEECH_KEY".to_owned(), "speech-secret-0004".to_owned());
    let agent = load_with(&text, "agent.toml", Format::Toml, &env, &|_| true).unwrap();
    let c = speech_config(agent.tts(clock, cancel).unwrap());
    assert_eq!(c.model, "my-kokoro");
    assert_eq!(c.speed, Some(1.25));
    assert_eq!(c.format, brisvox_providers::openai_speech::AudioFormat::Wav);
    assert_eq!(c.first_audio_timeout, Duration::from_secs(30));
    assert!(
        c.warm_text.is_empty(),
        "an empty warm_text turns the warm-up off"
    );
    assert_eq!(c.warm_interval, Duration::from_secs(5));
    assert!(c.api_key.is_some());
    assert!(!format!("{c:?}").contains("speech-secret-0004"));

    // Chatterbox: WAV, and the TTS language goes in the request's `language` field.
    let text = SPEECH
        .replace("preset = \"kokoro\"", "preset = \"chatterbox\"")
        .replace("language = \"en\"", "language = \"tr\"");
    let agent = load(&text).unwrap();
    let c = speech_config(
        agent
            .tts(
                brisvox_core::SystemClock::shared(),
                tokio_util::sync::CancellationToken::new(),
            )
            .unwrap(),
    );
    assert_eq!(c.model, "chatterbox");
    assert_eq!(c.format, brisvox_providers::openai_speech::AudioFormat::Wav);
    assert_eq!(c.extra_body.get("language"), Some(&serde_json::json!("tr")));
}

#[test]
fn the_speech_table_is_checked() {
    let blames = |text: &str, key: &str, needle: &str| {
        let found = issues(text);
        assert!(
            found
                .iter()
                .any(|i| i.key == key && i.message.contains(needle)),
            "{key} / {needle}: {found:?}"
        );
    };
    // Missing table, missing base_url, generic without a model.
    let table =
        "[tts.openai_speech]\npreset = \"kokoro\"\nbase_url = \"http://192.168.1.20:8880/v1\"\n";
    blames(&SPEECH.replace(table, ""), "tts.openai_speech", "base_url");
    blames(
        &SPEECH.replace("base_url = \"http://192.168.1.20:8880/v1\"", ""),
        "tts.openai_speech",
        "base_url",
    );
    blames(
        &SPEECH.replace("preset = \"kokoro\"", "preset = \"generic\""),
        "tts.openai_speech",
        "model",
    );
    // A key over plain http to a remote host.
    blames(
        &SPEECH.replace("[tts]\n", "[tts]\napi_key_env = \"OPENAI_API_KEY\"\n"),
        "tts.openai_speech.base_url",
        "unencrypted",
    );
    // A warm-up text over 200 bytes is refused at load, at its own position.
    let long = SPEECH.replace(
        "base_url = \"http://192.168.1.20:8880/v1\"",
        &format!(
            "base_url = \"http://192.168.1.20:8880/v1\"\nwarm_text = \"{}\"",
            "x".repeat(201)
        ),
    );
    blames(&long, "tts.openai_speech.warm_text", "200 bytes");
    assert!(
        issues(&long)
            .iter()
            .any(|i| i.key == "tts.openai_speech.warm_text" && i.position.is_some())
    );
    assert!(load(&long.replace(&"x".repeat(201), &"x".repeat(200))).is_ok());
    // The protocol delivers 24 kHz, except through the speaches preset.
    let at_16k = SPEECH.replace("[tts]\n", "[tts]\nsample_rate = 16000\n");
    blames(&at_16k, "tts.sample_rate", "24000");
    assert!(load(&at_16k.replace("preset = \"kokoro\"", "preset = \"speaches\"")).is_ok());
    // The wrong scheme.
    let bad = SPEECH.replace("http://192.168.1.20:8880/v1", "ftp://x/v1");
    assert!(issues(&bad).iter().any(|i| i.message.contains("scheme")));
}

#[test]
fn every_other_tts_provider_still_needs_its_key() {
    let found = issues(&edit("api_key_env = \"CARTESIA_API_KEY\"\n", ""));
    assert!(
        found
            .iter()
            .any(|i| i.key == "tts.api_key_env" && i.message.contains("CARTESIA_API_KEY")),
        "{found:?}"
    );
}
