//! The agent definition file format (docs/CONFIG.md is the user-facing reference).
//!
//! Every table rejects keys it does not know (`deny_unknown_fields`), so a typo is an error, not
//! a silently ignored setting. Optional settings are `Option`s: an absent key keeps the engine's
//! or the provider adapter's documented default (defaults live in one place, next to the code
//! that uses them). Values that take part in a cross-field check are [`Spanned`], so that check
//! can report the line and column of the offending value too.

use serde::Deserialize;
use toml::Spanned;

use crate::privacy::PiiKind;

use super::types::{
    Count, EnvVarName, HttpUrl, Ident, LanguageTag, LocalHttpUrl, LocalWsUrl, Millis, Rate, Real,
    Token, WsUrl,
};

/// One agent definition file.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AgentFile {
    /// Who the agent is: name, language, system prompt, tools.
    pub agent: AgentSection,
    /// How the end of a user turn is decided.
    #[serde(default)]
    pub turn: TurnSection,
    /// Which barge-ins interrupt the bot.
    #[serde(default)]
    pub interruption: InterruptionSection,
    /// Speculative LLM generation at a candidate end of turn.
    #[serde(default)]
    pub speculation: SpeculationSection,
    /// Speculative Smart Turn analysis when the VAD probability first falls.
    #[serde(default)]
    pub early_analysis: EarlyAnalysisSection,
    /// Frame length and the call rate assumed when the transport reports none.
    #[serde(default)]
    pub audio: AudioSection,
    /// The phrase spoken while a slow tool runs.
    #[serde(default)]
    pub filler: FillerSection,
    /// What the built-in telephony tools may do.
    #[serde(default)]
    pub telephony: TelephonySection,
    /// What call recording keeps and for how long; overrides the environment.
    #[serde(default)]
    pub recording: RecordingSection,
    /// The out-of-process agent the calls of this definition link to.
    #[serde(default)]
    pub agent_link: AgentLinkSection,
    /// Speech-to-text provider.
    pub stt: SttSection,
    /// LLM provider and the runner settings.
    pub llm: LlmSection,
    /// Text-to-speech provider.
    pub tts: TtsSection,
}

/// `[agent]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AgentSection {
    /// Agent name (1-64 letters, digits, `_`, `-`), used in logs and metrics.
    pub name: Ident,
    /// Conversation language: `en` or `tr` (optionally with a region, `tr-TR`). Selects the
    /// segmenter's abbreviations and the speech normalizer, and is the default STT and TTS
    /// language.
    #[cfg_attr(feature = "schema", schemars(with = "LanguageTag"))]
    pub language: Spanned<LanguageTag>,
    /// The system prompt (non-empty, at most 100 000 bytes).
    #[cfg_attr(
        feature = "schema",
        schemars(with = "String", length(min = 1, max = 100000))
    )]
    pub system_prompt: Spanned<String>,
    /// Tools the agent may call, by name (1-64 letters, digits, `_`, `-`; checked after reading
    /// so a bad entry is reported at its own position). The deployment registers their
    /// implementations; `AgentConfig::check_tools` checks
    /// they exist.
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(with = "Vec<Ident>"))]
    pub tools: Vec<Spanned<String>>,
    /// Tools of `tools` that are held for the user's confirmation: when the model calls one, it
    /// does not run; the model is told to ask the user, and the tool runs only after the user's
    /// next reply confirms (`docs/TOOLS.md`, "Confirmation gate"). Each must be a SideEffect
    /// tool and be listed in `tools`. Use it for tools whose misuse costs something: a transfer,
    /// a payment, a booking.
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(with = "Vec<Ident>"))]
    pub confirm_tools: Vec<Spanned<String>>,
}

/// `turn.policy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum TurnPolicyKind {
    /// Smart Turn decides; a fallback silence ends an undecided pause.
    Adaptive,
    /// A fixed silence after the VAD stop ends the turn.
    FixedSilence,
}

/// `[turn]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TurnSection {
    /// Default `adaptive`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<TurnPolicyKind>"))]
    pub policy: Option<Spanned<TurnPolicyKind>>,
    /// `fixed_silence` only (required there): silence that ends the turn.
    #[cfg_attr(feature = "schema", schemars(with = "Option<Millis<1, 10_000>>"))]
    pub silence_ms: Option<Spanned<Millis<1, 10_000>>>,
    /// `adaptive` only: silence that ends a turn Smart Turn did not complete. Default 3000.
    #[cfg_attr(feature = "schema", schemars(with = "Option<Millis<100, 30_000>>"))]
    pub fallback_silence_ms: Option<Spanned<Millis<100, 30_000>>>,
    /// `adaptive` only: a complete verdict also waits for the STT final. Default `false`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<bool>"))]
    pub require_final: Option<Spanned<bool>>,
    /// `adaptive` only: how long the STT signals may take. Default 800.
    #[cfg_attr(feature = "schema", schemars(with = "Option<Millis<0, 10_000>>"))]
    pub final_grace_ms: Option<Spanned<Millis<0, 10_000>>>,
    /// How long a turn end is held while STT finals are owed. Default 1500.
    #[cfg_attr(feature = "schema", schemars(with = "Option<Millis<1, 30_000>>"))]
    pub final_wait_ms: Option<Spanned<Millis<1, 30_000>>>,
    /// Backstop for a response that produces no audio. Default 20000; must exceed
    /// `final_wait_ms`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<Millis<1_000, 300_000>>"))]
    pub response_deadline_ms: Option<Spanned<Millis<1_000, 300_000>>>,
}

/// `interruption.preset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum InterruptionPreset {
    /// The recommended guards (300 ms speech, echo window, backchannels).
    Recommended,
    /// Every guard off: any barge-in interrupts at once.
    Off,
}

/// `interruption.confirm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ConfirmKind {
    /// Both `min_speech_ms` and `min_words` must hold.
    All,
    /// Either is enough.
    Any,
}

/// `[interruption]`: the preset, then per-field overrides.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct InterruptionSection {
    /// Default `recommended`.
    pub preset: Option<InterruptionPreset>,
    /// VAD must stay active this long before a barge-in interrupts (0: off).
    pub min_speech_ms: Option<Millis<0, 5_000>>,
    /// Interim words needed before a barge-in interrupts (0: off).
    pub min_words: Option<Count<0, 20>>,
    /// How the two guards combine.
    pub confirm: Option<ConfirmKind>,
    /// Echo window at the start of the bot's audio (0: off).
    pub echo_guard_ms: Option<Millis<0, 5_000>>,
    /// Words STT must confirm inside the echo window.
    pub echo_min_words: Option<Count<1, 20>>,
    /// Backchannel phrase sets: any of `"en"`, `"tr"`; `[]` turns them off.
    #[cfg_attr(
        feature = "schema",
        schemars(with = "Option<Vec<super::json_schema::BackchannelSet>>")
    )]
    pub backchannels: Option<Vec<Spanned<String>>>,
    /// Wait for a first interim before a candidate is assumed not to be a backchannel.
    pub backchannel_wait_ms: Option<Millis<0, 5_000>>,
    /// Speech after which a backchannel verdict stops blocking (0: no cap).
    pub backchannel_max_speech_ms: Option<Millis<0, 10_000>>,
    /// The VAD's minimum silence before it reports the stop.
    pub vad_hangover_ms: Option<Millis<0, 2_000>>,
}

/// `[speculation]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SpeculationSection {
    /// Master switch. Default `false`.
    pub enabled: Option<bool>,
    /// Also speculate on the best partial transcript at the VAD stop. Default `true`.
    pub on_partial: Option<bool>,
    /// Stable-partial gate: offer the partial of a VAD stop only after the caller stayed silent
    /// and the text did not change for this long. Default 0 (at once).
    pub partial_stable_ms: Option<Millis<0, 60_000>>,
    /// Candidates per user turn. Default 3.
    pub max_per_turn: Option<Count<1, 16>>,
    /// Age after which an unconfirmed speculation is dropped. Default 5000.
    pub max_age_ms: Option<Millis<100, 60_000>>,
    /// Events a speculative response may buffer. Default 256.
    pub max_buffered_events: Option<Count<1, 4_096>>,
}

/// `[early_analysis]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct EarlyAnalysisSection {
    /// Master switch. Default `false`; needs `turn.policy = "adaptive"`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<bool>"))]
    pub enabled: Option<Spanned<bool>>,
    /// Time below the VAD threshold before the hint. Default 64.
    pub fall_delay_ms: Option<Millis<0, 1_000>>,
    /// Most audio since the snapshot for the verdict to be reused. Default 300.
    pub max_tail_ms: Option<Millis<1, 5_000>>,
    /// Early analyses per user turn. Default 4.
    pub max_per_turn: Option<Count<1, 32>>,
}

/// `[audio]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AudioSection {
    /// Length of one audio frame. Default 20; must be a whole number of samples at 16 kHz and
    /// at `call_sample_rate`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<Millis<5, 100>>"))]
    pub frame_ms: Option<Spanned<Millis<5, 100>>>,
    /// Call rate (both directions) assumed when the transport reports none (telephony
    /// transports report 8000 themselves). Default 16000.
    #[cfg_attr(feature = "schema", schemars(with = "Option<Rate>"))]
    pub call_sample_rate: Option<Spanned<Rate>>,
}

/// `[filler]`: when a slow tool round is announced with a filler phrase. The phrase
/// itself (and its pre-rendered clip) is set in code (`SessionBuilder::with_filler_policy`,
/// `with_filler_clips`); the default is "One moment, please.".
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct FillerSection {
    /// Master switch. Default `true`.
    pub enabled: Option<bool>,
    /// Time without a tool result before the filler, the same for every tool that has a filler.
    /// Default 1200.
    #[cfg_attr(feature = "schema", schemars(with = "Option<Millis<100, 10_000>>"))]
    pub delay_ms: Option<Spanned<Millis<100, 10_000>>>,
    /// `true`: each tool's own delay (its `filler_after`, 700 ms unless the tool sets another)
    /// instead of `delay_ms`. Default `false`; cannot be combined with `delay_ms`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<bool>"))]
    pub per_tool: Option<Spanned<bool>>,
}

/// `[recording]`: what call recording keeps (docs/PRIVACY.md). Recording itself is
/// switched on by the environment (`BRISVOX_RECORDING_DIR`, the directory is not an agent
/// setting); a key set here wins over the matching environment variable. A `[recording]` table
/// without the directory is an error at startup, not a silently ignored privacy setting.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RecordingSection {
    /// Write each call's transcript. Default `true`.
    pub transcript: Option<bool>,
    /// Write each call's audio (two WAV files; audio cannot be redacted). Default `false`.
    pub audio: Option<bool>,
    /// Replace identifiers in the transcript before it is written. Default `true`.
    pub redact: Option<bool>,
    /// Which kinds the redaction replaces: any of `email`, `iban`, `id`, `card`, `phone`,
    /// `number`. Default all. Not empty while `redact` is on.
    #[cfg_attr(feature = "schema", schemars(with = "Option<Vec<PiiKind>>"))]
    pub redact_kinds: Option<Spanned<Vec<PiiKind>>>,
    /// Delete recordings older than this many days (1 to 3650). Default 30 (the environment's
    /// `BRISVOX_RECORDING_RETENTION` can say `forever`; the file cannot).
    #[cfg_attr(
        feature = "schema",
        schemars(with = "Option<u32>", range(min = 1, max = 3650))
    )]
    pub retention_days: Option<Spanned<u32>>,
}

/// `[telephony]`: what the built-in telephony tools (`transfer_call`, `send_dtmf`, `end_call`)
/// may do on this agent's calls (docs/TELEPHONY.md). The tools exist only in a server
/// with a carrier's call control configured (`BRISVOX_CALL_CONTROL`, docs/OPERATIONS.md); an
/// agent lists the ones it uses in `agent.tools` like any other tool.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TelephonySection {
    /// Where `transfer_call` may send the call, by name: the model picks a name, never a
    /// number. Without any, `transfer_call` is not offered.
    #[serde(default)]
    pub destinations: Vec<DestinationEntry>,
    /// Offer `send_dtmf` when the carrier binding can send tones. Default `true`.
    pub dtmf: Option<bool>,
    /// Offer warm transfers (`transfer_call` with `mode = "warm"`: the receiving party hears a
    /// summary first). Needs the callback listener (`BRISVOX_CALLBACK_ADDR`), which finishes a
    /// transfer, and a carrier binding that can do it; otherwise the tool offers cold transfers
    /// only. Default `false`.
    pub warm: Option<bool>,
    /// What to do when an outbound call is answered by a machine.
    pub voicemail: Option<VoicemailSection>,
}

/// `[agent_link]`: where the out-of-process agent of this definition's calls listens
/// (`docs/AGENT_PROTOCOL.md`). Without it the server-wide `BRISVOX_AGENT_URL` applies.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AgentLinkSection {
    /// The agent process's `ws://` or `wss://` URL; wins over `BRISVOX_AGENT_URL`.
    #[cfg_attr(
        feature = "schema",
        schemars(
            with = "Option<String>",
            regex(
                pattern = "^wss?://[^\\s\\u0000-\\u001f/?#@:][^\\s\\u0000-\\u001f/?#@]*(?:[/?#][^\\s\\u0000-\\u001f]*)?$"
            )
        )
    )]
    pub url: Option<Spanned<String>>,
    /// Environment variable holding the bearer token for `url`. Default: the value of
    /// `BRISVOX_AGENT_TOKEN`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<EnvVarName>"))]
    pub token_env: Option<Spanned<EnvVarName>>,
    /// Accept a plaintext `ws://` URL to a host outside the private networks (loopback,
    /// RFC 1918, link-local, `*.internal`, ...). Default `false`: such a URL is refused at
    /// startup, because the link carries transcripts and tool calls. Also
    /// `BRISVOX_AGENT_ALLOW_INSECURE=1`.
    pub allow_insecure: Option<bool>,
    /// Give this definition's agent process the caller's number, the called number and the
    /// carrier's call id in `session.start.call` (telephony calls only; `docs/AGENT_PROTOCOL.md`,
    /// "Caller identity"). Default: `BRISVOX_AGENT_CALLER_IDENTITY`, else `false`. The numbers
    /// are withheld while the server redacts phone numbers (`docs/PRIVACY.md`).
    pub caller_identity: Option<bool>,
    /// Other agent processes a call may pick by name in its start message (the websocket
    /// start message's metadata key `agent_route`, the TwiML parameter `agent_route`).
    #[serde(default)]
    pub routes: Vec<AgentRouteEntry>,
}

/// One `[[agent_link.routes]]` entry.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AgentRouteEntry {
    /// The name a start message uses (1-32 letters, digits, `_`, `-`).
    #[cfg_attr(
        feature = "schema",
        schemars(with = "String", regex(pattern = "^[A-Za-z0-9_\\-]{1,32}$"))
    )]
    pub name: Spanned<String>,
    /// The agent process's `ws://` or `wss://` URL.
    #[cfg_attr(
        feature = "schema",
        schemars(
            with = "String",
            regex(
                pattern = "^wss?://[^\\s\\u0000-\\u001f/?#@:][^\\s\\u0000-\\u001f/?#@]*(?:[/?#][^\\s\\u0000-\\u001f]*)?$"
            )
        )
    )]
    pub url: Spanned<String>,
    /// Environment variable holding this route's bearer token. A route never gets the default
    /// token: it may be another team's agent.
    #[cfg_attr(feature = "schema", schemars(with = "Option<EnvVarName>"))]
    pub token_env: Option<Spanned<EnvVarName>>,
    /// As `[agent_link] allow_insecure`, for this route only.
    pub allow_insecure: Option<bool>,
    /// As `[agent_link] caller_identity`, for this route only. Never inherited from the
    /// section or the environment: a route may be another team's agent, which gets the
    /// caller's number only when its own entry says so. Default `false`.
    pub caller_identity: Option<bool>,
}

/// `[telephony.voicemail]`: answering-machine handling of calls the carrier's detector
/// classifies (`docs/TELEPHONY.md`, section 6). The verdict reaches the call as the stream
/// parameter `answered_by` or as a callback (`BRISVOX_CALLBACK_ADDR`). A machine whose greeting
/// ended is played `message`, then the call is hung up; a person gets the normal conversation.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct VoicemailSection {
    /// The fixed message left on a machine, spoken as it is (1 to 1000 characters). Put nothing
    /// in it the callee did not consent to hear on a recording.
    #[cfg_attr(
        feature = "schema",
        schemars(with = "String", length(min = 1, max = 1000))
    )]
    pub message: Spanned<String>,
    /// What to do when the detector cannot tell. Default `treat_as_human`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<VoicemailUnknown>"))]
    pub on_unknown: Option<Spanned<VoicemailUnknown>>,
    /// `true` for a detector that never reports the end of a greeting (Twilio
    /// `MachineDetection=Enable`, Telnyx `detect`): a machine is hung up on instead of being
    /// spoken over. Default `false` (Twilio `DetectMessageEnd`, Telnyx `greeting_end`).
    pub detect_only: Option<bool>,
}

/// `telephony.voicemail.on_unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum VoicemailUnknown {
    /// Carry on with the conversation.
    TreatAsHuman,
    /// Wait for the greeting to end (a fixed time), then leave the message.
    TreatAsMachine,
    /// End the call.
    HangUp,
}

/// One `[[telephony.destinations]]` entry.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DestinationEntry {
    /// The name the model uses (1-32 letters, digits, `_`, `-`).
    #[cfg_attr(
        feature = "schema",
        schemars(with = "String", regex(pattern = "^[A-Za-z0-9_\\-]{1,32}$"))
    )]
    pub name: Spanned<String>,
    /// The E.164 number (`+` and 8 to 15 digits).
    #[cfg_attr(
        feature = "schema",
        schemars(with = "String", regex(pattern = "^\\+[0-9]{8,15}$"))
    )]
    pub number: Spanned<String>,
    /// When to use it, shown to the model (cut to 120 characters).
    #[serde(default)]
    pub description: String,
}

/// `stt.provider`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SttProviderKind {
    /// Deepgram streaming `/v1/listen`.
    Deepgram,
    /// AssemblyAI Universal Streaming.
    Assemblyai,
    /// Gladia live.
    Gladia,
    /// A self-hosted sherpa-onnx streaming WebSocket server.
    SherpaOnnx,
    /// A server speaking the OpenAI Realtime transcription protocol: OpenAI, NVIDIA Speech NIM,
    /// Speaches, vLLM.
    OpenaiRealtimeStt,
}

/// `[stt]`: the provider, its key and language, and one options table named after it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SttSection {
    /// Which adapter.
    #[cfg_attr(feature = "schema", schemars(with = "SttProviderKind"))]
    pub provider: Spanned<SttProviderKind>,
    /// Environment variable holding the API key. Required except for `sherpa_onnx` (no key) and
    /// the local dialects of `openai_realtime_stt`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<EnvVarName>"))]
    pub api_key_env: Option<Spanned<EnvVarName>>,
    /// Recognition language. Default: `agent.language`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<LanguageTag>"))]
    pub language: Option<Spanned<LanguageTag>>,
    /// `[stt.deepgram]` options (only with `provider = "deepgram"`).
    #[cfg_attr(feature = "schema", schemars(with = "Option<DeepgramOptions>"))]
    pub deepgram: Option<Spanned<DeepgramOptions>>,
    /// `[stt.assemblyai]` options.
    #[cfg_attr(feature = "schema", schemars(with = "Option<AssemblyAiOptions>"))]
    pub assemblyai: Option<Spanned<AssemblyAiOptions>>,
    /// `[stt.gladia]` options.
    #[cfg_attr(feature = "schema", schemars(with = "Option<GladiaOptions>"))]
    pub gladia: Option<Spanned<GladiaOptions>>,
    /// `[stt.sherpa_onnx]` options.
    #[cfg_attr(feature = "schema", schemars(with = "Option<SherpaOnnxOptions>"))]
    pub sherpa_onnx: Option<Spanned<SherpaOnnxOptions>>,
    /// `[stt.openai_realtime_stt]` options.
    #[cfg_attr(
        feature = "schema",
        schemars(with = "Option<OpenAiRealtimeSttOptions>")
    )]
    pub openai_realtime_stt: Option<Spanned<OpenAiRealtimeSttOptions>>,
    /// `[stt.secondary]`: a second STT provider calls fail over to when the primary
    /// is down. Takes the keys above except `language` (the call's language is the primary's)
    /// and `secondary`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<SttSection>"))]
    pub secondary: Option<Spanned<Box<SttSection>>>,
}

/// `stt.deepgram.region`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DeepgramRegion {
    /// `api.deepgram.com`.
    Global,
    /// `api.eu.deepgram.com`.
    Eu,
    /// `api.au.deepgram.com`.
    Au,
    /// `api.in.deepgram.com`.
    India,
}

/// `[stt.deepgram]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DeepgramOptions {
    /// Model. Default `nova-3`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<Token>"))]
    pub model: Option<Spanned<Token>>,
    /// Provider endpointing silence; 0 turns it off. Default 300.
    pub endpointing_ms: Option<Millis<0, 10_000>>,
    /// `utterance_end_ms` (1000..=5000).
    pub utterance_end_ms: Option<Millis<1_000, 5_000>>,
    /// `smart_format`. Default `true`.
    pub smart_format: Option<bool>,
    /// Keyterm prompting (Nova-3), at most 100 terms.
    #[cfg_attr(
        feature = "schema",
        schemars(with = "Option<Vec<String>>", length(max = 100))
    )]
    pub keyterms: Option<Spanned<Vec<String>>>,
    /// Hosting region. Default `global`.
    pub region: Option<DeepgramRegion>,
    /// Replaces `wss://<region host>`.
    pub base_url: Option<WsUrl>,
}

/// `stt.assemblyai.region`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AssemblyAiRegion {
    /// The nearest edge.
    Edge,
    /// United States.
    Us,
    /// European Union.
    Eu,
}

/// `[stt.assemblyai]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AssemblyAiOptions {
    /// `speech_model`. Default `universal-3-6-pro`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<Token>"))]
    pub speech_model: Option<Spanned<Token>>,
    /// Hosting region. Default `edge`.
    pub region: Option<AssemblyAiRegion>,
    /// Keyterm prompting, at most 100 terms.
    #[cfg_attr(
        feature = "schema",
        schemars(with = "Option<Vec<String>>", length(max = 100))
    )]
    pub keyterms: Option<Spanned<Vec<String>>>,
    /// `format_turns`.
    pub format_turns: Option<bool>,
    /// `min_turn_silence` in ms.
    #[cfg_attr(feature = "schema", schemars(with = "Option<Millis<0, 10_000>>"))]
    pub min_turn_silence_ms: Option<Spanned<Millis<0, 10_000>>>,
    /// `max_turn_silence` in ms; at least `min_turn_silence_ms`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<Millis<0, 30_000>>"))]
    pub max_turn_silence_ms: Option<Spanned<Millis<0, 30_000>>>,
    /// Replaces the region's endpoint.
    pub base_url: Option<WsUrl>,
}

/// `stt.gladia.region`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GladiaRegion {
    /// `us-west`.
    UsWest,
    /// `eu-west`.
    EuWest,
}

/// `[stt.gladia]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct GladiaOptions {
    /// Model. Default `solaria-1`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<Token>"))]
    pub model: Option<Spanned<Token>>,
    /// Region. Default: Gladia's choice.
    pub region: Option<GladiaRegion>,
    /// Let Gladia switch languages inside a session.
    pub code_switching: Option<bool>,
    /// Custom vocabulary, at most 100 entries.
    #[cfg_attr(
        feature = "schema",
        schemars(with = "Option<Vec<String>>", length(max = 100))
    )]
    pub vocabulary: Option<Spanned<Vec<String>>>,
    /// Endpointing silence (10..=10000 ms).
    pub endpointing_ms: Option<Millis<10, 10_000>>,
    /// Replaces the API base URL.
    pub base_url: Option<HttpUrl>,
}

/// `[stt.sherpa_onnx]`: a self-hosted sherpa-onnx streaming WebSocket server
/// (`docs/providers/sherpa-onnx.md`). The model decides the language; `stt.language` is not sent.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SherpaOnnxOptions {
    /// Server address, `ws://host[:port]`. Default `ws://127.0.0.1:6006`.
    pub base_url: Option<LocalWsUrl>,
    /// Path of the WebSocket endpoint. Default `/`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<Token>"))]
    pub path: Option<Spanned<Token>>,
    /// Rate of the audio sent: the model's training rate (the Python server does not resample).
    /// Default 16000.
    pub sample_rate: Option<Rate>,
    /// How long to wait for the end of a `Done` exchange (the finalize answer) before answering
    /// with the latest hypothesis (100..=10000 ms). Default 1500.
    pub finalize_timeout_ms: Option<Millis<100, 10_000>>,
    /// Silence sent before `Done` so the decoder flushes the last word (0..=5000 ms, default
    /// 300; the Python server pads only 0.3 s itself, the C++ server 0.8 s: set 0 there).
    pub finalize_padding_ms: Option<Millis<0, 5_000>>,
}

/// `stt.openai_realtime_stt.dialect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SttDialect {
    /// OpenAI's Realtime API in transcription mode (needs `api_key_env`).
    Openai,
    /// NVIDIA Speech NIM `/v1/realtime?intent=transcription`.
    Nim,
    /// Speaches `/v1/realtime?intent=transcription`.
    Speaches,
    /// vLLM `/v1/realtime`.
    Vllm,
}

/// `[stt.openai_realtime_stt]`: a server speaking the OpenAI Realtime transcription protocol
/// (`docs/providers/openai-realtime-stt.md`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct OpenAiRealtimeSttOptions {
    /// The server's dialect. Default `openai`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<SttDialect>"))]
    pub dialect: Option<Spanned<SttDialect>>,
    /// Server address, `ws[s]://host[:port]`. Default: the dialect's (OpenAI's cloud,
    /// `ws://127.0.0.1:9000` for NIM, `ws://127.0.0.1:8000` for Speaches and vLLM).
    pub base_url: Option<LocalWsUrl>,
    /// Replaces the dialect's path and query (`/v1/realtime?...`).
    #[cfg_attr(feature = "schema", schemars(with = "Option<Token>"))]
    pub path_and_query: Option<Spanned<Token>>,
    /// The transcription model. Default: OpenAI `gpt-live-transcribe`, NIM the server's own;
    /// required for `speaches` and `vllm`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<Token>"))]
    pub model: Option<Spanned<Token>>,
    /// Transcription prompt (OpenAI, NIM), at most 4096 bytes.
    #[cfg_attr(
        feature = "schema",
        schemars(with = "Option<String>", length(max = 4096))
    )]
    pub prompt: Option<Spanned<String>>,
    /// NIM: automatic punctuation. Default `true`.
    pub punctuation: Option<bool>,
    /// A delta replaces the hypothesis so far instead of extending it. Default: extend.
    pub delta_replaces: Option<bool>,
    /// The least audio a commit may hold (0..=2000 ms); less is answered empty without a
    /// round trip. Default: the dialect's (100 ms for OpenAI and Speaches).
    pub min_commit_ms: Option<Millis<0, 2_000>>,
    /// How long to wait for the answer to a commit (100..=10000 ms). Default 2000.
    pub finalize_timeout_ms: Option<Millis<100, 10_000>>,
    /// Rate of the audio sent. Default: the dialect's (24000 OpenAI and Speaches, 16000 NIM and
    /// vLLM).
    pub sample_rate: Option<Rate>,
}

/// `llm.provider`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LlmProviderKind {
    /// OpenAI Responses API.
    Openai,
    /// Anthropic Messages API.
    Anthropic,
    /// Google Gemini.
    Gemini,
    /// Any OpenAI Chat Completions compatible server (Groq, Together, Fireworks, vLLM, Ollama, OpenRouter).
    OpenaiCompatible,
}

/// `[llm]`: the provider, its key, the runner settings and one options table named after it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct LlmSection {
    /// Which adapter.
    #[cfg_attr(feature = "schema", schemars(with = "LlmProviderKind"))]
    pub provider: Spanned<LlmProviderKind>,
    /// Environment variable holding the API key. Required except for `openai_compatible`
    /// (a local vLLM or Ollama needs none).
    #[cfg_attr(feature = "schema", schemars(with = "Option<EnvVarName>"))]
    pub api_key_env: Option<Spanned<EnvVarName>>,
    /// Model calls per user turn that may request tools. Default 6.
    pub max_tool_rounds: Option<Count<1, 32>>,
    /// Token budget of the context window. Default 4000.
    pub context_window_tokens: Option<Count<256, 2_000_000>>,
    /// `[llm.openai]` options.
    #[cfg_attr(feature = "schema", schemars(with = "Option<OpenAiOptions>"))]
    pub openai: Option<Spanned<OpenAiOptions>>,
    /// `[llm.anthropic]` options.
    #[cfg_attr(feature = "schema", schemars(with = "Option<AnthropicOptions>"))]
    pub anthropic: Option<Spanned<AnthropicOptions>>,
    /// `[llm.gemini]` options.
    #[cfg_attr(feature = "schema", schemars(with = "Option<GeminiOptions>"))]
    pub gemini: Option<Spanned<GeminiOptions>>,
    /// `[llm.openai_compatible]` options.
    #[cfg_attr(feature = "schema", schemars(with = "Option<OpenAiCompatibleOptions>"))]
    pub openai_compatible: Option<Spanned<OpenAiCompatibleOptions>>,
    /// `[llm.secondary]`: a second LLM provider that takes over a request the primary
    /// failed before any content. Takes the keys above except `max_tool_rounds`,
    /// `context_window_tokens` (the call's, from the primary's table) and `secondary`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<LlmSection>"))]
    pub secondary: Option<Spanned<Box<LlmSection>>>,
}

/// `llm.openai.reasoning_effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OpenAiReasoningEffort {
    /// No reasoning.
    Disabled,
    /// Low.
    Low,
    /// Medium.
    Medium,
    /// High.
    High,
    /// Extra high.
    Xhigh,
    /// Maximum.
    Max,
}

/// `[llm.openai]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct OpenAiOptions {
    /// Model. Default: the adapter's.
    pub model: Option<Token>,
    /// Reasoning effort. Default: the model's.
    pub reasoning_effort: Option<OpenAiReasoningEffort>,
    /// Output token cap.
    pub max_output_tokens: Option<Count<1, 1_000_000>>,
    /// Let the model call several tools at once.
    pub parallel_tool_calls: Option<bool>,
    /// `service_tier`.
    pub service_tier: Option<Token>,
    /// Replaces `https://api.openai.com/v1`.
    pub base_url: Option<HttpUrl>,
}

/// `llm.anthropic.effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AnthropicEffort {
    /// Low.
    Low,
    /// Medium.
    Medium,
    /// High.
    High,
    /// Extra high.
    Xhigh,
    /// Maximum.
    Max,
}

/// `llm.anthropic.thinking`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AnthropicThinking {
    /// No thinking.
    Disabled,
    /// The model decides.
    Adaptive,
    /// Think only between tool calls.
    BetweenTools,
    /// Send nothing; the model's default applies.
    ModelDefault,
}

/// `[llm.anthropic]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AnthropicOptions {
    /// Model. Default: the adapter's.
    pub model: Option<Token>,
    /// `max_tokens`. Default 1024.
    pub max_tokens: Option<Count<1, 1_000_000>>,
    /// `effort`.
    pub effort: Option<AnthropicEffort>,
    /// Thinking mode.
    pub thinking: Option<AnthropicThinking>,
    /// Replaces `https://api.anthropic.com`.
    pub base_url: Option<HttpUrl>,
}

/// `llm.gemini.thinking_level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GeminiThinkingLevel {
    /// Minimal.
    Minimal,
    /// Low.
    Low,
    /// Medium.
    Medium,
    /// High.
    High,
}

/// `[llm.gemini]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct GeminiOptions {
    /// Model. Default: the adapter's.
    pub model: Option<Token>,
    /// Thinking level.
    pub thinking_level: Option<GeminiThinkingLevel>,
    /// Output token cap.
    pub max_output_tokens: Option<Count<1, 1_000_000>>,
    /// Sampling temperature (0..=2).
    pub temperature: Option<Real<0, 2_000>>,
    /// Replaces the Gemini API base URL.
    pub base_url: Option<HttpUrl>,
}

/// `llm.openai_compatible.preset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CompatiblePreset {
    /// Groq.
    Groq,
    /// Together AI.
    Together,
    /// Fireworks AI.
    Fireworks,
    /// A local vLLM server.
    Vllm,
    /// A local Ollama server.
    Ollama,
    /// OpenRouter (`https://openrouter.ai/api/v1`); needs `api_key_env` and a `model`.
    #[serde(rename = "openrouter")]
    OpenRouter,
}

/// `[llm.openai_compatible]`: a `preset`, or a `base_url` and a `model`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct OpenAiCompatibleOptions {
    /// Vendor preset (base URL, suggested model and quirks).
    pub preset: Option<CompatiblePreset>,
    /// Server base URL (overrides the preset's).
    pub base_url: Option<HttpUrl>,
    /// Model (overrides the preset's suggestion).
    pub model: Option<Token>,
    /// Output token cap.
    pub max_tokens: Option<Count<1, 1_000_000>>,
    /// Sampling temperature (0..=2).
    pub temperature: Option<Real<0, 2_000>>,
    /// Sent as the top-level request field `reasoning_effort` (OpenRouter, vLLM, OpenAI and other
    /// servers whose reasoning models think by default: `none` or `low` keeps the first token
    /// fast). Left out when unset; a server that does not know the field may refuse the request.
    pub reasoning_effort: Option<Token>,
    /// OpenRouter provider routing (OpenRouter-only: an error unless `preset = "openrouter"` or
    /// `base_url` is on openrouter.ai): the upstream providers to try in this order, sent as
    /// `provider.order` in the request body (slugs as on the model's page, e.g. `openai`; at
    /// most 100). With `openrouter_allow_fallbacks = false` the request is served by one of them
    /// or refused. Left out when unset.
    #[cfg_attr(
        feature = "schema",
        schemars(with = "Option<Vec<String>>", length(max = 100))
    )]
    pub openrouter_order: Option<Spanned<Vec<String>>>,
    /// OpenRouter `provider.allow_fallbacks` (OpenRouter's default is `true`; OpenRouter-only like
    /// `openrouter_order`). Sent only together with `openrouter_order`.
    pub openrouter_allow_fallbacks: Option<bool>,
}

/// `tts.provider`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum TtsProviderKind {
    /// Cartesia Sonic over WebSocket.
    Cartesia,
    /// ElevenLabs multi-context WebSocket.
    Elevenlabs,
    /// Deepgram Aura `/v1/speak`.
    DeepgramAura,
    /// A local server speaking the OpenAI `POST /v1/audio/speech` protocol (Kokoro-FastAPI,
    /// Speaches, Chatterbox-TTS-Server, vLLM-Omni).
    OpenaiSpeech,
}

/// `[tts]`: the provider, its key, voice, rate and language, and one options table named after
/// it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TtsSection {
    /// Which adapter.
    #[cfg_attr(feature = "schema", schemars(with = "TtsProviderKind"))]
    pub provider: Spanned<TtsProviderKind>,
    /// Environment variable holding the API key. Required except for `openai_speech` (a local
    /// Kokoro or Chatterbox server needs none).
    #[cfg_attr(feature = "schema", schemars(with = "Option<EnvVarName>"))]
    pub api_key_env: Option<Spanned<EnvVarName>>,
    /// Provider voice id (for Deepgram Aura the model string, e.g. `aura-2-thalia-en`; for
    /// `openai_speech` the server's voice name, e.g. `af_heart` on Kokoro).
    #[cfg_attr(feature = "schema", schemars(with = "Token"))]
    pub voice: Spanned<Token>,
    /// Rate the provider delivers (the engine resamples to the call). Default 24000.
    #[cfg_attr(feature = "schema", schemars(with = "Option<Rate>"))]
    pub sample_rate: Option<Spanned<Rate>>,
    /// Synthesis language. Default: `agent.language`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<LanguageTag>"))]
    pub language: Option<Spanned<LanguageTag>>,
    /// `[tts.cartesia]` options.
    #[cfg_attr(feature = "schema", schemars(with = "Option<CartesiaOptions>"))]
    pub cartesia: Option<Spanned<CartesiaOptions>>,
    /// `[tts.elevenlabs]` options.
    #[cfg_attr(feature = "schema", schemars(with = "Option<ElevenLabsOptions>"))]
    pub elevenlabs: Option<Spanned<ElevenLabsOptions>>,
    /// `[tts.deepgram_aura]` options.
    #[cfg_attr(feature = "schema", schemars(with = "Option<DeepgramAuraOptions>"))]
    pub deepgram_aura: Option<Spanned<DeepgramAuraOptions>>,
    /// `[tts.openai_speech]` options.
    #[cfg_attr(feature = "schema", schemars(with = "Option<OpenAiSpeechOptions>"))]
    pub openai_speech: Option<Spanned<OpenAiSpeechOptions>>,
    /// `[tts.secondary]`: a second TTS provider calls fail over to when the primary is
    /// down. Takes the keys above, `voice` included (a voice id belongs to one provider), except
    /// `sample_rate` and `language` (the call's, from the primary's table) and `secondary`.
    #[cfg_attr(feature = "schema", schemars(with = "Option<TtsSection>"))]
    pub secondary: Option<Spanned<Box<TtsSection>>>,
}

/// `[tts.cartesia]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CartesiaOptions {
    /// Model. Default: the adapter's (`sonic-3.6`).
    #[cfg_attr(feature = "schema", schemars(with = "Option<Token>"))]
    pub model: Option<Spanned<Token>>,
    /// Server-side text buffering (0..=5000 ms).
    pub max_buffer_delay_ms: Option<Millis<0, 5_000>>,
    /// Replaces the WebSocket URL.
    pub url: Option<WsUrl>,
}

/// `[tts.elevenlabs]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ElevenLabsOptions {
    /// Model. Default: the adapter's (`eleven_flash_v2_5`).
    #[cfg_attr(feature = "schema", schemars(with = "Option<Token>"))]
    pub model: Option<Spanned<Token>>,
    /// Voice stability (0..=1).
    pub stability: Option<Real<0, 1_000>>,
    /// Similarity boost (0..=1).
    pub similarity_boost: Option<Real<0, 1_000>>,
    /// Style exaggeration (0..=1).
    pub style: Option<Real<0, 1_000>>,
    /// Speaking rate (0.7..=1.2).
    pub speed: Option<Real<700, 1_200>>,
    /// Speaker boost.
    pub use_speaker_boost: Option<bool>,
    /// Replaces the WebSocket base URL.
    pub url: Option<WsUrl>,
}

/// `tts.openai_speech.preset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SpeechPreset {
    /// Any server with `POST {base_url}/audio/speech` (`model` is required).
    Generic,
    /// Kokoro-FastAPI.
    Kokoro,
    /// Chatterbox-TTS-Server (WAV responses).
    Chatterbox,
    /// Speaches.
    Speaches,
    /// vLLM-Omni serving a TTS model.
    VllmOmni,
}

/// `tts.openai_speech.format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SpeechFormat {
    /// Headerless 24 kHz PCM16 (`response_format = "pcm"`).
    Pcm,
    /// WAV (`response_format = "wav"`), for servers without `pcm`.
    Wav,
}

/// `[tts.openai_speech]`: the server's `base_url`, and optionally a `preset` and overrides.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct OpenAiSpeechOptions {
    /// Server quirks (model id, audio format, request fields). Default: `generic`.
    pub preset: Option<SpeechPreset>,
    /// The API base including its version prefix, e.g. `http://localhost:8880/v1`. Required.
    pub base_url: Option<LocalHttpUrl>,
    /// Model id (overrides the preset's; required for `generic`).
    pub model: Option<Token>,
    /// Speaking rate (0.25..=4).
    pub speed: Option<Real<250, 4_000>>,
    /// Audio format (overrides the preset's).
    pub format: Option<SpeechFormat>,
    /// Longest wait from sending a segment to its first audio byte (default 15000 ms; raise it
    /// for a CPU server, or one that answers only when the whole segment is synthesized).
    pub first_audio_timeout_ms: Option<Millis<500, 120_000>>,
    /// Text of the background model warm-up request started at call start (audio discarded; at
    /// most 200 bytes; `""` turns it off). Default: `"Hi."` for the `kokoro` preset, off for the
    /// others. Hides the cold start of a GPU server that has idled (Kokoro: 2.8 s).
    #[cfg_attr(
        feature = "schema",
        schemars(with = "Option<String>", length(max = 200))
    )]
    pub warm_text: Option<Spanned<String>>,
    /// At most one warm-up per this many ms (default 60000), however many calls start.
    pub warm_interval_ms: Option<Millis<1_000, 3_600_000>>,
}

/// `[tts.deepgram_aura]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DeepgramAuraOptions {
    /// Speaking rate (0.7..=1.5).
    pub speed: Option<Real<700, 1_500>>,
    /// Replaces the WebSocket URL.
    pub url: Option<WsUrl>,
}
