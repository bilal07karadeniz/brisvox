//! Which out-of-process agent a call links to: the server-wide `BRISVOX_AGENT_URL`,
//! the definition's own `[agent_link]` section, and the named routes a call's start message may
//! pick. See `docs/AGENT_PROTOCOL.md`, "Routing".
//!
//! Precedence: the route named by the call's start message, else the definition's
//! `[agent_link] url`, else `BRISVOX_AGENT_URL`. A start message names a route, never a URL: the
//! operator decides which processes exist, so a caller cannot make the server connect to an
//! address of their choosing, and the token of one agent process is never sent to another (a
//! route has its own `token_env`, the default token belongs to the default process only).

use std::sync::Arc;

use brisvox_agent_link::AgentLinkConfig;

use crate::config::AgentConfig;
use crate::{Error, Result};

/// The agent processes of a server, resolved at startup.
#[derive(Debug, Default)]
pub struct AgentLinks {
    /// The process every call without a (known) route links to; none: such calls run on the
    /// definition alone.
    pub default: Option<Arc<AgentLinkConfig>>,
    /// The named processes, by route name.
    pub routes: Vec<(String, Arc<AgentLinkConfig>)>,
}

impl AgentLinks {
    /// The agent processes of `agent`'s calls, from the definition's `[agent_link]` section and
    /// the environment `get` (`BRISVOX_AGENT_URL`, `BRISVOX_AGENT_TOKEN` and the timeouts of
    /// [`AgentLinkConfig::from_env`]).
    ///
    /// # Errors
    /// [`Error::Config`] for an environment value that does not parse or a URL the link rejects.
    pub fn resolve(agent: &AgentConfig, get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let section = agent.agent_link();
        // The definition's choice wins over the environment; the environment sets the default
        // process only (a route has its own switch, below).
        let caller_identity = match section.caller_identity {
            Some(on) => on,
            None => parse_flag(
                "BRISVOX_AGENT_CALLER_IDENTITY",
                get("BRISVOX_AGENT_CALLER_IDENTITY"),
            )?,
        };
        let config_err = |e: brisvox_agent_link::LinkError| Error::Config(e.to_string());
        let default = match &section.url {
            Some(url) => {
                let mut config =
                    AgentLinkConfig::from_env_for_with(&get, url, section.allow_insecure)
                        .map_err(config_err)?;
                // The definition's own token, else the server-wide one `from_env_for` read.
                if let Some(token) = &section.token {
                    config.auth_token = Some(token.clone());
                }
                config.caller_identity = caller_identity;
                Some(Arc::new(config))
            }
            None => {
                let mut config = match get("BRISVOX_AGENT_URL").filter(|u| !u.trim().is_empty()) {
                    Some(url) => Some(
                        AgentLinkConfig::from_env_for_with(
                            &get,
                            url.trim(),
                            section.allow_insecure,
                        )
                        .map_err(config_err)?,
                    ),
                    None => None,
                };
                if let (Some(config), Some(token)) = (config.as_mut(), &section.token) {
                    config.auth_token = Some(token.clone());
                }
                if let Some(config) = config.as_mut() {
                    config.caller_identity = caller_identity;
                }
                config.map(Arc::new)
            }
        };
        let mut routes = Vec::with_capacity(section.routes.len());
        for route in &section.routes {
            let mut config = AgentLinkConfig::from_env_for_with(
                &get,
                &route.url,
                route.allow_insecure || section.allow_insecure,
            )
            .map_err(config_err)?;
            // Never the default token: a route may be another team's agent.
            config.auth_token.clone_from(&route.token);
            config.caller_identity = route.caller_identity;
            routes.push((route.name.clone(), Arc::new(config)));
        }
        Ok(Self { default, routes })
    }

    /// Whether any call can link to an agent process.
    pub fn is_empty(&self) -> bool {
        self.default.is_none() && self.routes.is_empty()
    }
}

/// An environment switch as a flag (`1`/`0`, `true`/`false`, `yes`/`no`, `on`/`off`); unset or
/// blank is off. A value that is none of those is an error: a privacy switch is never silently
/// ignored.
fn parse_flag(name: &str, value: Option<String>) -> Result<bool> {
    let Some(value) = value.filter(|v| !v.trim().is_empty()) else {
        return Ok(false);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(Error::Config(format!(
            "{name}: {value:?} is not true or false"
        ))),
    }
}

#[cfg(all(
    test,
    feature = "provider-deepgram",
    feature = "provider-openai",
    feature = "provider-cartesia"
))]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config;

    fn agent(extra: &str, env: &[(&str, &str)]) -> AgentConfig {
        let text = format!(
            r#"
[agent]
name = "routed"
language = "en"
system_prompt = "x"

[stt]
provider = "deepgram"
api_key_env = "K_STT"

[llm]
provider = "openai"
api_key_env = "K_LLM"

[tts]
provider = "cartesia"
api_key_env = "K_TTS"
voice = "v"

{extra}
"#
        );
        let mut keys: BTreeMap<String, String> = [("K_STT", "a"), ("K_LLM", "b"), ("K_TTS", "c")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        keys.extend(env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
        config::load_str(&text, "routed.toml", &keys).unwrap()
    }

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn without_a_section_the_environment_decides() {
        let none = AgentLinks::resolve(&agent("", &[]), env(&[])).unwrap();
        assert!(none.is_empty());
        let links = AgentLinks::resolve(
            &agent("", &[]),
            env(&[
                ("BRISVOX_AGENT_URL", "ws://env:1"),
                ("BRISVOX_AGENT_TOKEN", "t0"),
            ]),
        )
        .unwrap();
        let default = links.default.as_ref().unwrap();
        assert_eq!(default.url, "ws://env:1");
        assert_eq!(default.auth_token.as_ref().unwrap().expose(), "t0");
        assert!(links.routes.is_empty());
    }

    #[test]
    fn the_definition_overrides_the_environment_and_routes_keep_their_own_tokens() {
        let section = r#"
[agent_link]
url = "wss://file.example/agent"
token_env = "FILE_TOKEN"

[[agent_link.routes]]
name = "billing"
url = "wss://billing.example/agent"
token_env = "BILLING_TOKEN"

[[agent_link.routes]]
name = "public"
url = "ws://public.example/agent"
allow_insecure = true
"#;
        let agent = agent(
            section,
            &[
                ("FILE_TOKEN", "file-secret"),
                ("BILLING_TOKEN", "billing-secret"),
            ],
        );
        let links = AgentLinks::resolve(
            &agent,
            env(&[
                ("BRISVOX_AGENT_URL", "ws://env:1"),
                ("BRISVOX_AGENT_TOKEN", "default-secret"),
                ("BRISVOX_AGENT_CONNECT_TIMEOUT_MS", "750"),
            ]),
        )
        .unwrap();
        let default = links.default.as_ref().unwrap();
        assert_eq!(default.url, "wss://file.example/agent");
        assert_eq!(default.auth_token.as_ref().unwrap().expose(), "file-secret");
        assert_eq!(links.routes.len(), 2);
        let (name, billing) = &links.routes[0];
        assert_eq!(name, "billing");
        assert_eq!(
            billing.auth_token.as_ref().unwrap().expose(),
            "billing-secret"
        );
        assert_eq!(
            billing.connect_timeout,
            std::time::Duration::from_millis(750)
        );
        // The default token is never sent to a route that has none of its own.
        let (_, public) = &links.routes[1];
        assert!(public.auth_token.is_none());
        assert!(!format!("{links:?}").contains("secret"));
    }

    #[test]
    fn the_caller_identity_is_off_unless_the_definition_or_the_environment_opts_in() {
        let url = "[agent_link]
url = \"ws://agent.internal/a\"
";
        let default_of = |section: &str, pairs: &[(&str, &str)]| {
            AgentLinks::resolve(&agent(section, &[]), env(pairs))
                .unwrap()
                .default
                .unwrap()
                .caller_identity
        };
        assert!(!default_of(url, &[]));
        // The environment switch sets the default process.
        assert!(default_of(url, &[("BRISVOX_AGENT_CALLER_IDENTITY", "1")]));
        assert!(!default_of(url, &[("BRISVOX_AGENT_CALLER_IDENTITY", "")]));
        // The definition's own choice wins, both ways.
        let on = format!("{url}caller_identity = true\n");
        assert!(default_of(&on, &[]));
        let off = format!("{url}caller_identity = false\n");
        assert!(!default_of(
            &off,
            &[("BRISVOX_AGENT_CALLER_IDENTITY", "true")]
        ));
        // A value that is no flag fails the start: a privacy switch is never ignored.
        let bad = AgentLinks::resolve(
            &agent(url, &[]),
            env(&[("BRISVOX_AGENT_CALLER_IDENTITY", "maybe")]),
        );
        assert!(bad.is_err_and(|e| e.to_string().contains("BRISVOX_AGENT_CALLER_IDENTITY")));
        // A route has its own switch and inherits neither the section's nor the environment's:
        // it may be another team's agent.
        let routes = "[agent_link]
url = \"ws://agent.internal/a\"
caller_identity = true

[[agent_link.routes]]
name = \"billing\"
url = \"ws://billing.internal/a\"

[[agent_link.routes]]
name = \"support\"
url = \"ws://support.internal/a\"
caller_identity = true
";
        let links = AgentLinks::resolve(
            &agent(routes, &[]),
            env(&[("BRISVOX_AGENT_CALLER_IDENTITY", "1")]),
        )
        .unwrap();
        assert!(links.default.as_ref().unwrap().caller_identity);
        assert!(!links.routes[0].1.caller_identity);
        assert!(links.routes[1].1.caller_identity);
    }

    #[test]
    fn routes_alone_leave_calls_without_a_route_on_the_definition() {
        let section = r#"
[[agent_link.routes]]
name = "billing"
url = "ws://billing.internal/agent"
"#;
        let links = AgentLinks::resolve(&agent(section, &[]), env(&[])).unwrap();
        assert!(links.default.is_none());
        assert_eq!(links.routes.len(), 1);
        assert!(!links.is_empty());
    }

    #[test]
    fn plaintext_to_a_public_host_needs_an_explicit_allow() {
        let refuse = |section: &str, pairs: &[(&str, &str)]| {
            AgentLinks::resolve(&agent(section, &[]), env(pairs))
                .unwrap_err()
                .to_string()
        };
        let public = "[agent_link]
url = \"ws://agent.example.com/a\"
";
        assert!(refuse(public, &[]).contains("plaintext"));
        let route = "[[agent_link.routes]]
name = \"r\"
url = \"ws://1.2.3.4:9\"
";
        assert!(refuse(route, &[]).contains("plaintext"));
        assert!(
            refuse("", &[("BRISVOX_AGENT_URL", "ws://agent.example.com")]).contains("plaintext")
        );
        // The definition's switch, the route's own switch and the environment switch each work.
        let allowed = format!(
            "{public}allow_insecure = true
"
        );
        assert!(AgentLinks::resolve(&agent(&allowed, &[]), env(&[])).is_ok());
        let allowed_route = format!(
            "{route}allow_insecure = true
"
        );
        assert!(AgentLinks::resolve(&agent(&allowed_route, &[]), env(&[])).is_ok());
        let url_only = "[agent_link]
allow_insecure = true
";
        let links = AgentLinks::resolve(
            &agent(url_only, &[]),
            env(&[("BRISVOX_AGENT_URL", "ws://agent.example.com")]),
        );
        assert!(links.is_ok_and(|l| l.default.is_some_and(|d| d.allow_insecure)));
        let by_env = AgentLinks::resolve(
            &agent(public, &[]),
            env(&[("BRISVOX_AGENT_ALLOW_INSECURE", "1")]),
        );
        assert!(by_env.is_ok());
    }

    #[test]
    fn a_bad_section_is_a_config_error_with_its_key() {
        let keys: BTreeMap<String, String> = [("K_STT", "a"), ("K_LLM", "b"), ("K_TTS", "c")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let load = |extra: &str| {
            let text = format!(
                "[agent]\nname = \"a\"\nlanguage = \"en\"\nsystem_prompt = \"x\"\n[stt]\nprovider = \"deepgram\"\napi_key_env = \"K_STT\"\n[llm]\nprovider = \"openai\"\napi_key_env = \"K_LLM\"\n[tts]\nprovider = \"cartesia\"\napi_key_env = \"K_TTS\"\nvoice = \"v\"\n{extra}"
            );
            config::load_str(&text, "x.toml", &keys)
                .unwrap_err()
                .to_string()
        };
        assert!(load("[agent_link]\nurl = \"http://x\"\n").contains("agent_link.url"));
        assert!(
            load("[agent_link]\ntoken_env = \"NOT_SET_ANYWHERE\"\n")
                .contains("agent_link.token_env")
        );
        let dup = "[[agent_link.routes]]\nname = \"a\"\nurl = \"ws://x\"\n[[agent_link.routes]]\nname = \"a\"\nurl = \"ws://y\"\n";
        assert!(load(dup).contains("defined twice"));
        let bad_name = "[[agent_link.routes]]\nname = \"has space\"\nurl = \"ws://x\"\n";
        assert!(load(bad_name).contains("agent_link.routes[0].name"));
        let bad_url = "[[agent_link.routes]]\nname = \"a\"\nurl = \"https://x\"\n";
        assert!(load(bad_url).contains("agent_link.routes[0].url"));
    }
}
