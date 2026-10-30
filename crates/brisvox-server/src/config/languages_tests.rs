//! Tests of the `[languages]` table: the engine settings it maps to, the multilingual mode of the
//! STT providers, and the checks that every listed language can be spoken and heard by the
//! chosen providers (docs/providers/language-switching.md).

use std::collections::BTreeMap;

use super::*;

/// A Turkish/English agent: Deepgram Nova-3 (STT) and ElevenLabs Flash v2.5 (TTS).
const MULTI: &str = r#"
[agent]
name = "destek"
language = "tr"
system_prompt = "You are a helpful agent."

[languages]
default = "tr"
switch_after = 3
min_words = 3

[[languages.speak]]
code = "tr"
voice = "tr-voice"

[[languages.speak]]
code = "en"
voice = "en-voice"
tts_model = "eleven_v3"

[stt]
provider = "deepgram"
api_key_env = "DEEPGRAM_API_KEY"

[llm]
provider = "openai"
api_key_env = "OPENAI_API_KEY"

[tts]
provider = "elevenlabs"
api_key_env = "ELEVENLABS_API_KEY"
voice = "tr-voice"
"#;

fn env() -> BTreeMap<String, String> {
    [
        ("DEEPGRAM_API_KEY", "dg-secret-value-0001"),
        ("ASSEMBLYAI_API_KEY", "aai-secret-value-0001"),
        ("GLADIA_API_KEY", "gl-secret-value-0001"),
        ("OPENAI_API_KEY", "sk-secret-value-0002"),
        ("ELEVENLABS_API_KEY", "el-secret-value-0003"),
        ("CARTESIA_API_KEY", "ca-secret-value-0003"),
        ("DEEPGRAM_AURA_KEY", "au-secret-value-0003"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

fn load(text: &str) -> Result<AgentConfig, ConfigError> {
    load_with(text, "agent.toml", Format::Toml, &env(), &|_| true)
}

fn issues(text: &str) -> Vec<Issue> {
    match load(text) {
        Ok(_) => panic!("expected an error for:\n{text}"),
        Err(e) => e.issues,
    }
}

fn pos(text: &str, needle: &str) -> (usize, usize) {
    error::line_column(text, text.find(needle).unwrap())
}

#[test]
fn the_languages_map_to_the_engine_settings() {
    let agent = load(MULTI).unwrap();
    let settings = agent.languages().expect("languages are configured");
    assert_eq!(settings.switch_after, 3);
    assert_eq!(settings.min_words, 3);
    assert!((settings.switch_confidence - 0.9).abs() < f32::EPSILON);
    let profiles = settings.profiles();
    assert_eq!(profiles.len(), 2);
    assert_eq!(
        (
            &*profiles[0].code,
            &*profiles[0].voice,
            profiles[0].tts_model.as_deref()
        ),
        ("tr", "tr-voice", None)
    );
    assert_eq!(
        (
            &*profiles[1].code,
            &*profiles[1].voice,
            profiles[1].tts_model.as_deref()
        ),
        ("en", "en-voice", Some("eleven_v3"))
    );
    assert_eq!(settings.default_profile().map(|p| &*p.code), Some("tr"));
}

#[test]
fn a_single_language_agent_has_no_languages() {
    let text = MULTI
        .replace("[languages]\ndefault = \"tr\"\nswitch_after = 3\nmin_words = 3\n", "")
        .replace(
            "[[languages.speak]]\ncode = \"tr\"\nvoice = \"tr-voice\"\n\n[[languages.speak]]\ncode = \"en\"\nvoice = \"en-voice\"\ntts_model = \"eleven_v3\"\n",
            "",
        );
    assert!(load(&text).unwrap().languages().is_none());
}

#[test]
fn the_default_is_the_agent_language_unless_named() {
    let text = MULTI.replace("default = \"tr\"\n", "");
    let settings = load(&text).unwrap().languages().unwrap();
    assert_eq!(settings.default_profile().map(|p| &*p.code), Some("tr"));
    let text = MULTI.replace("default = \"tr\"", "default = \"en\"");
    let settings = load(&text).unwrap().languages().unwrap();
    assert_eq!(settings.default_profile().map(|p| &*p.code), Some("en"));
}

#[test]
fn deepgram_is_asked_for_language_multi_with_the_languages_as_hints() {
    let agent = load(MULTI).unwrap();
    let Ok(SttProviderConfig::Deepgram(stt)) = agent.stt() else {
        panic!("a Deepgram STT expected");
    };
    assert_eq!(stt.language.as_deref(), Some("multi"));
    assert_eq!(stt.language_hints, ["tr", "en"]);
}

#[test]
fn assemblyai_and_gladia_get_their_multilingual_modes() {
    let assembly = MULTI.replace(
        "provider = \"deepgram\"\napi_key_env = \"DEEPGRAM_API_KEY\"",
        "provider = \"assemblyai\"\napi_key_env = \"ASSEMBLYAI_API_KEY\"",
    );
    let Ok(SttProviderConfig::AssemblyAi(stt)) = load(&assembly).unwrap().stt() else {
        panic!("an AssemblyAI STT expected");
    };
    assert!(stt.language.is_none() && stt.language_detection);
    assert_eq!(stt.language_codes, ["tr", "en"]);
    let gladia = MULTI.replace(
        "provider = \"deepgram\"\napi_key_env = \"DEEPGRAM_API_KEY\"",
        "provider = \"gladia\"\napi_key_env = \"GLADIA_API_KEY\"",
    );
    let Ok(SttProviderConfig::Gladia(stt)) = load(&gladia).unwrap().stt() else {
        panic!("a Gladia STT expected");
    };
    assert!(stt.language.is_none() && stt.code_switching);
    assert_eq!(stt.languages, ["tr", "en"]);
}

#[test]
fn a_session_builder_gets_the_languages() {
    // `configure` hands them to the engine; the tracker starts in the default language.
    let agent = load(MULTI).unwrap();
    assert_eq!(&*agent.voice(), "tr-voice");
}

#[test]
fn one_language_is_not_a_choice() {
    let text = MULTI.replace(
        "[[languages.speak]]\ncode = \"en\"\nvoice = \"en-voice\"\ntts_model = \"eleven_v3\"\n",
        "",
    );
    let found = issues(&text);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0].key, "languages.speak");
    assert!(found[0].message.contains("at least two"), "{found:#?}");
}

#[test]
fn unsupported_duplicate_and_unlisted_default_languages_are_refused_with_positions() {
    let german = MULTI.replace("code = \"en\"", "code = \"de\"");
    let found = issues(&german);
    assert!(
        found
            .iter()
            .any(|i| i.key == "languages.speak[1].code"
                && i.position == Some(pos(&german, "\"de\""))),
        "{found:#?}"
    );
    let twice = MULTI.replace("code = \"en\"", "code = \"tr-TR\"");
    assert!(
        issues(&twice)
            .iter()
            .any(|i| i.key == "languages.speak[1].code" && i.message.contains("twice"))
    );
    // The call starts in `agent.language`, which must be listed.
    let unlisted = MULTI
        .replace("default = \"tr\"\n", "")
        .replace("language = \"tr\"", "language = \"en\"")
        .replace("code = \"en\"", "code = \"tr-TR\"");
    assert!(issues(&unlisted).iter().any(|i| i.key == "agent.language"));
}

#[test]
fn a_tts_provider_that_cannot_speak_a_listed_language_is_refused() {
    // Deepgram Aura has no Turkish voice.
    let aura = MULTI
        .replace("tts_model = \"eleven_v3\"\n", "")
        .replace(
            "provider = \"elevenlabs\"\napi_key_env = \"ELEVENLABS_API_KEY\"",
            "provider = \"deepgram_aura\"\napi_key_env = \"DEEPGRAM_AURA_KEY\"",
        )
        .replace("voice = \"en-voice\"", "voice = \"aura-2-thalia-en\"")
        .replace("voice = \"tr-voice\"", "voice = \"aura-2-thalia-en\"");
    let found = issues(&aura);
    assert!(
        found.iter().any(|i| i.key == "languages.speak[0].code"
            && i.message.contains("Deepgram Aura has no voice for `tr`")
            && i.position == Some(pos(&aura, "\"tr\"\nvoice"))),
        "{found:#?}"
    );
    // ElevenLabs Flash v2 is English only: fine for the English entry, not for the Turkish one.
    let flash = MULTI.replace(
        "code = \"tr\"
voice = \"tr-voice\"
",
        "code = \"tr\"
voice = \"tr-voice\"
tts_model = \"eleven_flash_v2\"
",
    );
    let found = issues(&flash);
    assert!(
        found
            .iter()
            .any(|i| i.key == "languages.speak[0].tts_model" && i.message.contains("English only")),
        "{found:#?}"
    );
    assert!(load(&MULTI.replace("eleven_v3", "eleven_flash_v2")).is_ok());
    // Cartesia sonic-2 lists no Turkish.
    let cartesia = MULTI
        .replace(
            "provider = \"elevenlabs\"
api_key_env = \"ELEVENLABS_API_KEY\"",
            "provider = \"cartesia\"
api_key_env = \"CARTESIA_API_KEY\"",
        )
        .replace(
            "tts_model = \"eleven_v3\"
",
            "",
        );
    let old = format!(
        "{cartesia}
[tts.cartesia]
model = \"sonic-2\"
"
    );
    let found = issues(&old);
    assert!(
        found
            .iter()
            .any(|i| i.key == "tts.cartesia.model" && i.message.contains("Turkish")),
        "{found:#?}"
    );
    // Cartesia sonic-3 does.
    let ok = format!(
        "{cartesia}
[tts.cartesia]
model = \"sonic-3.6\"
"
    );
    assert!(load(&ok).is_ok(), "{:#?}", load(&ok).err());
}

#[test]
fn an_stt_that_hears_one_language_is_refused() {
    let sherpa = MULTI.replace(
        "provider = \"deepgram\"\napi_key_env = \"DEEPGRAM_API_KEY\"",
        "provider = \"sherpa_onnx\"",
    );
    let found = issues(&sherpa);
    assert!(
        found
            .iter()
            .any(|i| i.key == "stt.provider" && i.message.contains("sherpa_onnx")),
        "{found:#?}"
    );
    let flux = format!("{MULTI}\n[stt.deepgram]\nmodel = \"flux-general-en\"\n");
    let found = issues(&flux);
    assert!(
        found.iter().any(|i| i.key == "stt.deepgram.model"),
        "{found:#?}"
    );
    let fixed = MULTI.replace(
        "api_key_env = \"DEEPGRAM_API_KEY\"",
        "api_key_env = \"DEEPGRAM_API_KEY\"\nlanguage = \"tr\"",
    );
    let found = issues(&fixed);
    assert!(
        found
            .iter()
            .any(|i| i.key == "stt.language" && i.message.contains("[languages]")),
        "{found:#?}"
    );
}

#[test]
fn the_switching_numbers_are_range_checked() {
    for (from, to) in [
        ("switch_after = 3", "switch_after = 0"),
        ("switch_after = 3", "switch_after = 11"),
        ("min_words = 3", "min_words = 0"),
        ("switch_after = 3", "switch_confidence = 1.5"),
    ] {
        let text = MULTI.replace(from, to);
        assert!(load(&text).is_err(), "{to}");
    }
}

#[test]
fn only_the_deepgram_models_with_a_turkish_multi_mode_are_accepted() {
    for model in ["nova-3-medical", "nova-2-medical", "flux-general-multi"] {
        let text = format!("{MULTI}\n[stt.deepgram]\nmodel = \"{model}\"\n");
        assert!(
            issues(&text).iter().any(|i| i.key == "stt.deepgram.model"),
            "{model}"
        );
    }
    for model in ["nova-3", "nova-2"] {
        let text = format!("{MULTI}\n[stt.deepgram]\nmodel = \"{model}\"\n");
        assert!(load(&text).is_ok(), "{model}");
    }
}

/// The failover provider can speak another voice per language.
const WITH_SECONDARY: &str = r#"
[tts.secondary]
provider = "cartesia"
api_key_env = "CARTESIA_API_KEY"
voice = "backup-voice"
"#;

#[test]
fn the_secondary_tts_voice_can_differ_per_language() {
    let text = MULTI.replace(
        "voice = \"en-voice\"\n",
        "voice = \"en-voice\"\nsecondary_voice = \"backup-en\"\n",
    );
    // Without a secondary the key has nothing to do.
    let found = issues(&text);
    assert!(
        found
            .iter()
            .any(|i| i.key == "languages.speak[1].secondary_voice"),
        "{found:#?}"
    );
    let agent = load(&format!("{text}{WITH_SECONDARY}")).unwrap();
    assert_eq!(
        agent.tts_secondary_language_voices(),
        [(Arc::from("en"), Arc::from("backup-en"))]
    );
    assert_eq!(agent.tts_secondary_voice().as_deref(), Some("backup-voice"));
}
