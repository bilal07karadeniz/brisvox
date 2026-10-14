//! The runnable projects in `examples/` cannot rot: every agent definition in them loads
//! through the server's config loader and builds its provider adapters (placeholder keys, no
//! network), starts on the binary's tool setup, and every environment template reads through
//! the server's own settings parsers, so a renamed variable or a removed key fails here. The
//! Python halves (webhook, booking tools, the page) have their own tests, run by
//! `scripts/verify.sh`.
#![cfg(feature = "all-providers")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
// test helpers

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use brisvox_core::SystemClock;
use brisvox_server::config::{self, AgentConfig};

/// `examples/` at the repository root.
fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

/// The project directories of `examples/` (the older single-file collection in `agents/` has
/// its own tests).
fn projects() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(examples_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n != "agents"))
        .collect();
    found.sort();
    found
}

/// The agent definitions of the projects: every `*.toml` / `*.yaml` directly in a project.
fn agent_files() -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in projects() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("toml" | "yaml" | "yml")
            ) && path.file_name().is_some_and(|n| n != "Cargo.toml")
            {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Placeholder keys for every environment variable the examples' providers name.
fn keys() -> BTreeMap<String, String> {
    [
        "DEEPGRAM_API_KEY",
        "OPENAI_API_KEY",
        "CARTESIA_API_KEY",
        "ANTHROPIC_API_KEY",
        "ELEVENLABS_API_KEY",
        "ASSEMBLYAI_API_KEY",
        "GROQ_API_KEY",
    ]
    .into_iter()
    .map(|k| (k.to_owned(), "placeholder".to_owned()))
    .collect()
}

fn load(path: &Path) -> AgentConfig {
    config::load_file(path, &keys()).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn rel(path: &Path) -> String {
    path.strip_prefix(examples_dir())
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

/// An `*.env.example` as the environment a user would get after filling it in: comments
/// skipped, blank values given a placeholder of the right shape.
fn env_template(path: &Path) -> HashMap<String, String> {
    let text = std::fs::read_to_string(path).unwrap();
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| {
            let (k, v) = (k.trim().to_owned(), v.trim().to_owned());
            let v = if !v.is_empty() {
                v
            } else if k.ends_with("ACCOUNT_SID") {
                format!("AC{}", "0".repeat(32))
            } else if k.ends_with("STREAM_SECRET") {
                // The server wants at least 32 bytes.
                "a".repeat(40)
            } else if k.ends_with("CALLER_ID") {
                "+15555550199".to_owned()
            } else {
                "placeholder".to_owned()
            };
            (k, v)
        })
        .collect()
}

#[test]
fn every_example_agent_file_loads_and_builds_its_adapters() {
    let files = agent_files();
    let names: Vec<String> = files.iter().map(|p| rel(p)).collect();
    for expected in [
        "phone-receptionist/agent.toml",
        "phone-receptionist/reminder.toml",
        "web-assistant/agent.toml",
        "booking-agent/agent.toml",
        "rust-tools/agent.toml",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "{expected} in {names:?}"
        );
    }
    let (clock, cancel) = (
        SystemClock::shared(),
        tokio_util::sync::CancellationToken::new(),
    );
    for path in files {
        let agent = load(&path);
        agent
            .stt()
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        agent
            .llm()
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        agent
            .tts(clock.clone(), cancel.clone())
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(
            agent.system_prompt().len() > 100,
            "{}: a real prompt",
            path.display()
        );
    }
}

#[test]
fn the_examples_say_what_the_readmes_say() {
    let dir = examples_dir();

    let receptionist = load(&dir.join("phone-receptionist/agent.toml"));
    assert_eq!(
        receptionist.tool_names().collect::<Vec<_>>(),
        ["transfer_call", "end_call"]
    );
    let policy = receptionist.telephony_policy();
    let destinations: Vec<&str> = policy
        .destinations()
        .iter()
        .map(|d| d.name.as_str())
        .collect();
    assert_eq!(destinations, ["front_desk", "billing"]);
    assert!(
        receptionist.voicemail().is_none(),
        "inbound: no machine to talk to"
    );
    assert!(
        receptionist.system_prompt().contains("transfer_call")
            && receptionist.system_prompt().contains("front_desk"),
        "the prompt names the tool and a destination that exist"
    );

    let reminder = load(&dir.join("phone-receptionist/reminder.toml"));
    let voicemail = reminder
        .voicemail()
        .expect("the outbound agent leaves messages");
    assert!(voicemail.message.len() > 40 && voicemail.message.len() <= 1000);
    assert!(
        !voicemail.detect_only,
        "DetectMessageEnd reports the end of a greeting"
    );
    assert!(
        reminder
            .telephony_policy()
            .destinations()
            .iter()
            .any(|d| d.name == "front_desk")
    );

    // The out-of-process agents' tools are the agent process's: none are listed server side.
    for project in ["booking-agent", "web-assistant"] {
        let agent = load(&dir.join(project).join("agent.toml"));
        assert_eq!(agent.tool_names().count(), 0, "{project}");
    }
}

/// The examples start on the binary's tool setup: the phone agents' built-in telephony tools
/// come from a Twilio call control, the others need none.
#[cfg(feature = "call-control")]
#[tokio::test]
async fn the_example_agents_start_with_the_binarys_tool_setup() {
    use brisvox_engine::Offload;
    use brisvox_server::{EngineRunner, RunnerConfig, ToolSetup, Vad};

    let clock = SystemClock::shared();
    let env = env_template(&examples_dir().join("phone-receptionist/twilio.env.example"));
    let shutdown = tokio_util::sync::CancellationToken::new();
    for path in agent_files() {
        // The Rust-tools example brings its own tools; its crate tests that they resolve.
        if rel(&path).starts_with("rust-tools/") {
            continue;
        }
        let agent = load(&path);
        let providers =
            brisvox_server::ConfiguredProviders::from_agent(&agent, &clock, &shutdown).unwrap();
        let tools = ToolSetup::from_env(&env, &clock, None).unwrap();
        EngineRunner::with_tools(
            agent,
            providers,
            Offload::Inline(clock.clone()),
            Vad::Off,
            RunnerConfig::default(),
            tools,
            &clock,
        )
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
    shutdown.cancel();
}

/// The Twilio template is what the server's settings parsers accept, names the example's agent
/// file, and its stream URLs agree (the webhook's TwiML must equal the signed public URL).
#[test]
fn the_phone_receptionists_environment_reads_through_the_servers_settings() {
    let root = examples_dir().join("..");
    let env = env_template(&examples_dir().join("phone-receptionist/twilio.env.example"));
    let host = brisvox_server::HostSettings::from_env(&env).unwrap();
    let twilio = host.twilio.as_ref().expect("the Twilio listener is on");
    assert!(
        host.websocket.is_none(),
        "the unauthenticated listener stays off"
    );
    assert!(
        twilio.addr.ip().is_loopback(),
        "published through a TLS proxy or tunnel only"
    );
    let url = twilio.config.public_url.as_deref().unwrap();
    assert!(
        url.starts_with("wss://") && url.ends_with("/twilio/stream"),
        "{url}"
    );
    assert_eq!(env["BRISVOX_STREAM_URL"], env["BRISVOX_TWILIO_PUBLIC_URL"]);
    assert!(env["TWIML_PUBLIC_URL"].ends_with("/voice"));
    // The template names the example's own agent file, relative to the repository root.
    let agent = host.agent_file.as_deref().unwrap();
    load(&root.join(agent));
    assert_eq!(agent, Path::new("examples/phone-receptionist/agent.toml"));
    // Every provider key the agent names is in the template.
    for key in ["ASSEMBLYAI_API_KEY", "GROQ_API_KEY", "DEEPGRAM_API_KEY"] {
        assert!(env.contains_key(key), "{key}");
    }
}

#[cfg(feature = "webrtc")]
#[test]
fn the_web_assistants_environment_reads_through_the_servers_settings() {
    let root = examples_dir().join("..");
    let env = env_template(&examples_dir().join("web-assistant/server.env.example"));
    let host = brisvox_server::HostSettings::from_env(&env).unwrap();
    let webrtc = host.webrtc.as_ref().expect("the WebRTC listener is on");
    assert!(webrtc.addr.ip().is_loopback());
    assert_eq!(
        webrtc.config.config.cors_origin.as_deref(),
        Some("http://localhost:8000"),
        "the origin of `python3 -m http.server 8000`"
    );
    assert!(host.websocket.is_none());
    let agent = host.agent_file.as_deref().unwrap();
    assert_eq!(agent, Path::new("examples/web-assistant/agent.toml"));
    load(&root.join(agent));
    for key in ["DEEPGRAM_API_KEY", "OPENAI_API_KEY", "CARTESIA_API_KEY"] {
        assert!(env.contains_key(key), "{key}");
    }
}

/// Every `BRISVOX_*` variable an example's README or template names is one the server documents
/// (`docs/OPERATIONS.md`), one an example's own script reads, or one of `scripts/verify.sh`.
#[test]
fn the_examples_only_name_documented_variables() {
    let own: BTreeSet<&str> = ["BRISVOX_STREAM_URL", "BRISVOX_REQUIRE_EXAMPLES"]
        .into_iter()
        .collect();
    let operations = std::fs::read_to_string(examples_dir().join("../docs/OPERATIONS.md")).unwrap();
    let mut checked = 0;
    for project in projects() {
        for entry in std::fs::read_dir(&project).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !(name == "README.md" || name.ends_with(".env.example")) {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let mut rest = text.as_str();
            while let Some(at) = rest.find("BRISVOX_") {
                let tail = &rest[at..];
                let end = tail
                    .find(|c: char| !(c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit()))
                    .unwrap_or(tail.len());
                let var = tail[..end].trim_end_matches('_');
                if !own.contains(var) {
                    assert!(
                        operations.contains(var),
                        "{} names {var}, which docs/OPERATIONS.md does not document",
                        path.display()
                    );
                    checked += 1;
                }
                rest = &tail[end..];
            }
        }
    }
    assert!(checked > 10, "the scan found the variables ({checked})");
}

/// No secret is committed in an example: keys come from the environment.
#[test]
fn the_examples_hold_no_secrets() {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n != "__pycache__") {
                    walk(&path, out);
                }
            } else {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(&examples_dir(), &mut files);
    assert!(files.len() > 15, "{}", files.len());
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            !(name == ".env" || (name.ends_with(".env") && !name.ends_with(".example"))),
            "{}: a filled-in environment file",
            path.display()
        );
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            let at = || format!("{}:{}", path.display(), n + 1);
            // `KEY=value` lines of the templates: only the documented non-secret ones carry values.
            if name.ends_with(".env.example")
                && !line.trim_start().starts_with('#')
                && let Some((key, value)) = line.split_once('=')
            {
                let secret = key.contains("KEY")
                    || key.contains("TOKEN")
                    || key.contains("PASSWORD")
                    || key.contains("SECRET");
                assert!(
                    !(secret && !value.trim().is_empty()),
                    "{}: {key} has a value",
                    at()
                );
            }
            // Key shapes of the providers and Twilio.
            let looks_like_key = line
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
                .any(|w| {
                    (w.starts_with("sk-") && w.len() > 20)
                        || (w.starts_with("AC")
                            && w.len() == 34
                            && w[2..].bytes().all(|b| b.is_ascii_hexdigit())
                            && w[2..].bytes().any(|b| b != b'0'))
                        || (w.len() == 32
                            && w.bytes().all(|b| b.is_ascii_hexdigit())
                            && !w.bytes().all(|b| b == b'0'))
                });
            assert!(!looks_like_key, "{}: looks like a credential: {line}", at());
        }
    }
}
