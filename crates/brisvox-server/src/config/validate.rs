//! Cross-field checks of an agent definition: rules no single value can check by itself (a
//! provider feature not compiled in, a language the chosen provider does not speak, a sample
//! rate the provider does not deliver, an options table for another provider, settings of one
//! turn policy given with the other). Every problem is collected; none stops the others.

use std::collections::BTreeSet;
use std::ops::Range;

use brisvox_agent::telephony::tools::{PolicyError, TelephonyPolicy};
use brisvox_core::frame::SampleRate;
use brisvox_engine::Timeouts;
use toml::Spanned;

use super::Provider;
use super::error::{Issue, line_column};
use super::schema::{
    AgentFile, CompatiblePreset, LlmProviderKind, LlmSection, SttDialect, SttProviderKind,
    SttSection, TtsProviderKind, TtsSection, TurnPolicyKind,
};
use super::types::{LanguageTag, LocalWsUrl, Rate, ident_problem, url_host};

/// Longest accepted system prompt, in bytes.
pub const MAX_SYSTEM_PROMPT_BYTES: usize = 100_000;

/// Most entries of a keyterm or vocabulary list.
pub const MAX_TERMS: usize = 100;

/// Longest keyterm or vocabulary entry, in characters.
pub const MAX_TERM_CHARS: usize = 100;

/// The conversation languages the engine's segmenter and normalizer support.
pub const ENGINE_LANGUAGES: [&str; 2] = ["en", "tr"];

/// Collects issues with positions computed from the file text.
pub(crate) struct Issues<'a> {
    /// The file text (for line and column).
    text: &'a str,
    /// What was found so far.
    pub(crate) found: Vec<(usize, Issue)>,
}

impl<'a> Issues<'a> {
    /// An empty collection over `text`.
    pub(crate) fn new(text: &'a str) -> Self {
        Self {
            text,
            found: Vec::new(),
        }
    }

    /// Records a problem of `key` at `span`.
    pub(crate) fn at(&mut self, key: &str, span: Range<usize>, message: String) {
        self.found.push((
            span.start,
            Issue {
                key: key.to_owned(),
                position: Some(line_column(self.text, span.start)),
                message,
            },
        ));
    }

    /// Records a problem without a position (sorted after the positioned ones).
    pub(crate) fn general(&mut self, key: &str, message: String) {
        self.found.push((
            usize::MAX,
            Issue {
                key: key.to_owned(),
                position: None,
                message,
            },
        ));
    }

    /// The issues in file order.
    pub(crate) fn into_sorted(mut self) -> Vec<Issue> {
        self.found.sort_by_key(|(offset, _)| *offset);
        self.found.into_iter().map(|(_, issue)| issue).collect()
    }
}

/// Runs every cross-field check of `file` (read from `text`). `compiled` says whether a
/// provider's adapter is built into this binary (a parameter so tests can exercise the "not
/// compiled in" error in an all-features build).
pub(crate) fn check(
    file: &AgentFile,
    issues: &mut Issues<'_>,
    compiled: &dyn Fn(Provider) -> bool,
) {
    check_agent(file, issues);
    check_turn(file, issues);
    check_interruption(file, issues);
    check_audio(file, issues);
    check_filler(file, issues);
    check_telephony(file, issues);
    check_recording(file, issues);
    check_agent_link(file, issues);
    check_stt(file, issues, compiled);
    check_llm(file, issues, compiled);
    check_tts(file, issues, compiled);
}

/// The language a stage runs in: its own override, else the agent's.
fn stage_language<'f>(
    file: &'f AgentFile,
    own: Option<&'f Spanned<LanguageTag>>,
) -> &'f Spanned<LanguageTag> {
    own.unwrap_or(&file.agent.language)
}

/// `[agent]`.
fn check_agent(file: &AgentFile, issues: &mut Issues<'_>) {
    let agent = &file.agent;
    let language = agent.language.get_ref();
    if !ENGINE_LANGUAGES.contains(&language.primary().as_str()) {
        issues.at(
            "agent.language",
            agent.language.span(),
            format!(
                "`{language}` is not supported: the engine's segmenter and speech normalizer \
                 support \"en\" and \"tr\""
            ),
        );
    }
    let prompt = agent.system_prompt.get_ref();
    if prompt.trim().is_empty() {
        issues.at(
            "agent.system_prompt",
            agent.system_prompt.span(),
            "must not be empty".to_owned(),
        );
    } else if prompt.len() > MAX_SYSTEM_PROMPT_BYTES {
        issues.at(
            "agent.system_prompt",
            agent.system_prompt.span(),
            format!(
                "is {} bytes, more than the {MAX_SYSTEM_PROMPT_BYTES} allowed",
                prompt.len()
            ),
        );
    }
    let mut seen = BTreeSet::new();
    for (i, tool) in agent.tools.iter().enumerate() {
        if let Some(problem) = ident_problem(tool.get_ref()) {
            issues.at(&format!("agent.tools[{i}]"), tool.span(), problem);
        } else if !seen.insert(tool.get_ref().as_str()) {
            issues.at(
                &format!("agent.tools[{i}]"),
                tool.span(),
                format!("tool `{}` is listed twice", tool.get_ref()),
            );
        }
    }
    let mut held = BTreeSet::new();
    for (i, tool) in agent.confirm_tools.iter().enumerate() {
        let key = format!("agent.confirm_tools[{i}]");
        if !seen.contains(tool.get_ref().as_str()) {
            issues.at(
                &key,
                tool.span(),
                format!("tool `{}` is not listed in `agent.tools`", tool.get_ref()),
            );
        } else if !held.insert(tool.get_ref().as_str()) {
            issues.at(
                &key,
                tool.span(),
                format!("tool `{}` is listed twice", tool.get_ref()),
            );
        }
    }
}

/// `[turn]` and `[early_analysis]`.
fn check_turn(file: &AgentFile, issues: &mut Issues<'_>) {
    let turn = &file.turn;
    let policy = turn
        .policy
        .as_ref()
        .map_or(TurnPolicyKind::Adaptive, |p| *p.get_ref());
    match policy {
        TurnPolicyKind::FixedSilence => {
            if turn.silence_ms.is_none() {
                let span = turn.policy.as_ref().map_or(0..0, Spanned::span);
                issues.at(
                    "turn.silence_ms",
                    span,
                    "is required when turn.policy = \"fixed_silence\"".to_owned(),
                );
            }
            let only_adaptive = |key: &str, span: Option<Range<usize>>, issues: &mut Issues<'_>| {
                if let Some(span) = span {
                    issues.at(
                        key,
                        span,
                        "applies only to turn.policy = \"adaptive\" (the policy is \
                         \"fixed_silence\")"
                            .to_owned(),
                    );
                }
            };
            only_adaptive(
                "turn.fallback_silence_ms",
                turn.fallback_silence_ms.as_ref().map(Spanned::span),
                issues,
            );
            only_adaptive(
                "turn.require_final",
                turn.require_final.as_ref().map(Spanned::span),
                issues,
            );
            only_adaptive(
                "turn.final_grace_ms",
                turn.final_grace_ms.as_ref().map(Spanned::span),
                issues,
            );
            if let Some(enabled) = &file.early_analysis.enabled
                && *enabled.get_ref()
            {
                issues.at(
                    "early_analysis.enabled",
                    enabled.span(),
                    "early analysis runs Smart Turn early, so it needs turn.policy = \"adaptive\" \
                     (the policy is \"fixed_silence\")"
                        .to_owned(),
                );
            }
        }
        TurnPolicyKind::Adaptive => {
            if let Some(silence) = &turn.silence_ms {
                issues.at(
                    "turn.silence_ms",
                    silence.span(),
                    "applies only to turn.policy = \"fixed_silence\" (the policy is \
                     \"adaptive\"; use fallback_silence_ms)"
                        .to_owned(),
                );
            }
        }
    }
    let defaults = brisvox_engine::EngineConfig::DEFAULT;
    let final_wait = turn
        .final_wait_ms
        .as_ref()
        .map_or(defaults.final_wait, |m| m.get_ref().duration());
    let finalize = Timeouts::DEFAULT.finalize;
    if let Some(wait) = &turn.final_wait_ms
        && final_wait >= finalize
    {
        issues.at(
            "turn.final_wait_ms",
            wait.span(),
            format!(
                "must be below the STT finalize timeout ({} ms): the turn driver gives up \
                 before the STT session is restarted",
                finalize.as_millis()
            ),
        );
    }
    if let Some(deadline) = &turn.response_deadline_ms
        && deadline.get_ref().duration() <= final_wait
    {
        issues.at(
            "turn.response_deadline_ms",
            deadline.span(),
            format!(
                "must exceed turn.final_wait_ms ({} ms)",
                final_wait.as_millis()
            ),
        );
    }
}

/// `[interruption]`.
fn check_interruption(file: &AgentFile, issues: &mut Issues<'_>) {
    for (i, tag) in file.interruption.backchannels.iter().flatten().enumerate() {
        let name = tag.get_ref().to_ascii_lowercase();
        if !ENGINE_LANGUAGES.contains(&name.as_str()) {
            issues.at(
                &format!("interruption.backchannels[{i}]"),
                tag.span(),
                format!(
                    "no backchannel phrase set `{}` (available: \"en\", \"tr\")",
                    tag.get_ref().escape_debug()
                ),
            );
        }
    }
}

/// `[filler]`: one delay source only.
fn check_filler(file: &AgentFile, issues: &mut Issues<'_>) {
    let filler = &file.filler;
    let per_tool = filler.per_tool.as_ref().is_some_and(|b| *b.get_ref());
    if per_tool && let Some(delay) = &filler.delay_ms {
        issues.at(
            "filler.delay_ms",
            delay.span(),
            "filler.per_tool = true takes each tool's own delay, so filler.delay_ms has no \
             effect; remove one of them"
                .to_owned(),
        );
    }
}

/// Longest `[recording] retention_days`.
pub const MAX_RETENTION_DAYS: u32 = 3_650;

/// `[recording]`: a retention in range, and redaction that redacts something.
fn check_recording(file: &AgentFile, issues: &mut Issues<'_>) {
    let recording = &file.recording;
    if let Some(days) = &recording.retention_days
        && !(1..=MAX_RETENTION_DAYS).contains(days.get_ref())
    {
        issues.at(
            "recording.retention_days",
            days.span(),
            format!("must be 1 to {MAX_RETENTION_DAYS} days"),
        );
    }
    if let Some(kinds) = &recording.redact_kinds {
        if kinds.get_ref().is_empty() && recording.redact != Some(false) {
            issues.at(
                "recording.redact_kinds",
                kinds.span(),
                "an empty list redacts nothing; list the kinds or set recording.redact = false"
                    .to_owned(),
            );
        }
        if recording.redact == Some(false) {
            issues.at(
                "recording.redact_kinds",
                kinds.span(),
                "recording.redact = false turns redaction off, so redact_kinds has no effect; remove one of them"
                    .to_owned(),
            );
        }
    }
}

/// Most `[[agent_link.routes]]`.
pub const MAX_AGENT_ROUTES: usize = 32;

/// `[agent_link]`: WebSocket URLs, route names that fit a start message, each name once.
fn check_agent_link(file: &AgentFile, issues: &mut Issues<'_>) {
    let section = &file.agent_link;
    let url_ok = |url: &str| url.starts_with("ws://") || url.starts_with("wss://");
    if let Some(url) = &section.url
        && !url_ok(url.get_ref())
    {
        issues.at(
            "agent_link.url",
            url.span(),
            "must start with ws:// or wss://".to_owned(),
        );
    }
    if section.routes.len() > MAX_AGENT_ROUTES {
        issues.general(
            "agent_link.routes",
            format!(
                "{} routes: at most {MAX_AGENT_ROUTES}",
                section.routes.len()
            ),
        );
    }
    let mut seen: Vec<&str> = Vec::new();
    for (i, route) in section.routes.iter().enumerate() {
        let name = route.name.get_ref();
        let valid = !name.is_empty()
            && name.len() <= 32
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid {
            issues.at(
                &format!("agent_link.routes[{i}].name"),
                route.name.span(),
                "must be 1 to 32 letters, digits, `_` or `-`".to_owned(),
            );
        } else if seen.contains(&name.as_str()) {
            issues.at(
                &format!("agent_link.routes[{i}].name"),
                route.name.span(),
                format!("route {name:?} is defined twice"),
            );
        } else {
            seen.push(name);
        }
        if !url_ok(route.url.get_ref()) {
            issues.at(
                &format!("agent_link.routes[{i}].url"),
                route.url.span(),
                "must start with ws:// or wss://".to_owned(),
            );
        }
    }
}

/// Longest `[telephony.voicemail] message`, in characters.
pub const MAX_VOICEMAIL_CHARS: usize = 1000;

/// Most `[[telephony.destinations]]` (the model reads them all in the tool's description).
pub const MAX_DESTINATIONS: usize = 32;

/// `[telephony]`: every destination is a valid name with an E.164 number, each name once.
fn check_telephony(file: &AgentFile, issues: &mut Issues<'_>) {
    let destinations = &file.telephony.destinations;
    if destinations.len() > MAX_DESTINATIONS {
        issues.general(
            "telephony.destinations",
            format!(
                "{} destinations: at most {MAX_DESTINATIONS}",
                destinations.len()
            ),
        );
    }
    if let Some(voicemail) = &file.telephony.voicemail {
        let message = voicemail.message.get_ref();
        if message.trim().is_empty() {
            issues.at(
                "telephony.voicemail.message",
                voicemail.message.span(),
                "must not be empty".to_owned(),
            );
        } else if message.chars().count() > MAX_VOICEMAIL_CHARS {
            issues.at(
                "telephony.voicemail.message",
                voicemail.message.span(),
                format!(
                    "is {} characters, more than the {MAX_VOICEMAIL_CHARS} allowed (voicemail                      systems cut long recordings)",
                    message.chars().count()
                ),
            );
        }
    }
    let mut policy = TelephonyPolicy::new();
    for (i, d) in destinations.iter().enumerate() {
        match policy
            .clone()
            .destination(d.name.get_ref(), d.number.get_ref(), &d.description)
        {
            Ok(next) => policy = next,
            Err(PolicyError::InvalidNumber { reason, .. }) => issues.at(
                &format!("telephony.destinations[{i}].number"),
                d.number.span(),
                format!("not an E.164 number like +15550100: {reason}"),
            ),
            Err(e) => issues.at(
                &format!("telephony.destinations[{i}].name"),
                d.name.span(),
                e.to_string(),
            ),
        }
    }
}

/// Whether `frame_ms` is a whole number of samples at `rate`.
fn whole_samples(frame_ms: u64, rate: SampleRate) -> bool {
    frame_ms
        .saturating_mul(u64::from(rate.hz()))
        .is_multiple_of(1_000)
}

/// `[audio]`.
fn check_audio(file: &AgentFile, issues: &mut Issues<'_>) {
    let audio = &file.audio;
    let defaults = brisvox_engine::EngineRates::DEFAULT;
    let frame_ms = audio.frame_ms.as_ref().map_or(
        u64::try_from(defaults.frame.as_millis()).unwrap_or(20),
        |f| f.get_ref().get(),
    );
    let call = audio
        .call_sample_rate
        .as_ref()
        .map_or(defaults.fallback_call.input, |r| r.get_ref().0);
    for rate in [defaults.model, call] {
        if !whole_samples(frame_ms, rate) {
            let (key, span) = match (&audio.frame_ms, &audio.call_sample_rate) {
                (Some(frame), _) => ("audio.frame_ms", frame.span()),
                (None, Some(rate)) => ("audio.call_sample_rate", rate.span()),
                (None, None) => ("audio.frame_ms", 0..0),
            };
            issues.at(
                key,
                span,
                format!(
                    "a {frame_ms} ms frame is not a whole number of samples at {} Hz",
                    rate.hz()
                ),
            );
            break;
        }
    }
}

/// Reports `provider` when its adapter is not compiled in.
fn check_compiled(
    issues: &mut Issues<'_>,
    key: &str,
    span: Range<usize>,
    provider: Provider,
    compiled: &dyn Fn(Provider) -> bool,
) {
    if !compiled(provider) {
        issues.at(
            key,
            span,
            format!(
                "provider \"{}\" is not compiled into this server: rebuild with \
                 `--features {}` (or `all-providers`)",
                provider.name(),
                provider.feature()
            ),
        );
    }
}

/// Reports every options table that is set but belongs to a provider other than `chosen`.
fn check_tables(
    issues: &mut Issues<'_>,
    stage: &str,
    chosen: Provider,
    tables: &[(Provider, Option<Range<usize>>)],
) {
    for (provider, span) in tables {
        if let Some(span) = span
            && *provider != chosen
        {
            issues.at(
                &format!("{stage}.{}", provider.name()),
                span.clone(),
                format!(
                    "options for \"{}\" but {stage}.provider is \"{}\"",
                    provider.name(),
                    chosen.name()
                ),
            );
        }
    }
}

/// Checks a keyterm / vocabulary list.
fn check_terms(issues: &mut Issues<'_>, key: &str, terms: Option<&Spanned<Vec<String>>>) {
    let Some(terms) = terms else { return };
    let list = terms.get_ref();
    if list.len() > MAX_TERMS {
        issues.at(
            key,
            terms.span(),
            format!(
                "has {} entries, more than the {MAX_TERMS} allowed",
                list.len()
            ),
        );
    }
    if list
        .iter()
        .any(|t| t.trim().is_empty() || t.chars().count() > MAX_TERM_CHARS)
    {
        issues.at(
            key,
            terms.span(),
            format!("entries must be non-empty and at most {MAX_TERM_CHARS} characters"),
        );
    }
}

/// The span of an optional spanned value.
fn span_of<T>(value: Option<&Spanned<T>>) -> Option<Range<usize>> {
    value.map(Spanned::span)
}

/// Reports each key of a `[<stage>.secondary]` table that is set but belongs to the call (taken
/// from `[<stage>]`) or is not allowed twice. `set` lists the keys with their span, if they have
/// one (else the table's `outer` span is blamed).
fn check_secondary_fields(
    issues: &mut Issues<'_>,
    stage: &str,
    outer: Range<usize>,
    set: &[(&str, bool, Option<Range<usize>>)],
) {
    for (name, is_set, span) in set {
        if *is_set {
            let why = if *name == "secondary" {
                "a secondary provider cannot have a secondary of its own".to_owned()
            } else {
                format!("it is a setting of the call: it comes from [{stage}] and the agent")
            };
            issues.at(
                &format!("{stage}.secondary.{name}"),
                span.clone().unwrap_or_else(|| outer.clone()),
                format!("is not allowed in [{stage}.secondary]: {why}"),
            );
        }
    }
}

/// `[stt]` and `[stt.secondary]`.
fn check_stt(file: &AgentFile, issues: &mut Issues<'_>, compiled: &dyn Fn(Provider) -> bool) {
    let stt = &file.stt;
    let language = stage_language(file, stt.language.as_ref());
    check_stt_section(&file.stt, "stt", language, issues, compiled);
    if let Some(secondary) = &stt.secondary {
        let s = secondary.get_ref();
        check_secondary_fields(
            issues,
            "stt",
            secondary.span(),
            &[
                (
                    "language",
                    s.language.is_some(),
                    span_of(s.language.as_ref()),
                ),
                (
                    "secondary",
                    s.secondary.is_some(),
                    span_of(s.secondary.as_ref()),
                ),
            ],
        );
        // The secondary speaks the primary's language.
        check_stt_section(s, "stt.secondary", language, issues, compiled);
    }
}

/// One STT table (`[stt]`, or `[stt.secondary]` when `prefix` says so) running in `language`.
fn check_stt_section(
    stt: &SttSection,
    prefix: &str,
    language: &Spanned<LanguageTag>,
    issues: &mut Issues<'_>,
    compiled: &dyn Fn(Provider) -> bool,
) {
    let chosen = Provider::from(*stt.provider.get_ref());
    check_compiled(
        issues,
        &format!("{prefix}.provider"),
        stt.provider.span(),
        chosen,
        compiled,
    );
    check_tables(
        issues,
        prefix,
        chosen,
        &[
            (Provider::Deepgram, span_of(stt.deepgram.as_ref())),
            (Provider::AssemblyAi, span_of(stt.assemblyai.as_ref())),
            (Provider::Gladia, span_of(stt.gladia.as_ref())),
            (Provider::SherpaOnnx, span_of(stt.sherpa_onnx.as_ref())),
            (
                Provider::OpenAiRealtimeStt,
                span_of(stt.openai_realtime_stt.as_ref()),
            ),
        ],
    );
    // The hosted providers need their key; the self-hosted ones take one only if the server
    // (or its proxy) wants it.
    let key_required = match *stt.provider.get_ref() {
        SttProviderKind::Deepgram | SttProviderKind::Assemblyai | SttProviderKind::Gladia => true,
        SttProviderKind::SherpaOnnx => false,
        SttProviderKind::OpenaiRealtimeStt => stt.openai_realtime_stt.as_ref().is_none_or(|o| {
            o.get_ref()
                .dialect
                .as_ref()
                .is_none_or(|d| *d.get_ref() == SttDialect::Openai)
        }),
    };
    if key_required && stt.api_key_env.is_none() {
        issues.at(
            &format!("{prefix}.api_key_env"),
            stt.provider.span(),
            format!(
                "is required for provider \"{}\" (name the environment variable holding the \
                 key, e.g. api_key_env = \"{}\")",
                chosen.name(),
                chosen.example_env()
            ),
        );
    }
    let turkish = language.get_ref().primary() == "tr";
    match *stt.provider.get_ref() {
        SttProviderKind::Deepgram => {
            let options = stt.deepgram.as_ref().map(Spanned::get_ref);
            let model = options.and_then(|o| o.model.as_ref());
            check_terms(
                issues,
                &format!("{prefix}.deepgram.keyterms"),
                options.and_then(|o| o.keyterms.as_ref()),
            );
            // docs/providers/deepgram.md "Models and Turkish": Nova-3, Nova-2 and Base list
            // Turkish; Enhanced, Nova (1) and Flux do not.
            let model_name = model.map_or("nova-3", |m| m.get_ref().as_str());
            let turkish_model = ["nova-3", "nova-2", "base"]
                .iter()
                .any(|m| model_name == *m || model_name.starts_with(&format!("{m}-")));
            if turkish && !turkish_model {
                let (key, span) = model.map_or(("stt.language".to_owned(), language.span()), |m| {
                    (format!("{prefix}.deepgram.model"), m.span())
                });
                issues.at(
                    &key,
                    span,
                    format!(
                        "Deepgram model `{model_name}` does not support Turkish (nova-3, nova-2 \
                         and base do)"
                    ),
                );
            }
        }
        SttProviderKind::Assemblyai => {
            let options = stt.assemblyai.as_ref().map(Spanned::get_ref);
            check_terms(
                issues,
                &format!("{prefix}.assemblyai.keyterms"),
                options.and_then(|o| o.keyterms.as_ref()),
            );
            let model = options.and_then(|o| o.speech_model.as_ref());
            let model_name = model.map_or("universal-3-6-pro", |m| m.get_ref().as_str());
            // docs/providers/assemblyai.md "Turkish": only the Pro streaming models.
            if turkish && !model_name.ends_with("-pro") {
                let (key, span) = model.map_or(("stt.language".to_owned(), language.span()), |m| {
                    (format!("{prefix}.assemblyai.speech_model"), m.span())
                });
                issues.at(
                    &key,
                    span,
                    format!(
                        "AssemblyAI model `{model_name}` does not support Turkish in streaming \
                         (universal-3-6-pro and universal-3-5-pro do)"
                    ),
                );
            }
            if let Some(o) = options
                && let (Some(min), Some(max)) = (&o.min_turn_silence_ms, &o.max_turn_silence_ms)
                && min.get_ref().get() > max.get_ref().get()
            {
                issues.at(
                    &format!("{prefix}.assemblyai.max_turn_silence_ms"),
                    max.span(),
                    format!(
                        "must be at least min_turn_silence_ms ({} ms)",
                        min.get_ref().get()
                    ),
                );
            }
        }
        SttProviderKind::Gladia => {
            let options = stt.gladia.as_ref().map(Spanned::get_ref);
            check_terms(
                issues,
                &format!("{prefix}.gladia.vocabulary"),
                options.and_then(|o| o.vocabulary.as_ref()),
            );
        }
        SttProviderKind::SherpaOnnx => {
            // A model decides the language; the path must be one the adapter accepts and the
            // proxy token's transport must be safe.
            let options = stt.sherpa_onnx.as_ref().map(Spanned::get_ref);
            if let Some(path) = options.and_then(|o| o.path.as_ref())
                && !is_url_path(path.get_ref().as_str())
            {
                issues.at(
                    &format!("{prefix}.sherpa_onnx.path"),
                    path.span(),
                    "must start with '/' and hold only letters, digits, '.', '_' and '-' between \
                     the slashes"
                        .to_owned(),
                );
            }
            check_plain_ws_with_key(
                issues,
                &format!("{prefix}.sherpa_onnx.base_url"),
                options.and_then(|o| o.base_url.as_ref()),
                stt.api_key_env.is_some(),
            );
        }
        SttProviderKind::OpenaiRealtimeStt => {
            let table = stt.openai_realtime_stt.as_ref();
            let options = table.map(Spanned::get_ref);
            let dialect = options
                .and_then(|o| o.dialect.as_ref())
                .map_or(SttDialect::Openai, |d| *d.get_ref());
            if matches!(dialect, SttDialect::Speaches | SttDialect::Vllm)
                && options.is_none_or(|o| o.model.is_none())
            {
                issues.at(
                    &format!("{prefix}.openai_realtime_stt.model"),
                    table.map_or_else(|| stt.provider.span(), Spanned::span),
                    "is required for the speaches and vllm dialects (the model the server \
                     serves)"
                        .to_owned(),
                );
            }
            if let Some(model) = options.and_then(|o| o.model.as_ref())
                && !is_model_id(model.get_ref().as_str())
            {
                issues.at(
                    &format!("{prefix}.openai_realtime_stt.model"),
                    model.span(),
                    "must hold only letters, digits, '.', '_', '-', '/' and ':' (no empty parts)"
                        .to_owned(),
                );
            }
            if let Some(path) = options.and_then(|o| o.path_and_query.as_ref())
                && !is_path_and_query(path.get_ref().as_str())
            {
                issues.at(
                    &format!("{prefix}.openai_realtime_stt.path_and_query"),
                    path.span(),
                    "must start with '/' and hold no '//', spaces or '#'".to_owned(),
                );
            }
            if let Some(prompt) = options.and_then(|o| o.prompt.as_ref())
                && prompt.get_ref().len() > 4096
            {
                issues.at(
                    &format!("{prefix}.openai_realtime_stt.prompt"),
                    prompt.span(),
                    "must be at most 4096 bytes".to_owned(),
                );
            }
            check_plain_ws_with_key(
                issues,
                &format!("{prefix}.openai_realtime_stt.base_url"),
                options.and_then(|o| o.base_url.as_ref()),
                stt.api_key_env.is_some(),
            );
        }
    }
}

/// A URL-safe word: ASCII letters, digits, `.`, `_` and `-` (the adapters' `is_token`).
fn is_word(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A path the sherpa-onnx adapter accepts: `/`, then words separated by slashes.
fn is_url_path(path: &str) -> bool {
    path.starts_with('/') && path.split('/').all(|part| part.is_empty() || is_word(part))
}

/// A model id the realtime transcription adapter accepts: words separated by `/` or `:`.
fn is_model_id(model: &str) -> bool {
    model.split(['/', ':']).all(is_word)
}

/// A path and query the realtime transcription adapter accepts.
fn is_path_and_query(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains("//")
        && !path
            .chars()
            .any(|c| c.is_whitespace() || c == '#' || c.is_control())
}

/// A key or token is never sent in plain text to another machine: with `has_key`, a `ws://`
/// `base_url` must be a loopback host (the adapters apply the same rule when they are built).
fn check_plain_ws_with_key(
    issues: &mut Issues<'_>,
    key: &str,
    url: Option<&LocalWsUrl>,
    has_key: bool,
) {
    let Some(url) = url else { return };
    let Some(rest) = url.as_str().strip_prefix("ws://") else {
        return;
    };
    // `LocalWsUrl` is scheme + `host[:port]` only; `url_host` reads a bracketed IPv6 literal
    // (`[::1]:6006`) correctly, which a split at the last ':' did not.
    let loopback = brisvox_providers::loopback::is_loopback_host(url_host(rest));
    if has_key && !loopback {
        issues.general(
            key,
            "plain ws:// to another machine would send the api_key unencrypted: use wss:// or \
             drop api_key_env"
                .to_owned(),
        );
    }
}

/// `[llm]` and `[llm.secondary]`.
fn check_llm(file: &AgentFile, issues: &mut Issues<'_>, compiled: &dyn Fn(Provider) -> bool) {
    let llm = &file.llm;
    check_llm_section(llm, "llm", issues, compiled);
    if let Some(secondary) = &llm.secondary {
        let s = secondary.get_ref();
        // The turn loop's limits are the call's, read from the primary's table.
        check_secondary_fields(
            issues,
            "llm",
            secondary.span(),
            &[
                ("max_tool_rounds", s.max_tool_rounds.is_some(), None),
                (
                    "context_window_tokens",
                    s.context_window_tokens.is_some(),
                    None,
                ),
                (
                    "secondary",
                    s.secondary.is_some(),
                    span_of(s.secondary.as_ref()),
                ),
            ],
        );
        check_llm_section(s, "llm.secondary", issues, compiled);
    }
}

/// One LLM table (`[llm]`, or `[llm.secondary]` when `prefix` says so).
fn check_llm_section(
    llm: &LlmSection,
    prefix: &str,
    issues: &mut Issues<'_>,
    compiled: &dyn Fn(Provider) -> bool,
) {
    let chosen = Provider::from(*llm.provider.get_ref());
    check_compiled(
        issues,
        &format!("{prefix}.provider"),
        llm.provider.span(),
        chosen,
        compiled,
    );
    check_tables(
        issues,
        prefix,
        chosen,
        &[
            (Provider::OpenAi, span_of(llm.openai.as_ref())),
            (Provider::Anthropic, span_of(llm.anthropic.as_ref())),
            (Provider::Gemini, span_of(llm.gemini.as_ref())),
            (
                Provider::OpenAiCompatible,
                span_of(llm.openai_compatible.as_ref()),
            ),
        ],
    );
    let kind = *llm.provider.get_ref();
    if kind != LlmProviderKind::OpenaiCompatible && llm.api_key_env.is_none() {
        issues.at(
            &format!("{prefix}.api_key_env"),
            llm.provider.span(),
            format!(
                "is required for provider \"{}\" (name the environment variable holding the \
                 key, e.g. api_key_env = \"{}\")",
                chosen.name(),
                chosen.example_env()
            ),
        );
    }
    if kind == LlmProviderKind::OpenaiCompatible {
        match &llm.openai_compatible {
            None => issues.at(
                &format!("{prefix}.openai_compatible"),
                llm.provider.span(),
                "is required for provider \"openai_compatible\": set `preset`, or `base_url` and \
                 `model`"
                    .to_owned(),
            ),
            Some(options) => {
                let o = options.get_ref();
                if o.preset.is_none() && (o.base_url.is_none() || o.model.is_none()) {
                    issues.at(
                        &format!("{prefix}.openai_compatible"),
                        options.span(),
                        "needs a `preset`, or both `base_url` and `model`".to_owned(),
                    );
                }
                let openrouter = o.preset == Some(CompatiblePreset::OpenRouter)
                    || o.base_url
                        .as_ref()
                        .is_some_and(|url| is_openrouter_host(url.as_str()));
                // OpenRouter's request field: any other server ignores it or refuses it, so a
                // routing pin that is not on an OpenRouter endpoint would silently not pin.
                for (key, span, set) in [
                    (
                        "openrouter_order",
                        o.openrouter_order
                            .as_ref()
                            .map_or_else(|| options.span(), Spanned::span),
                        o.openrouter_order.is_some(),
                    ),
                    (
                        "openrouter_allow_fallbacks",
                        options.span(),
                        o.openrouter_allow_fallbacks.is_some(),
                    ),
                ] {
                    if set && !openrouter {
                        issues.at(
                            &format!("{prefix}.openai_compatible.{key}"),
                            span,
                            "is OpenRouter-only: set `preset = \"openrouter\"` or a `base_url` on openrouter.ai, or remove it"
                                .to_owned(),
                        );
                    }
                }
                check_terms(
                    issues,
                    &format!("{prefix}.openai_compatible.openrouter_order"),
                    o.openrouter_order.as_ref(),
                );
                if o.openrouter_allow_fallbacks.is_some() && o.openrouter_order.is_none() {
                    issues.at(
                        &format!("{prefix}.openai_compatible.openrouter_allow_fallbacks"),
                        options.span(),
                        "is only sent together with `openrouter_order`".to_owned(),
                    );
                }
            }
        }
    }
}

/// Whether `url` points at OpenRouter: the host is `openrouter.ai` or a subdomain of it.
///
/// A backslash is rejected outright: URL parsers that follow the WHATWG rules read `\` as `/`,
/// so `https://evil.example\@openrouter.ai/` would reach `evil.example` while this check, which
/// splits the authority at `/`, would see the user info `evil.example\` and the host
/// `openrouter.ai`.
fn is_openrouter_host(url: &str) -> bool {
    if url.contains('\\') {
        return false;
    }
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = url_host(rest).trim_end_matches('.').to_ascii_lowercase();
    host == "openrouter.ai" || host.ends_with(".openrouter.ai")
}

/// Rates Deepgram Aura delivers as PCM16 among the engine's [`SampleRate`]s
/// (docs/providers/deepgram-aura.md: linear16 at 8000, 16000, 24000, 32000, 48000).
const AURA_RATES: [SampleRate; 4] = [
    SampleRate::Hz8000,
    SampleRate::Hz16000,
    SampleRate::Hz24000,
    SampleRate::Hz48000,
];

/// Languages with an Aura-2 voice (the adapter's `SUPPORTED_LANGUAGES`, repeated here so the
/// check also runs in a build without the adapter; a test keeps them equal).
pub(crate) const AURA_LANGUAGES: [&str; 7] = ["en", "es", "de", "fr", "nl", "it", "ja"];

/// `[tts]` and `[tts.secondary]`.
fn check_tts(file: &AgentFile, issues: &mut Issues<'_>, compiled: &dyn Fn(Provider) -> bool) {
    let tts = &file.tts;
    let language = stage_language(file, tts.language.as_ref());
    let language_key = if tts.language.is_some() {
        "tts.language"
    } else {
        "agent.language"
    };
    let rate = tts.sample_rate.as_ref();
    check_tts_section(tts, "tts", (language, language_key), rate, issues, compiled);
    if let Some(secondary) = &tts.secondary {
        let s = secondary.get_ref();
        check_secondary_fields(
            issues,
            "tts",
            secondary.span(),
            &[
                (
                    "sample_rate",
                    s.sample_rate.is_some(),
                    span_of(s.sample_rate.as_ref()),
                ),
                (
                    "language",
                    s.language.is_some(),
                    span_of(s.language.as_ref()),
                ),
                (
                    "secondary",
                    s.secondary.is_some(),
                    span_of(s.secondary.as_ref()),
                ),
            ],
        );
        // The secondary delivers the primary's rate, in the primary's language.
        check_tts_section(
            s,
            "tts.secondary",
            (language, language_key),
            rate,
            issues,
            compiled,
        );
    }
}

/// One TTS table (`[tts]`, or `[tts.secondary]` when `prefix` says so) speaking `language`
/// (with the key to blame for it) and asked for `rate` (the call's: the primary's).
fn check_tts_section(
    tts: &TtsSection,
    prefix: &str,
    (language, language_key): (&Spanned<LanguageTag>, &str),
    rate: Option<&Spanned<Rate>>,
    issues: &mut Issues<'_>,
    compiled: &dyn Fn(Provider) -> bool,
) {
    let chosen = Provider::from(*tts.provider.get_ref());
    check_compiled(
        issues,
        &format!("{prefix}.provider"),
        tts.provider.span(),
        chosen,
        compiled,
    );
    check_tables(
        issues,
        prefix,
        chosen,
        &[
            (Provider::Cartesia, span_of(tts.cartesia.as_ref())),
            (Provider::ElevenLabs, span_of(tts.elevenlabs.as_ref())),
            (Provider::DeepgramAura, span_of(tts.deepgram_aura.as_ref())),
            (Provider::OpenAiSpeech, span_of(tts.openai_speech.as_ref())),
        ],
    );
    if *tts.provider.get_ref() != TtsProviderKind::OpenaiSpeech && tts.api_key_env.is_none() {
        issues.at(
            &format!("{prefix}.api_key_env"),
            tts.provider.span(),
            format!(
                "is required for provider \"{}\" (name the environment variable holding the \
                 key, e.g. api_key_env = \"{}\")",
                chosen.name(),
                chosen.example_env()
            ),
        );
    }
    let primary = language.get_ref().primary();
    let subject = if prefix == "tts" {
        "Deepgram Aura"
    } else {
        "the secondary Deepgram Aura"
    };
    match *tts.provider.get_ref() {
        TtsProviderKind::DeepgramAura => {
            if !AURA_LANGUAGES.contains(&primary.as_str()) {
                issues.at(
                    language_key,
                    language.span(),
                    format!(
                        "Deepgram Aura has no voice for `{}` (it speaks {}); use Cartesia or \
                         ElevenLabs for this language",
                        language.get_ref(),
                        AURA_LANGUAGES.join(", ")
                    ),
                );
            } else {
                // The Aura voice id carries its language as the last `-` part
                // (`aura-2-thalia-en`).
                let voice = tts.voice.get_ref().as_str();
                let suffix = voice
                    .rsplit(['-', '_'])
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if AURA_LANGUAGES.contains(&suffix.as_str()) && suffix != primary {
                    issues.at(
                        &format!("{prefix}.voice"),
                        tts.voice.span(),
                        format!(
                            "voice `{voice}` speaks `{suffix}` but the TTS language is `{}`",
                            language.get_ref()
                        ),
                    );
                }
            }
            if let Some(rate) = rate
                && !AURA_RATES.contains(&rate.get_ref().0)
            {
                issues.at(
                    "tts.sample_rate",
                    rate.span(),
                    format!(
                        "{subject} does not deliver PCM16 at {} Hz (8000, 16000, 24000 or \
                         48000)",
                        rate.get_ref().0.hz()
                    ),
                );
            }
        }
        TtsProviderKind::Elevenlabs => {
            let model = tts
                .elevenlabs
                .as_ref()
                .and_then(|o| o.get_ref().model.as_ref());
            // docs/providers/elevenlabs.md: `eleven_flash_v2` is English only.
            if let Some(model) = model
                && model.get_ref().as_str() == "eleven_flash_v2"
                && primary != "en"
            {
                issues.at(
                    &format!("{prefix}.elevenlabs.model"),
                    model.span(),
                    format!(
                        "`eleven_flash_v2` is English only but the TTS language is `{}` (use \
                         eleven_flash_v2_5)",
                        language.get_ref()
                    ),
                );
            }
        }
        TtsProviderKind::Cartesia => {}
        TtsProviderKind::OpenaiSpeech => check_openai_speech(tts, prefix, rate, issues),
    }
}

/// Rate of the OpenAI `pcm` format, which Kokoro-FastAPI, vLLM-Omni and (after the WAV header
/// check) Chatterbox-TTS-Server deliver.
const SPEECH_SERVER_RATE: SampleRate = SampleRate::Hz24000;

/// Longest `tts.openai_speech.warm_text`, in bytes (a test keeps it equal to the adapter's).
const MAX_WARM_TEXT_BYTES: usize = 200;

/// `[tts.openai_speech]`: the table with a `base_url` (and a `model` for the generic preset), a
/// server rate the engine can take, and no API key over plain HTTP to a remote host.
fn check_openai_speech(
    tts: &TtsSection,
    prefix: &str,
    rate: Option<&Spanned<Rate>>,
    issues: &mut Issues<'_>,
) {
    use super::schema::SpeechPreset;

    let key = format!("{prefix}.openai_speech");
    let Some(options) = &tts.openai_speech else {
        issues.at(
            &key,
            tts.provider.span(),
            "is required for provider \"openai_speech\": set `base_url` (for example \
             \"http://localhost:8880/v1\") and a `preset` or a `model`"
                .to_owned(),
        );
        return;
    };
    let o = options.get_ref();
    let preset = o.preset.unwrap_or(SpeechPreset::Generic);
    let Some(base_url) = &o.base_url else {
        issues.at(
            &key,
            options.span(),
            "needs `base_url`, the server's API base including /v1".to_owned(),
        );
        return;
    };
    if preset == SpeechPreset::Generic && o.model.is_none() {
        issues.at(
            &key,
            options.span(),
            "the `generic` preset needs a `model` (or pick a preset: kokoro, chatterbox, \
             speaches, vllm_omni)"
                .to_owned(),
        );
    }
    if let Some(text) = &o.warm_text
        && text.get_ref().len() > MAX_WARM_TEXT_BYTES
    {
        issues.at(
            &format!("{key}.warm_text"),
            text.span(),
            format!(
                "must be at most {} bytes (\"\" turns the warm-up off)",
                MAX_WARM_TEXT_BYTES
            ),
        );
    }
    if tts.api_key_env.is_some() && base_url.is_remote_plain_http() {
        issues.at(
            &format!("{key}.base_url"),
            options.span(),
            "an API key would be sent unencrypted over http:// to a remote host: use https://, \
             or drop api_key_env if the server needs none"
                .to_owned(),
        );
    }
    if preset != SpeechPreset::Speaches
        && let Some(rate) = rate
        && rate.get_ref().0 != SPEECH_SERVER_RATE
    {
        issues.at(
            "tts.sample_rate",
            rate.span(),
            format!(
                "the OpenAI speech protocol delivers 24000 Hz, not {} Hz (omit tts.sample_rate: \
                 the engine resamples to the call; the `speaches` preset can resample on the \
                 server)",
                rate.get_ref().0.hz()
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_openrouter_host_check_accepts_only_the_real_host() {
        for yes in [
            "https://openrouter.ai/api/v1",
            "https://OpenRouter.ai./api/v1",
            "https://eu.openrouter.ai/api/v1",
            "https://user@openrouter.ai:443/api/v1",
        ] {
            assert!(is_openrouter_host(yes), "{yes}");
        }
        for no in [
            "https://api.example.com/v1",
            "https://openrouter.ai.evil.example/v1",
            "https://evilopenrouter.ai/v1",
            "https://openrouter.ai@evil.example/v1",
            // WHATWG parsers read the backslash as a slash: this URL goes to evil.example.
            "https://evil.example\\@openrouter.ai/api/v1",
            "https://openrouter.ai\\.evil.example/v1",
        ] {
            assert!(!is_openrouter_host(no), "{no}");
        }
    }

    #[test]
    fn frame_sample_alignment() {
        assert!(whole_samples(20, SampleRate::Hz8000));
        assert!(whole_samples(20, SampleRate::Hz22050) == (20 * 22_050 % 1000 == 0));
        assert!(!whole_samples(5, SampleRate::Hz22050));
    }

    #[cfg(feature = "provider-openai-speech")]
    #[test]
    fn warm_text_limit_matches_the_adapter() {
        assert_eq!(
            MAX_WARM_TEXT_BYTES,
            brisvox_providers::openai_speech::MAX_WARM_TEXT_BYTES
        );
    }

    #[cfg(feature = "provider-deepgram-aura")]
    #[test]
    fn aura_language_list_matches_the_adapter() {
        assert_eq!(
            AURA_LANGUAGES,
            brisvox_providers::deepgram_aura::SUPPORTED_LANGUAGES
        );
    }
}
