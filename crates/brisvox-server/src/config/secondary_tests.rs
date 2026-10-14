//! Tests of the `[stt|llm|tts].secondary` tables: mapping, keys from the environment,
//! what a secondary may not set, and that it is checked like a primary.

use std::collections::BTreeMap;

use brisvox_core::SystemClock;
use tokio_util::sync::CancellationToken;

use super::*;

/// A complete definition with a secondary in every stage: AssemblyAI, Anthropic and ElevenLabs.
const WITH_SECONDARIES: &str = r#"
[agent]
name = "support-en"
language = "en"
system_prompt = "You are a helpful agent."

[stt]
provider = "deepgram"
api_key_env = "DEEPGRAM_API_KEY"

[stt.secondary]
provider = "assemblyai"
api_key_env = "ASSEMBLYAI_API_KEY"

[stt.secondary.assemblyai]
speech_model = "universal-3-6-pro"

[llm]
provider = "openai"
api_key_env = "OPENAI_API_KEY"

[llm.secondary]
provider = "anthropic"
api_key_env = "ANTHROPIC_API_KEY"

[tts]
provider = "cartesia"
api_key_env = "CARTESIA_API_KEY"
voice = "voice-123"

[tts.secondary]
provider = "elevenlabs"
api_key_env = "ELEVENLABS_API_KEY"
voice = "voice-b"
"#;

/// The same definition without secondaries.
const BASE: &str = r#"
[agent]
name = "support-en"
language = "en"
system_prompt = "You are a helpful agent."

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

fn env() -> BTreeMap<String, String> {
    [
        ("DEEPGRAM_API_KEY", STT_SECRET),
        ("OPENAI_API_KEY", LLM_SECRET),
        ("CARTESIA_API_KEY", TTS_SECRET),
        ("ANTHROPIC_API_KEY", LLM_SECRET),
        ("ELEVENLABS_API_KEY", TTS_SECRET),
        ("ASSEMBLYAI_API_KEY", STT_SECRET),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

fn load_in(text: &str, env: &BTreeMap<String, String>) -> Result<AgentConfig, ConfigError> {
    load_with(text, "agent.toml", Format::Toml, env, &|_| true)
}

fn load(text: &str) -> Result<AgentConfig, ConfigError> {
    load_in(text, &env())
}

fn issues(text: &str) -> Vec<Issue> {
    match load(text) {
        Ok(_) => panic!("expected an error for:\n{text}"),
        Err(e) => e.issues,
    }
}

fn only_issue(text: &str) -> Issue {
    let found = issues(text);
    assert_eq!(found.len(), 1, "{found:#?}");
    found.into_iter().next().unwrap()
}

fn pos(text: &str, needle: &str) -> (usize, usize) {
    error::line_column(text, text.find(needle).unwrap())
}

#[test]
fn secondaries_load_and_map_to_their_own_adapters_and_keys() {
    let agent = load(WITH_SECONDARIES).unwrap();
    assert!(matches!(agent.stt(), Ok(SttProviderConfig::Deepgram(_))));
    let Ok(Some(SttProviderConfig::AssemblyAi(stt))) = agent.stt_secondary() else {
        panic!("an AssemblyAI secondary expected");
    };
    assert_eq!(stt.api_key.expose(), STT_SECRET);
    assert_eq!(
        stt.language.as_deref(),
        Some("en"),
        "the primary's language"
    );
    let Ok(Some(LlmProviderConfig::Anthropic(llm))) = agent.llm_secondary() else {
        panic!("an Anthropic secondary expected");
    };
    assert_eq!(llm.api_key.expose(), LLM_SECRET);
    let Ok(Some(TtsProviderConfig::ElevenLabs(tts))) =
        agent.tts_secondary(SystemClock::shared(), CancellationToken::new())
    else {
        panic!("an ElevenLabs secondary expected");
    };
    assert_eq!(tts.api_key.expose(), TTS_SECRET);
    // Each provider speaks its own voice; the engine's voice stays the primary's.
    assert_eq!(agent.tts_secondary_voice().as_deref(), Some("voice-b"));
    assert_eq!(&*agent.voice(), "voice-123");
    // A key never shows.
    let shown = format!("{agent:?}");
    for secret in [STT_SECRET, LLM_SECRET, TTS_SECRET] {
        assert!(!shown.contains(secret), "{shown}");
    }
}

#[test]
fn a_definition_without_secondaries_has_none() {
    let agent = load(BASE).unwrap();
    assert!(matches!(agent.stt_secondary(), Ok(None)));
    assert!(matches!(agent.llm_secondary(), Ok(None)));
    assert!(matches!(
        agent.tts_secondary(SystemClock::shared(), CancellationToken::new()),
        Ok(None)
    ));
    assert!(agent.tts_secondary_voice().is_none());
}

#[test]
fn an_openai_compatible_secondary_needs_no_key() {
    let text = format!(
        "{BASE}\n[llm.secondary]\nprovider = \"openai_compatible\"\n\n\
         [llm.secondary.openai_compatible]\npreset = \"ollama\"\n"
    );
    let agent = load(&text).unwrap();
    assert!(matches!(
        agent.llm_secondary(),
        Ok(Some(LlmProviderConfig::OpenAiCompatible(_)))
    ));
}

#[test]
fn a_secondarys_key_is_read_from_the_environment_like_the_primarys() {
    let mut env = env();
    env.remove("ELEVENLABS_API_KEY");
    env.insert("ASSEMBLYAI_API_KEY".to_owned(), "  ".to_owned());
    let e = load_in(WITH_SECONDARIES, &env).unwrap_err();
    let keys: Vec<_> = e.issues.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(
        keys,
        ["stt.secondary.api_key_env", "tts.secondary.api_key_env"]
    );
    assert_eq!(
        e.issues[1].position,
        Some(pos(WITH_SECONDARIES, "\"ELEVENLABS_API_KEY\""))
    );
    assert!(e.issues[0].message.contains("is set but empty"), "{e}");
    assert!(e.issues[1].message.contains("is not set"), "{e}");
}

#[test]
fn settings_of_the_call_are_not_allowed_in_a_secondary() {
    for (from, to, key) in [
        (
            "api_key_env = \"ASSEMBLYAI_API_KEY\"",
            "api_key_env = \"ASSEMBLYAI_API_KEY\"\nlanguage = \"en\"",
            "stt.secondary.language",
        ),
        (
            "api_key_env = \"ANTHROPIC_API_KEY\"",
            "api_key_env = \"ANTHROPIC_API_KEY\"\nmax_tool_rounds = 3",
            "llm.secondary.max_tool_rounds",
        ),
        (
            "api_key_env = \"ANTHROPIC_API_KEY\"",
            "api_key_env = \"ANTHROPIC_API_KEY\"\ncontext_window_tokens = 8000",
            "llm.secondary.context_window_tokens",
        ),
        (
            "api_key_env = \"ELEVENLABS_API_KEY\"",
            "api_key_env = \"ELEVENLABS_API_KEY\"\nsample_rate = 16000",
            "tts.secondary.sample_rate",
        ),
        (
            "api_key_env = \"ELEVENLABS_API_KEY\"",
            "api_key_env = \"ELEVENLABS_API_KEY\"\nlanguage = \"en\"",
            "tts.secondary.language",
        ),
    ] {
        let text = WITH_SECONDARIES.replacen(from, to, 1);
        let issue = only_issue(&text);
        assert_eq!(issue.key, key, "{issue}");
        assert!(issue.position.is_some(), "{issue}");
        assert!(
            issue.message.contains("is not allowed in") && issue.message.contains("secondary"),
            "{issue}"
        );
    }
}

#[test]
fn a_secondary_cannot_have_a_secondary_of_its_own() {
    let text = format!(
        "{WITH_SECONDARIES}\n[tts.secondary.secondary]\nprovider = \"cartesia\"\n\
         api_key_env = \"CARTESIA_API_KEY\"\nvoice = \"v\"\n"
    );
    let issue = only_issue(&text);
    assert_eq!(issue.key, "tts.secondary.secondary");
    assert_eq!(
        issue.position,
        Some(pos(&text, "[tts.secondary.secondary]"))
    );
    assert!(issue.message.contains("secondary of its own"), "{issue}");
}

#[test]
fn a_secondary_is_checked_like_a_primary() {
    // An unknown key.
    let text = WITH_SECONDARIES.replacen(
        "speech_model = \"universal-3-6-pro\"",
        "speech_model = \"universal-3-6-pro\"\nmodle = \"x\"",
        1,
    );
    let issue = only_issue(&text);
    assert_eq!(issue.key, "stt.secondary.assemblyai.modle");
    assert_eq!(issue.position, Some(pos(&text, "modle")));

    // An options table for another provider.
    let text = format!("{WITH_SECONDARIES}\n[stt.secondary.gladia]\n");
    let issue = only_issue(&text);
    assert_eq!(issue.key, "stt.secondary.gladia");
    assert!(
        issue
            .message
            .contains("but stt.secondary.provider is \"assemblyai\""),
        "{issue}"
    );

    // A model that cannot speak the call's language, blamed on the secondary's own key.
    let text = WITH_SECONDARIES
        .replacen("language = \"en\"", "language = \"tr\"", 1)
        .replacen("universal-3-6-pro", "universal-streaming", 1);
    let issue = only_issue(&text);
    assert_eq!(issue.key, "stt.secondary.assemblyai.speech_model");
    assert_eq!(issue.position, Some(pos(&text, "\"universal-streaming\"")));

    // A required LLM key.
    let text = WITH_SECONDARIES.replacen("api_key_env = \"ANTHROPIC_API_KEY\"", "", 1);
    let issue = only_issue(&text);
    assert_eq!(issue.key, "llm.secondary.api_key_env");

    // A TTS secondary that cannot speak the call's language.
    let text = WITH_SECONDARIES
        .replacen(
            "provider = \"elevenlabs\"",
            "provider = \"deepgram_aura\"",
            1,
        )
        .replacen("language = \"en\"", "language = \"tr\"", 1);
    let found = load(&text).unwrap_err().issues;
    assert!(
        found
            .iter()
            .any(|i| i.message.contains("Deepgram Aura has no voice")),
        "{found:#?}"
    );
}

#[test]
fn a_secondary_delivers_the_primarys_rate() {
    // Aura delivers 8, 16, 24 and 48 kHz: a 22.05 kHz call rate is the primary's choice, and
    // the secondary cannot meet it.
    let text = WITH_SECONDARIES
        .replacen(
            "voice = \"voice-123\"",
            "voice = \"voice-123\"\nsample_rate = 22050",
            1,
        )
        .replacen(
            "provider = \"elevenlabs\"",
            "provider = \"deepgram_aura\"",
            1,
        )
        .replacen("voice-b", "aura-2-thalia-en", 1);
    let issue = only_issue(&text);
    assert_eq!(issue.key, "tts.sample_rate");
    assert!(
        issue.message.contains("the secondary Deepgram Aura"),
        "{issue}"
    );
}
