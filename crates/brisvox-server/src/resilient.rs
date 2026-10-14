//! The resilience layer on the server's providers.
//!
//! [`ResilientProviders`] is a [`ProviderFactory`] over the process's provider instances. It owns
//! the process-wide part of the layer: one [`Resilience`] (the process retry budget) and one
//! [`Leg`] (provider plus circuit breaker) per provider instance, built once at startup. Every
//! call gets its own [`ResilientStt`], [`ResilientLlm`] and [`ResilientTts`] over those legs
//! ([`ProviderFactory::providers`]), so a call has its own retry budget and counters while an
//! outage is judged across all calls: after the breaker opens, calls stop piling onto the
//! failing provider and go to its secondary at once, if the agent has one
//! (`[stt|llm|tts].secondary`, `docs/CONFIG.md`).
//!
//! Without a secondary a wrapper adds the breaker, the retry budgets, in-place recovery of a
//! dead STT or TTS session and, for TTS, the proactive refresh of idle sessions
//! ([`IdleRefresh::Auto`](brisvox_providers::resilience::IdleRefresh): a provider with an idle
//! limit, as Cartesia's 5 minutes, gets a fresh session at 80 % of it instead of a reconnect on
//! the next text).
//!
//! # A secondary TTS speaks with its own voice
//! The engine passes one voice id in every `connect`, and a voice id belongs to one provider.
//! TTS legs are therefore [`OwnVoice`]s: the primary forwards the engine's voice, the secondary
//! replaces it with the voice of `[tts.secondary]`.

use std::sync::Arc;
use std::time::Duration;

use brisvox_providers::resilience::{
    CircuitBreaker, Leg, Resilience, ResilienceConfig, ResilienceProbe, ResilientLlm, ResilientStt,
    ResilientTts, RetryBudget,
};
use brisvox_providers::{LlmProvider, SttProvider, TtsConfig, TtsProvider, TtsSession};

use crate::config::Env;
use crate::observability::{
    BreakerSnapshot, BudgetSnapshot, ProviderStage, ResilienceSnapshot, ResilienceSource,
};
use crate::providers::{CallProviders, ProviderFactory};
use crate::settings::{count_var, duration_var};
use crate::{Error, Result};

/// The providers of one call behind the resilience layer.
pub type CallResilient<S, L, T> =
    CallProviders<ResilientStt<S, S>, ResilientLlm<L, L>, ResilientTts<OwnVoice<T>, OwnVoice<T>>>;

/// A read-only view of the process-wide resilience state: the breaker of every leg and the
/// process retry budget. Cheap to hold; it shares the breakers with the legs, so a scrape
/// sees them live. Get it from [`ResilientProviders::resilience_handle`].
#[derive(Debug, Clone)]
pub struct ResilienceHandle {
    legs: Vec<(ProviderStage, &'static str, Arc<CircuitBreaker>)>,
    budget: Arc<RetryBudget>,
}

impl ResilienceSource for ResilienceHandle {
    fn resilience_snapshot(&self) -> ResilienceSnapshot {
        ResilienceSnapshot {
            breakers: self
                .legs
                .iter()
                .map(|(stage, role, breaker)| BreakerSnapshot {
                    stage: *stage,
                    role,
                    provider: breaker.name(),
                    state: breaker.state(),
                    stats: breaker.stats(),
                })
                .collect(),
            budget: BudgetSnapshot {
                available: self.budget.available(),
                granted: self.budget.granted(),
                denied: self.budget.denied(),
            },
        }
    }
}

/// A TTS provider that speaks with its own voice instead of the one the engine asks for (or,
/// without one, forwards the engine's).
#[derive(Debug)]
pub struct OwnVoice<T> {
    inner: Arc<T>,
    voice: Option<Arc<str>>,
}

impl<T> OwnVoice<T> {
    /// `inner` speaking `voice`, or the engine's voice when `voice` is `None`.
    pub const fn new(inner: Arc<T>, voice: Option<Arc<str>>) -> Self {
        Self { inner, voice }
    }
}

impl<T: TtsProvider> TtsProvider for OwnVoice<T> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    async fn connect(&self, config: TtsConfig) -> brisvox_providers::Result<TtsSession> {
        let config = match &self.voice {
            Some(voice) => TtsConfig {
                voice: Arc::clone(voice),
                ..config
            },
            None => config,
        };
        self.inner.connect(config).await
    }

    async fn warm(&self) -> brisvox_providers::Result<()> {
        self.inner.warm().await
    }

    fn idle_limit(&self) -> Option<Duration> {
        self.inner.idle_limit()
    }
}

/// The providers one process runs its calls on, behind the resilience layer (see the
/// [module docs](self)).
pub struct ResilientProviders<S, L, T> {
    resilience: Resilience,
    stt: Leg<S>,
    stt_secondary: Option<Leg<S>>,
    llm: Leg<L>,
    llm_secondary: Option<Leg<L>>,
    tts: Leg<OwnVoice<T>>,
    tts_secondary: Option<Leg<OwnVoice<T>>>,
}

impl<S, L, T> std::fmt::Debug for ResilientProviders<S, L, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResilientProviders")
            .field("stt_secondary", &self.stt_secondary.is_some())
            .field("llm_secondary", &self.llm_secondary.is_some())
            .field("tts_secondary", &self.tts_secondary.is_some())
            .finish_non_exhaustive()
    }
}

impl<S: SttProvider, L: LlmProvider, T: TtsProvider> ResilientProviders<S, L, T> {
    /// The process's providers behind `resilience`, each with a breaker of its own, and no
    /// secondaries yet.
    pub fn new(resilience: Resilience, providers: CallProviders<S, L, T>) -> Self {
        let CallProviders { stt, llm, tts } = providers;
        Self {
            stt: resilience.leg(stt.name(), stt),
            stt_secondary: None,
            llm: resilience.leg(llm.name(), llm),
            llm_secondary: None,
            tts: resilience.leg(tts.name(), Arc::new(OwnVoice::new(tts, None))),
            tts_secondary: None,
            resilience,
        }
    }

    /// Calls fail over to `stt` when the primary STT is down.
    #[must_use]
    pub fn with_stt_secondary(mut self, stt: Arc<S>) -> Self {
        self.stt_secondary = Some(self.resilience.leg(stt.name(), stt));
        self
    }

    /// Calls fail over to `llm` when the primary LLM fails a request before any content.
    #[must_use]
    pub fn with_llm_secondary(mut self, llm: Arc<L>) -> Self {
        self.llm_secondary = Some(self.resilience.leg(llm.name(), llm));
        self
    }

    /// Calls fail over to `tts`, speaking `voice`, when the primary TTS is down.
    #[must_use]
    pub fn with_tts_secondary(mut self, tts: Arc<T>, voice: Arc<str>) -> Self {
        let name = tts.name();
        self.tts_secondary = Some(
            self.resilience
                .leg(name, Arc::new(OwnVoice::new(tts, Some(voice)))),
        );
        self
    }

    /// The process-wide context (configuration, clock, process retry budget).
    pub const fn resilience(&self) -> &Resilience {
        &self.resilience
    }

    /// Whether STT, LLM and TTS (in this order) have a secondary.
    pub const fn has_secondaries(&self) -> (bool, bool, bool) {
        (
            self.stt_secondary.is_some(),
            self.llm_secondary.is_some(),
            self.tts_secondary.is_some(),
        )
    }

    /// A live view of the breakers and the process retry budget, for `/metrics`.
    pub fn resilience_handle(&self) -> Arc<ResilienceHandle> {
        let mut legs = vec![
            (
                ProviderStage::Stt,
                "primary",
                Arc::clone(self.stt.breaker()),
            ),
            (
                ProviderStage::Llm,
                "primary",
                Arc::clone(self.llm.breaker()),
            ),
            (
                ProviderStage::Tts,
                "primary",
                Arc::clone(self.tts.breaker()),
            ),
        ];
        if let Some(leg) = &self.stt_secondary {
            legs.push((ProviderStage::Stt, "secondary", Arc::clone(leg.breaker())));
        }
        if let Some(leg) = &self.llm_secondary {
            legs.push((ProviderStage::Llm, "secondary", Arc::clone(leg.breaker())));
        }
        if let Some(leg) = &self.tts_secondary {
            legs.push((ProviderStage::Tts, "secondary", Arc::clone(leg.breaker())));
        }
        Arc::new(ResilienceHandle {
            legs,
            budget: Arc::clone(self.resilience.process_budget()),
        })
    }

    /// Read-only handles on the resilience counters of one call's wrappers (made by
    /// [`ProviderFactory::providers`]).
    pub fn call_probes(
        providers: &CallResilient<S, L, T>,
    ) -> Vec<(ProviderStage, ResilienceProbe)> {
        vec![
            (ProviderStage::Stt, providers.stt.probe()),
            (ProviderStage::Llm, providers.llm.probe()),
            (ProviderStage::Tts, providers.tts.probe()),
        ]
    }

    /// The primary STT leg (shared by every call; its breaker is process-wide).
    pub const fn stt_leg(&self) -> &Leg<S> {
        &self.stt
    }

    /// The primary LLM leg.
    pub const fn llm_leg(&self) -> &Leg<L> {
        &self.llm
    }

    /// The primary TTS leg.
    pub const fn tts_leg(&self) -> &Leg<OwnVoice<T>> {
        &self.tts
    }
}

impl<S: SttProvider, L: LlmProvider, T: TtsProvider> ProviderFactory
    for ResilientProviders<S, L, T>
{
    type Stt = ResilientStt<S, S>;
    type Llm = ResilientLlm<L, L>;
    type Tts = ResilientTts<OwnVoice<T>, OwnVoice<T>>;

    fn providers(&self) -> CallProviders<Self::Stt, Self::Llm, Self::Tts> {
        let r = &self.resilience;
        let mut stt = ResilientStt::new(self.stt.clone(), r);
        if let Some(secondary) = &self.stt_secondary {
            stt = stt.with_secondary(secondary.clone());
        }
        let mut llm = ResilientLlm::new(self.llm.clone(), r);
        if let Some(secondary) = &self.llm_secondary {
            llm = llm.with_secondary(secondary.clone());
        }
        let mut tts = ResilientTts::new(self.tts.clone(), r);
        if let Some(secondary) = &self.tts_secondary {
            tts = tts.with_secondary(secondary.clone());
        }
        CallProviders {
            stt: Arc::new(stt),
            llm: Arc::new(llm),
            tts: Arc::new(tts),
        }
    }

    fn resilience_probes(
        &self,
        providers: &CallProviders<Self::Stt, Self::Llm, Self::Tts>,
    ) -> Vec<(ProviderStage, ResilienceProbe)> {
        Self::call_probes(providers)
    }
}

/// Environment variable: consecutive failures that open a provider's circuit breaker
/// ([`BreakerConfig::failure_threshold`](brisvox_providers::resilience::BreakerConfig), default 5).
pub const ENV_BREAKER_FAILURES: &str = "BRISVOX_BREAKER_FAILURES";
/// Environment variable: how long an open breaker refuses attempts before it lets one probe
/// through (`BreakerConfig::open_for`, default `30s`).
pub const ENV_BREAKER_OPEN_FOR: &str = "BRISVOX_BREAKER_OPEN_FOR";
/// Environment variable: retries one call may make (`ResilienceConfig::session_budget` capacity,
/// default 3).
pub const ENV_CALL_RETRY_BUDGET: &str = "BRISVOX_CALL_RETRY_BUDGET";
/// Environment variable: one retry token comes back to a call's budget per this long (default
/// `20s`; `0s`: the budget is always full).
pub const ENV_CALL_RETRY_REFILL: &str = "BRISVOX_CALL_RETRY_REFILL";
/// Environment variable: retries the whole process may make (`ResilienceConfig::process_budget`
/// capacity, default 50).
pub const ENV_PROCESS_RETRY_BUDGET: &str = "BRISVOX_PROCESS_RETRY_BUDGET";
/// Environment variable: one retry token comes back to the process budget per this long (default
/// `100ms`; `0s`: always full).
pub const ENV_PROCESS_RETRY_REFILL: &str = "BRISVOX_PROCESS_RETRY_REFILL";
/// Environment variable: deadline of a provider attempt that may be followed by another
/// (`ResilienceConfig::attempt_timeout`, default `2s`).
pub const ENV_ATTEMPT_TIMEOUT: &str = "BRISVOX_PROVIDER_ATTEMPT_TIMEOUT";
/// Environment variable: with a secondary LLM, how long a response may stay silent before it is
/// re-issued to the secondary (`ResilienceConfig::llm_first_event`, default `4s`).
pub const ENV_LLM_FIRST_EVENT: &str = "BRISVOX_LLM_FIRST_EVENT";
/// Environment variable: how long a provider session must live to count as healthy
/// (`ResilienceConfig::healthy_after`, default `10s`).
pub const ENV_HEALTHY_AFTER: &str = "BRISVOX_PROVIDER_HEALTHY_AFTER";

/// The resilience layer's settings: the defaults with the `BRISVOX_BREAKER_*`,
/// `BRISVOX_*_RETRY_*` and related variables of `env` (`docs/OPERATIONS.md`).
///
/// # Errors
/// [`Error::Config`] naming the variable when a value does not parse, is out of range, or is
/// zero where zero would disable the thing it sets (a threshold, a capacity, a deadline).
pub fn resilience_from_env(env: &dyn Env) -> Result<ResilienceConfig> {
    let mut config = ResilienceConfig::DEFAULT;
    let positive = |name: &str, d: std::time::Duration| {
        if d.is_zero() {
            Err(Error::Config(format!("{name}: must be longer than zero")))
        } else {
            Ok(d)
        }
    };
    let count = |name: &str, min: usize, max: usize| -> Result<Option<u32>> {
        count_var(env, name, min, max)?
            .map(|n| {
                u32::try_from(n).map_err(|_| Error::Config(format!("{name}: {n} is out of range")))
            })
            .transpose()
    };
    if let Some(n) = count(ENV_BREAKER_FAILURES, 1, 1_000)? {
        config.breaker.failure_threshold = n;
    }
    if let Some(d) = duration_var(env, ENV_BREAKER_OPEN_FOR)? {
        config.breaker.open_for = positive(ENV_BREAKER_OPEN_FOR, d)?;
    }
    if let Some(n) = count(ENV_CALL_RETRY_BUDGET, 1, 1_000)? {
        config.session_budget.capacity = n;
    }
    if let Some(d) = duration_var(env, ENV_CALL_RETRY_REFILL)? {
        config.session_budget.refill_every = d;
    }
    if let Some(n) = count(ENV_PROCESS_RETRY_BUDGET, 1, 1_000_000)? {
        config.process_budget.capacity = n;
    }
    if let Some(d) = duration_var(env, ENV_PROCESS_RETRY_REFILL)? {
        config.process_budget.refill_every = d;
    }
    if let Some(d) = duration_var(env, ENV_ATTEMPT_TIMEOUT)? {
        config.attempt_timeout = positive(ENV_ATTEMPT_TIMEOUT, d)?;
    }
    if let Some(d) = duration_var(env, ENV_LLM_FIRST_EVENT)? {
        config.llm_first_event = positive(ENV_LLM_FIRST_EVENT, d)?;
    }
    if let Some(d) = duration_var(env, ENV_HEALTHY_AFTER)? {
        config.healthy_after = positive(ENV_HEALTHY_AFTER, d)?;
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn unset_means_the_documented_defaults() {
        assert_eq!(
            resilience_from_env(&env(&[])).unwrap(),
            ResilienceConfig::DEFAULT
        );
    }

    #[test]
    fn every_variable_reaches_its_field() {
        let c = resilience_from_env(&env(&[
            (ENV_BREAKER_FAILURES, "3"),
            (ENV_BREAKER_OPEN_FOR, "45s"),
            (ENV_CALL_RETRY_BUDGET, "7"),
            (ENV_CALL_RETRY_REFILL, "0s"),
            (ENV_PROCESS_RETRY_BUDGET, "200"),
            (ENV_PROCESS_RETRY_REFILL, "50ms"),
            (ENV_ATTEMPT_TIMEOUT, "1500ms"),
            (ENV_LLM_FIRST_EVENT, "6s"),
            (ENV_HEALTHY_AFTER, "20s"),
        ]))
        .unwrap();
        assert_eq!(c.breaker.failure_threshold, 3);
        assert_eq!(c.breaker.open_for, Duration::from_secs(45));
        assert_eq!(c.session_budget.capacity, 7);
        assert_eq!(c.session_budget.refill_every, Duration::ZERO);
        assert_eq!(c.process_budget.capacity, 200);
        assert_eq!(c.process_budget.refill_every, Duration::from_millis(50));
        assert_eq!(c.attempt_timeout, Duration::from_millis(1500));
        assert_eq!(c.llm_first_event, Duration::from_secs(6));
        assert_eq!(c.healthy_after, Duration::from_secs(20));
        // Settings nobody asked about stay as they were.
        assert_eq!(c.stt_replay, ResilienceConfig::DEFAULT.stt_replay);
    }

    #[test]
    fn bad_values_name_the_variable() {
        for (name, value) in [
            (ENV_BREAKER_FAILURES, "0"),
            (ENV_BREAKER_FAILURES, "many"),
            (ENV_BREAKER_OPEN_FOR, "0s"),
            (ENV_BREAKER_OPEN_FOR, "soon"),
            (ENV_CALL_RETRY_BUDGET, "0"),
            (ENV_PROCESS_RETRY_BUDGET, "1000001"),
            (ENV_CALL_RETRY_REFILL, "-1s"),
            (ENV_ATTEMPT_TIMEOUT, "0ms"),
            (ENV_LLM_FIRST_EVENT, "0"),
            (ENV_HEALTHY_AFTER, "0s"),
        ] {
            let err = resilience_from_env(&env(&[(name, value)])).unwrap_err();
            assert!(
                err.to_string().contains(name),
                "{name}={value}: {err} does not name the variable"
            );
        }
    }
}
