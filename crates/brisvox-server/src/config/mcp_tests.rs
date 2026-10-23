//! Tests of the `[[mcp.servers]]` tables: what loads, what each mistake is reported as (with its
//! key path and position), the secrets, and the names the LLM would see.

use std::collections::BTreeMap;

use super::*;

/// A definition without MCP servers; tests append `[[mcp.servers]]` tables.
const BASE: &str = r#"
[agent]
name = "mcp-agent"
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

const BEARER: &str = "mcp-bearer-secret-0004";
const CHILD_SECRET: &str = "child-secret-value-0005";

fn env() -> BTreeMap<String, String> {
    [
        ("DEEPGRAM_API_KEY", "dg-secret-value-0001"),
        ("OPENAI_API_KEY", "sk-secret-value-0002"),
        ("CARTESIA_API_KEY", "ca-secret-value-0003"),
        ("MCP_BEARER", BEARER),
        ("MCP_CHILD_SECRET", CHILD_SECRET),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

fn with(servers: &str) -> String {
    format!("{BASE}\n{servers}")
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

fn pos(text: &str, needle: &str) -> (usize, usize) {
    error::line_column(text, text.find(needle).unwrap())
}

const GOOD: &str = r#"
[[mcp.servers]]
name = "files"
command = "mcp-server-files"
args = ["--root", "/data"]
env = { LOG_LEVEL = "warn" }
env_from = { API_TOKEN = "MCP_CHILD_SECRET" }
tools = ["list_dir", "files.read", "write_file"]
read_only = ["list_dir", "files.read"]
confirm = ["write_file"]
prefix = true
timeout_ms = 8000
filler_after_ms = 300

[[mcp.servers]]
name = "crm"
url = "https://crm.example.com/mcp"
bearer_env = "MCP_BEARER"
tools = ["find_customer"]
read_only = ["find_customer"]
prefix = "crm-"
"#;

#[test]
fn a_file_without_mcp_has_no_servers() {
    let agent = load(BASE).unwrap();
    assert!(agent.mcp().is_empty());
}

#[test]
fn good_servers_load_with_kinds_names_and_secrets() {
    let agent = load(&with(GOOD)).unwrap();
    let servers = &agent.mcp().servers;
    assert_eq!(servers.len(), 2);

    let files = &servers[0];
    assert_eq!(files.name, "files");
    assert_eq!(files.timeout, Duration::from_secs(8));
    assert_eq!(files.filler_after, Some(Duration::from_millis(300)));
    assert_eq!(files.startup_timeout, mcp::DEFAULT_MCP_STARTUP_TIMEOUT);
    let McpTransportSettings::Stdio { command, args, env } = &files.transport else {
        panic!("stdio expected")
    };
    assert_eq!(command, "mcp-server-files");
    assert_eq!(args, &["--root", "/data"]);
    let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(names, ["LOG_LEVEL", "API_TOKEN"]);
    assert_eq!(env[1].1.expose(), CHILD_SECRET);
    let tools: Vec<(&str, &str, bool, bool)> = files
        .tools
        .iter()
        .map(|t| (t.name.as_str(), t.exposed.as_str(), t.read_only, t.confirm))
        .collect();
    assert_eq!(
        tools,
        [
            ("list_dir", "files_list_dir", true, false),
            ("files.read", "files_files_read", true, false),
            ("write_file", "files_write_file", false, true),
        ]
    );

    let crm = &servers[1];
    assert_eq!(crm.timeout, mcp::DEFAULT_MCP_TIMEOUT);
    assert_eq!(crm.filler_after, None);
    let McpTransportSettings::Http { url, bearer } = &crm.transport else {
        panic!("http expected")
    };
    assert_eq!(url, "https://crm.example.com/mcp");
    assert_eq!(bearer.as_ref().unwrap().expose(), BEARER);
    assert_eq!(crm.tools[0].exposed, "crm-find_customer");

    let exposed: Vec<&str> = agent.mcp().exposed_names().collect();
    assert_eq!(
        exposed,
        [
            "files_list_dir",
            "files_files_read",
            "files_write_file",
            "crm-find_customer"
        ]
    );
}

#[test]
fn secrets_never_reach_debug_output() {
    let agent = load(&with(GOOD)).unwrap();
    let shown = format!("{:?}", agent.mcp());
    assert!(
        !shown.contains(BEARER) && !shown.contains(CHILD_SECRET),
        "{shown}"
    );
    assert!(shown.contains("[REDACTED]"), "{shown}");
}

#[test]
fn names_without_a_prefix_stay_and_dots_become_underscores() {
    let agent = load(&with(
        r#"
[[mcp.servers]]
name = "fs"
command = "x"
tools = ["a.b-c", "plain"]
"#,
    ))
    .unwrap();
    let exposed: Vec<&str> = agent.mcp().exposed_names().collect();
    assert_eq!(exposed, ["a_b-c", "plain"]);
    assert_eq!(exposed_name("pre_", "x.y"), "pre_x_y");
}

#[test]
fn both_transports_or_none_is_an_error_at_the_server() {
    let both = with(
        r#"
[[mcp.servers]]
name = "fs"
command = "x"
url = "https://example.com/mcp"
tools = ["a"]
"#,
    );
    let issue = only_issue(&both);
    assert_eq!(issue.key, "mcp.servers[0].url");
    assert_eq!(
        issue.position,
        Some(pos(&both, "\"https://example.com/mcp\""))
    );
    assert!(issue.message.contains("not both"), "{issue:?}");

    let none = with(
        r#"
[[mcp.servers]]
name = "fs"
tools = ["a"]
"#,
    );
    let issue = only_issue(&none);
    assert_eq!(issue.key, "mcp.servers[0]");
    assert_eq!(issue.position, Some(pos(&none, "\"fs\"")));
    assert!(issue.message.contains("`command`"), "{issue:?}");
}

#[test]
fn the_allowlist_is_required_and_not_empty() {
    let missing = with(
        r#"
[[mcp.servers]]
name = "fs"
command = "x"
"#,
    );
    let issue = only_issue(&missing);
    assert!(issue.message.contains("missing field `tools`"), "{issue:?}");

    let empty = with(
        r#"
[[mcp.servers]]
name = "fs"
command = "x"
tools = []
"#,
    );
    let issue = only_issue(&empty);
    assert_eq!(issue.key, "mcp.servers[0].tools");
    assert!(issue.message.contains("allowlist"), "{issue:?}");
}

#[test]
fn tool_names_are_checked_where_they_stand() {
    let text = with(
        r#"
[[mcp.servers]]
name = "fs"
command = "x"
tools = ["ok", "has space", "ok"]
"#,
    );
    let found = issues(&text);
    assert_eq!(found.len(), 2, "{found:#?}");
    assert_eq!(found[0].key, "mcp.servers[0].tools[1]");
    assert_eq!(found[0].position, Some(pos(&text, "\"has space\"")));
    assert!(found[0].message.contains("not an MCP tool name"));
    assert_eq!(found[1].key, "mcp.servers[0].tools[2]");
    assert!(found[1].message.contains("listed twice"));
}

#[test]
fn read_only_and_confirm_are_subsets_that_do_not_overlap() {
    let text = with(
        r#"
[[mcp.servers]]
name = "fs"
command = "x"
tools = ["a", "b"]
read_only = ["a", "zzz"]
confirm = ["a", "b"]
"#,
    );
    let found = issues(&text);
    let keys: Vec<&str> = found.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "mcp.servers[0].read_only[0]",
            "mcp.servers[0].read_only[1]",
            "mcp.servers[0].confirm[0]"
        ],
        "{found:#?}"
    );
    assert!(found[0].message.contains("both `read_only` and `confirm`"));
    assert!(found[1].message.contains("not in this server's `tools`"));
    assert!(found[2].message.contains("both `read_only` and `confirm`"));
}

#[test]
fn unknown_keys_are_refused() {
    let text = with(
        r#"
[[mcp.servers]]
name = "fs"
command = "x"
tools = ["a"]
readonly = ["a"]
"#,
    );
    let issue = only_issue(&text);
    assert!(
        issue.message.contains("unknown field `readonly`"),
        "{issue:?}"
    );
    assert_eq!(issue.position.map(|p| p.0), Some(pos(&text, "readonly").0));
}

#[test]
fn transport_keys_belong_to_their_transport() {
    let http_with_args = with(
        r#"
[[mcp.servers]]
name = "crm"
url = "https://example.com/mcp"
args = ["x"]
tools = ["a"]
"#,
    );
    assert_eq!(only_issue(&http_with_args).key, "mcp.servers[0].args");

    let stdio_with_bearer = with(
        r#"
[[mcp.servers]]
name = "fs"
command = "x"
bearer_env = "MCP_BEARER"
tools = ["a"]
"#,
    );
    let issue = only_issue(&stdio_with_bearer);
    assert_eq!(issue.key, "mcp.servers[0].bearer_env");
    assert!(issue.message.contains("env_from"), "{issue:?}");

    let env_clash = with(
        r#"
[[mcp.servers]]
name = "fs"
command = "x"
env = { TOKEN = "a" }
env_from = { TOKEN = "MCP_BEARER" }
tools = ["a"]
"#,
    );
    assert_eq!(only_issue(&env_clash).key, "mcp.servers[0].env_from.TOKEN");

    let bad_var = with(
        r#"
[[mcp.servers]]
name = "fs"
command = "x"
env = { "NOT OK" = "a" }
tools = ["a"]
"#,
    );
    assert_eq!(only_issue(&bad_var).key, "mcp.servers[0].env.NOT OK");
}

fn http_entry(url: &str, extra: &str) -> String {
    with(&format!(
        "[[mcp.servers]]
name = \"crm\"
url = \"{url}\"
tools = [\"a\"]
{extra}"
    ))
}

#[test]
fn plain_http_is_for_private_hosts_only_unless_the_entry_says_otherwise() {
    // To a host on the internet, with or without a bearer token: the tool arguments and results
    // carry what the caller said.
    for extra in [
        "",
        "bearer_env = \"MCP_BEARER\"
",
    ] {
        let text = http_entry("http://crm.example.com/mcp", extra);
        let issue = only_issue(&text);
        assert_eq!(issue.key, "mcp.servers[0].url");
        assert!(issue.message.contains("unencrypted"), "{issue:?}");
        assert!(issue.message.contains("allow_insecure"), "{issue:?}");
        assert_eq!(
            issue.position,
            Some(pos(&text, "\"http://crm.example.com/mcp\""))
        );
    }
    // Loopback, private addresses and service names are fine.
    for url in [
        "http://127.0.0.1:8080/mcp",
        "http://localhost/mcp",
        "http://[::1]:9/mcp",
        "http://10.1.2.3/mcp",
        "http://192.168.1.20:8080/mcp",
        "http://crm.internal/mcp",
        "http://mcp:8080/mcp",
    ] {
        load(&http_entry(
            url,
            "bearer_env = \"MCP_BEARER\"
",
        ))
        .unwrap_or_else(|e| panic!("{url}: {e}"));
    }
    // https is always fine; the explicit switch allows the rest.
    load(&http_entry("https://crm.example.com/mcp", "")).unwrap();
    load(&http_entry(
        "http://crm.example.com/mcp",
        "allow_insecure = true
",
    ))
    .unwrap();
    // A public name that merely starts like a private one is still public.
    assert!(!issues(&http_entry("http://10.example.com/mcp", "")).is_empty());
    assert!(!issues(&http_entry("http://localhost.example.com/mcp", "")).is_empty());
}

#[test]
fn allow_insecure_belongs_to_a_url_server() {
    let text = with(
        "[[mcp.servers]]
name = \"fs\"
command = \"x\"
allow_insecure = true
tools = [\"a\"]
",
    );
    assert_eq!(only_issue(&text).key, "mcp.servers[0].allow_insecure");
}

#[test]
fn a_url_with_a_query_or_fragment_is_refused_because_urls_appear_in_messages() {
    for url in [
        "https://crm.example.com/mcp?token=abc",
        "https://crm.example.com/mcp#frag",
    ] {
        let text = http_entry(url, "");
        let issue = only_issue(&text);
        assert_eq!(issue.key, "mcp.servers[0].url", "{url}");
        assert!(issue.message.contains("query or a fragment"), "{issue:?}");
        assert!(!issue.message.contains("abc"), "{issue:?}");
    }
}

#[test]
fn max_in_flight_defaults_to_32_and_is_bounded() {
    let agent = load(&http_entry("https://crm.example.com/mcp", "")).unwrap();
    assert_eq!(agent.mcp().servers[0].max_in_flight, 32);
    let agent = load(&http_entry(
        "https://crm.example.com/mcp",
        "max_in_flight = 256
",
    ))
    .unwrap();
    assert_eq!(agent.mcp().servers[0].max_in_flight, 256);
    for bad in [0, 257] {
        let text = http_entry(
            "https://crm.example.com/mcp",
            &format!(
                "max_in_flight = {bad}
"
            ),
        );
        let issue = only_issue(&text);
        assert!(issue.message.contains("out of range"), "{issue:?}");
        assert_eq!(issue.position, Some(pos(&text, &format!("{bad}"))));
    }
}

#[test]
fn a_url_that_is_not_http_or_has_user_info_is_refused() {
    for url in [
        "ftp://example.com/mcp",
        "https://user:pw@example.com/mcp",
        "example.com",
    ] {
        let text = with(&format!(
            "[[mcp.servers]]\nname = \"crm\"\nurl = \"{url}\"\ntools = [\"a\"]\n"
        ));
        let found = issues(&text);
        assert!(!found.is_empty(), "{url}");
        assert!(
            !found[0].message.contains("pw"),
            "the message must not repeat user info: {found:?}"
        );
    }
}

#[test]
fn a_missing_secret_variable_fails_the_start_naming_the_variable() {
    let text = with(
        r#"
[[mcp.servers]]
name = "crm"
url = "https://example.com/mcp"
bearer_env = "NOT_SET_ANYWHERE"
tools = ["a"]
"#,
    );
    let issue = only_issue(&text);
    assert_eq!(issue.key, "mcp.servers[0].bearer_env");
    assert!(issue.message.contains("NOT_SET_ANYWHERE"), "{issue:?}");
    assert_eq!(issue.position, Some(pos(&text, "\"NOT_SET_ANYWHERE\"")));

    let child = with(
        r#"
[[mcp.servers]]
name = "fs"
command = "x"
env_from = { TOKEN = "NOT_SET_EITHER" }
tools = ["a"]
"#,
    );
    let issue = only_issue(&child);
    assert_eq!(issue.key, "mcp.servers[0].env_from.TOKEN");
    assert!(issue.message.contains("NOT_SET_EITHER"), "{issue:?}");
}

#[test]
fn names_cannot_clash_with_other_tools_or_each_other() {
    // With the agent's own tool list.
    let own = with(
        r#"
[[mcp.servers]]
name = "orders"
command = "x"
tools = ["lookup_order"]
"#,
    );
    let issue = only_issue(&own);
    assert_eq!(issue.key, "mcp.servers[0].tools[0]");
    assert!(issue.message.contains("`lookup_order`") && issue.message.contains("prefix"));
    // With the telephony tools and the confirmation tool.
    for reserved in ["transfer_call", "confirm_pending"] {
        let text = with(&format!(
            "[[mcp.servers]]\nname = \"a\"\ncommand = \"x\"\ntools = [\"{reserved}\"]\n"
        ));
        assert_eq!(
            only_issue(&text).key,
            "mcp.servers[0].tools[0]",
            "{reserved}"
        );
    }
    // Between servers: a prefix resolves it.
    let two = |prefix: &str| {
        with(&format!(
            "[[mcp.servers]]\nname = \"a\"\ncommand = \"x\"\ntools = [\"search\"]\n\
             [[mcp.servers]]\nname = \"b\"\ncommand = \"y\"\ntools = [\"search\"]\n{prefix}"
        ))
    };
    let issue = only_issue(&two(""));
    assert_eq!(issue.key, "mcp.servers[1].tools[0]");
    load(&two("prefix = true\n")).unwrap();
    // A name too long once prefixed.
    let long = with(&format!(
        "[[mcp.servers]]\nname = \"a\"\ncommand = \"x\"\nprefix = \"{}\"\ntools = [\"{}\"]\n",
        "p".repeat(30),
        "t".repeat(40)
    ));
    assert!(only_issue(&long).message.contains("longer than 64"));
}

#[test]
fn server_names_and_prefixes_are_checked() {
    let dup = with(
        r#"
[[mcp.servers]]
name = "a"
command = "x"
tools = ["t1"]
[[mcp.servers]]
name = "a"
command = "y"
tools = ["t2"]
"#,
    );
    let issue = only_issue(&dup);
    assert_eq!(issue.key, "mcp.servers[1].name");
    assert!(issue.message.contains("defined twice"));
    let bad_name =
        with("[[mcp.servers]]\nname = \"has space\"\ncommand = \"x\"\ntools = [\"t\"]\n");
    assert_eq!(only_issue(&bad_name).key, "mcp.servers[0].name");
    let bad_prefix =
        with("[[mcp.servers]]\nname = \"a\"\ncommand = \"x\"\nprefix = \"a b\"\ntools = [\"t\"]\n");
    assert_eq!(only_issue(&bad_prefix).key, "mcp.servers[0].prefix");
    let numeric_prefix =
        with("[[mcp.servers]]\nname = \"a\"\ncommand = \"x\"\nprefix = 5\ntools = [\"t\"]\n");
    assert!(!issues(&numeric_prefix).is_empty());
}

#[test]
fn timeouts_are_range_checked_at_their_position() {
    for (key, value) in [
        ("timeout_ms", 30_001),
        ("filler_after_ms", 0),
        ("startup_timeout_ms", 999),
    ] {
        let text = with(&format!(
            "[[mcp.servers]]\nname = \"a\"\ncommand = \"x\"\ntools = [\"t\"]\n{key} = {value}\n"
        ));
        let issue = only_issue(&text);
        assert!(issue.message.contains("out of range"), "{key}: {issue:?}");
        assert_eq!(issue.position, Some(pos(&text, &format!("{value}"))));
    }
}

#[test]
fn the_number_of_servers_and_tools_is_bounded() {
    let mut text = BASE.to_owned();
    for i in 0..=MAX_MCP_SERVERS {
        text.push_str(&format!(
            "\n[[mcp.servers]]\nname = \"s{i}\"\ncommand = \"x\"\ntools = [\"t\"]\nprefix = true\n"
        ));
    }
    let found = issues(&text);
    assert!(
        found
            .iter()
            .any(|i| i.key == "mcp.servers" && i.message.contains("at most 8"))
    );

    let many: Vec<String> = (0..=mcp::MAX_MCP_TOOLS_PER_SERVER)
        .map(|i| format!("\"t{i}\""))
        .collect();
    let text = with(&format!(
        "[[mcp.servers]]\nname = \"a\"\ncommand = \"x\"\ntools = [{}]\n",
        many.join(", ")
    ));
    assert!(
        issues(&text)
            .iter()
            .any(|i| i.key == "mcp.servers[0].tools")
    );
}

#[test]
fn without_the_mcp_feature_the_file_is_refused_at_the_first_server() {
    let text = with(GOOD);
    let agent = load(&text).unwrap();
    let error = mcp::require_compiled(&agent, false).unwrap_err();
    assert_eq!(error.issues.len(), 1, "{error:#?}");
    let issue = &error.issues[0];
    assert_eq!(issue.key, "mcp.servers[0]");
    assert_eq!(issue.position, Some(pos(&text, "\"files\"")));
    assert!(issue.message.contains("feature `mcp`"), "{issue:?}");
    // With it compiled in, the same file is fine; a file without servers always is.
    mcp::require_compiled(&agent, true).unwrap();
    mcp::require_compiled(&load(BASE).unwrap(), false).unwrap();
}

#[test]
fn the_mcp_example_loads() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/agents/mcp/docs-en.toml");
    let text = std::fs::read_to_string(path).unwrap();
    let agent = load(&text).unwrap();
    let exposed: Vec<&str> = agent.mcp().exposed_names().collect();
    assert_eq!(
        exposed,
        ["wiki_ask_wiki_question", "wiki_read_wiki_structure"]
    );
    assert!(agent.mcp().servers[0].tools.iter().all(|t| t.read_only));
    // The tools are offered by the server's allowlist, not by `agent.tools`.
    assert_eq!(agent.tool_names().count(), 0);
}

#[cfg(feature = "yaml")]
#[test]
fn yaml_reads_the_same_tables() {
    let yaml = r#"
agent:
  name: mcp-agent
  language: en
  system_prompt: You are a helpful agent.
stt: { provider: deepgram, api_key_env: DEEPGRAM_API_KEY }
llm: { provider: openai, api_key_env: OPENAI_API_KEY }
tts: { provider: cartesia, api_key_env: CARTESIA_API_KEY, voice: voice-123 }
mcp:
  servers:
    - name: files
      command: mcp-server-files
      args: ["--root", "/data"]
      env: { LOG_LEVEL: warn }
      tools: [list_dir, write_file]
      read_only: [list_dir]
      confirm: [write_file]
      prefix: true
    - name: crm
      url: https://crm.example.com/mcp
      bearer_env: MCP_BEARER
      tools: [find_customer]
      prefix: "crm_"
"#;
    let agent = load_with(yaml, "agent.yaml", Format::Yaml, &env(), &|_| true).unwrap();
    let exposed: Vec<&str> = agent.mcp().exposed_names().collect();
    assert_eq!(
        exposed,
        ["files_list_dir", "files_write_file", "crm_find_customer"]
    );
    // A mistake is positioned in the YAML text.
    let bad = yaml.replace("tools: [find_customer]", "tools: []");
    let err = load_with(&bad, "agent.yaml", Format::Yaml, &env(), &|_| true).unwrap_err();
    assert_eq!(err.issues[0].key, "mcp.servers[1].tools");
}
