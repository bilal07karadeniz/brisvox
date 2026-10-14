//! Configuration of one engine session.
//!
//! Every default here is a documented decision from `docs/PIPELINE.md` (sections 1, 2, 5, 7).
//! [`EngineConfig::validate`] rejects values that cannot work, once, before a session starts.

use std::time::Duration;

use brisvox_agent::tools::FillerConfig;
use brisvox_agent::{
    BackchannelSet, Confirm, DEFAULT_BACKCHANNEL_MAX_SPEECH, DEFAULT_BACKCHANNEL_WAIT,
    DEFAULT_VAD_HANGOVER, InterruptionPolicy, TurnPolicy, WindowPolicy,
};
use brisvox_core::frame::SampleRate;
use brisvox_core::queue::{OverflowPolicies, QueueConfig};
use brisvox_transport::CallRates;

use crate::error::{Error, Result};

/// Audio rates inside the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineRates {
    /// The rate the user-side chain works at: VAD, Smart Turn and STT. Caller audio is resampled
    /// to it right after the transport pump. Must be 16 kHz (the only rate the Silero VAD and
    /// Smart Turn wrappers of `brisvox-inference` accept); [`EngineConfig::validate`] rejects
    /// others. Default 16 kHz.
    pub model: SampleRate,
    /// The rate the TTS provider delivers. The TTS stage resamples it to the call's output rate,
    /// so any [`SampleRate`] works and `validate` does not restrict it. Default 24 kHz.
    pub tts: SampleRate,
    /// The call rates assumed when the transport does not report any (the loopback does not;
    /// `CallInfo::sample_rate` returns `None`). Default 16 kHz both ways.
    pub fallback_call: CallRates,
    /// Length of one outgoing audio frame, the granularity of interruption and of playback
    /// positions. It must be a whole number of samples at the model rate and at both fallback
    /// call rates (`validate` checks); the rate a transport reports at `Started` is checked by
    /// the pump when it is known. Default 20 ms.
    pub frame: Duration,
}

impl EngineRates {
    /// 16 kHz models and STT, 24 kHz TTS, 16 kHz fallback call, 20 ms frames.
    pub const DEFAULT: Self = Self {
        model: SampleRate::Hz16000,
        tts: SampleRate::Hz24000,
        fallback_call: CallRates::symmetric(SampleRate::Hz16000),
        frame: Duration::from_millis(20),
    };
}

impl EngineRates {
    /// Checks the rates a transport reported at `Started` against the frame length: the engine
    /// cuts audio into `frame`-long pieces at both call rates, so `frame` must be a whole number
    /// of samples at each (the fallback rates are checked by [`EngineConfig::validate`]).
    ///
    /// # Errors
    /// [`Error::Config`] when it is not.
    pub fn check_call_rates(&self, rates: CallRates) -> Result<()> {
        if whole_samples(self.frame, rates.input) && whole_samples(self.frame, rates.output) {
            Ok(())
        } else {
            Err(Error::Config(
                "rates.frame must be a whole number of samples at the call's rates",
            ))
        }
    }
}

impl Default for EngineRates {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Inbox shapes of the stages. Every queue is bounded; the overflow policy of each audio lane is
/// in the field docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueConfigs {
    /// The first stage after the transport pump: 8 data frames (160 ms of 20 ms audio),
    /// drop-oldest audio (`OverflowPolicies::REALTIME_INPUT`), so a slow pipeline loses the
    /// oldest caller audio instead of building latency. Drops are counted in
    /// `RunningPipeline::stats`.
    pub input: QueueConfig,
    /// The user-side stages after the first (STT, turn driver): 8 data frames, lossless
    /// (backpressure). Loss on this side happens only at the deliberate lossy points (the first
    /// stage, the VAD tee, the STT audio send).
    pub user: QueueConfig,
    /// The bot-side stages (LLM runner, segmenter, TTS): 32 data frames, lossless: text and
    /// control frames must not be lost.
    pub bot: QueueConfig,
    /// The stage that writes to the transport: 4 data frames, lossless. Together with the
    /// transport's wire queue this bounds the audio that was released but not yet played.
    pub sink: QueueConfig,
}

impl QueueConfigs {
    /// The design values, see the field docs.
    pub const DEFAULT: Self = Self {
        input: QueueConfig {
            system_capacity: 32,
            data_capacity: 8,
            policies: OverflowPolicies::REALTIME_INPUT,
        },
        user: QueueConfig {
            system_capacity: 32,
            data_capacity: 8,
            policies: OverflowPolicies::DEFAULT,
        },
        bot: QueueConfig {
            system_capacity: 32,
            data_capacity: 32,
            policies: OverflowPolicies::DEFAULT,
        },
        sink: QueueConfig {
            system_capacity: 32,
            data_capacity: 4,
            policies: OverflowPolicies::DEFAULT,
        },
    };

    fn all(&self) -> [QueueConfig; 4] {
        [self.input, self.user, self.bot, self.sink]
    }
}

impl Default for QueueConfigs {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Explicit timeouts. Every network or IO await in the engine uses one of these, measured on the
/// injected clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Establishing a provider session (STT or TTS connect, LLM warm-up). Default 5 s.
    pub connect: Duration,
    /// One send to a provider. Default 2 s.
    pub send: Duration,
    /// Wait before a reconnect attempt. Default 1 s.
    pub reconnect_backoff: Duration,
    /// Reconnect attempts before the stage degrades. Default 3.
    pub max_reconnects: u32,
    /// LLM request to first event. Default 8 s.
    pub llm_first_token: Duration,
    /// Silence between LLM events once the stream started. Default 15 s.
    pub llm_idle: Duration,
    /// One VAD job on the inference pool. Default 250 ms.
    pub vad_job: Duration,
    /// One Smart Turn analysis on the inference pool. Default 2 s.
    pub analysis_job: Duration,
    /// Joining an aborted helper task. Default 500 ms.
    pub join: Duration,
    /// Graceful pipeline stop before it is cancelled. Default 5 s.
    pub stop: Duration,
    /// Waiting for the client's echo of the end-of-response mark before the engine decides the
    /// bot finished speaking (a client that never acks). Default 10 s.
    pub mark: Duration,
    /// How long the STT stage waits for the answer to a `Finalize` before it gives the turn a
    /// synthetic `UtteranceFinal` and restarts the STT session (a late real answer is then
    /// never seen). Longer than [`EngineConfig::final_wait`], which is how long the turn driver
    /// holds a turn end for the final: the driver gives up first and the provider gets a
    /// little more time before its session is declared dead. Default 3 s.
    pub finalize: Duration,
}

impl Timeouts {
    /// The design values, see the field docs.
    pub const DEFAULT: Self = Self {
        connect: Duration::from_secs(5),
        send: Duration::from_secs(2),
        reconnect_backoff: Duration::from_secs(1),
        max_reconnects: 3,
        llm_first_token: Duration::from_secs(8),
        llm_idle: Duration::from_secs(15),
        vad_job: Duration::from_millis(250),
        analysis_job: Duration::from_secs(2),
        join: Duration::from_millis(500),
        stop: Duration::from_secs(5),
        mark: Duration::from_secs(10),
        finalize: Duration::from_secs(3),
    };

    fn durations(&self) -> [Duration; 11] {
        [
            self.connect,
            self.send,
            self.reconnect_backoff,
            self.llm_first_token,
            self.llm_idle,
            self.vad_job,
            self.analysis_job,
            self.join,
            self.stop,
            self.mark,
            self.finalize,
        ]
    }
}

impl Default for Timeouts {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The barge-in guards of a session: a `Copy` description of an
/// [`InterruptionPolicy`], so that [`EngineConfig`] stays `Copy`.
/// [`InterruptionConfig::policy`] builds the policy the [`TurnDriver`](crate::TurnDriver) hands
/// to its turn machine; a deployment that needs other backchannel phrase lists passes its own
/// policy with [`TurnDriver::with_interruption`](crate::TurnDriver::with_interruption).
///
/// The field docs are those of the policy's fields of the same name. The numbers of
/// [`InterruptionConfig::RECOMMENDED`] are a starting point chosen without live data (not
/// tuned).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterruptionConfig {
    /// VAD must stay active this long before a barge-in interrupts. Zero: off.
    pub min_speech: Duration,
    /// STT interims must hold this many words before a barge-in interrupts. Zero: off.
    pub min_words: u32,
    /// How `min_speech` and `min_words` combine when both are on.
    pub confirm: Confirm,
    /// During the first `echo_guard` of the bot's audio VAD alone cannot interrupt; STT must
    /// confirm `echo_min_words` words. Zero: off.
    pub echo_guard: Duration,
    /// Words STT must confirm inside the echo window.
    pub echo_min_words: u32,
    /// Use the English backchannel phrases ("yeah", "uh-huh", ...).
    pub english: bool,
    /// Use the Turkish backchannel phrases ("evet", "hı-hı", ...).
    pub turkish: bool,
    /// How long a candidate without any interim waits before it is assumed not to be a
    /// backchannel. Used only when a backchannel set is on; zero means the policy default.
    pub backchannel_wait: Duration,
    /// Once VAD has been active this long a backchannel verdict stops blocking. Zero: no cap.
    pub backchannel_max_speech: Duration,
    /// The VAD's `min_silence`: how long after the speech ended it reports the stop.
    pub vad_hangover: Duration,
}

impl InterruptionConfig {
    /// Every guard off: each candidate barge-in interrupts at once (the plain always-interrupt behaviour).
    pub const OFF: Self = Self {
        min_speech: Duration::ZERO,
        min_words: 0,
        confirm: Confirm::All,
        echo_guard: Duration::ZERO,
        echo_min_words: 1,
        english: false,
        turkish: false,
        backchannel_wait: Duration::ZERO,
        backchannel_max_speech: Duration::ZERO,
        vad_hangover: Duration::ZERO,
    };

    /// The same guards as [`InterruptionPolicy::recommended`]: 300 ms minimum speech, a 400 ms
    /// echo window needing one STT word, English and Turkish backchannels, a 350 ms wait for
    /// a first interim, a 1.5 s cap on backchannel blocking and a 100 ms VAD hangover.
    pub const RECOMMENDED: Self = Self {
        min_speech: Duration::from_millis(300),
        min_words: 0,
        confirm: Confirm::All,
        echo_guard: Duration::from_millis(400),
        echo_min_words: 1,
        english: true,
        turkish: true,
        backchannel_wait: DEFAULT_BACKCHANNEL_WAIT,
        backchannel_max_speech: DEFAULT_BACKCHANNEL_MAX_SPEECH,
        vad_hangover: DEFAULT_VAD_HANGOVER,
    };

    /// The policy this configuration describes.
    pub fn policy(&self) -> InterruptionPolicy {
        let mut backchannels = Vec::new();
        if self.english {
            backchannels.push(BackchannelSet::english());
        }
        if self.turkish {
            backchannels.push(BackchannelSet::turkish());
        }
        InterruptionPolicy {
            min_speech: self.min_speech,
            min_words: self.min_words,
            confirm: self.confirm,
            echo_guard: self.echo_guard,
            echo_min_words: self.echo_min_words,
            backchannels,
            backchannel_wait: self.backchannel_wait,
            backchannel_max_speech: self.backchannel_max_speech,
            vad_hangover: self.vad_hangover,
        }
    }
}

impl Default for InterruptionConfig {
    /// [`InterruptionConfig::RECOMMENDED`].
    fn default() -> Self {
        Self::RECOMMENDED
    }
}

/// The LLM runner's settings ([`LlmRunner`](crate::LlmRunner)). The timeouts live in
/// [`Timeouts`] (`llm_first_token`, `llm_idle`, `join`, `analysis_job`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LlmConfig {
    /// The limits of the context window every LLM request is built with
    /// ([`brisvox_agent::build_window`]). Default: 4000 tokens, 8 recent turns.
    pub window: WindowPolicy,
    /// LLM rounds that may end in tool calls within one user turn. When the model asks for more
    /// tools after this many rounds, its calls are answered with an error result without running
    /// them and the runner speaks a fixed fallback line. Default 6.
    pub max_tool_rounds: u32,
    /// Retries of a round that failed before it produced any output (an error, a stalled first
    /// token). A failure after output is never retried (the caller may already have heard it).
    /// Default 1.
    pub retries: u32,
    /// Wait before a retry. Default 200 ms.
    pub retry_delay: Duration,
    /// A response whose buffered size is at least this many bytes is finished (its tool-call
    /// arguments parsed) on the [`Offload`](crate::Offload) instead of inline: parsing is linear in
    /// the size (CONTRIBUTING.md: work above about 50 us leaves the async workers). Default 4 KiB.
    pub inline_finish_bytes: usize,
    /// Longest one background refresh of the window summary may take (a summarizer is an LLM
    /// call). A request never waits for it: it is built with the summary at hand, and the
    /// refresh runs after the response, off the request path. After repeated failures the
    /// attempts back off exponentially from this value. Default 8 s.
    pub summary_timeout: Duration,
    /// When the window is still over budget because of one turn with many tool rounds, tool
    /// results of that turn (oldest first) are clipped to this many estimated tokens until the
    /// request fits. Default 48.
    pub tool_result_floor_tokens: usize,
    /// When a slow tool round is announced with a filler phrase: on or off, and the delay
    /// measured on the injected clock from the start of the round. The default is
    /// [`FillerConfig::DEFAULT`]: on, after 1.2 s ([`brisvox_agent::tools::DEFAULT_FILLER_DELAY`])
    /// for every tool that has a filler (a tool opts out with `ToolSpec::with_filler_after(None)`).
    /// `delay: None` uses each tool's own `ToolSpec::filler_after` instead. The runner builds its
    /// [`FillerPolicy`](brisvox_agent::tools::FillerPolicy) from this unless
    /// `LlmRunnerBuilder::with_filler_policy` replaces it.
    pub filler: FillerConfig,
}

impl LlmConfig {
    /// The design values, see the field docs.
    pub const DEFAULT: Self = Self {
        window: WindowPolicy::new(4_000, 8),
        max_tool_rounds: 6,
        retries: 1,
        retry_delay: Duration::from_millis(200),
        inline_finish_bytes: 4 * 1024,
        summary_timeout: Duration::from_secs(8),
        tool_result_floor_tokens: 48,
        filler: FillerConfig::DEFAULT,
    };
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Speculative generation (`docs/PIPELINE.md` section 12): start the LLM at a candidate end
/// of turn with the transcript so far, commit the result when the turn is confirmed with the same
/// text, discard it otherwise. **Off by default**: every default pipeline behaves exactly as
/// without this feature. The measurement (`docs/LATENCY.md`, "Speculative LLM start") kept it
/// opt-in: the gain is at most the STT finalize round trip under the adaptive policy
/// (about 150 ms) and exists only when the partial at the VAD stop already equals the final,
/// which depends on the STT; every miss costs one extra LLM request. Enable it where the
/// hit rate was measured high, or with `on_partial` off under a fixed silence longer than the
/// finals gate (no request from a partial, the gain is the silence beyond the final).
///
/// The [`TurnDriver`](crate::TurnDriver) offers candidates (a non-final `Transcript` downstream,
/// see [`crate::speculation`]), the [`LlmRunner`](crate::LlmRunner) runs them with its output
/// buffered. A runner that drives a flow does not speculate (a pending transition changes the
/// context behind the request).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeculationConfig {
    /// Master switch. Default `false`.
    pub enabled: bool,
    /// Also speculate at the VAD stop with the best partial transcript (the final segments heard
    /// so far, else the latest interim), before the STT final arrives. Faster by the finalize
    /// round trip, but the interim of a real STT often lags the final by a word, so it costs a
    /// wasted request more often. With `false` only the final transcript of the stop is a
    /// candidate. Default `true` (when enabled).
    pub on_partial: bool,
    /// The stable-partial gate: the partial transcript of a VAD stop is offered only
    /// once the caller has stayed silent, and the partial text has not changed, for this long
    /// (measured on the injected clock from the VAD stop; a change of the text after the stop
    /// restarts it, changes before the stop do not count). A caller who resumes inside the wait
    /// costs no request at all; the STT final of the stop (when it comes first) supersedes the
    /// wait, so a gate at or above the STT finalize round trip turns `on_partial` into
    /// final-only. Zero (the default) offers at the VAD stop at once, as before this setting
    /// existed. Only used with [`Self::on_partial`]. Validated: not above [`Self::max_age`].
    pub partial_stable_for: Duration,
    /// Candidates the driver offers per user turn (a restart after a changed text counts), so a
    /// stuttering transcript cannot cost an unbounded number of requests. Default 3.
    pub max_per_turn: u32,
    /// A speculation that was neither committed nor discarded within this long is dropped by
    /// itself (a turn that never confirms). Measured on the injected clock from the request.
    /// Default 5 s.
    pub max_age: Duration,
    /// Events a speculative response may buffer before it is committed. When the buffer is full
    /// the speculative stream waits (back-pressure), nothing is dropped. Default 256; at most
    /// [`Self::MAX_BUFFERED_EVENTS_LIMIT`] ([`EngineConfig::validate`] rejects more, so the bound
    /// of the per-session buffer is real).
    pub max_buffered_events: usize,
}

impl SpeculationConfig {
    /// The largest accepted [`Self::max_buffered_events`]: 4096 events (the buffer holds at most
    /// one response; a bigger value only defeats the bound, and a value past tokio's channel
    /// limit would panic).
    pub const MAX_BUFFERED_EVENTS_LIMIT: usize = 4096;

    /// Speculation off (the default).
    pub const OFF: Self = Self {
        enabled: false,
        on_partial: true,
        partial_stable_for: Duration::ZERO,
        max_per_turn: 3,
        max_age: Duration::from_secs(5),
        max_buffered_events: 256,
    };

    /// Speculation on with the other defaults of [`Self::OFF`].
    pub const ENABLED: Self = Self {
        enabled: true,
        ..Self::OFF
    };
}

impl Default for SpeculationConfig {
    /// [`SpeculationConfig::OFF`].
    fn default() -> Self {
        Self::OFF
    }
}

/// Speculative Smart Turn analysis (`docs/LATENCY.md` "Early analysis"): start the
/// end-of-turn analysis when the VAD probability first falls, before `min_silence` confirms the
/// stop, so the verdict is ready (or nearly) when the stop arrives. **Off by default**: every
/// default pipeline behaves exactly as without this feature, and no `UserSpeechFalling` frame is
/// ever sent.
///
/// The VAD worker sends a `SystemFrame::UserSpeechFalling` hint; the
/// [`TurnDriver`](crate::TurnDriver) analyses the audio it holds at that moment, and at the
/// confirmed stop REUSES that verdict when the stop is the one of the same fall and little
/// audio arrived since (the reuse rule, including how "silence only" is decided by the VAD, is
/// in the `turn_driver` module docs), else runs the normal analysis. A fall that does not
/// become a stop (the user was only breathing) costs one wasted analysis: see `early_wasted` in
/// the driver's statistics.
///
/// **The switch has two halves.** This configuration turns on the driver half only. The VAD
/// half is [`VadSetup::with_early_analysis`](crate::VadSetup::with_early_analysis) (or
/// `with_fall_hint`), which the assembly must apply when it builds the `VadSetup`: with the flag
/// set and a plain `VadSetup` no hint is ever sent and the feature is inert (`early_hints` stays
/// 0). Also, the VAD's own parameters (`VadWorkerConfig::params`) are not part of
/// `EngineConfig`, so [`EngineConfig::validate`] cannot check them; `with_early_analysis` logs a
/// warning when the delay cannot precede the stop (see [`Self::fall_delay`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EarlyAnalysisConfig {
    /// Master switch. Default `false`.
    pub enabled: bool,
    /// How long the probability must stay below the VAD's negative threshold after the last
    /// speech window before the hint is sent (zero: the first such window). A longer delay
    /// filters out the short dips inside a sentence (fewer wasted analyses) and shortens the
    /// head start. Rounded up to whole VAD windows, it must stay below the VAD's
    /// minimum silence to have any effect (with the default 100 ms minimum silence and 32 ms
    /// windows: up to 96 ms); a hint that would arrive with the stop is not sent. The VAD's `speech_pad` must also not exceed its `min_silence` (the
    /// stop's time must be known at the hint). Default 64 ms.
    pub fall_delay: Duration,
    /// The most audio that may have arrived since the snapshot for the verdict to be reused at
    /// the stop. Longer than this and the normal analysis runs (counted as `early_wasted`).
    /// Default 300 ms.
    pub max_tail: Duration,
    /// The most early analyses started per user turn (an analysis that was reused still counts).
    /// This is the bound on the extra Smart Turn work a speaker who pauses a lot can cause (each
    /// wasted analysis costs about the CPU of the one at the stop, `docs/RISKS.md`): hints beyond
    /// it are ignored (`early_capped`) and those pauses use the normal analysis at the stop.
    /// Default 4.
    pub max_per_turn: u32,
}

impl EarlyAnalysisConfig {
    /// Early analysis off (the default).
    pub const OFF: Self = Self {
        enabled: false,
        fall_delay: Duration::from_millis(64),
        max_tail: Duration::from_millis(300),
        max_per_turn: 4,
    };

    /// Early analysis on with the other defaults of [`Self::OFF`].
    pub const ENABLED: Self = Self {
        enabled: true,
        ..Self::OFF
    };
}

impl Default for EarlyAnalysisConfig {
    /// [`EarlyAnalysisConfig::OFF`].
    fn default() -> Self {
        Self::OFF
    }
}

/// Everything configurable about one engine session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineConfig {
    /// Audio rates and frame length.
    pub rates: EngineRates,
    /// Inbox shapes.
    pub queues: QueueConfigs,
    /// Explicit timeouts.
    pub timeouts: Timeouts,
    /// How the end of a user turn is decided. Default: the adaptive (Smart Turn) policy.
    pub turn: TurnPolicy,
    /// Which barge-ins are real (backchannels, the echo window, minimum speech). Default:
    /// [`InterruptionConfig::RECOMMENDED`].
    pub interruption: InterruptionConfig,
    /// How long a turn end is held while STT finals are still owed (finals gate). Default 1.5 s.
    pub final_wait: Duration,
    /// Backstop after `UserTurnEnded`: if the bot finished no audio by then, the turn driver
    /// feeds a synthetic `BotAudioEnded`. Extended on every TTS progress event. Default 20 s.
    pub response_deadline: Duration,
    /// Subtracted from the transport's released-audio position to estimate what the caller has
    /// heard (audio queued in the wire queue and the client buffer). Default 160 ms (a wire queue
    /// of 8 frames of 20 ms); use zero where the oracle is wire arrival time (loopback tests).
    pub lead_margin: Duration,
    /// How long played-text truncation waits for the final `PlaybackPosition` after a `Clear`
    /// before it uses the last known position. Default 250 ms. At most half of
    /// `timeouts.analysis_job` (the runner's bound for the whole truncation pass;
    /// [`Self::validate`] enforces it).
    pub settle_wait: Duration,
    /// The LLM runner: context window, tool rounds, retries, summary. See [`LlmConfig`].
    pub llm: LlmConfig,
    /// Speculative generation at a candidate end of turn. Default: off. See
    /// [`SpeculationConfig`].
    pub speculation: SpeculationConfig,
    /// Speculative Smart Turn analysis when the VAD probability first falls. Default: off. See
    /// [`EarlyAnalysisConfig`].
    pub early_analysis: EarlyAnalysisConfig,
}

impl EngineConfig {
    /// The design values, see the field docs of each part.
    pub const DEFAULT: Self = Self {
        rates: EngineRates::DEFAULT,
        queues: QueueConfigs::DEFAULT,
        timeouts: Timeouts::DEFAULT,
        turn: TurnPolicy::adaptive(),
        interruption: InterruptionConfig::RECOMMENDED,
        final_wait: Duration::from_millis(1_500),
        response_deadline: Duration::from_secs(20),
        lead_margin: Duration::from_millis(160),
        settle_wait: Duration::from_millis(250),
        llm: LlmConfig::DEFAULT,
        speculation: SpeculationConfig::OFF,
        early_analysis: EarlyAnalysisConfig::OFF,
    };

    /// Checks the rules a session relies on.
    ///
    /// # Errors
    /// [`Error::Config`] naming the first broken rule: a zero timeout, final wait, response
    /// deadline or settle wait, a zero frame length, a frame length that is not a whole number
    /// of samples at the model rate or a fallback call rate, a model rate other than 16 kHz, a
    /// zero queue capacity, a settle wait longer than the response deadline, or a response
    /// deadline not longer than the final wait, or an STT `finalize` timeout not longer than the
    /// final wait (the driver must give up before the STT session is restarted), or an LLM
    /// setting that cannot work (no tool round, an empty window, a zero summary timeout or
    /// finish size), or enabled speculation with a zero age, buffer or per-turn budget, or enabled early analysis
    /// with a zero tail or per-turn budget.
    pub fn validate(&self) -> Result<()> {
        if self.rates.frame.is_zero() {
            return Err(Error::Config("rates.frame must be greater than zero"));
        }
        if self.rates.model != SampleRate::Hz16000 {
            return Err(Error::Config(
                "rates.model must be 16 kHz (the VAD and Smart Turn models require it)",
            ));
        }
        let rates = &self.rates;
        for rate in [
            rates.model,
            rates.fallback_call.input,
            rates.fallback_call.output,
        ] {
            if !whole_samples(rates.frame, rate) {
                return Err(Error::Config(
                    "rates.frame must be a whole number of samples at the model rate and the fallback call rates",
                ));
            }
        }
        if self.timeouts.durations().iter().any(Duration::is_zero) {
            return Err(Error::Config("every timeout must be greater than zero"));
        }
        if self.final_wait.is_zero() || self.settle_wait.is_zero() {
            return Err(Error::Config(
                "final_wait and settle_wait must be greater than zero",
            ));
        }
        if self.response_deadline <= self.final_wait {
            return Err(Error::Config("response_deadline must exceed final_wait"));
        }
        if self.timeouts.finalize <= self.final_wait {
            return Err(Error::Config(
                "timeouts.finalize must exceed final_wait (the turn driver gives up before the STT session is restarted)",
            ));
        }
        if self.settle_wait >= self.response_deadline {
            return Err(Error::Config(
                "settle_wait must be shorter than response_deadline",
            ));
        }
        // The runner bounds the whole truncation pass by `analysis_job`; a settle wait that eats
        // it would answer every interrupted pass with `Keep` and leave unheard text in the context.
        if self.settle_wait.saturating_mul(2) > self.timeouts.analysis_job {
            return Err(Error::Config(
                "settle_wait must be at most half of timeouts.analysis_job (it runs inside that bound)",
            ));
        }
        let llm = &self.llm;
        if llm.max_tool_rounds == 0 {
            return Err(Error::Config("llm.max_tool_rounds must be at least 1"));
        }
        if llm.window.max_tokens == 0 {
            return Err(Error::Config(
                "llm.window.max_tokens must be greater than zero",
            ));
        }
        if llm.summary_timeout.is_zero() || llm.inline_finish_bytes == 0 {
            return Err(Error::Config(
                "llm.summary_timeout and llm.inline_finish_bytes must be greater than zero",
            ));
        }
        let speculation = &self.speculation;
        if speculation.enabled
            && (speculation.max_age.is_zero()
                || speculation.max_per_turn == 0
                || speculation.max_buffered_events == 0)
        {
            return Err(Error::Config(
                "speculation.max_age, max_per_turn and max_buffered_events must be greater than zero",
            ));
        }
        if speculation.enabled && speculation.partial_stable_for > speculation.max_age {
            return Err(Error::Config(
                "speculation.partial_stable_for is above speculation.max_age: a partial would never be offered in time",
            ));
        }
        if speculation.enabled
            && speculation.max_buffered_events > SpeculationConfig::MAX_BUFFERED_EVENTS_LIMIT
        {
            return Err(Error::Config(
                "speculation.max_buffered_events is above SpeculationConfig::MAX_BUFFERED_EVENTS_LIMIT (4096)",
            ));
        }
        let early = &self.early_analysis;
        if early.enabled && (early.max_tail.is_zero() || early.max_per_turn == 0) {
            return Err(Error::Config(
                "early_analysis.max_tail and max_per_turn must be greater than zero",
            ));
        }
        let empty_lane = |q: &QueueConfig| q.system_capacity == 0 || q.data_capacity == 0;
        if self.queues.all().iter().any(empty_lane) {
            return Err(Error::Config("queue capacities must be at least 1"));
        }
        Ok(())
    }
}

/// Whether `frame` at `rate` is a whole number of samples.
fn whole_samples(frame: Duration, rate: SampleRate) -> bool {
    (frame.as_nanos() * u128::from(rate.hz())).is_multiple_of(1_000_000_000)
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_match_the_design() {
        let config = EngineConfig::default();
        config.validate().unwrap();
        assert_eq!(config.rates.model, SampleRate::Hz16000);
        assert_eq!(config.rates.tts, SampleRate::Hz24000);
        assert_eq!(config.queues.input.data_capacity, 8);
        assert_eq!(
            config.queues.input.policies,
            OverflowPolicies::REALTIME_INPUT
        );
        assert_eq!(config.queues.sink.data_capacity, 4);
        assert_eq!(config.final_wait, Duration::from_millis(1_500));
        assert_eq!(config.response_deadline, Duration::from_secs(20));
        assert_eq!(config.settle_wait, Duration::from_millis(250));
        assert_eq!(config.timeouts.llm_first_token, Duration::from_secs(8));
        assert_eq!(config.timeouts.llm_idle, Duration::from_secs(15));
        assert_eq!(config.turn, TurnPolicy::adaptive());
    }

    #[test]
    fn validate_names_the_broken_rule() {
        let base = EngineConfig::default();
        // The model chain is 16 kHz only.
        for rate in [SampleRate::Hz8000, SampleRate::Hz22050, SampleRate::Hz48000] {
            let mut bad = base;
            bad.rates.model = rate;
            assert!(
                matches!(bad.validate(), Err(Error::Config(m)) if m.contains("rates.model")),
                "{rate:?}"
            );
        }

        // A frame must be whole samples at every rate it is cut at: 15 ms is 330.75 samples at
        // 22.05 kHz; 62 us is 0.992 samples at the 16 kHz model rate.
        let mut bad = base;
        bad.rates.fallback_call = CallRates::symmetric(SampleRate::Hz22050);
        bad.rates.frame = Duration::from_millis(15);
        assert!(matches!(bad.validate(), Err(Error::Config(m)) if m.contains("whole number")));
        let mut bad = base;
        bad.rates.fallback_call = CallRates {
            input: SampleRate::Hz16000,
            output: SampleRate::Hz8000,
        };
        bad.rates.frame = Duration::from_micros(62);
        assert!(matches!(bad.validate(), Err(Error::Config(m)) if m.contains("whole number")));
        // 20 ms and 10 ms are whole at every supported rate; 22.05 kHz needs a multiple of 10 ms.
        for rate in [
            SampleRate::Hz8000,
            SampleRate::Hz22050,
            SampleRate::Hz44100,
            SampleRate::Hz48000,
        ] {
            let mut ok = base;
            ok.rates.fallback_call = CallRates::symmetric(rate);
            ok.validate().unwrap();
        }

        let mut bad = base;
        bad.timeouts.connect = Duration::ZERO;
        assert!(matches!(bad.validate(), Err(Error::Config(m)) if m.contains("timeout")));

        let mut bad = base;
        bad.rates.frame = Duration::ZERO;
        assert!(matches!(bad.validate(), Err(Error::Config(m)) if m.contains("frame")));

        let mut bad = base;
        bad.final_wait = Duration::ZERO;
        assert!(bad.validate().is_err());

        let mut bad = base;
        bad.response_deadline = bad.final_wait;
        assert!(matches!(bad.validate(), Err(Error::Config(m)) if m.contains("response_deadline")));

        let mut bad = base;
        bad.settle_wait = bad.response_deadline;
        assert!(matches!(bad.validate(), Err(Error::Config(m)) if m.contains("settle_wait")));

        // The settle wait runs inside the runner's analysis_job bound and must leave room in it.
        let mut bad = base;
        bad.settle_wait = bad.timeouts.analysis_job;
        assert!(matches!(bad.validate(), Err(Error::Config(m)) if m.contains("analysis_job")));
        let mut ok = base;
        ok.settle_wait = ok.timeouts.analysis_job / 2;
        ok.validate().unwrap();

        let mut bad = base;
        bad.queues.bot.data_capacity = 0;
        assert!(matches!(bad.validate(), Err(Error::Config(m)) if m.contains("queue")));

        // A zero lead margin is legitimate (loopback tests).
        let mut ok = base;
        ok.lead_margin = Duration::ZERO;
        ok.validate().unwrap();
    }

    #[test]
    fn llm_settings_are_validated_and_default_to_the_design() {
        let base = EngineConfig::default();
        assert_eq!(base.llm, LlmConfig::DEFAULT);
        assert_eq!(base.llm.window, WindowPolicy::default());
        assert_eq!((base.llm.max_tool_rounds, base.llm.retries), (6, 1));
        let mut bad = base;
        bad.llm.max_tool_rounds = 0;
        assert!(matches!(bad.validate(), Err(Error::Config(m)) if m.contains("max_tool_rounds")));
        let mut bad = base;
        bad.llm.window = WindowPolicy::new(0, 8);
        assert!(matches!(bad.validate(), Err(Error::Config(m)) if m.contains("window")));
        let mut bad = base;
        bad.llm.summary_timeout = Duration::ZERO;
        assert!(matches!(bad.validate(), Err(Error::Config(m)) if m.contains("summary_timeout")));
        let mut ok = base;
        ok.llm.retries = 0;
        ok.validate().unwrap();
    }

    #[test]
    fn early_analysis_is_off_by_default_and_validated_when_on() {
        let base = EngineConfig::default();
        assert_eq!(base.early_analysis, EarlyAnalysisConfig::OFF);
        assert!(!base.early_analysis.enabled);
        let mut on = base;
        on.early_analysis = EarlyAnalysisConfig::ENABLED;
        on.validate().unwrap();
        assert_eq!(on.early_analysis.fall_delay, Duration::from_millis(64));
        for mutate in [
            (|c: &mut EarlyAnalysisConfig| c.max_tail = Duration::ZERO)
                as fn(&mut EarlyAnalysisConfig),
            |c| c.max_per_turn = 0,
        ] {
            let mut bad = on;
            mutate(&mut bad.early_analysis);
            assert!(
                matches!(bad.validate(), Err(Error::Config(m)) if m.contains("early_analysis")),
                "{:?}",
                bad.early_analysis
            );
            // Off, the same values are not checked.
            bad.early_analysis.enabled = false;
            bad.validate().unwrap();
        }
    }

    #[test]
    fn speculation_is_off_by_default_and_validated_when_on() {
        let base = EngineConfig::default();
        assert_eq!(base.speculation, SpeculationConfig::OFF);
        assert!(!base.speculation.enabled);
        let mut on = base;
        on.speculation = SpeculationConfig::ENABLED;
        assert!(on.speculation.enabled);
        on.validate().unwrap();
        let mut at_limit = on;
        at_limit.speculation.max_buffered_events = SpeculationConfig::MAX_BUFFERED_EVENTS_LIMIT;
        at_limit.validate().unwrap();
        // The gate is zero by default and may equal the age.
        assert!(base.speculation.partial_stable_for.is_zero());
        let mut gate_at_age = on;
        gate_at_age.speculation.partial_stable_for = on.speculation.max_age;
        gate_at_age.validate().unwrap();
        for mutate in [
            (|c: &mut SpeculationConfig| c.max_age = Duration::ZERO) as fn(&mut SpeculationConfig),
            |c| c.max_per_turn = 0,
            |c| c.partial_stable_for = c.max_age + Duration::from_millis(1),
            |c| c.max_buffered_events = 0,
            |c| c.max_buffered_events = SpeculationConfig::MAX_BUFFERED_EVENTS_LIMIT + 1,
            |c| c.max_buffered_events = usize::MAX,
        ] {
            let mut bad = on;
            mutate(&mut bad.speculation);
            assert!(
                matches!(bad.validate(), Err(Error::Config(m)) if m.contains("speculation")),
                "{:?}",
                bad.speculation
            );
            // The same values are harmless while the feature is off.
            bad.speculation.enabled = false;
            bad.validate().unwrap();
        }
    }

    #[test]
    fn the_interruption_config_describes_the_agent_policies() {
        assert_eq!(
            InterruptionConfig::RECOMMENDED.policy(),
            InterruptionPolicy::recommended()
        );
        assert_eq!(
            InterruptionConfig::OFF.policy(),
            InterruptionPolicy::disabled()
        );
        assert!(InterruptionConfig::OFF.policy().is_disabled());
        assert_eq!(
            InterruptionConfig::default(),
            InterruptionConfig::RECOMMENDED
        );
        assert_eq!(
            EngineConfig::default().interruption,
            InterruptionConfig::RECOMMENDED
        );
        // One language is enough to switch the backchannel guard on.
        let english_only = InterruptionConfig {
            turkish: false,
            ..InterruptionConfig::RECOMMENDED
        };
        assert_eq!(english_only.policy().backchannels.len(), 1);
    }

    #[test]
    fn the_stt_finalize_timeout_outlasts_the_finals_gate() {
        let config = EngineConfig::default();
        assert_eq!(config.timeouts.finalize, Duration::from_secs(3));
        assert!(config.timeouts.finalize > config.final_wait);
        let mut bad = config;
        bad.timeouts.finalize = Duration::ZERO;
        assert!(matches!(bad.validate(), Err(Error::Config(m)) if m.contains("timeout")));
        // Equal or shorter than the gate: the session would be restarted while the driver still
        // holds the turn.
        for finalize in [config.final_wait, config.final_wait / 2] {
            let mut bad = config;
            bad.timeouts.finalize = finalize;
            assert!(
                matches!(bad.validate(), Err(Error::Config(m)) if m.contains("timeouts.finalize")),
                "finalize {finalize:?}"
            );
        }
        let mut ok = config;
        ok.timeouts.finalize = config.final_wait + Duration::from_millis(1);
        ok.validate().unwrap();
    }

    #[test]
    fn rates_reported_by_the_transport_are_checked_against_the_frame_length() {
        let rates = EngineRates::DEFAULT;
        for rate in [
            SampleRate::Hz8000,
            SampleRate::Hz16000,
            SampleRate::Hz22050,
            SampleRate::Hz44100,
            SampleRate::Hz48000,
        ] {
            rates.check_call_rates(CallRates::symmetric(rate)).unwrap();
        }
        // 15 ms is 330.75 samples at 22.05 kHz.
        let odd = EngineRates {
            frame: Duration::from_millis(15),
            ..rates
        };
        assert!(matches!(
            odd.check_call_rates(CallRates::symmetric(SampleRate::Hz22050)),
            Err(Error::Config(m)) if m.contains("call's rates")
        ));
        odd.check_call_rates(CallRates::symmetric(SampleRate::Hz16000))
            .unwrap();
        // Either direction can break the rule.
        assert!(
            odd.check_call_rates(CallRates {
                input: SampleRate::Hz16000,
                output: SampleRate::Hz44100
            })
            .is_err()
        );
    }
}
