//! Tests of the self-hosted STT providers in an agent definition: `sherpa_onnx` and
//! `openai_realtime_stt`. Keyless providers, the `[stt.<provider>]` tables, their mapping onto
//! the adapter configurations, and the checks that keep a key off the wire in plain text.

use std::collections::BTreeMap;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_providers::openai_realtime_stt::Dialect;

use super::*;

const AGENT: &str = r#"
[agent]
name = "local"
language = "tr"
system_prompt = "You are a helpful agent."

[llm]
provider = "openai_compatible"

[llm.openai_compatible]
preset = "ollama"
model = "llama3.2"

[tts]
provider = "cartesia"
api_key_env = "CARTESIA_API_KEY"
voice = "voice-123"
"#;

const TTS_SECRET: &str = "ca-secret-value-0003";
const TOKEN: &str = "proxy-token-0005";

fn env() -> BTreeMap<String, String> {
    [
        ("CARTESIA_API_KEY", TTS_SECRET),
        ("STT_TOKEN", TOKEN),
        ("OPENAI_API_KEY", TOKEN),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

/// `AGENT` with the `[stt]` text `stt` put in front of the `[llm]` table.
fn with_stt(stt: &str) -> String {
    AGENT.replacen("[llm]", &format!("{stt}\n[llm]"), 1)
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

fn only_issue(text: &str) -> Issue {
    let found = issues(text);
    assert_eq!(found.len(), 1, "{found:#?}");
    found.into_iter().next().unwrap()
}

#[test]
fn sherpa_onnx_needs_no_key_and_maps_its_options() {
    let agent = load(&with_stt(
        r#"
[stt]
provider = "sherpa_onnx"

[stt.sherpa_onnx]
base_url = "ws://gpu-box.lan:6006"
path = "/asr"
sample_rate = 16000
finalize_timeout_ms = 800
finalize_padding_ms = 450
"#,
    ))
    .unwrap();
    let Ok(SttProviderConfig::SherpaOnnx(c)) = agent.stt() else {
        panic!("a sherpa-onnx configuration expected");
    };
    assert_eq!(c.base_url, "ws://gpu-box.lan:6006");
    assert_eq!(c.path, "/asr");
    assert_eq!(c.sample_rate, SampleRate::Hz16000);
    assert_eq!(c.finalize_timeout, Duration::from_millis(800));
    assert_eq!(c.finalize_padding, Duration::from_millis(450));
    assert!(c.bearer.is_none());
    // Without a table the adapter's defaults stand.
    let agent = load(&with_stt("[stt]\nprovider = \"sherpa_onnx\"\n")).unwrap();
    let Ok(SttProviderConfig::SherpaOnnx(c)) = agent.stt() else {
        panic!("a sherpa-onnx configuration expected");
    };
    assert_eq!(c.base_url, "ws://127.0.0.1:6006");
    assert_eq!(c.finalize_padding, Duration::from_millis(300));
}

#[test]
fn a_token_for_a_proxy_is_read_from_the_environment_and_never_travels_in_plain_text() {
    let agent = load(&with_stt(
        "[stt]\nprovider = \"sherpa_onnx\"\napi_key_env = \"STT_TOKEN\"\n[stt.sherpa_onnx]\nbase_url = \"wss://stt.example.com\"\n",
    ))
    .unwrap();
    let Ok(SttProviderConfig::SherpaOnnx(c)) = agent.stt() else {
        panic!("a sherpa-onnx configuration expected");
    };
    assert_eq!(c.bearer.as_ref().map(|t| t.expose().as_str()), Some(TOKEN));
    // The Debug output of the agent never shows it.
    assert!(!format!("{agent:?}").contains(TOKEN));
    // A token over plain ws:// to another machine is refused at load time; loopback is fine.
    let issue = only_issue(&with_stt(
        "[stt]\nprovider = \"sherpa_onnx\"\napi_key_env = \"STT_TOKEN\"\n[stt.sherpa_onnx]\nbase_url = \"ws://10.0.0.7:6006\"\n",
    ));
    assert_eq!(issue.key, "stt.sherpa_onnx.base_url");
    assert!(issue.message.contains("unencrypted"), "{}", issue.message);
    assert!(
        load(&with_stt(
            "[stt]\nprovider = \"sherpa_onnx\"\napi_key_env = \"STT_TOKEN\"\n[stt.sherpa_onnx]\nbase_url = \"ws://localhost:6006\"\n",
        ))
        .is_ok()
    );
}

#[test]
fn the_base_url_is_an_address_not_a_path_and_a_wrong_scheme_is_refused_with_its_position() {
    for bad in [
        "http://x",
        "ws://host/path",
        "ws://user@host",
        "ws://",
        "host:6006",
    ] {
        let text = with_stt(&format!(
            "[stt]\nprovider = \"sherpa_onnx\"\n[stt.sherpa_onnx]\nbase_url = \"{bad}\"\n"
        ));
        let found = issues(&text);
        assert_eq!(found.len(), 1, "{bad}: {found:#?}");
        assert_eq!(found[0].key, "stt.sherpa_onnx.base_url", "{bad}");
        assert!(found[0].position.is_some(), "{bad}");
    }
}

#[test]
fn the_openai_dialect_needs_a_key_the_local_dialects_do_not() {
    let issue = only_issue(&with_stt("[stt]\nprovider = \"openai_realtime_stt\"\n"));
    assert_eq!(issue.key, "stt.api_key_env");
    assert!(
        issue.message.contains("OPENAI_API_KEY"),
        "{}",
        issue.message
    );

    let agent = load(&with_stt(
        "[stt]\nprovider = \"openai_realtime_stt\"\napi_key_env = \"OPENAI_API_KEY\"\n[stt.openai_realtime_stt]\nprompt = \"ürün adları\"\n",
    ))
    .unwrap();
    let Ok(SttProviderConfig::OpenAiRealtimeStt(c)) = agent.stt() else {
        panic!("an openai_realtime_stt configuration expected");
    };
    assert_eq!(c.dialect, Dialect::OpenAi);
    assert_eq!(c.prompt.as_deref(), Some("ürün adları"));
    assert_eq!(c.api_key.as_ref().map(|k| k.expose().as_str()), Some(TOKEN));

    let agent = load(&with_stt(
        r#"
[stt]
provider = "openai_realtime_stt"

[stt.openai_realtime_stt]
dialect = "nim"
base_url = "ws://gpu-box.lan:9000"
punctuation = false
delta_replaces = true
min_commit_ms = 0
finalize_timeout_ms = 1500
sample_rate = 16000
"#,
    ))
    .unwrap();
    let Ok(SttProviderConfig::OpenAiRealtimeStt(c)) = agent.stt() else {
        panic!("an openai_realtime_stt configuration expected");
    };
    assert_eq!(c.dialect, Dialect::Nim);
    assert!(c.api_key.is_none());
    assert_eq!(c.base_url.as_deref(), Some("ws://gpu-box.lan:9000"));
    assert!(!c.punctuation);
    assert_eq!(c.delta_replaces, Some(true));
    assert_eq!(c.min_commit, Some(Duration::ZERO));
    assert_eq!(c.finalize_timeout, Duration::from_millis(1500));
    assert_eq!(c.sample_rate, Some(SampleRate::Hz16000));
}

#[test]
fn speaches_and_vllm_need_a_model() {
    for dialect in ["speaches", "vllm"] {
        let text = with_stt(&format!(
            "[stt]\nprovider = \"openai_realtime_stt\"\n[stt.openai_realtime_stt]\ndialect = \"{dialect}\"\n"
        ));
        let issue = only_issue(&text);
        assert_eq!(issue.key, "stt.openai_realtime_stt.model", "{dialect}");
        let ok = with_stt(&format!(
            "[stt]\nprovider = \"openai_realtime_stt\"\n[stt.openai_realtime_stt]\ndialect = \"{dialect}\"\nmodel = \"Systran/faster-whisper-small\"\n"
        ));
        let agent = load(&ok).unwrap();
        let Ok(SttProviderConfig::OpenAiRealtimeStt(c)) = agent.stt() else {
            panic!("an openai_realtime_stt configuration expected");
        };
        assert_eq!(c.model.as_deref(), Some("Systran/faster-whisper-small"));
    }
}

#[test]
fn an_unknown_dialect_is_a_read_error_at_its_position() {
    let text = with_stt(
        "[stt]\nprovider = \"openai_realtime_stt\"\n[stt.openai_realtime_stt]\ndialect = \"whisper\"\n",
    );
    let found = issues(&text);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert!(found[0].position.is_some());
}

#[test]
fn a_hosted_provider_still_needs_its_key() {
    let issue = only_issue(&with_stt("[stt]\nprovider = \"deepgram\"\n"));
    assert_eq!(issue.key, "stt.api_key_env");
    assert!(
        issue.message.contains("DEEPGRAM_API_KEY"),
        "{}",
        issue.message
    );
}

#[test]
fn an_options_table_for_another_provider_is_an_error() {
    let issue = only_issue(&with_stt(
        "[stt]\nprovider = \"sherpa_onnx\"\n[stt.openai_realtime_stt]\ndialect = \"nim\"\n",
    ));
    assert_eq!(issue.key, "stt.openai_realtime_stt");
}

#[test]
fn a_local_secondary_needs_no_key() {
    let text = with_stt(
        r#"
[stt]
provider = "sherpa_onnx"

[stt.secondary]
provider = "openai_realtime_stt"

[stt.secondary.openai_realtime_stt]
dialect = "nim"
"#,
    );
    let agent = load(&text).unwrap();
    assert!(matches!(
        agent.stt_secondary(),
        Ok(Some(SttProviderConfig::OpenAiRealtimeStt(_)))
    ));
}

#[test]
fn the_local_examples_load() {
    // The `[tts]` of the local examples names the OpenAI speech provider: the test
    // swaps it for a provider of this build, the rest of each file is read as written.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/agents/local");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let (head, _) = text.split_once("\n[tts]").expect("a [tts] table");
        let text = format!(
            "{head}\n[tts]\nprovider = \"cartesia\"\napi_key_env = \"CARTESIA_API_KEY\"\nvoice = \"v\"\n"
        );
        let agent = load(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(
            matches!(
                agent.stt().unwrap(),
                SttProviderConfig::SherpaOnnx(_) | SttProviderConfig::OpenAiRealtimeStt(_)
            ),
            "{}",
            path.display()
        );
        agent.llm().unwrap();
        seen += 1;
    }
    assert!(
        seen >= 2,
        "expected the local examples in {}",
        dir.display()
    );
}

// ---------------------------------------------------------------------------------------------
// Regressions found in review
// ---------------------------------------------------------------------------------------------

/// S2: a bracketed IPv6 loopback is loopback, with or without a port, even with a key.
#[test]
fn a_key_may_go_to_an_ipv6_loopback_in_plain_ws() {
    for url in [
        "ws://[::1]",
        "ws://[::1]:6006",
        "ws://127.0.0.1",
        "ws://127.0.0.2:6006",
        "ws://LOCALHOST:6006",
    ] {
        let text = with_stt(&format!(
            "[stt]\nprovider = \"sherpa_onnx\"\napi_key_env = \"STT_TOKEN\"\n[stt.sherpa_onnx]\nbase_url = \"{url}\"\n"
        ));
        assert!(load(&text).is_ok(), "{url}: {:?}", load(&text).err());
        let text = with_stt(&format!(
            "[stt]\nprovider = \"openai_realtime_stt\"\napi_key_env = \"OPENAI_API_KEY\"\n[stt.openai_realtime_stt]\nbase_url = \"{url}\"\n"
        ));
        assert!(load(&text).is_ok(), "{url}: {:?}", load(&text).err());
    }
    // A routable IPv6 address (and a name that merely starts like a loopback) is still refused.
    for url in [
        "ws://[2001:db8::1]:6006",
        "ws://[fe80::1]",
        "ws://localhost.evil.com",
    ] {
        let text = with_stt(&format!(
            "[stt]\nprovider = \"sherpa_onnx\"\napi_key_env = \"STT_TOKEN\"\n[stt.sherpa_onnx]\nbase_url = \"{url}\"\n"
        ));
        let issue = only_issue(&text);
        assert_eq!(issue.key, "stt.sherpa_onnx.base_url", "{url}");
        assert!(issue.message.contains("unencrypted"), "{url}");
    }
}

/// S1: values the adapter would refuse at connect are refused while the file is read.
#[test]
fn local_stt_paths_and_models_the_adapter_would_refuse_fail_at_load() {
    for bad in ["no-slash", "/a b", "/a?x=1", "/a#f", "/ü", "/a:b"] {
        let text = with_stt(&format!(
            "[stt]\nprovider = \"sherpa_onnx\"\n[stt.sherpa_onnx]\npath = \"{bad}\"\n"
        ));
        let found = issues(&text);
        assert_eq!(found.len(), 1, "{bad}: {found:#?}");
        assert_eq!(found[0].key, "stt.sherpa_onnx.path", "{bad}");
        assert!(found[0].position.is_some(), "{bad}");
    }
    for ok in ["/", "/asr", "/v1/stream", "/asr/"] {
        let text = with_stt(&format!(
            "[stt]\nprovider = \"sherpa_onnx\"\n[stt.sherpa_onnx]\npath = \"{ok}\"\n"
        ));
        assert!(load(&text).is_ok(), "{ok}: {:?}", load(&text).err());
    }

    for bad in ["m?x=1", "m#f", "a//b", "/m", "m/", "mü", "a:"] {
        let text = with_stt(&format!(
            "[stt]\nprovider = \"openai_realtime_stt\"\napi_key_env = \"OPENAI_API_KEY\"\n[stt.openai_realtime_stt]\nmodel = \"{bad}\"\n"
        ));
        let found = issues(&text);
        assert_eq!(found.len(), 1, "{bad}: {found:#?}");
        assert_eq!(found[0].key, "stt.openai_realtime_stt.model", "{bad}");
        assert!(found[0].position.is_some(), "{bad}");
    }
    for ok in [
        "gpt-live-transcribe",
        "Systran/faster-whisper-small",
        "org/m:q4",
    ] {
        let text = with_stt(&format!(
            "[stt]\nprovider = \"openai_realtime_stt\"\napi_key_env = \"OPENAI_API_KEY\"\n[stt.openai_realtime_stt]\nmodel = \"{ok}\"\n"
        ));
        assert!(load(&text).is_ok(), "{ok}: {:?}", load(&text).err());
    }

    for bad in ["v1/realtime", "/a//b", "/a#f"] {
        let text = with_stt(&format!(
            "[stt]\nprovider = \"openai_realtime_stt\"\napi_key_env = \"OPENAI_API_KEY\"\n[stt.openai_realtime_stt]\npath_and_query = \"{bad}\"\n"
        ));
        let found = issues(&text);
        assert_eq!(found.len(), 1, "{bad}: {found:#?}");
        assert_eq!(
            found[0].key, "stt.openai_realtime_stt.path_and_query",
            "{bad}"
        );
        assert!(found[0].position.is_some(), "{bad}");
    }
    let text = with_stt(
        "[stt]\nprovider = \"openai_realtime_stt\"\napi_key_env = \"OPENAI_API_KEY\"\n[stt.openai_realtime_stt]\npath_and_query = \"/v1/realtime?intent=transcription&model=m\"\n",
    );
    assert!(load(&text).is_ok(), "{:?}", load(&text).err());
}
