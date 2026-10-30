//! Pure mappings from a validated agent definition onto the engine's [`EngineConfig`] and the
//! provider adapters' configuration structs. Absent settings keep the engine's and the
//! adapters' own defaults (they start from `EngineConfig::DEFAULT` and `XxxConfig::new`).
//!
//! The functions expect a definition that passed validation: they never fail, and a value a
//! check would have rejected is mapped as given (the engine's and the adapters' own validation
//! still run when the session and the providers are built).
//!
//! Languages: Deepgram STT gets the tag as written (it accepts `tr` and `tr-TR`); AssemblyAI,
//! Gladia, Cartesia and ElevenLabs get the primary subtag (`tr`), the form their APIs document.

use std::time::Duration;

use brisvox_agent::tools::FillerConfig;
use brisvox_agent::{Confirm, Language, TurnPolicy};
use brisvox_engine::{EarlyAnalysisConfig, EngineConfig, InterruptionConfig, SpeculationConfig};
use brisvox_transport::CallRates;

use super::schema::{
    AgentFile, ConfirmKind, FillerSection, InterruptionPreset, InterruptionSection, TurnPolicyKind,
    TurnSection,
};
use super::types::LanguageTag;

#[cfg(any(
    feature = "provider-deepgram",
    feature = "provider-assemblyai",
    feature = "provider-gladia",
    feature = "provider-sherpa-onnx",
    feature = "provider-openai-realtime-stt",
    feature = "provider-openai",
    feature = "provider-anthropic",
    feature = "provider-gemini",
    feature = "provider-openai-compatible",
    feature = "provider-cartesia",
    feature = "provider-elevenlabs",
    feature = "provider-deepgram-aura",
    feature = "provider-openai-speech",
))]
use brisvox_core::Secret;

/// Fixed-silence turn length used when a `fixed_silence` policy has no `silence_ms` (validation
/// rejects that; this only keeps the mapping total).
const FALLBACK_FIXED_SILENCE: Duration = Duration::from_millis(800);

/// The engine's language for a conversation language tag: Turkish for `tr`, else English.
pub fn engine_language(tag: &LanguageTag) -> Language {
    if tag.primary() == "tr" {
        Language::Tr
    } else {
        Language::En
    }
}

/// The [`EngineConfig`] a definition describes.
pub fn engine_config(file: &AgentFile) -> EngineConfig {
    let mut config = EngineConfig::DEFAULT;
    config.turn = turn_policy(&file.turn);
    if let Some(wait) = &file.turn.final_wait_ms {
        config.final_wait = wait.get_ref().duration();
    }
    if let Some(deadline) = &file.turn.response_deadline_ms {
        config.response_deadline = deadline.get_ref().duration();
    }
    config.interruption = interruption(&file.interruption);

    let s = &file.speculation;
    let mut speculation = SpeculationConfig::OFF;
    speculation.enabled = s.enabled.unwrap_or(speculation.enabled);
    speculation.on_partial = s.on_partial.unwrap_or(speculation.on_partial);
    if let Some(stable) = s.partial_stable_ms {
        speculation.partial_stable_for = stable.duration();
    }
    if let Some(n) = s.max_per_turn {
        speculation.max_per_turn = n.get();
    }
    if let Some(age) = s.max_age_ms {
        speculation.max_age = age.duration();
    }
    if let Some(n) = s.max_buffered_events {
        speculation.max_buffered_events = n.as_usize();
    }
    config.speculation = speculation;

    let e = &file.early_analysis;
    let mut early = EarlyAnalysisConfig::OFF;
    early.enabled = e.enabled.as_ref().is_some_and(|b| *b.get_ref());
    if let Some(delay) = e.fall_delay_ms {
        early.fall_delay = delay.duration();
    }
    if let Some(tail) = e.max_tail_ms {
        early.max_tail = tail.duration();
    }
    if let Some(n) = e.max_per_turn {
        early.max_per_turn = n.get();
    }
    config.early_analysis = early;

    if let Some(frame) = &file.audio.frame_ms {
        config.rates.frame = frame.get_ref().duration();
    }
    if let Some(rate) = &file.audio.call_sample_rate {
        config.rates.fallback_call = CallRates::symmetric(rate.get_ref().0);
    }
    if let Some(rate) = &file.tts.sample_rate {
        config.rates.tts = rate.get_ref().0;
    }

    config.llm.filler = filler(&file.filler);

    if let Some(rounds) = file.llm.max_tool_rounds {
        config.llm.max_tool_rounds = rounds.get();
    }
    if let Some(tokens) = file.llm.context_window_tokens {
        config.llm.window.max_tokens = tokens.as_usize();
    }
    config
}

/// The filler settings of `[filler]`: on with the fixed 1.2 s by default
/// ([`FillerConfig::DEFAULT`]); `per_tool = true` takes each tool's own delay.
fn filler(section: &FillerSection) -> FillerConfig {
    let mut filler = FillerConfig::DEFAULT;
    filler.enabled = section.enabled.unwrap_or(filler.enabled);
    if section.per_tool.as_ref().is_some_and(|b| *b.get_ref()) {
        filler.delay = None;
    } else if let Some(delay) = &section.delay_ms {
        filler.delay = Some(delay.get_ref().duration());
    }
    filler
}

/// The turn policy of `[turn]`.
fn turn_policy(turn: &TurnSection) -> TurnPolicy {
    let kind = turn
        .policy
        .as_ref()
        .map_or(TurnPolicyKind::Adaptive, |p| *p.get_ref());
    match kind {
        TurnPolicyKind::FixedSilence => TurnPolicy::fixed(
            turn.silence_ms
                .as_ref()
                .map_or(FALLBACK_FIXED_SILENCE, |s| s.get_ref().duration()),
        ),
        TurnPolicyKind::Adaptive => {
            let TurnPolicy::Adaptive {
                fallback_silence,
                require_final,
                final_grace,
            } = TurnPolicy::adaptive()
            else {
                return TurnPolicy::adaptive();
            };
            TurnPolicy::Adaptive {
                fallback_silence: turn
                    .fallback_silence_ms
                    .as_ref()
                    .map_or(fallback_silence, |m| m.get_ref().duration()),
                require_final: turn
                    .require_final
                    .as_ref()
                    .map_or(require_final, |b| *b.get_ref()),
                final_grace: turn
                    .final_grace_ms
                    .as_ref()
                    .map_or(final_grace, |m| m.get_ref().duration()),
            }
        }
    }
}

/// The barge-in guards of `[interruption]`: the preset, then the overrides.
fn interruption(section: &InterruptionSection) -> InterruptionConfig {
    let mut c = match section.preset {
        Some(InterruptionPreset::Off) => InterruptionConfig::OFF,
        Some(InterruptionPreset::Recommended) | None => InterruptionConfig::RECOMMENDED,
    };
    if let Some(ms) = section.min_speech_ms {
        c.min_speech = ms.duration();
    }
    if let Some(n) = section.min_words {
        c.min_words = n.get();
    }
    if let Some(confirm) = section.confirm {
        c.confirm = match confirm {
            ConfirmKind::All => Confirm::All,
            ConfirmKind::Any => Confirm::Any,
        };
    }
    if let Some(ms) = section.echo_guard_ms {
        c.echo_guard = ms.duration();
    }
    if let Some(n) = section.echo_min_words {
        c.echo_min_words = n.get();
    }
    if let Some(sets) = &section.backchannels {
        c.english = sets.iter().any(|t| t.get_ref().eq_ignore_ascii_case("en"));
        c.turkish = sets.iter().any(|t| t.get_ref().eq_ignore_ascii_case("tr"));
    }
    if let Some(ms) = section.backchannel_wait_ms {
        c.backchannel_wait = ms.duration();
    }
    if let Some(ms) = section.backchannel_max_speech_ms {
        c.backchannel_max_speech = ms.duration();
    }
    if let Some(ms) = section.vad_hangover_ms {
        c.vad_hangover = ms.duration();
    }
    c
}

/// Deepgram STT: `[stt.deepgram]` over [`DeepgramConfig::new`].
///
/// [`DeepgramConfig::new`]: brisvox_providers::deepgram::DeepgramConfig::new
#[cfg(feature = "provider-deepgram")]
pub fn deepgram(
    options: Option<&super::schema::DeepgramOptions>,
    api_key: Secret<String>,
    language: &LanguageTag,
) -> brisvox_providers::deepgram::DeepgramConfig {
    use super::schema::DeepgramRegion;
    use brisvox_providers::deepgram::{DeepgramConfig, Endpointing, Region};

    let mut c = DeepgramConfig::new(api_key);
    c.language = Some(language.as_str().to_owned());
    let Some(o) = options else { return c };
    if let Some(model) = &o.model {
        c.model = model.get_ref().as_str().to_owned();
    }
    if let Some(ms) = o.endpointing_ms {
        c.endpointing = match u32::try_from(ms.get()) {
            Ok(0) => Endpointing::Off,
            Ok(ms) => Endpointing::Ms(ms),
            Err(_) => Endpointing::Ms(u32::MAX),
        };
    }
    if let Some(ms) = o.utterance_end_ms {
        c.utterance_end_ms = u32::try_from(ms.get()).ok();
    }
    if let Some(on) = o.smart_format {
        c.smart_format = on;
    }
    if let Some(terms) = &o.keyterms {
        c.keyterms.clone_from(terms.get_ref());
    }
    if let Some(region) = o.region {
        c.region = match region {
            DeepgramRegion::Global => Region::Global,
            DeepgramRegion::Eu => Region::Eu,
            DeepgramRegion::Au => Region::Au,
            DeepgramRegion::India => Region::India,
        };
    }
    if let Some(url) = &o.base_url {
        c.base_url = Some(url.as_str().to_owned());
    }
    c
}

/// AssemblyAI STT: `[stt.assemblyai]` over [`AssemblyAiConfig::new`].
///
/// [`AssemblyAiConfig::new`]: brisvox_providers::assemblyai::AssemblyAiConfig::new
#[cfg(feature = "provider-assemblyai")]
pub fn assemblyai(
    options: Option<&super::schema::AssemblyAiOptions>,
    api_key: Secret<String>,
    language: &LanguageTag,
) -> brisvox_providers::assemblyai::AssemblyAiConfig {
    use super::schema::AssemblyAiRegion;
    use brisvox_providers::assemblyai::{AssemblyAiConfig, Region};

    let mut c = AssemblyAiConfig::new(api_key);
    c.language = Some(language.primary());
    let Some(o) = options else { return c };
    if let Some(model) = &o.speech_model {
        c.speech_model = model.get_ref().as_str().to_owned();
    }
    if let Some(region) = o.region {
        c.region = match region {
            AssemblyAiRegion::Edge => Region::Edge,
            AssemblyAiRegion::Us => Region::Us,
            AssemblyAiRegion::Eu => Region::Eu,
        };
    }
    if let Some(terms) = &o.keyterms {
        c.keyterms.clone_from(terms.get_ref());
    }
    if let Some(on) = o.format_turns {
        c.format_turns = on;
    }
    if let Some(ms) = &o.min_turn_silence_ms {
        c.min_turn_silence_ms = u32::try_from(ms.get_ref().get()).ok();
    }
    if let Some(ms) = &o.max_turn_silence_ms {
        c.max_turn_silence_ms = u32::try_from(ms.get_ref().get()).ok();
    }
    if let Some(url) = &o.base_url {
        c.base_url = Some(url.as_str().to_owned());
    }
    c
}

/// Gladia STT: `[stt.gladia]` over [`GladiaConfig::new`].
///
/// [`GladiaConfig::new`]: brisvox_providers::gladia::GladiaConfig::new
#[cfg(feature = "provider-gladia")]
pub fn gladia(
    options: Option<&super::schema::GladiaOptions>,
    api_key: Secret<String>,
    language: &LanguageTag,
) -> brisvox_providers::gladia::GladiaConfig {
    use super::schema::GladiaRegion;
    use brisvox_providers::gladia::{GladiaConfig, Region};

    let mut c = GladiaConfig::new(api_key);
    c.language = Some(language.primary());
    let Some(o) = options else { return c };
    if let Some(model) = &o.model {
        c.model = model.get_ref().as_str().to_owned();
    }
    if let Some(region) = o.region {
        c.region = Some(match region {
            GladiaRegion::UsWest => Region::UsWest,
            GladiaRegion::EuWest => Region::EuWest,
        });
    }
    if let Some(on) = o.code_switching {
        c.code_switching = on;
    }
    if let Some(words) = &o.vocabulary {
        c.vocabulary.clone_from(words.get_ref());
    }
    if let Some(ms) = o.endpointing_ms {
        // Whole milliseconds up to 10 000: exact in f64.
        #[allow(clippy::cast_precision_loss)]
        let seconds = ms.get() as f64 / 1000.0;
        c.endpointing = seconds;
    }
    if let Some(url) = &o.base_url {
        c.base_url = Some(url.as_str().to_owned());
    }
    c
}

/// sherpa-onnx STT: `[stt.sherpa_onnx]` over [`SherpaOnnxConfig::new`]. The key, if the section
/// names one, is a bearer token for a reverse proxy in front of the server.
///
/// [`SherpaOnnxConfig::new`]: brisvox_providers::sherpa_onnx::SherpaOnnxConfig::new
#[cfg(feature = "provider-sherpa-onnx")]
pub fn sherpa_onnx(
    options: Option<&super::schema::SherpaOnnxOptions>,
    bearer: Option<Secret<String>>,
) -> brisvox_providers::sherpa_onnx::SherpaOnnxConfig {
    use brisvox_providers::sherpa_onnx::SherpaOnnxConfig;

    let mut c = SherpaOnnxConfig::new();
    c.bearer = bearer;
    let Some(o) = options else { return c };
    if let Some(url) = &o.base_url {
        c.base_url = url.as_str().to_owned();
    }
    if let Some(path) = &o.path {
        c.path = path.get_ref().as_str().to_owned();
    }
    if let Some(rate) = o.sample_rate {
        c.sample_rate = rate.0;
    }
    if let Some(ms) = o.finalize_timeout_ms {
        c.finalize_timeout = ms.duration();
    }
    if let Some(ms) = o.finalize_padding_ms {
        c.finalize_padding = ms.duration();
    }
    c
}

/// OpenAI-Realtime-transcription STT: `[stt.openai_realtime_stt]` over
/// [`OpenAiRealtimeSttConfig::new`]. The key is optional (a local server runs without one).
///
/// [`OpenAiRealtimeSttConfig::new`]: brisvox_providers::openai_realtime_stt::OpenAiRealtimeSttConfig::new
#[cfg(feature = "provider-openai-realtime-stt")]
pub fn openai_realtime_stt(
    options: Option<&super::schema::OpenAiRealtimeSttOptions>,
    api_key: Option<Secret<String>>,
) -> brisvox_providers::openai_realtime_stt::OpenAiRealtimeSttConfig {
    use super::schema::SttDialect;
    use brisvox_providers::openai_realtime_stt::{Dialect, OpenAiRealtimeSttConfig};

    let dialect = options
        .and_then(|o| o.dialect.as_ref())
        .map_or(Dialect::OpenAi, |d| match *d.get_ref() {
            SttDialect::Openai => Dialect::OpenAi,
            SttDialect::Nim => Dialect::Nim,
            SttDialect::Speaches => Dialect::Speaches,
            SttDialect::Vllm => Dialect::Vllm,
        });
    let mut c = OpenAiRealtimeSttConfig::new(dialect);
    c.api_key = api_key;
    let Some(o) = options else { return c };
    if let Some(url) = &o.base_url {
        c.base_url = Some(url.as_str().to_owned());
    }
    if let Some(path) = &o.path_and_query {
        c.path_and_query = Some(path.get_ref().as_str().to_owned());
    }
    if let Some(model) = &o.model {
        c.model = Some(model.get_ref().as_str().to_owned());
    }
    if let Some(prompt) = &o.prompt {
        c.prompt = Some(prompt.get_ref().clone());
    }
    if let Some(on) = o.punctuation {
        c.punctuation = on;
    }
    if let Some(on) = o.delta_replaces {
        c.delta_replaces = Some(on);
    }
    if let Some(ms) = o.min_commit_ms {
        c.min_commit = Some(ms.duration());
    }
    if let Some(ms) = o.finalize_timeout_ms {
        c.finalize_timeout = ms.duration();
    }
    if let Some(rate) = o.sample_rate {
        c.sample_rate = Some(rate.0);
    }
    c
}

/// OpenAI: `[llm.openai]` over [`OpenAiConfig::new`].
///
/// [`OpenAiConfig::new`]: brisvox_providers::openai::OpenAiConfig::new
#[cfg(feature = "provider-openai")]
pub fn openai(
    options: Option<&super::schema::OpenAiOptions>,
    api_key: Secret<String>,
) -> brisvox_providers::openai::OpenAiConfig {
    use super::schema::OpenAiReasoningEffort as E;
    use brisvox_providers::openai::{OpenAiConfig, ReasoningEffort};

    let mut c = OpenAiConfig::new(api_key);
    let Some(o) = options else { return c };
    if let Some(model) = &o.model {
        c.model = model.as_str().to_owned();
    }
    if let Some(effort) = o.reasoning_effort {
        c.reasoning_effort = Some(match effort {
            E::Disabled => ReasoningEffort::Disabled,
            E::Low => ReasoningEffort::Low,
            E::Medium => ReasoningEffort::Medium,
            E::High => ReasoningEffort::High,
            E::Xhigh => ReasoningEffort::Xhigh,
            E::Max => ReasoningEffort::Max,
        });
    }
    if let Some(n) = o.max_output_tokens {
        c.max_output_tokens = Some(n.get());
    }
    if let Some(on) = o.parallel_tool_calls {
        c.parallel_tool_calls = on;
    }
    if let Some(tier) = &o.service_tier {
        c.service_tier = Some(tier.as_str().to_owned());
    }
    if let Some(url) = &o.base_url {
        c.base_url = url.as_str().to_owned();
    }
    c
}

/// Anthropic: `[llm.anthropic]` over [`AnthropicConfig::new`].
///
/// [`AnthropicConfig::new`]: brisvox_providers::anthropic::AnthropicConfig::new
#[cfg(feature = "provider-anthropic")]
pub fn anthropic(
    options: Option<&super::schema::AnthropicOptions>,
    api_key: Secret<String>,
) -> brisvox_providers::anthropic::AnthropicConfig {
    use super::schema::{AnthropicEffort as E, AnthropicThinking as T};
    use brisvox_providers::anthropic::{AnthropicConfig, Effort, ThinkingMode};

    let mut c = AnthropicConfig::new(api_key);
    let Some(o) = options else { return c };
    if let Some(model) = &o.model {
        c.model = model.as_str().to_owned();
    }
    if let Some(n) = o.max_tokens {
        c.max_tokens = n.get();
    }
    if let Some(effort) = o.effort {
        c.effort = Some(match effort {
            E::Low => Effort::Low,
            E::Medium => Effort::Medium,
            E::High => Effort::High,
            E::Xhigh => Effort::Xhigh,
            E::Max => Effort::Max,
        });
    }
    if let Some(thinking) = o.thinking {
        c.thinking = match thinking {
            T::Disabled => ThinkingMode::Disabled,
            T::Adaptive => ThinkingMode::Adaptive,
            T::BetweenTools => ThinkingMode::BetweenTools,
            T::ModelDefault => ThinkingMode::ModelDefault,
        };
    }
    if let Some(url) = &o.base_url {
        c.base_url = url.as_str().to_owned();
    }
    c
}

/// Gemini: `[llm.gemini]` over [`GeminiConfig::new`].
///
/// [`GeminiConfig::new`]: brisvox_providers::gemini::GeminiConfig::new
#[cfg(feature = "provider-gemini")]
pub fn gemini(
    options: Option<&super::schema::GeminiOptions>,
    api_key: Secret<String>,
) -> brisvox_providers::gemini::GeminiConfig {
    use super::schema::GeminiThinkingLevel as L;
    use brisvox_providers::gemini::{GeminiConfig, ThinkingLevel};

    let mut c = GeminiConfig::new(api_key);
    let Some(o) = options else { return c };
    if let Some(model) = &o.model {
        c.model = model.as_str().to_owned();
    }
    if let Some(level) = o.thinking_level {
        c.thinking_level = Some(match level {
            L::Minimal => ThinkingLevel::Minimal,
            L::Low => ThinkingLevel::Low,
            L::Medium => ThinkingLevel::Medium,
            L::High => ThinkingLevel::High,
        });
    }
    if let Some(n) = o.max_output_tokens {
        c.max_output_tokens = Some(n.get());
    }
    if let Some(t) = o.temperature {
        c.temperature = Some(t.get());
    }
    if let Some(url) = &o.base_url {
        c.base_url = url.as_str().to_owned();
    }
    c
}

/// An OpenAI-compatible server: the preset (or [`OpenAiCompatibleConfig::new`] over `base_url`
/// and `model`), then the overrides. `options` must name a preset or both `base_url` and
/// `model` (validation checks it); without them the mapping falls back to an empty base URL,
/// which the adapter rejects at construction.
///
/// [`OpenAiCompatibleConfig::new`]: brisvox_providers::openai_compatible::OpenAiCompatibleConfig::new
#[cfg(feature = "provider-openai-compatible")]
pub fn openai_compatible(
    options: Option<&super::schema::OpenAiCompatibleOptions>,
    api_key: Option<Secret<String>>,
) -> brisvox_providers::openai_compatible::OpenAiCompatibleConfig {
    use super::schema::CompatiblePreset as P;
    use brisvox_providers::openai_compatible::{OpenAiCompatibleConfig, Preset};

    let preset = options.and_then(|o| o.preset).map(|p| match p {
        P::Groq => Preset::Groq,
        P::Together => Preset::Together,
        P::Fireworks => Preset::Fireworks,
        P::Vllm => Preset::Vllm,
        P::Ollama => Preset::Ollama,
        P::OpenRouter => Preset::OpenRouter,
    });
    let mut c = preset.map_or_else(
        || OpenAiCompatibleConfig::new(String::new(), String::new()),
        OpenAiCompatibleConfig::preset,
    );
    c.api_key = api_key;
    let Some(o) = options else { return c };
    if let Some(url) = &o.base_url {
        c.base_url = url.as_str().to_owned();
    }
    if let Some(model) = &o.model {
        c.model = model.as_str().to_owned();
    }
    if let Some(n) = o.max_tokens {
        c.max_tokens = Some(n.get());
    }
    if let Some(t) = o.temperature {
        c.temperature = Some(t.get());
    }
    if let Some(effort) = &o.reasoning_effort {
        c.extra_body.push((
            "reasoning_effort".to_owned(),
            serde_json::Value::String(effort.as_str().to_owned()),
        ));
    }
    if let Some(order) = &o.openrouter_order {
        let mut routing = serde_json::Map::new();
        routing.insert(
            "order".to_owned(),
            serde_json::Value::Array(
                order
                    .get_ref()
                    .iter()
                    .map(|slug| serde_json::Value::String(slug.clone()))
                    .collect(),
            ),
        );
        if let Some(allow) = o.openrouter_allow_fallbacks {
            routing.insert("allow_fallbacks".to_owned(), serde_json::Value::Bool(allow));
        }
        c.extra_body
            .push(("provider".to_owned(), serde_json::Value::Object(routing)));
    }
    c
}

/// Cartesia TTS: `[tts.cartesia]` over [`CartesiaConfig::new`].
///
/// [`CartesiaConfig::new`]: brisvox_providers::cartesia::CartesiaConfig::new
#[cfg(feature = "provider-cartesia")]
pub fn cartesia(
    options: Option<&super::schema::CartesiaOptions>,
    api_key: Secret<String>,
    language: &LanguageTag,
    clock: brisvox_core::SharedClock,
    shutdown: tokio_util::sync::CancellationToken,
) -> brisvox_providers::cartesia::CartesiaConfig {
    use brisvox_providers::cartesia::CartesiaConfig;

    let mut c = CartesiaConfig::new(api_key, clock);
    c.shutdown = shutdown;
    c.language = Some(language.primary());
    let Some(o) = options else { return c };
    if let Some(model) = &o.model {
        c.model_id = model.get_ref().as_str().to_owned();
    }
    if let Some(ms) = o.max_buffer_delay_ms {
        c.max_buffer_delay_ms = u16::try_from(ms.get()).unwrap_or(u16::MAX);
    }
    if let Some(url) = &o.url {
        c.url = url.as_str().to_owned();
    }
    c
}

/// ElevenLabs TTS: `[tts.elevenlabs]` over [`ElevenLabsConfig::new`]. Voice settings are sent
/// only when at least one is given.
///
/// [`ElevenLabsConfig::new`]: brisvox_providers::elevenlabs::ElevenLabsConfig::new
#[cfg(feature = "provider-elevenlabs")]
pub fn elevenlabs(
    options: Option<&super::schema::ElevenLabsOptions>,
    api_key: Secret<String>,
    language: &LanguageTag,
    clock: brisvox_core::SharedClock,
    shutdown: tokio_util::sync::CancellationToken,
) -> brisvox_providers::elevenlabs::ElevenLabsConfig {
    use brisvox_providers::elevenlabs::{ElevenLabsConfig, VoiceSettings};

    let mut c = ElevenLabsConfig::new(api_key, clock);
    c.shutdown = shutdown;
    c.language_code = Some(language.primary());
    let Some(o) = options else { return c };
    if let Some(model) = &o.model {
        c.model_id = model.get_ref().as_str().to_owned();
    }
    let voice = VoiceSettings {
        stability: o.stability.map(|v| v.get()),
        similarity_boost: o.similarity_boost.map(|v| v.get()),
        style: o.style.map(|v| v.get()),
        speed: o.speed.map(|v| v.get()),
        use_speaker_boost: o.use_speaker_boost,
    };
    if voice != VoiceSettings::default() {
        c.voice_settings = Some(voice);
    }
    if let Some(url) = &o.url {
        c.url = url.as_str().to_owned();
    }
    c
}

/// A local OpenAI-compatible speech server: the preset (default `generic`) over `base_url`, then
/// the overrides. Validation requires `base_url` (and a `model` for `generic`); without them the
/// mapping falls back to an empty base URL, which the adapter rejects at construction.
#[cfg(feature = "provider-openai-speech")]
pub fn openai_speech(
    options: Option<&super::schema::OpenAiSpeechOptions>,
    api_key: Option<Secret<String>>,
    language: &LanguageTag,
    clock: brisvox_core::SharedClock,
    shutdown: tokio_util::sync::CancellationToken,
) -> brisvox_providers::openai_speech::OpenAiSpeechConfig {
    use super::schema::{SpeechFormat, SpeechPreset as P};
    use brisvox_providers::openai_speech::{AudioFormat, OpenAiSpeechConfig, Preset};

    let preset = match options.and_then(|o| o.preset).unwrap_or(P::Generic) {
        P::Generic => Preset::Generic,
        P::Kokoro => Preset::Kokoro,
        P::Chatterbox => Preset::Chatterbox,
        P::Speaches => Preset::Speaches,
        P::VllmOmni => Preset::VllmOmni,
    };
    let base_url = options
        .and_then(|o| o.base_url.as_ref())
        .map(|u| u.as_str().to_owned())
        .unwrap_or_default();
    let mut c = OpenAiSpeechConfig::preset(preset, base_url, clock);
    c.shutdown = shutdown;
    c.api_key = api_key;
    if preset == Preset::Chatterbox {
        // The route's `language` field picks the multilingual model's language (the code format
        // is UNVERIFIED, docs/providers/openai-speech.md).
        c.extra_body.insert(
            "language".to_owned(),
            serde_json::Value::String(language.primary()),
        );
    }
    let Some(o) = options else { return c };
    if let Some(model) = &o.model {
        c.model = model.as_str().to_owned();
    }
    if let Some(speed) = o.speed {
        c.speed = Some(speed.get());
    }
    if let Some(format) = o.format {
        c.format = match format {
            SpeechFormat::Pcm => AudioFormat::Pcm,
            SpeechFormat::Wav => AudioFormat::Wav,
        };
    }
    if let Some(timeout) = o.first_audio_timeout_ms {
        c.first_audio_timeout = timeout.duration();
    }
    if let Some(text) = &o.warm_text {
        c.warm_text.clone_from(text.get_ref());
    }
    if let Some(interval) = o.warm_interval_ms {
        c.warm_interval = interval.duration();
    }
    c
}

/// Deepgram Aura TTS: `[tts.deepgram_aura]` over [`DeepgramAuraConfig::new`].
///
/// [`DeepgramAuraConfig::new`]: brisvox_providers::deepgram_aura::DeepgramAuraConfig::new
#[cfg(feature = "provider-deepgram-aura")]
pub fn deepgram_aura(
    options: Option<&super::schema::DeepgramAuraOptions>,
    api_key: Secret<String>,
    language: &LanguageTag,
    clock: brisvox_core::SharedClock,
    shutdown: tokio_util::sync::CancellationToken,
) -> brisvox_providers::deepgram_aura::DeepgramAuraConfig {
    use brisvox_providers::deepgram_aura::DeepgramAuraConfig;

    let mut c = DeepgramAuraConfig::new(api_key, clock);
    c.shutdown = shutdown;
    c.language = Some(language.as_str().to_owned());
    let Some(o) = options else { return c };
    if let Some(speed) = o.speed {
        c.speed = Some(speed.get());
    }
    if let Some(url) = &o.url {
        c.url = url.as_str().to_owned();
    }
    c
}

/// Switches the STT adapter configuration `config` to its multilingual mode for the `[languages]`
/// `codes` (empty: no change). What each provider gets (docs/CONFIG.md, "Languages"):
/// - Deepgram: `language=multi` with `language_hint` for each code; the transcripts then carry
///   the language of each word, which the adapter reports per final segment.
/// - AssemblyAI: `language_codes` steering and `language_detection=true` (the final turn reports
///   `language_code` and `language_confidence`).
/// - Gladia: `language_config.languages` with `code_switching` (every utterance is detected).
/// - OpenAI Realtime transcription: the codes as `languages` for `gpt-live-transcribe`; other
///   models detect the language themselves. No language is reported (the engine falls back to
///   text detection).
/// - sherpa-onnx: unchanged (validation refuses it with languages).
pub fn multilingual_stt(
    config: super::SttProviderConfig,
    codes: &[String],
) -> super::SttProviderConfig {
    #[allow(unused_mut)] // no adapter compiled in: nothing to change
    let mut config = config;
    if codes.is_empty() {
        return config;
    }
    match &mut config {
        #[cfg(feature = "provider-deepgram")]
        super::SttProviderConfig::Deepgram(c) => {
            c.language = Some("multi".to_owned());
            c.language_hints = codes.to_vec();
        }
        #[cfg(feature = "provider-assemblyai")]
        super::SttProviderConfig::AssemblyAi(c) => {
            c.language = None;
            c.language_codes = codes.to_vec();
            c.language_detection = true;
        }
        #[cfg(feature = "provider-gladia")]
        super::SttProviderConfig::Gladia(c) => {
            c.language = None;
            c.languages = codes.to_vec();
            c.code_switching = true;
        }
        #[cfg(feature = "provider-openai-realtime-stt")]
        super::SttProviderConfig::OpenAiRealtimeStt(c) => {
            c.language = None;
            c.languages = codes.to_vec();
        }
        #[allow(unreachable_patterns)] // a build with only one adapter has no other variant
        _ => {}
    }
    config
}
