//! The provider instances calls run on.
//!
//! The engine's [`SessionBuilder`](brisvox_engine::SessionBuilder) is generic over one STT, one
//! LLM and one TTS type. An agent definition picks the adapters at run time, so
//! [`AnyStt`], [`AnyLlm`] and [`AnyTts`] wrap the adapters compiled into this build (one
//! variant per `provider-*` feature) and forward every trait method to the chosen one.
//!
//! [`ConfiguredProviders`] builds the three instances (and the agent's secondaries) once per
//! process from an [`AgentConfig`] (its keys come from the environment, see `docs/CONFIG.md`).
//! The adapters keep their warm connections and pools per instance, and a call opens its own
//! session on them (`connect`, `stream`). Every call gets them behind the resilience layer
//! ([`crate::resilient`]): circuit breakers shared by all calls, retry budgets,
//! failover to the secondary, idle refresh of TTS sessions. [`ProviderFactory`] is the seam:
//! tests hand the runner mock providers instead (`brisvox-sim`).

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::SharedClock;
use brisvox_providers::resilience::{
    Resilience, ResilienceConfig, ResilienceProbe, ResilientLlm, ResilientStt, ResilientTts,
};
use brisvox_providers::{
    LlmProvider, LlmRequest, LlmStream, SttConfig, SttProvider, SttSession, TtsConfig, TtsProvider,
    TtsSession,
};
use tokio_util::sync::CancellationToken;

use crate::config::{AgentConfig, LlmProviderConfig, SttProviderConfig, TtsProviderConfig};
use crate::observability::ProviderStage;
use crate::resilient::{OwnVoice, ResilientProviders};
use crate::{Error, Result};

/// The three providers one call runs on.
#[derive(Debug)]
pub struct CallProviders<S, L, T> {
    /// Speech to text.
    pub stt: Arc<S>,
    /// The language model.
    pub llm: Arc<L>,
    /// Text to speech.
    pub tts: Arc<T>,
}

impl<S, L, T> Clone for CallProviders<S, L, T> {
    fn clone(&self) -> Self {
        Self {
            stt: Arc::clone(&self.stt),
            llm: Arc::clone(&self.llm),
            tts: Arc::clone(&self.tts),
        }
    }
}

/// Where the [`EngineRunner`](crate::runner::EngineRunner) gets each call's providers.
pub trait ProviderFactory: Send + Sync + 'static {
    /// The STT adapter.
    type Stt: SttProvider;
    /// The LLM adapter.
    type Llm: LlmProvider;
    /// The TTS adapter.
    type Tts: TtsProvider;

    /// The providers of one new call. Runs on a tokio worker when the call starts: keep it
    /// cheap (clone `Arc`s, build a mock), never block.
    fn providers(&self) -> CallProviders<Self::Stt, Self::Llm, Self::Tts>;

    /// Read-only handles on the resilience counters of the wrappers in `providers` (the set
    /// [`Self::providers`] just made), so the call's failovers and retries reach `/metrics`. The default has none: a factory without a resilience layer has nothing to count.
    fn resilience_probes(
        &self,
        providers: &CallProviders<Self::Stt, Self::Llm, Self::Tts>,
    ) -> Vec<(ProviderStage, ResilienceProbe)> {
        let _ = providers;
        Vec::new()
    }
}

/// The STT adapters compiled into this build.
#[non_exhaustive]
pub enum AnyStt {
    /// Deepgram.
    #[cfg(feature = "provider-deepgram")]
    Deepgram(brisvox_providers::deepgram::DeepgramStt),
    /// AssemblyAI.
    #[cfg(feature = "provider-assemblyai")]
    AssemblyAi(brisvox_providers::assemblyai::AssemblyAiStt),
    /// Gladia.
    #[cfg(feature = "provider-gladia")]
    Gladia(brisvox_providers::gladia::GladiaStt),
    /// A sherpa-onnx streaming server.
    #[cfg(feature = "provider-sherpa-onnx")]
    SherpaOnnx(brisvox_providers::sherpa_onnx::SherpaOnnxStt),
    /// A server speaking the OpenAI Realtime transcription protocol.
    #[cfg(feature = "provider-openai-realtime-stt")]
    OpenAiRealtimeStt(brisvox_providers::openai_realtime_stt::OpenAiRealtimeStt),
}

/// The LLM adapters compiled into this build.
#[non_exhaustive]
pub enum AnyLlm {
    /// OpenAI.
    #[cfg(feature = "provider-openai")]
    OpenAi(brisvox_providers::openai::OpenAiLlm),
    /// Anthropic.
    #[cfg(feature = "provider-anthropic")]
    Anthropic(brisvox_providers::anthropic::AnthropicLlm),
    /// Gemini.
    #[cfg(feature = "provider-gemini")]
    Gemini(brisvox_providers::gemini::GeminiLlm),
    /// An OpenAI-compatible server.
    #[cfg(feature = "provider-openai-compatible")]
    OpenAiCompatible(brisvox_providers::openai_compatible::OpenAiCompatibleLlm),
}

/// The TTS adapters compiled into this build.
#[non_exhaustive]
pub enum AnyTts {
    /// Cartesia.
    #[cfg(feature = "provider-cartesia")]
    Cartesia(brisvox_providers::cartesia::CartesiaTts),
    /// ElevenLabs.
    #[cfg(feature = "provider-elevenlabs")]
    ElevenLabs(brisvox_providers::elevenlabs::ElevenLabsTts),
    /// Deepgram Aura.
    #[cfg(feature = "provider-deepgram-aura")]
    DeepgramAura(brisvox_providers::deepgram_aura::DeepgramAuraTts),
    /// A local server speaking the OpenAI `/audio/speech` protocol.
    #[cfg(feature = "provider-openai-speech")]
    OpenAiSpeech(brisvox_providers::openai_speech::OpenAiSpeechTts),
}

/// Forwards `$call` to the adapter inside `$value` (`p` names it), for every variant compiled in.
/// With no adapter of the kind compiled in the enum is empty and the match has no arm.
macro_rules! forward {
    ($value:expr, $p:ident => $call:expr, [$($variant:ident: $feature:literal),* $(,)?]) => {
        match *$value {
            $(
                #[cfg(feature = $feature)]
                Self::$variant(ref $p) => $call,
            )*
        }
    };
}

/// The variants of each enum, with their features (one list, used by every forward).
macro_rules! stt_forward {
    ($value:expr, $p:ident => $call:expr) => {
        forward!($value, $p => $call, [
            Deepgram: "provider-deepgram",
            AssemblyAi: "provider-assemblyai",
            Gladia: "provider-gladia",
            SherpaOnnx: "provider-sherpa-onnx",
            OpenAiRealtimeStt: "provider-openai-realtime-stt",
        ])
    };
}

macro_rules! llm_forward {
    ($value:expr, $p:ident => $call:expr) => {
        forward!($value, $p => $call, [
            OpenAi: "provider-openai",
            Anthropic: "provider-anthropic",
            Gemini: "provider-gemini",
            OpenAiCompatible: "provider-openai-compatible",
        ])
    };
}

macro_rules! tts_forward {
    ($value:expr, $p:ident => $call:expr) => {
        forward!($value, $p => $call, [
            Cartesia: "provider-cartesia",
            ElevenLabs: "provider-elevenlabs",
            DeepgramAura: "provider-deepgram-aura",
            OpenAiSpeech: "provider-openai-speech",
        ])
    };
}

impl SttProvider for AnyStt {
    fn name(&self) -> &'static str {
        stt_forward!(self, p => p.name())
    }

    #[cfg_attr(
        not(any(
            feature = "provider-deepgram",
            feature = "provider-assemblyai",
            feature = "provider-gladia",
            feature = "provider-sherpa-onnx",
            feature = "provider-openai-realtime-stt"
        )),
        allow(unused_variables)
    )]
    async fn connect(&self, config: SttConfig) -> brisvox_providers::Result<SttSession> {
        stt_forward!(self, p => p.connect(config).await)
    }

    async fn warm(&self) -> brisvox_providers::Result<()> {
        stt_forward!(self, p => p.warm().await)
    }
}

impl LlmProvider for AnyLlm {
    fn name(&self) -> &'static str {
        llm_forward!(self, p => p.name())
    }

    #[cfg_attr(
        not(any(
            feature = "provider-openai",
            feature = "provider-anthropic",
            feature = "provider-gemini",
            feature = "provider-openai-compatible"
        )),
        allow(unused_variables)
    )]
    async fn stream(&self, request: LlmRequest) -> brisvox_providers::Result<LlmStream> {
        llm_forward!(self, p => p.stream(request).await)
    }

    async fn warm(&self) -> brisvox_providers::Result<()> {
        llm_forward!(self, p => p.warm().await)
    }
}

impl TtsProvider for AnyTts {
    fn name(&self) -> &'static str {
        tts_forward!(self, p => p.name())
    }

    #[cfg_attr(
        not(any(
            feature = "provider-cartesia",
            feature = "provider-elevenlabs",
            feature = "provider-deepgram-aura",
            feature = "provider-openai-speech"
        )),
        allow(unused_variables)
    )]
    async fn connect(&self, config: TtsConfig) -> brisvox_providers::Result<TtsSession> {
        tts_forward!(self, p => p.connect(config).await)
    }

    async fn warm(&self) -> brisvox_providers::Result<()> {
        tts_forward!(self, p => p.warm().await)
    }

    fn idle_limit(&self) -> Option<Duration> {
        tts_forward!(self, p => p.idle_limit())
    }
}

/// The providers an agent definition names, built once and shared by every call (see the
/// [module docs](self)), behind the resilience layer
/// ([`ResilientProviders`]): a call's STT, LLM and TTS are
/// [`ResilientStt`] and friends over the process's
/// instances, with the agent's `[stt|llm|tts].secondary` as the provider to fail over to.
pub struct ConfiguredProviders {
    providers: ResilientProviders<AnyStt, AnyLlm, AnyTts>,
}

impl std::fmt::Debug for ConfiguredProviders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfiguredProviders")
            .field("stt", &self.providers.stt_leg().provider().name())
            .field("llm", &self.providers.llm_leg().provider().name())
            .field("tts", &self.providers.tts_leg().provider().name())
            .field("secondaries", &self.providers.has_secondaries())
            .finish()
    }
}

/// An adapter that would not build (bad base URL, TLS setup). Unused in a build without any
/// provider adapter.
#[allow(dead_code)]
fn build_error(stage: &str, e: &brisvox_providers::Error) -> Error {
    Error::Config(format!("cannot build the {stage} provider: {e}"))
}

/// The STT adapter `config` names.
#[cfg_attr(
    not(any(
        feature = "provider-deepgram",
        feature = "provider-assemblyai",
        feature = "provider-gladia",
        feature = "provider-sherpa-onnx",
        feature = "provider-openai-realtime-stt"
    )),
    allow(unreachable_code, unused_variables, clippy::diverging_sub_expression)
)]
fn build_stt(config: SttProviderConfig, clock: &SharedClock) -> Result<AnyStt> {
    Ok(match config {
        #[cfg(feature = "provider-deepgram")]
        SttProviderConfig::Deepgram(c) => AnyStt::Deepgram(
            brisvox_providers::deepgram::DeepgramStt::new(c, clock.clone()),
        ),
        #[cfg(feature = "provider-assemblyai")]
        SttProviderConfig::AssemblyAi(c) => AnyStt::AssemblyAi(
            brisvox_providers::assemblyai::AssemblyAiStt::new(c, clock.clone()),
        ),
        #[cfg(feature = "provider-gladia")]
        SttProviderConfig::Gladia(c) => AnyStt::Gladia(
            brisvox_providers::gladia::GladiaStt::new(c, clock.clone())
                .map_err(|e| build_error("stt", &e))?,
        ),
        #[cfg(feature = "provider-sherpa-onnx")]
        SttProviderConfig::SherpaOnnx(c) => AnyStt::SherpaOnnx(
            brisvox_providers::sherpa_onnx::SherpaOnnxStt::new(c, clock.clone()),
        ),
        #[cfg(feature = "provider-openai-realtime-stt")]
        SttProviderConfig::OpenAiRealtimeStt(c) => AnyStt::OpenAiRealtimeStt(
            brisvox_providers::openai_realtime_stt::OpenAiRealtimeStt::new(c, clock.clone()),
        ),
    })
}

/// The LLM adapter `config` names.
#[cfg_attr(
    not(any(
        feature = "provider-openai",
        feature = "provider-anthropic",
        feature = "provider-gemini",
        feature = "provider-openai-compatible"
    )),
    allow(unreachable_code, unused_variables, clippy::diverging_sub_expression)
)]
fn build_llm(config: LlmProviderConfig, clock: &SharedClock) -> Result<AnyLlm> {
    Ok(match config {
        #[cfg(feature = "provider-openai")]
        LlmProviderConfig::OpenAi(c) => AnyLlm::OpenAi(
            brisvox_providers::openai::OpenAiLlm::new(c, clock.clone())
                .map_err(|e| build_error("llm", &e))?,
        ),
        #[cfg(feature = "provider-anthropic")]
        LlmProviderConfig::Anthropic(c) => AnyLlm::Anthropic(
            brisvox_providers::anthropic::AnthropicLlm::new(c, clock.clone())
                .map_err(|e| build_error("llm", &e))?,
        ),
        #[cfg(feature = "provider-gemini")]
        LlmProviderConfig::Gemini(c) => AnyLlm::Gemini(
            brisvox_providers::gemini::GeminiLlm::new(c, clock.clone())
                .map_err(|e| build_error("llm", &e))?,
        ),
        #[cfg(feature = "provider-openai-compatible")]
        LlmProviderConfig::OpenAiCompatible(c) => AnyLlm::OpenAiCompatible(
            brisvox_providers::openai_compatible::OpenAiCompatibleLlm::new(c, clock.clone())
                .map_err(|e| build_error("llm", &e))?,
        ),
    })
}

/// The TTS adapter `config` names.
#[cfg_attr(
    not(any(
        feature = "provider-cartesia",
        feature = "provider-elevenlabs",
        feature = "provider-deepgram-aura",
        feature = "provider-openai-speech"
    )),
    allow(unreachable_code, unused_variables, clippy::diverging_sub_expression)
)]
fn build_tts(config: TtsProviderConfig) -> Result<AnyTts> {
    Ok(match config {
        #[cfg(feature = "provider-cartesia")]
        TtsProviderConfig::Cartesia(c) => AnyTts::Cartesia(
            brisvox_providers::cartesia::CartesiaTts::new(c).map_err(|e| build_error("tts", &e))?,
        ),
        #[cfg(feature = "provider-elevenlabs")]
        TtsProviderConfig::ElevenLabs(c) => AnyTts::ElevenLabs(
            brisvox_providers::elevenlabs::ElevenLabsTts::new(c)
                .map_err(|e| build_error("tts", &e))?,
        ),
        #[cfg(feature = "provider-deepgram-aura")]
        TtsProviderConfig::DeepgramAura(c) => AnyTts::DeepgramAura(
            brisvox_providers::deepgram_aura::DeepgramAuraTts::new(c)
                .map_err(|e| build_error("tts", &e))?,
        ),
        #[cfg(feature = "provider-openai-speech")]
        TtsProviderConfig::OpenAiSpeech(c) => AnyTts::OpenAiSpeech(
            brisvox_providers::openai_speech::OpenAiSpeechTts::new(c)
                .map_err(|e| build_error("tts", &e))?,
        ),
    })
}

impl ConfiguredProviders {
    /// Builds the agent's STT, LLM and TTS adapters (and their secondaries) on `clock`, behind
    /// the resilience layer with its default settings; `shutdown` stops the TTS adapters' warm
    /// connections and timers. Call it inside the tokio runtime, once at startup.
    ///
    /// # Errors
    /// [`Error::Config`] when an adapter is not compiled into this build or will not build.
    pub fn from_agent(
        agent: &AgentConfig,
        clock: &SharedClock,
        shutdown: &CancellationToken,
    ) -> Result<Self> {
        Self::from_agent_with(agent, clock, shutdown, ResilienceConfig::DEFAULT)
    }

    /// [`Self::from_agent`] with explicit resilience settings (breaker thresholds, retry
    /// budgets, idle refresh).
    ///
    /// # Errors
    /// As [`Self::from_agent`].
    pub fn from_agent_with(
        agent: &AgentConfig,
        clock: &SharedClock,
        shutdown: &CancellationToken,
        resilience: ResilienceConfig,
    ) -> Result<Self> {
        let stt = Arc::new(build_stt(agent.stt()?, clock)?);
        let llm = Arc::new(build_llm(agent.llm()?, clock)?);
        let tts = Arc::new(build_tts(agent.tts(clock.clone(), shutdown.clone())?)?);
        let mut providers = ResilientProviders::new(
            Resilience::new(resilience, clock.clone()),
            CallProviders { stt, llm, tts },
        );
        if let Some(config) = agent.stt_secondary()? {
            providers = providers.with_stt_secondary(Arc::new(build_stt(config, clock)?));
        }
        if let Some(config) = agent.llm_secondary()? {
            providers = providers.with_llm_secondary(Arc::new(build_llm(config, clock)?));
        }
        if let Some(config) = agent.tts_secondary(clock.clone(), shutdown.clone())? {
            let voice = agent
                .tts_secondary_voice()
                .ok_or_else(|| Error::Config("tts.secondary has no voice".to_owned()))?;
            providers = providers.with_tts_secondary(Arc::new(build_tts(config)?), voice);
        }
        Ok(Self { providers })
    }

    /// The resilience layer the calls run behind.
    pub const fn resilient(&self) -> &ResilientProviders<AnyStt, AnyLlm, AnyTts> {
        &self.providers
    }
}

impl ProviderFactory for ConfiguredProviders {
    type Stt = ResilientStt<AnyStt, AnyStt>;
    type Llm = ResilientLlm<AnyLlm, AnyLlm>;
    type Tts = ResilientTts<OwnVoice<AnyTts>, OwnVoice<AnyTts>>;

    fn providers(&self) -> CallProviders<Self::Stt, Self::Llm, Self::Tts> {
        self.providers.providers()
    }

    fn resilience_probes(
        &self,
        providers: &CallProviders<Self::Stt, Self::Llm, Self::Tts>,
    ) -> Vec<(ProviderStage, ResilienceProbe)> {
        self.providers.resilience_probes(providers)
    }
}
