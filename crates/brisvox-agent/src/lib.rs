//! Conversation logic for brisvox: the turn-taking state machine, interruption policy, LLM
//! context management, the tool engine, TTS segmentation, speculative generation and flows.

#![forbid(unsafe_code)]

pub mod assemble;
pub mod context;
pub mod error;
pub mod flows;
pub mod interrupt;
pub mod normalize;
pub mod played;
pub mod segment;
pub mod telephony;
pub mod turn;

pub mod tools;
pub mod window;

pub use assemble::{
    AssembleError, Assembled, CallError, Delta, InvalidCall, Response, ResponseAssembler,
};
pub use context::{Context, ContextError, Message, ToolCall, ToolCallId};
pub use error::{Error, Result};
pub use flows::{ContextStrategy, Flow, FlowBuilder, FlowDefError, FlowError, FlowManager, NodeId};
pub use interrupt::{
    Assessment, BackchannelSet, Confirm, DEFAULT_BACKCHANNEL_MAX_SPEECH, DEFAULT_BACKCHANNEL_WAIT,
    DEFAULT_VAD_HANGOVER, InterruptionPolicy, Observation, TextKind, VetoReason, count_words,
};
pub use normalize::{
    INLINE_CHUNK_CHARS, Normalizer, normalize, spell_cardinal, spell_digits, spell_ordinal,
};
pub use played::{
    PlayedConfig, PlayedText, TruncateError, Truncation, TruncationOutcome, TruncationStats,
    apply_truncation,
};
pub use segment::{Language, Segmenter, SegmenterConfig, Span};
pub use telephony::{
    AnsweredBy, Briefing, Destination, DtmfString, PhoneNumber, PolicyError, TelephonyError,
    TelephonyErrorKind, TelephonyOps, TelephonyPolicy, TransferMode, TransferReport,
    TransferRequest, register_telephony_tools,
};
pub use turn::{
    AnalysisId, BargeInCandidate, EndReason, InputKind, InterruptionStats, StateKind, TurnAction,
    TurnInput, TurnMachine, TurnPolicy, TurnState, TurnStats,
};
pub use window::{
    Summarizer, Summary, Window, WindowError, WindowPolicy, build_window, needs_summary,
    refresh_summary,
};
