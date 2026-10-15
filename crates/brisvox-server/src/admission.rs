//! The admission policy of the session manager (`docs/OPERATIONS.md`).
//!
//! [`SessionManager::try_admit`](crate::SessionManager::try_admit) used to refuse on two
//! grounds only: the manager is draining, or `max_sessions` is reached. Two more grounds are
//! evaluated here, in this order after those two:
//!
//! 1. **Pool pressure** ([`OverloadConfig`], [`LoadProbe`]). Calls whose DSP and inference run
//!    on the shared inference pool degrade together when the pool cannot keep up, so taking one
//!    more call while it is saturated hurts the calls the server already has. The gauge reads
//!    the pool (queue depth per worker thread, jobs refused or timed out, and the mean time a
//!    job waited in the queue over a window) and answers [`AdmissionError::Overloaded`] while it
//!    is hot. The queue-wait signal doubles as the CPU signal: a job waits in the queue only
//!    when the worker threads (hence the cores) are busy. Hot is entered on the first sample
//!    over the wait limit or with new refused or timed-out jobs; the queue depth counts once it
//!    has stayed over its limit for `depth_confirm` (a burst of call starts queues one job per
//!    call for microseconds, and a 2-thread pool sees 8 queued jobs at once without being
//!    behind). It is left only after one full window of calm samples at or below the lower
//!    resume limit (hysteresis, so the verdict does not flap at the limit). All time comes from
//!    the manager's injected clock.
//! 2. **Provider budgets** ([`AdmissionSettings::provider_limits`]). A call holds one permit of
//!    the semaphore of every limited provider it uses, for its whole life, and a call that finds
//!    one exhausted is refused with [`AdmissionError::ProviderBudget`] (all or none: permits
//!    taken for the other providers go back). The permits live in the call's
//!    [`SessionSlot`](crate::SessionSlot) and then in its supervisor, so they are given back on
//!    every end: unspent slot, normal end, error, panic, cancellation, abort.
//!
//! Pressure is sampled lazily, on every admission attempt and every `/readyz` probe: there is no
//! background task to cancel. A load balancer that polls `/readyz` takes an overloaded server out
//! of rotation (the probe also advances the calm window), and the gauge never reads a pool it
//! was not given. Because it is lazy, a sample is judged against the previous one only when that
//! one is less than a window old: refused or timed-out jobs from before a longer gap are history,
//! and a calm sample after such a gap clears an overloaded verdict at once (the quiet time counts
//! as calm). A call arriving on a long-idle pool is never refused for what happened minutes ago.
//!
//! The policy is configured from the environment ([`AdmissionSettings::from_env`]):
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `BRISVOX_OVERLOAD_QUEUE_PER_THREAD` | `4` | queued jobs per pool thread that turn the server overloaded; `0` ignores queue depth |
//! | `BRISVOX_OVERLOAD_RESUME_QUEUE_PER_THREAD` | half of the above | queued jobs per thread at or below which the server may recover |
//! | `BRISVOX_OVERLOAD_QUEUE_CONFIRM` | `100ms` | how long the queue must stay over its limit before it counts; `0` sheds on the first such sample |
//! | `BRISVOX_OVERLOAD_QUEUE_WAIT` | `50ms` | mean queue wait over a window that turns it overloaded; `0` ignores waits |
//! | `BRISVOX_OVERLOAD_WINDOW` | `2s` | calm time needed to recover, and the window of the wait average |
//! | `BRISVOX_PROVIDER_LIMITS` | none | `name=n,...`: concurrent calls per provider (`deepgram=50,openai=100`) |

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_inference::PoolHandle;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use crate::config::{AgentConfig, Env};
use crate::session::AdmissionError;
use crate::settings::{count_var, duration_var, var};
use crate::{Error, Result};

/// Environment variable: queued jobs per pool thread that make the server overloaded.
pub const ENV_OVERLOAD_QUEUE_PER_THREAD: &str = "BRISVOX_OVERLOAD_QUEUE_PER_THREAD";
/// Environment variable: queued jobs per pool thread at or below which the server recovers.
pub const ENV_OVERLOAD_RESUME_QUEUE_PER_THREAD: &str = "BRISVOX_OVERLOAD_RESUME_QUEUE_PER_THREAD";
/// Environment variable: how long the queue depth must stay over its limit to count.
pub const ENV_OVERLOAD_QUEUE_CONFIRM: &str = "BRISVOX_OVERLOAD_QUEUE_CONFIRM";
/// Environment variable: mean pool queue wait that makes the server overloaded.
pub const ENV_OVERLOAD_QUEUE_WAIT: &str = "BRISVOX_OVERLOAD_QUEUE_WAIT";
/// Environment variable: calm time needed to recover.
pub const ENV_OVERLOAD_WINDOW: &str = "BRISVOX_OVERLOAD_WINDOW";
/// Environment variable: per-provider concurrency limits.
pub const ENV_PROVIDER_LIMITS: &str = "BRISVOX_PROVIDER_LIMITS";

/// The provider names a limit may be set for: the `provider` values of an agent definition
/// (`docs/CONFIG.md`).
pub const KNOWN_PROVIDERS: [&str; 13] = [
    "deepgram",
    "assemblyai",
    "gladia",
    "sherpa_onnx",
    "openai_realtime_stt",
    "openai",
    "anthropic",
    "gemini",
    "openai_compatible",
    "cartesia",
    "elevenlabs",
    "deepgram_aura",
    "openai_speech",
];

/// Largest accepted provider limit (far above what one process serves).
pub const MAX_PROVIDER_LIMIT: usize = 1_000_000;

/// Largest accepted queue-per-thread limit.
const MAX_QUEUE_PER_THREAD: usize = 1_000_000;

/// Fewest completed jobs in a window for its mean wait to count (a quiet pool's two jobs prove
/// nothing).
const MIN_WINDOW_JOBS: u64 = 8;

/// What the gauge reads from the pool, all cumulative counters except the depth.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LoadSample {
    /// Worker threads (`0` once the pool shut down: no pressure is reported then).
    pub threads: usize,
    /// Jobs queued now, not yet started.
    pub queue_depth: usize,
    /// Jobs that ran to completion since the pool started.
    pub completed: u64,
    /// Sum of the time started jobs waited in the queue.
    pub wait_total: Duration,
    /// Jobs refused (queue full) plus requests that timed out, since the pool started.
    pub saturated: u64,
}

/// A source of [`LoadSample`]s: the inference pool in the server, a script in tests.
///
/// Called from `try_admit` on a transport's accept task: it must be a few atomic reads, not a
/// lock held across work.
pub trait LoadProbe: Send + Sync + 'static {
    /// The pool's counters now.
    fn sample(&self) -> LoadSample;
}

impl LoadProbe for PoolHandle {
    fn sample(&self) -> LoadSample {
        let metrics = self.metrics();
        LoadSample {
            threads: self.threads(),
            queue_depth: metrics.queue_depth,
            completed: metrics.completed,
            wait_total: metrics.wait_total,
            saturated: metrics.rejected.saturating_add(metrics.timed_out),
        }
    }
}

/// When the server counts as overloaded (see the [module docs](self)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverloadConfig {
    /// Queued jobs per pool thread at or above which the server is overloaded; `0` ignores the
    /// queue depth.
    pub shed_queue_per_thread: u32,
    /// Queued jobs per pool thread at or below which it may recover (below `shed`).
    pub resume_queue_per_thread: u32,
    /// How long the queue depth must stay at or over `shed_queue_per_thread` (every sample in
    /// between over it too) before it makes the server overloaded; zero sheds on the first such
    /// sample. A pool that is really behind keeps its queue; a burst drains in microseconds.
    pub depth_confirm: Duration,
    /// Mean queue wait over a window at or above which the server is overloaded; zero ignores
    /// waits.
    pub wait_limit: Duration,
    /// Calm time needed to recover, and the length of the wait average's window.
    pub window: Duration,
}

impl OverloadConfig {
    /// Default `shed_queue_per_thread`.
    pub const DEFAULT_SHED_QUEUE_PER_THREAD: u32 = 4;
    /// Default `depth_confirm`.
    pub const DEFAULT_DEPTH_CONFIRM: Duration = Duration::from_millis(100);
    /// Default `wait_limit`.
    pub const DEFAULT_WAIT_LIMIT: Duration = Duration::from_millis(50);
    /// Default `window`.
    pub const DEFAULT_WINDOW: Duration = Duration::from_secs(2);

    /// Whether at least one signal is on.
    pub const fn is_active(&self) -> bool {
        self.shed_queue_per_thread > 0 || !self.wait_limit.is_zero()
    }
}

impl Default for OverloadConfig {
    fn default() -> Self {
        Self {
            shed_queue_per_thread: Self::DEFAULT_SHED_QUEUE_PER_THREAD,
            resume_queue_per_thread: Self::DEFAULT_SHED_QUEUE_PER_THREAD / 2,
            depth_confirm: Self::DEFAULT_DEPTH_CONFIRM,
            wait_limit: Self::DEFAULT_WAIT_LIMIT,
            window: Self::DEFAULT_WINDOW,
        }
    }
}

/// The admission settings of the process: when the pool counts as overloaded and how many
/// concurrent calls each provider may carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionSettings {
    /// The overload rule; `None` turns pressure shedding off. It acts only with a
    /// [`LoadProbe`] ([`AdmissionPolicy::new`]).
    pub overload: Option<OverloadConfig>,
    /// Concurrent calls per provider name (see [`KNOWN_PROVIDERS`]). A provider without an
    /// entry is unlimited.
    pub provider_limits: BTreeMap<String, usize>,
}

impl Default for AdmissionSettings {
    fn default() -> Self {
        Self {
            overload: Some(OverloadConfig::default()),
            provider_limits: BTreeMap::new(),
        }
    }
}

impl AdmissionSettings {
    /// Reads the settings from `env`; unset variables keep their defaults.
    ///
    /// # Errors
    /// [`Error::Config`] naming the variable when a value does not parse, is out of range, a
    /// resume limit is not below its shed limit, or `BRISVOX_PROVIDER_LIMITS` names an unknown
    /// provider or names one twice.
    pub fn from_env(env: &dyn Env) -> Result<Self> {
        let mut overload = OverloadConfig::default();
        if let Some(n) = count_var(env, ENV_OVERLOAD_QUEUE_PER_THREAD, 0, MAX_QUEUE_PER_THREAD)? {
            overload.shed_queue_per_thread = narrow(n);
            overload.resume_queue_per_thread = narrow(n / 2);
        }
        if let Some(n) = count_var(
            env,
            ENV_OVERLOAD_RESUME_QUEUE_PER_THREAD,
            0,
            MAX_QUEUE_PER_THREAD,
        )? {
            overload.resume_queue_per_thread = narrow(n);
        }
        if overload.shed_queue_per_thread > 0
            && overload.resume_queue_per_thread >= overload.shed_queue_per_thread
        {
            return Err(Error::Config(format!(
                "{ENV_OVERLOAD_RESUME_QUEUE_PER_THREAD}: must be lower than \
                 {ENV_OVERLOAD_QUEUE_PER_THREAD} ({})",
                overload.shed_queue_per_thread
            )));
        }
        if let Some(d) = duration_var(env, ENV_OVERLOAD_QUEUE_CONFIRM)? {
            overload.depth_confirm = d;
        }
        if let Some(d) = duration_var(env, ENV_OVERLOAD_QUEUE_WAIT)? {
            overload.wait_limit = d;
        }
        if let Some(d) = duration_var(env, ENV_OVERLOAD_WINDOW)? {
            if d.is_zero() {
                return Err(Error::Config(format!(
                    "{ENV_OVERLOAD_WINDOW}: must be at least 1ms"
                )));
            }
            overload.window = d;
        }
        let provider_limits = match var(env, ENV_PROVIDER_LIMITS)? {
            Some(text) => parse_provider_limits(&text)?,
            None => BTreeMap::new(),
        };
        Ok(Self {
            overload: overload.is_active().then_some(overload),
            provider_limits,
        })
    }

    /// Keeps the limits of the providers in `used` (the ones the process's agent runs on) and
    /// returns the names of the limits dropped. A limit for a provider no call uses would
    /// never count anything, which is more likely a typo in the agent file or the limit than
    /// intent: the caller logs the returned names.
    pub fn restrict_to(&mut self, used: &[&str]) -> Vec<String> {
        let mut dropped = Vec::new();
        self.provider_limits.retain(|name, _| {
            let keep = used.contains(&name.as_str());
            if !keep {
                dropped.push(name.clone());
            }
            keep
        });
        dropped
    }
}

/// The provider names (see [`KNOWN_PROVIDERS`]) the calls of `agent` run on: its STT, LLM and
/// TTS providers, without duplicates.
pub fn agent_providers(agent: &AgentConfig) -> Vec<&'static str> {
    use crate::config::schema::{LlmProviderKind, SttProviderKind, TtsProviderKind};
    let file = agent.definition();
    let names = [
        match *file.stt.provider.get_ref() {
            SttProviderKind::Deepgram => "deepgram",
            SttProviderKind::Assemblyai => "assemblyai",
            SttProviderKind::Gladia => "gladia",
            SttProviderKind::SherpaOnnx => "sherpa_onnx",
            SttProviderKind::OpenaiRealtimeStt => "openai_realtime_stt",
        },
        match *file.llm.provider.get_ref() {
            LlmProviderKind::Openai => "openai",
            LlmProviderKind::Anthropic => "anthropic",
            LlmProviderKind::Gemini => "gemini",
            LlmProviderKind::OpenaiCompatible => "openai_compatible",
        },
        match *file.tts.provider.get_ref() {
            TtsProviderKind::Cartesia => "cartesia",
            TtsProviderKind::Elevenlabs => "elevenlabs",
            TtsProviderKind::DeepgramAura => "deepgram_aura",
            TtsProviderKind::OpenaiSpeech => "openai_speech",
        },
    ];
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

fn narrow(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// `deepgram=50,openai=100` as a map; names are matched case-insensitively against
/// [`KNOWN_PROVIDERS`].
fn parse_provider_limits(text: &str) -> Result<BTreeMap<String, usize>> {
    let bad = |what: String| Error::Config(format!("{ENV_PROVIDER_LIMITS}: {what}"));
    let mut limits = BTreeMap::new();
    for entry in text.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let Some((name, limit)) = entry.split_once('=') else {
            return Err(bad(format!("{entry:?} is not name=number")));
        };
        let name = name.trim().to_ascii_lowercase();
        if !KNOWN_PROVIDERS.contains(&name.as_str()) {
            return Err(bad(format!(
                "unknown provider {name:?} (known: {})",
                KNOWN_PROVIDERS.join(", ")
            )));
        }
        let limit = limit
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=MAX_PROVIDER_LIMIT).contains(n))
            .ok_or_else(|| {
                bad(format!(
                    "{entry:?}: the limit is not a whole number from 1 to {MAX_PROVIDER_LIMIT}"
                ))
            })?;
        if limits.insert(name.clone(), limit).is_some() {
            return Err(bad(format!("{name:?} is given twice")));
        }
    }
    Ok(limits)
}

/// The pressure gauge's last verdict and the numbers behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PressureStats {
    /// Whether new calls are refused for pressure right now.
    pub overloaded: bool,
    /// Jobs queued at the last sample.
    pub queue_depth: usize,
    /// Pool threads at the last sample.
    pub threads: usize,
    /// Queued jobs per thread at the last sample, times 1000.
    pub queue_per_thread_milli: u64,
    /// Mean queue wait of the last window that had enough jobs (zero before there is one).
    pub mean_wait: Duration,
    /// Times the server turned overloaded since it started.
    pub entered: u64,
}

/// One provider's budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderBudgetStats {
    /// The provider name.
    pub name: String,
    /// Most concurrent calls.
    pub limit: usize,
    /// Calls holding a permit now.
    pub in_use: usize,
    /// Admissions refused because this provider was exhausted.
    pub rejected: u64,
}

/// What the policy holds now: the pressure verdict and the budgets.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AdmissionSnapshot {
    /// The pressure gauge; `None` without one (no pool, or shedding off).
    pub pressure: Option<PressureStats>,
    /// The limited providers, by name.
    pub providers: Vec<ProviderBudgetStats>,
}

impl AdmissionSnapshot {
    /// Whether the server is refusing for pressure now.
    pub fn overloaded(&self) -> bool {
        self.pressure.is_some_and(|p| p.overloaded)
    }

    /// A provider with no room left, if any: while one is exhausted no call can be admitted
    /// (every call holds a permit of each limited provider).
    pub fn exhausted_provider(&self) -> Option<&ProviderBudgetStats> {
        self.providers.iter().find(|p| p.in_use >= p.limit)
    }

    /// The JSON members `"pressure"` (only with a gauge) and `"providers"` for `/readyz`,
    /// without the surrounding braces and with a leading comma.
    pub(crate) fn write_json(&self, out: &mut String) {
        if let Some(p) = &self.pressure {
            let _ = write!(
                out,
                r#","pressure":{{"overloaded":{},"queue_depth":{},"threads":{},"queue_per_thread_milli":{},"mean_wait_us":{},"entered":{}}}"#,
                p.overloaded,
                p.queue_depth,
                p.threads,
                p.queue_per_thread_milli,
                p.mean_wait.as_micros(),
                p.entered
            );
        }
        out.push_str(r#","providers":["#);
        for (i, p) in self.providers.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(r#"{"name":""#);
            for c in p.name.chars() {
                match c {
                    '"' | '\\' => {
                        out.push('\\');
                        out.push(c);
                    }
                    c if c.is_control() => out.push(' '),
                    c => out.push(c),
                }
            }
            let _ = write!(
                out,
                r#"","limit":{},"in_use":{},"rejected":{}}}"#,
                p.limit, p.in_use, p.rejected
            );
        }
        out.push(']');
    }
}

/// The permits one admitted call holds (one per limited provider). Dropping them gives the
/// budgets back.
#[derive(Debug, Default)]
pub(crate) struct Permits {
    _held: Vec<OwnedSemaphorePermit>,
}

struct Budget {
    name: Arc<str>,
    limit: usize,
    semaphore: Arc<Semaphore>,
    rejected: AtomicU64,
}

impl Budget {
    fn in_use(&self) -> usize {
        self.limit
            .saturating_sub(self.semaphore.available_permits())
    }
}

/// The admission rules a [`SessionManager`](crate::SessionManager) applies beyond its session
/// limit (see the [module docs](self)). Build it once per process.
pub struct AdmissionPolicy {
    gauge: Option<Gauge>,
    budgets: Vec<Budget>,
}

impl fmt::Debug for AdmissionPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdmissionPolicy")
            .field("pressure", &self.gauge.is_some())
            .field(
                "providers",
                &self.budgets.iter().map(|b| &*b.name).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Default for AdmissionPolicy {
    fn default() -> Self {
        Self::none()
    }
}

impl AdmissionPolicy {
    /// No rules beyond the session limit (what [`SessionManager::new`](crate::SessionManager::new)
    /// uses).
    pub const fn none() -> Self {
        Self {
            gauge: None,
            budgets: Vec::new(),
        }
    }

    /// The rules of `settings`. Pressure shedding needs `probe` (and an active
    /// [`OverloadConfig`]); without either there is no gauge. A limit of zero is raised to one
    /// (the settings' parsing refuses zero; this keeps a hand-built value from locking the
    /// server out).
    pub fn new(settings: &AdmissionSettings, probe: Option<Arc<dyn LoadProbe>>) -> Self {
        let gauge = match (settings.overload, probe) {
            (Some(config), Some(probe)) if config.is_active() => Some(Gauge::new(config, probe)),
            _ => None,
        };
        let budgets = settings
            .provider_limits
            .iter()
            .map(|(name, limit)| {
                let limit = (*limit).clamp(1, Semaphore::MAX_PERMITS);
                Budget {
                    name: Arc::from(name.as_str()),
                    limit,
                    semaphore: Arc::new(Semaphore::new(limit)),
                    rejected: AtomicU64::new(0),
                }
            })
            .collect();
        Self { gauge, budgets }
    }

    /// Whether the pool is under pressure at `now` (samples the pool).
    pub(crate) fn overloaded(&self, now: Instant) -> bool {
        self.gauge
            .as_ref()
            .is_some_and(|g| g.observe(now).overloaded)
    }

    /// One permit of every limited provider, or the refusal naming the first exhausted one
    /// (counted). Never waits.
    pub(crate) fn acquire(&self) -> std::result::Result<Permits, AdmissionError> {
        let mut held = Vec::with_capacity(self.budgets.len());
        for budget in &self.budgets {
            match Arc::clone(&budget.semaphore).try_acquire_owned() {
                Ok(permit) => held.push(permit),
                Err(_) => {
                    budget.rejected.fetch_add(1, Ordering::Relaxed);
                    // `held` drops here: the permits already taken go back.
                    return Err(AdmissionError::ProviderBudget {
                        provider: Arc::clone(&budget.name),
                        limit: budget.limit,
                    });
                }
            }
        }
        Ok(Permits { _held: held })
    }

    /// The pressure verdict (sampling the pool at `now`) and the budgets.
    pub fn snapshot(&self, now: Instant) -> AdmissionSnapshot {
        AdmissionSnapshot {
            pressure: self.gauge.as_ref().map(|g| g.observe(now)),
            providers: self
                .budgets
                .iter()
                .map(|b| ProviderBudgetStats {
                    name: b.name.to_string(),
                    limit: b.limit,
                    in_use: b.in_use(),
                    rejected: b.rejected.load(Ordering::Relaxed),
                })
                .collect(),
        }
    }
}

/// The pressure gauge: lazy (driven by [`observe`](Self::observe) calls), hysteretic.
struct Gauge {
    config: OverloadConfig,
    probe: Arc<dyn LoadProbe>,
    state: Mutex<GaugeState>,
}

/// Counters at the start of the current wait window.
#[derive(Debug, Clone, Copy)]
struct WindowStart {
    at: Instant,
    completed: u64,
    wait_total: Duration,
}

struct GaugeState {
    overloaded: bool,
    /// Since when every sample was calm (only while overloaded).
    calm_since: Option<Instant>,
    window: Option<WindowStart>,
    /// `LoadSample::saturated` at the previous sample; `None` before the first.
    saturated_seen: Option<u64>,
    /// When the previous sample was taken; `None` before the first.
    last_sample_at: Option<Instant>,
    /// The last window with enough jobs: mean wait at or above the limit.
    wait_hot: bool,
    /// ... and below half the limit (so a recovery needs clear headroom, not a borderline).
    wait_calm: bool,
    mean_wait: Duration,
    /// Since when every sample had the queue over its limit; `None` after one that did not.
    depth_over_since: Option<Instant>,
    last_depth: usize,
    last_threads: usize,
    entered: u64,
}

impl Gauge {
    fn new(config: OverloadConfig, probe: Arc<dyn LoadProbe>) -> Self {
        Self {
            config,
            probe,
            state: Mutex::new(GaugeState {
                overloaded: false,
                calm_since: None,
                window: None,
                saturated_seen: None,
                last_sample_at: None,
                wait_hot: false,
                wait_calm: true,
                mean_wait: Duration::ZERO,
                depth_over_since: None,
                last_depth: 0,
                last_threads: 0,
                entered: 0,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, GaugeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes a sample at `now`, applies the rules and returns the verdict.
    fn observe(&self, now: Instant) -> PressureStats {
        let sample = self.probe.sample();
        let config = &self.config;
        let mut st = self.lock();

        let threads = u128::try_from(sample.threads).unwrap_or(u128::MAX);
        let depth = u128::try_from(sample.queue_depth).unwrap_or(u128::MAX);
        let per_thread_milli = depth.saturating_mul(1000).checked_div(threads).unwrap_or(0);
        let per_thread_milli = u64::try_from(per_thread_milli).unwrap_or(u64::MAX);
        st.last_depth = sample.queue_depth;
        st.last_threads = sample.threads;

        // The gauge is lazy: the previous sample may be minutes old. Events and verdicts from
        // before a whole window ago say nothing about the pool now.
        let previous_at = st.last_sample_at;
        let stale =
            previous_at.is_some_and(|at| now.saturating_duration_since(at) >= config.window);
        st.last_sample_at = Some(now);

        // Jobs refused or timed out since the previous sample (none for the first one, and not
        // those of a stale gap).
        let new_saturation = !stale
            && st
                .saturated_seen
                .is_some_and(|seen| sample.saturated > seen);
        st.saturated_seen = Some(sample.saturated);

        // The wait window rolls when it is old enough.
        match st.window {
            None => st.window = Some(window_start(now, &sample)),
            Some(w) if now.saturating_duration_since(w.at) >= config.window => {
                let jobs = sample.completed.saturating_sub(w.completed);
                let waited = sample.wait_total.saturating_sub(w.wait_total);
                let mean = u32::try_from(jobs)
                    .ok()
                    .filter(|_| jobs >= MIN_WINDOW_JOBS)
                    .and_then(|jobs| waited.checked_div(jobs));
                match mean {
                    Some(mean) if !config.wait_limit.is_zero() => {
                        st.mean_wait = mean;
                        st.wait_hot = mean >= config.wait_limit;
                        st.wait_calm = mean.saturating_mul(2) < config.wait_limit;
                    }
                    // Too few jobs to judge (an idle pool is a calm pool), or waits are off.
                    _ => {
                        st.wait_hot = false;
                        st.wait_calm = true;
                    }
                }
                st.window = Some(window_start(now, &sample));
            }
            Some(_) => {}
        }

        let shed = u64::from(config.shed_queue_per_thread).saturating_mul(1000);
        let resume = u64::from(config.resume_queue_per_thread).saturating_mul(1000);
        let depth_over = config.shed_queue_per_thread > 0 && per_thread_milli >= shed;
        if !depth_over || stale {
            st.depth_over_since = None;
        }
        let depth_hot = depth_over
            && now.saturating_duration_since(*st.depth_over_since.get_or_insert(now))
                >= config.depth_confirm;
        let depth_calm = config.shed_queue_per_thread == 0 || per_thread_milli <= resume;
        let hot = depth_hot || new_saturation || st.wait_hot;
        let calm = depth_calm && !new_saturation && st.wait_calm;

        if hot {
            st.calm_since = None;
            if !st.overloaded {
                st.overloaded = true;
                st.entered = st.entered.saturating_add(1);
                tracing::warn!(
                    target: "brisvox::server",
                    queue_depth = sample.queue_depth,
                    threads = sample.threads,
                    new_saturation,
                    mean_wait = ?st.mean_wait,
                    "pool under pressure: refusing new calls"
                );
            }
        } else if st.overloaded {
            if calm {
                // After a stale gap the quiet time counts as calm: the pool had a whole window
                // without a sample that could say otherwise, and this one is calm.
                let from = match previous_at {
                    Some(at) if stale => at,
                    _ => now,
                };
                let since = *st.calm_since.get_or_insert(from);
                if now.saturating_duration_since(since) >= config.window {
                    st.overloaded = false;
                    st.calm_since = None;
                    tracing::info!(target: "brisvox::server", "pool pressure cleared: admitting calls again");
                }
            } else {
                // Between the resume and shed limits: not calm enough to recover yet.
                st.calm_since = None;
            }
        }

        PressureStats {
            overloaded: st.overloaded,
            queue_depth: st.last_depth,
            threads: st.last_threads,
            queue_per_thread_milli: per_thread_milli,
            mean_wait: st.mean_wait,
            entered: st.entered,
        }
    }
}

const fn window_start(at: Instant, sample: &LoadSample) -> WindowStart {
    WindowStart {
        at,
        completed: sample.completed,
        wait_total: sample.wait_total,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;

    use brisvox_core::{Clock, SimClock};

    use super::*;

    /// The simulation clock's reading (the policy takes `now` from its caller).
    fn now() -> Instant {
        SimClock::new().now()
    }

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// A pool the test drives by hand.
    #[derive(Default)]
    struct Script {
        threads: AtomicUsize,
        depth: AtomicUsize,
        completed: AtomicU64,
        wait_ns: AtomicU64,
        saturated: AtomicU64,
    }

    impl Script {
        fn pool(threads: usize) -> Arc<Self> {
            let s = Self::default();
            s.threads.store(threads, Ordering::SeqCst);
            Arc::new(s)
        }

        fn depth(&self, n: usize) {
            self.depth.store(n, Ordering::SeqCst);
        }
    }

    impl LoadProbe for Script {
        fn sample(&self) -> LoadSample {
            LoadSample {
                threads: self.threads.load(Ordering::SeqCst),
                queue_depth: self.depth.load(Ordering::SeqCst),
                completed: self.completed.load(Ordering::SeqCst),
                wait_total: Duration::from_nanos(self.wait_ns.load(Ordering::SeqCst)),
                saturated: self.saturated.load(Ordering::SeqCst),
            }
        }
    }

    fn policy(script: &Arc<Script>, overload: OverloadConfig) -> AdmissionPolicy {
        let probe: Arc<dyn LoadProbe> = script.clone();
        AdmissionPolicy::new(
            &AdmissionSettings {
                overload: Some(overload),
                provider_limits: BTreeMap::new(),
            },
            Some(probe),
        )
    }

    fn config() -> OverloadConfig {
        OverloadConfig {
            shed_queue_per_thread: 4,
            resume_queue_per_thread: 1,
            // These tests judge single samples; the confirmation has a test of its own.
            depth_confirm: Duration::ZERO,
            wait_limit: Duration::from_millis(50),
            window: Duration::from_secs(2),
        }
    }

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn the_defaults_and_every_variable_parse() {
        let s = AdmissionSettings::from_env(&env(&[])).unwrap();
        assert_eq!(s, AdmissionSettings::default());
        let o = s.overload.unwrap();
        assert_eq!((o.shed_queue_per_thread, o.resume_queue_per_thread), (4, 2));

        let s = AdmissionSettings::from_env(&env(&[
            (ENV_OVERLOAD_QUEUE_PER_THREAD, " 10 "),
            (ENV_OVERLOAD_RESUME_QUEUE_PER_THREAD, "3"),
            (ENV_OVERLOAD_QUEUE_WAIT, "20ms"),
            (ENV_OVERLOAD_WINDOW, "5s"),
            (ENV_OVERLOAD_QUEUE_CONFIRM, "250ms"),
            (ENV_PROVIDER_LIMITS, "Deepgram=50, openai = 100,"),
        ]))
        .unwrap();
        let o = s.overload.unwrap();
        assert_eq!(o.shed_queue_per_thread, 10);
        assert_eq!(o.resume_queue_per_thread, 3);
        assert_eq!(o.wait_limit, Duration::from_millis(20));
        assert_eq!(o.window, Duration::from_secs(5));
        assert_eq!(o.depth_confirm, Duration::from_millis(250));
        assert_eq!(s.provider_limits.get("deepgram"), Some(&50));
        assert_eq!(s.provider_limits.get("openai"), Some(&100));

        // The resume limit follows the shed limit when only that is set.
        let s = AdmissionSettings::from_env(&env(&[(ENV_OVERLOAD_QUEUE_PER_THREAD, "9")])).unwrap();
        assert_eq!(s.overload.unwrap().resume_queue_per_thread, 4);
    }

    #[test]
    fn both_signals_off_turns_shedding_off() {
        let s = AdmissionSettings::from_env(&env(&[
            (ENV_OVERLOAD_QUEUE_PER_THREAD, "0"),
            (ENV_OVERLOAD_QUEUE_WAIT, "0"),
        ]))
        .unwrap();
        assert_eq!(s.overload, None);
        // One signal alone keeps it on.
        let s = AdmissionSettings::from_env(&env(&[(ENV_OVERLOAD_QUEUE_PER_THREAD, "0")])).unwrap();
        assert!(s.overload.is_some());
    }

    #[test]
    fn bad_values_name_the_variable() {
        for (name, value) in [
            (ENV_OVERLOAD_QUEUE_PER_THREAD, "many"),
            (ENV_OVERLOAD_QUEUE_PER_THREAD, "-1"),
            (ENV_OVERLOAD_RESUME_QUEUE_PER_THREAD, "4"),
            (ENV_OVERLOAD_QUEUE_WAIT, "fast"),
            (ENV_OVERLOAD_WINDOW, "0s"),
            (ENV_OVERLOAD_WINDOW, "soon"),
            (ENV_PROVIDER_LIMITS, "deepgram"),
            (ENV_PROVIDER_LIMITS, "deepgram=0"),
            (ENV_PROVIDER_LIMITS, "deepgram=lots"),
            (ENV_PROVIDER_LIMITS, "deepgram=2000000"),
            (ENV_PROVIDER_LIMITS, "nosuch=3"),
            (ENV_PROVIDER_LIMITS, "openai=1,openai=2"),
        ] {
            let err = AdmissionSettings::from_env(&env(&[(name, value)])).unwrap_err();
            assert!(
                err.to_string().contains(name),
                "{name}={value}: {err} does not name the variable"
            );
        }
    }

    #[test]
    fn limits_for_providers_no_call_uses_are_dropped_and_reported() {
        let mut s = AdmissionSettings::from_env(&env(&[(
            ENV_PROVIDER_LIMITS,
            "deepgram=5,openai=6,cartesia=7",
        )]))
        .unwrap();
        let dropped = s.restrict_to(&["deepgram", "cartesia"]);
        assert_eq!(dropped, vec!["openai".to_owned()]);
        assert_eq!(s.provider_limits.len(), 2);
    }

    #[tokio::test]
    async fn a_queue_over_the_limit_sheds_at_once_and_recovers_only_after_a_calm_window() {
        let script = Script::pool(2);
        let p = policy(&script, config());
        let t0 = now();

        // 7 queued on 2 threads: 3.5 per thread, under the shed limit of 4.
        script.depth(7);
        assert!(!p.overloaded(at(t0, 0)));
        // 8 queued: 4 per thread, at the limit. Shed at once, on the very sample.
        script.depth(8);
        assert!(p.overloaded(at(t0, 100)));

        // Back at the resume limit (1 per thread): not calm for long enough yet.
        script.depth(2);
        assert!(p.overloaded(at(t0, 200)));
        assert!(
            p.overloaded(at(t0, 2_100)),
            "calm for 1.9 s of the 2 s window"
        );
        // The window of calm samples is over.
        assert!(!p.overloaded(at(t0, 2_200)));
        assert_eq!(p.snapshot(at(t0, 2_200)).pressure.unwrap().entered, 1);
    }

    #[tokio::test]
    async fn a_queue_counts_only_once_it_stayed_over_the_limit_for_the_confirmation_time() {
        let script = Script::pool(2);
        let p = policy(
            &script,
            OverloadConfig {
                depth_confirm: Duration::from_millis(100),
                ..config()
            },
        );
        let t0 = now();
        // A burst of call starts: 24 jobs queued on 2 threads, drained by the next sample.
        script.depth(24);
        assert!(!p.overloaded(at(t0, 0)));
        script.depth(0);
        assert!(!p.overloaded(at(t0, 5)));
        // Over again: the confirmation starts over, and a sample within it does not shed ...
        script.depth(10);
        assert!(!p.overloaded(at(t0, 150)));
        assert!(!p.overloaded(at(t0, 200)));
        // ... while one that finds the queue still over the limit after it does.
        assert!(p.overloaded(at(t0, 250)));
        assert_eq!(p.snapshot(at(t0, 250)).pressure.unwrap().entered, 1);
        // A stale first sample does not confirm a later one.
        script.depth(0);
        let later = at(t0, 600_000);
        assert!(!p.overloaded(later));
        script.depth(10);
        assert!(!p.overloaded(at(later, 3_000)));
        assert!(!p.overloaded(at(later, 600_000)));
        assert!(p.overloaded(at(later, 600_100)));
    }

    #[tokio::test]
    async fn between_the_resume_and_shed_limits_the_verdict_holds_and_the_calm_clock_restarts() {
        let script = Script::pool(1);
        let p = policy(&script, config());
        let t0 = now();

        script.depth(5);
        assert!(p.overloaded(at(t0, 0)));
        // Calm from t=100 ...
        script.depth(0);
        assert!(p.overloaded(at(t0, 100)));
        // ... but a middling sample (3 per thread: under shed, over resume) restarts the clock,
        script.depth(3);
        assert!(p.overloaded(at(t0, 1_500)));
        script.depth(0);
        assert!(p.overloaded(at(t0, 1_600)));
        // so 2.1 s after the first calm sample is not enough, 2 s after the last restart is.
        assert!(p.overloaded(at(t0, 3_500)));
        assert!(!p.overloaded(at(t0, 3_700)));
    }

    #[tokio::test]
    async fn a_hot_sample_during_recovery_restarts_it_and_counts_one_entry_per_episode() {
        let script = Script::pool(1);
        let p = policy(&script, config());
        let t0 = now();
        script.depth(4);
        assert!(p.overloaded(at(t0, 0)));
        script.depth(0);
        assert!(p.overloaded(at(t0, 100)));
        script.depth(9);
        assert!(p.overloaded(at(t0, 1_900)));
        script.depth(0);
        assert!(p.overloaded(at(t0, 2_000)));
        assert!(p.overloaded(at(t0, 3_900)));
        assert!(!p.overloaded(at(t0, 4_100)));
        // A second episode.
        script.depth(4);
        assert!(p.overloaded(at(t0, 5_000)));
        assert_eq!(p.snapshot(at(t0, 5_000)).pressure.unwrap().entered, 2);
    }

    #[tokio::test]
    async fn refused_or_timed_out_pool_jobs_shed_the_next_admission() {
        let script = Script::pool(4);
        let p = policy(&script, config());
        let t0 = now();
        assert!(
            !p.overloaded(at(t0, 0)),
            "the first sample only sets the baseline"
        );
        script.saturated.store(3, Ordering::SeqCst);
        assert!(p.overloaded(at(t0, 10)));
        // No further events: calm after a window.
        assert!(p.overloaded(at(t0, 20)));
        assert!(!p.overloaded(at(t0, 2_100)));
    }

    #[tokio::test]
    async fn pool_events_from_before_a_long_gap_do_not_shed_the_next_call() {
        let script = Script::pool(4);
        let p = policy(&script, config());
        let t0 = now();
        assert!(!p.overloaded(at(t0, 0)));
        // A job timed out, and nobody asked for ten minutes (no /readyz polling either).
        script.saturated.store(1, Ordering::SeqCst);
        let later = at(t0, 600_000);
        assert!(!p.overloaded(later), "the event is ten minutes old");
        assert_eq!(p.snapshot(later).pressure.unwrap().entered, 0);
        // The baseline moved on: a new event within the window still sheds.
        script.saturated.store(2, Ordering::SeqCst);
        assert!(p.overloaded(at(later, 10)));
    }

    #[tokio::test]
    async fn an_overloaded_verdict_older_than_a_window_is_re_evaluated_on_the_next_call() {
        let script = Script::pool(1);
        let p = policy(&script, config());
        let t0 = now();
        script.depth(9);
        assert!(p.overloaded(at(t0, 0)));
        // The queue drained long ago; the first call afterwards is judged on the pool now.
        script.depth(0);
        assert!(!p.overloaded(at(t0, 600_000)));
        assert_eq!(p.snapshot(at(t0, 600_000)).pressure.unwrap().entered, 1);
        // A gap does not excuse a hot sample.
        script.depth(9);
        assert!(p.overloaded(at(t0, 1_200_000)));
        // Nor a middling one: not calm, so the verdict holds after a gap too.
        script.depth(3);
        assert!(p.overloaded(at(t0, 1_800_000)));
    }

    #[tokio::test]
    async fn a_slow_queue_sheds_even_when_it_is_short_and_a_quiet_pool_is_never_judged() {
        let script = Script::pool(4);
        let p = policy(&script, config());
        let t0 = now();
        assert!(!p.overloaded(at(t0, 0)));
        // Two jobs in the window with a huge wait: too few to judge.
        script.completed.store(2, Ordering::SeqCst);
        script.wait_ns.store(1_000_000_000, Ordering::SeqCst);
        assert!(!p.overloaded(at(t0, 2_100)));
        // 100 jobs that waited 80 ms on average (over the 50 ms limit) in the next window.
        script.completed.store(102, Ordering::SeqCst);
        script
            .wait_ns
            .store(1_000_000_000 + 8_000_000_000, Ordering::SeqCst);
        assert!(p.overloaded(at(t0, 4_200)));
        assert_eq!(
            p.snapshot(at(t0, 4_200)).pressure.unwrap().mean_wait,
            Duration::from_millis(80)
        );
        // The following window: 100 jobs at 10 ms. Calm; recovery takes one more window.
        script.completed.store(202, Ordering::SeqCst);
        script
            .wait_ns
            .store(9_000_000_000 + 1_000_000_000, Ordering::SeqCst);
        // (Sampled in between: a gap of a whole window would itself count as calm.)
        assert!(p.overloaded(at(t0, 5_000)));
        assert!(p.overloaded(at(t0, 6_300)));
        assert!(!p.overloaded(at(t0, 8_400)));
    }

    #[tokio::test]
    async fn a_pool_with_no_threads_or_no_gauge_reports_no_pressure() {
        let script = Script::pool(0);
        script.depth(100);
        let p = policy(&script, config());
        assert!(!p.overloaded(now()));
        assert!(!AdmissionPolicy::none().overloaded(now()));
        assert_eq!(
            AdmissionPolicy::none().snapshot(now()),
            AdmissionSnapshot::default()
        );
        // Settings with shedding off build no gauge even with a probe.
        let probe: Arc<dyn LoadProbe> = Script::pool(1);
        let off = AdmissionPolicy::new(
            &AdmissionSettings {
                overload: None,
                provider_limits: BTreeMap::new(),
            },
            Some(probe),
        );
        assert!(off.snapshot(now()).pressure.is_none());
    }

    /// The production probe against a real pool: depth, waits and both kinds of saturation reach
    /// the gauge from the right counters.
    #[tokio::test]
    async fn the_pool_probe_reads_depth_waits_and_saturation_from_a_real_pool() {
        use std::sync::mpsc;

        use brisvox_core::SystemClock;
        use brisvox_inference::{InferencePool, PoolConfig};

        let clock = SystemClock::shared();
        let pool = InferencePool::new(
            &PoolConfig {
                threads: 1,
                queue_capacity: 2,
                name: "admission-probe".to_owned(),
            },
            clock.clone(),
        )
        .unwrap();
        let handle = pool.handle();
        let idle = LoadProbe::sample(&handle);
        assert_eq!(
            idle,
            LoadSample {
                threads: 1,
                ..LoadSample::default()
            }
        );

        let gauge = AdmissionPolicy::new(
            &AdmissionSettings {
                overload: Some(config()),
                provider_limits: BTreeMap::new(),
            },
            Some(Arc::new(handle.clone())),
        );
        let t0 = now();
        assert!(!gauge.overloaded(t0), "the first sample is the baseline");

        // Occupy the only worker until released.
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let blocker = {
            let handle = handle.clone();
            tokio::spawn(async move {
                handle
                    .run(Duration::from_secs(30), move || {
                        let _ = started_tx.send(());
                        let _ = release_rx.recv_timeout(Duration::from_secs(30));
                    })
                    .await
            })
        };
        tokio::task::spawn_blocking(move || started_rx.recv_timeout(Duration::from_secs(10)))
            .await
            .unwrap()
            .unwrap();

        // A queued job whose caller gives up: a timed-out request.
        let timed_out = handle.run(Duration::from_millis(25), || ()).await;
        assert!(timed_out.is_err());
        assert_eq!(LoadProbe::sample(&handle).saturated, 1);

        // A job that waits in the queue behind the blocker, then a full queue.
        let waiter = {
            let handle = handle.clone();
            tokio::spawn(async move { handle.run(Duration::from_secs(30), || ()).await })
        };
        for _ in 0..400 {
            if handle.queue_depth() >= 2 {
                break;
            }
            clock.sleep(Duration::from_millis(5)).await;
        }
        let refused = handle.run(Duration::from_secs(30), || ()).await;
        assert!(refused.is_err());
        let loaded = LoadProbe::sample(&handle);
        assert_eq!(loaded.threads, 1);
        assert_eq!(loaded.queue_depth, 2, "{loaded:?}");
        assert_eq!(loaded.saturated, 2, "one timed out plus one refused");
        assert_eq!(loaded.completed, 0);

        // The gauge sheds on that sample: 2 queued on 1 thread is under the depth limit of 4, so
        // it is the saturation counters that tripped it.
        assert!(gauge.overloaded(at(t0, 50)));

        // Let the queued job wait a while, then release the worker: it runs after having waited.
        clock.sleep(Duration::from_millis(60)).await;
        release_tx.send(()).unwrap();
        blocker.await.unwrap().unwrap();
        waiter.await.unwrap().unwrap();
        let done = LoadProbe::sample(&handle);
        assert_eq!(done.completed, 2);
        assert_eq!(done.queue_depth, 0);
        assert!(
            done.wait_total >= Duration::from_millis(40),
            "the waiter queued behind a blocked worker: {done:?}"
        );
        pool.shutdown();
    }

    fn budgets(limits: &[(&str, usize)]) -> AdmissionPolicy {
        AdmissionPolicy::new(
            &AdmissionSettings {
                overload: None,
                provider_limits: limits.iter().map(|(n, l)| ((*n).to_owned(), *l)).collect(),
            },
            None,
        )
    }

    #[test]
    fn permits_are_all_or_none_counted_per_provider_and_given_back_on_drop() {
        let p = budgets(&[("deepgram", 2), ("openai", 1)]);
        let a = p.acquire().unwrap();
        // openai is exhausted: the refusal names it, and the deepgram permit taken first for
        // this attempt went back.
        let err = p.acquire().unwrap_err();
        assert_eq!(
            err,
            AdmissionError::ProviderBudget {
                provider: Arc::from("openai"),
                limit: 1
            }
        );
        let snap = p.snapshot(now());
        let by = |n: &str| snap.providers.iter().find(|b| b.name == n).unwrap().clone();
        assert_eq!((by("deepgram").in_use, by("deepgram").rejected), (1, 0));
        assert_eq!((by("openai").in_use, by("openai").rejected), (1, 1));
        assert_eq!(snap.exhausted_provider().unwrap().name, "openai");

        drop(a);
        let snap = p.snapshot(now());
        assert!(snap.providers.iter().all(|b| b.in_use == 0));
        assert!(snap.exhausted_provider().is_none());
        assert!(p.acquire().is_ok());
    }

    #[test]
    fn the_json_members_list_pressure_and_budgets() {
        let mut out = String::new();
        AdmissionSnapshot::default().write_json(&mut out);
        assert_eq!(out, r#","providers":[]"#);

        let snap = AdmissionSnapshot {
            pressure: Some(PressureStats {
                overloaded: true,
                queue_depth: 9,
                threads: 2,
                queue_per_thread_milli: 4500,
                mean_wait: Duration::from_micros(1234),
                entered: 3,
            }),
            providers: vec![ProviderBudgetStats {
                name: "a\"b".into(),
                limit: 5,
                in_use: 2,
                rejected: 7,
            }],
        };
        let mut out = String::new();
        snap.write_json(&mut out);
        assert_eq!(
            out,
            r#","pressure":{"overloaded":true,"queue_depth":9,"threads":2,"queue_per_thread_milli":4500,"mean_wait_us":1234,"entered":3},"providers":[{"name":"a\"b","limit":5,"in_use":2,"rejected":7}]"#
        );
    }
}
