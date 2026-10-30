"""Typed messages of the brisvox agent protocol, version 1.

This module is the Python image of ``docs/AGENT_PROTOCOL.md`` (and of
``crates/brisvox-agent-link/src/protocol.rs``, the executable definition): every message the
server sends is a frozen dataclass, every message the agent sends is built by a function that
returns the wire dict. Both directions are plain JSON objects with a ``type`` field.

Design notes:

* Dataclasses, not pydantic: the protocol is small and flat, the SDK stays free of a compiled
  dependency, and "unknown fields are ignored" (the protocol's compatibility rule) is a matter of
  picking the known fields by hand.
* Parsing is strict about what the protocol requires (a missing or mistyped field raises
  :class:`ProtocolError` with code ``bad_message``) and lenient about everything else (extra
  fields, unknown ``type`` values become :class:`UnknownMessage`).
"""

from __future__ import annotations

import enum
import json
import logging
import re
from dataclasses import dataclass, field
from typing import Any, ClassVar, Literal, Mapping, Sequence

__all__ = [
    "PROTOCOL",
    "SUPPORTED_VERSIONS",
    "MAX_MESSAGE_BYTES",
    "MAX_LINE_BYTES",
    "MAX_NOTE_BYTES",
    "MAX_COMMAND_ID_BYTES",
    "MAX_TOOLS",
    "MAX_CONTEXT_MESSAGES",
    "MAX_SYSTEM_PROMPT_BYTES",
    "MAX_TOOL_TIMEOUT_MS",
    "EVENT_TYPES",
    "ErrorCode",
    "ProtocolError",
    "WireError",
    "Capabilities",
    "ResumeInfo",
    "Resumed",
    "Metrics",
    "TokenUsage",
    "StageLatency",
    "LatencySummary",
    "LlmUsage",
    "SttUsage",
    "TtsUsage",
    "UsageSummary",
    "Message",
    "Event",
    "SessionStart",
    "SessionEnd",
    "SessionUsage",
    "TurnUserStarted",
    "TurnUserEnded",
    "TurnBotStarted",
    "TurnBotEnded",
    "TurnInterrupted",
    "TurnMetrics",
    "UserTranscript",
    "UserText",
    "BotLlmStarted",
    "BotText",
    "BotLlmStopped",
    "BotTtsText",
    "BotTtsStopped",
    "LanguageSwitched",
    "ErrorEvent",
    "EventsLost",
    "ToolCall",
    "ToolCancel",
    "CommandResult",
    "UnknownMessage",
    "parse_server_message",
    "encode",
    "session_accept",
    "session_reject",
    "say",
    "end",
    "transfer",
    "context_update",
    "tools_set",
    "tool_result",
    "validate_tool_name",
    "validate_phone_number",
]

_log = logging.getLogger("brisvox_agent")

PROTOCOL = "brisvox.agent"
"""The ``protocol`` field of ``session.start``."""

SUPPORTED_VERSIONS: tuple[int, ...] = (1,)
"""The protocol versions this SDK speaks."""

MAX_MESSAGE_BYTES = 64 * 1024
"""Largest message either way; the server closes a link that sends a larger one."""
MAX_LINE_BYTES = 4 * 1024
"""Largest ``say.text`` (and ``end.say``)."""
MAX_NOTE_BYTES = 512
"""Largest ``transfer.summary`` / ``transfer.reason`` / ``end.reason``."""
MAX_COMMAND_ID_BYTES = 64
"""Largest command ``id``."""
MAX_TOOLS = 64
"""Most tools in ``session.accept``."""
MAX_CONTEXT_MESSAGES = 32
"""Most messages in ``session.accept.context`` and in one ``context.update``."""
MAX_SYSTEM_PROMPT_BYTES = 64 * 1024
"""Longest system prompt."""
MAX_TOOL_TIMEOUT_MS = 30_000
"""Largest ``timeout_ms`` of a tool declaration."""

EVENT_TYPES: tuple[str, ...] = (
    "turn.user_started",
    "turn.user_ended",
    "turn.bot_started",
    "turn.bot_ended",
    "turn.interrupted",
    "turn.metrics",
    "user.transcript",
    "user.text",
    "bot.llm_started",
    "bot.text",
    "bot.llm_stopped",
    "bot.tts_text",
    "bot.tts_stopped",
    "language.switched",
)
"""The event types an agent may subscribe to in ``session.accept.events``."""


class ErrorCode(str, enum.Enum):
    """The error codes of the protocol (``command.result.error.code``, ``session.reject.code``)."""

    BAD_MESSAGE = "bad_message"
    """Not JSON, a missing field, a field of the wrong type."""
    UNKNOWN_TYPE = "unknown_type"
    """A ``type`` this version does not know."""
    INVALID_ARGUMENT = "invalid_argument"
    """A value breaks a rule (an empty or too long text, a bad phone number)."""
    TOO_LARGE = "too_large"
    """A count bound was hit (messages of one ``context.update``, tool names)."""
    QUEUE_FULL = "queue_full"
    """The call's input queue is full: retry later."""
    UNSUPPORTED = "unsupported"
    """The call cannot do this (no call-control for ``transfer``)."""
    CALL_ENDED = "call_ended"
    """The call is over."""
    NOT_READY = "not_ready"
    """A handshake message after the handshake."""
    REJECTED = "rejected"
    """The carrier refused the transfer; nothing changed."""
    UNAVAILABLE = "unavailable"
    """The carrier could not be reached; nothing changed."""
    OUTCOME_UNKNOWN = "outcome_unknown"
    """The transfer may or may not have started (a timeout): do not repeat it blindly."""
    UNSUPPORTED_VERSION = "unsupported_version"
    """For ``session.reject``: the agent speaks none of the offered versions."""
    UNKNOWN_SESSION = "unknown_session"
    """For ``session.reject`` of a resumption: the agent does not know the call (it restarted,
    or the parked call expired)."""

    def __str__(self) -> str:  # pragma: no cover - cosmetic
        return self.value


class ProtocolError(Exception):
    """A message that breaks the protocol (either direction).

    ``code`` is the :class:`ErrorCode` the server would answer with.
    """

    def __init__(self, message: str, code: ErrorCode = ErrorCode.BAD_MESSAGE) -> None:
        super().__init__(message)
        self.code = code


@dataclass(frozen=True, slots=True)
class WireError:
    """``command.result.error``: a machine-readable ``code`` and a human ``message``.

    ``code`` is an :class:`ErrorCode` when the SDK knows it, else the raw string (a code added
    within version 1).
    """

    code: ErrorCode | str
    message: str


@dataclass(frozen=True, slots=True)
class Capabilities:
    """What the server can do on this call (``session.start.capabilities``)."""

    transfer: bool = False
    tools: bool = False


@dataclass(frozen=True, slots=True)
class CallIdentity:
    """Who is on the line (``session.start.call``): present only for a telephony call (Twilio,
    Telnyx, SIP) whose agent definition opted in (``[agent_link] caller_identity = true``).

    The numbers are what the carrier says; a caller ID can be spoofed, so do not treat it as
    proof of identity on its own. They are personal data: ``from_number`` and ``to_number`` are
    left out of ``repr()`` (and so out of a log line that prints the object).
    """

    carrier: str
    """``twilio``, ``telnyx`` or ``sip``."""
    carrier_call_id: str
    """The carrier's id of the call: Twilio ``CallSid``, Telnyx ``call_control_id``, SIP
    ``Call-ID``."""
    from_number: str | None = field(default=None, repr=False)
    """The caller's number (the SIP user part of ``From``). ``None`` when the carrier did not say,
    and when the server redacts phone numbers (:attr:`pii_redacted`). On an outbound call it is
    the number the call is placed from."""
    to_number: str | None = field(default=None, repr=False)
    """The called number, with the same absence rules as :attr:`from_number`."""
    pii_redacted: bool = False
    """``True``: the numbers were withheld because the server redacts phone numbers."""


@dataclass(frozen=True, slots=True)
class ResumeInfo:
    """``session.start.resume``: this ``session.start`` takes a call up again on a new link."""

    attempt: int
    """Which reconnection attempt of this outage it is (1 for the first)."""
    previous_seq: int
    """The last ``seq`` written on the lost link; the new link continues the count."""
    down_ms: int
    """Milliseconds since the loss."""


@dataclass(frozen=True, slots=True)
class Metrics:
    """Turn latencies in milliseconds (``turn.metrics``); ``None`` when unknown."""

    end_of_turn_ms: float | None = None
    stt_final_ms: float | None = None
    llm_ttft_ms: float | None = None
    tts_ttfb_ms: float | None = None
    response_ms: float | None = None


@dataclass(frozen=True, slots=True)
class TokenUsage:
    """Provider-reported LLM token usage of a response (``bot.llm_stopped.usage``)."""

    input_tokens: int = 0
    cached_input_tokens: int = 0
    """Of ``input_tokens``, the tokens served from the provider's prompt cache."""
    output_tokens: int = 0


@dataclass(frozen=True, slots=True)
class StageLatency:
    """Average and worst value of one latency stage over the turns of a call, in milliseconds."""

    avg_ms: float
    max_ms: float


@dataclass(frozen=True, slots=True)
class LatencySummary:
    """The per-call latency stages of ``session.usage``; ``None`` for a stage no turn had."""

    end_of_turn: StageLatency | None = None
    stt_final: StageLatency | None = None
    llm_ttft: StageLatency | None = None
    tts_ttfb: StageLatency | None = None
    response: StageLatency | None = None


@dataclass(frozen=True, slots=True)
class LlmUsage:
    """What the LLM provider billed in a call (discarded speculative responses included)."""

    provider: str
    model: str
    input_tokens: int = 0
    cached_input_tokens: int = 0
    output_tokens: int = 0
    responses_without_usage: int = 0
    """Responses that reported no usage: the token counts are lower bounds by those."""


@dataclass(frozen=True, slots=True)
class SttUsage:
    """The speech-to-text audio sent in a call."""

    provider: str
    model: str
    audio_seconds: float = 0.0


@dataclass(frozen=True, slots=True)
class TtsUsage:
    """The text-to-speech characters sent in a call."""

    provider: str
    model: str
    characters: int = 0


@dataclass(frozen=True, slots=True)
class UsageSummary:
    """The provider units of a call; a part is ``None`` when the call did not run that stage
    (a speech-to-speech call has no separate STT, LLM or TTS). Units, not money."""

    llm: LlmUsage | None = None
    stt: SttUsage | None = None
    tts: TtsUsage | None = None


# ---------------------------------------------------------------------------
# Server -> agent
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True, kw_only=True)
class Message:
    """Envelope of a server message: ``seq`` (the message number on the link, 1 for
    ``session.start``) and ``t_ms`` (milliseconds since the link opened)."""

    TYPE: ClassVar[str] = ""
    seq: int = 0
    t_ms: int = 0


class Event(Message):
    """Base of the events: the messages about what happens on the call."""

    __slots__ = ()


@dataclass(frozen=True, slots=True, kw_only=True)
class SessionStart(Message):
    """The first message: a call started and offers its protocol versions."""

    TYPE: ClassVar[str] = "session.start"
    protocol: str
    versions: tuple[int, ...]
    session_id: str
    agent: str
    capabilities: Capabilities = Capabilities()
    resume: ResumeInfo | None = None
    """Set when the server reconnects to a call that lost its link (see "Reconnecting")."""
    call: CallIdentity | None = None
    """Who is on the line; ``None`` unless the agent definition opted in and the call has a
    carrier (see "Caller identity")."""


@dataclass(frozen=True, slots=True, kw_only=True)
class SessionEnd(Event):
    """The last message (``reason``: ``call_ended``, ``hangup``, ``ended_by_agent``, ...)."""

    TYPE: ClassVar[str] = "session.end"
    reason: str


@dataclass(frozen=True, slots=True, kw_only=True)
class SessionUsage(Event):
    """What the call used and how fast it was, sent once right before ``session.end`` (always,
    whatever the agent subscribed to; missing when the link was down at the end of the call).
    Units, not money: multiply by your providers' prices."""

    TYPE: ClassVar[str] = "session.usage"
    duration_ms: int
    """From the start to the end of the call, on the server's clock."""
    turns: int
    """Turns that finished."""
    interrupted_turns: int
    """Of ``turns``, the ones the caller interrupted."""
    latency: LatencySummary = LatencySummary()
    usage: UsageSummary = UsageSummary()


@dataclass(frozen=True, slots=True, kw_only=True)
class TurnUserStarted(Event):
    """A user turn started (speech; also a confirmed barge-in)."""

    TYPE: ClassVar[str] = "turn.user_started"
    epoch: int


@dataclass(frozen=True, slots=True, kw_only=True)
class TurnUserEnded(Event):
    """The user turn ended; the engine answers it now."""

    TYPE: ClassVar[str] = "turn.user_ended"
    epoch: int


@dataclass(frozen=True, slots=True, kw_only=True)
class TurnBotStarted(Event):
    """The bot's audio of ``epoch`` started playing."""

    TYPE: ClassVar[str] = "turn.bot_started"
    epoch: int


@dataclass(frozen=True, slots=True, kw_only=True)
class TurnBotEnded(Event):
    """The bot's audio of ``epoch`` was played to its end."""

    TYPE: ClassVar[str] = "turn.bot_ended"
    epoch: int


@dataclass(frozen=True, slots=True, kw_only=True)
class TurnInterrupted(Event):
    """A barge-in: the call moved to ``epoch``; everything older was dropped."""

    TYPE: ClassVar[str] = "turn.interrupted"
    epoch: int


@dataclass(frozen=True, slots=True, kw_only=True)
class TurnMetrics(Event):
    """The turn's latencies when its first audio went out."""

    TYPE: ClassVar[str] = "turn.metrics"
    epoch: int
    metrics: Metrics = Metrics()


@dataclass(frozen=True, slots=True, kw_only=True)
class UserTranscript(Event):
    """A transcript segment from the STT (interim when ``final`` is false)."""

    TYPE: ClassVar[str] = "user.transcript"
    text: str
    final: bool


@dataclass(frozen=True, slots=True, kw_only=True)
class UserText(Event):
    """The whole text of the user turn the LLM answers (spoken finals and typed text)."""

    TYPE: ClassVar[str] = "user.text"
    epoch: int
    text: str


@dataclass(frozen=True, slots=True, kw_only=True)
class BotLlmStarted(Event):
    """The LLM started a response round (a tool loop has several)."""

    TYPE: ClassVar[str] = "bot.llm_started"
    epoch: int


@dataclass(frozen=True, slots=True, kw_only=True)
class BotText(Event):
    """A streamed LLM delta. A bot line arrives as one delta."""

    TYPE: ClassVar[str] = "bot.text"
    epoch: int
    text: str


@dataclass(frozen=True, slots=True, kw_only=True)
class BotLlmStopped(Event):
    """The response ended."""

    TYPE: ClassVar[str] = "bot.llm_stopped"
    epoch: int
    usage: TokenUsage | None = None
    """What the provider reported for the response, summed over the rounds of the turn;
    ``None`` when it reported none (an older server sends none either)."""


@dataclass(frozen=True, slots=True, kw_only=True)
class BotTtsText(Event):
    """A chunk sent to the TTS."""

    TYPE: ClassVar[str] = "bot.tts_text"
    epoch: int
    text: str


@dataclass(frozen=True, slots=True, kw_only=True)
class BotTtsStopped(Event):
    """The TTS produced the response's last audio."""

    TYPE: ClassVar[str] = "bot.tts_stopped"
    epoch: int


@dataclass(frozen=True, slots=True, kw_only=True)
class LanguageSwitched(Event):
    """The agent follows the caller into another language: the replies from the next turn on are
    in ``to`` (language codes such as ``en``, ``tr``)."""

    TYPE: ClassVar[str] = "language.switched"
    from_language: str
    to: str


@dataclass(frozen=True, slots=True, kw_only=True)
class ErrorEvent(Event):
    """A stage of the call reported an error, or the link refused something without a command
    id (``stage == "agent_link"``, the message then starts with the error code)."""

    TYPE: ClassVar[str] = "error"
    stage: str
    message: str
    fatal: bool


@dataclass(frozen=True, slots=True, kw_only=True)
class EventsLost(Event):
    """``count`` events never reached the agent (see "Backpressure" in the protocol doc)."""

    TYPE: ClassVar[str] = "events.lost"
    count: int


@dataclass(frozen=True, slots=True, kw_only=True)
class Resumed(Event):
    """Not a wire message: the SDK's own event, delivered to ``on_resumed`` handlers once a lost
    link of the call was taken up again (after the events queued before the loss). ``seq`` is that
    of the resuming ``session.start``."""

    TYPE: ClassVar[str] = "session.resumed"
    attempt: int
    previous_seq: int
    down_ms: int
    seen_seq: int = 0
    """The ``seq`` of the last server message this agent itself read on the lost link; the server's
    ``previous_seq`` equals it when nothing was lost in flight."""


@dataclass(frozen=True, slots=True, kw_only=True)
class ToolCall(Message):
    """Run the tool and answer with ``tool.result`` echoing ``id``."""

    TYPE: ClassVar[str] = "tool.call"
    id: int
    call_id: str
    name: str
    arguments: dict[str, Any]
    kind: Literal["read_only", "side_effect"]
    epoch: int
    idempotency_key: str
    timeout_ms: int


@dataclass(frozen=True, slots=True, kw_only=True)
class ToolCancel(Message):
    """The server stopped waiting for tool call ``id`` (``reason``: ``interrupted``,
    ``timeout``, ``abandoned``): a later result is ignored. Advice only for ``side_effect``."""

    TYPE: ClassVar[str] = "tool.cancel"
    id: int
    reason: str


@dataclass(frozen=True, slots=True, kw_only=True)
class CommandResult(Message):
    """The outcome of a command that carried an ``id``."""

    TYPE: ClassVar[str] = "command.result"
    id: str
    ok: bool
    error: WireError | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class UnknownMessage(Message):
    """A message whose ``type`` this SDK does not know (an addition within version 1)."""

    type: str
    raw: Mapping[str, Any] = field(default_factory=dict)


# --- parsing -----------------------------------------------------------------------------------


def _need(obj: Mapping[str, Any], key: str, kinds: type | tuple[type, ...], what: str) -> Any:
    """Required field ``key`` of type ``kinds`` (``bool`` is never accepted as an int)."""
    if key not in obj:
        raise ProtocolError(f"{what}: missing field {key!r}")
    value = obj[key]
    ok = isinstance(value, kinds)
    if ok and isinstance(value, bool) and bool not in (kinds if isinstance(kinds, tuple) else (kinds,)):
        ok = False
    if not ok:
        raise ProtocolError(f"{what}: field {key!r} has the wrong type")
    return value


def _opt_number(obj: Mapping[str, Any], key: str) -> float | None:
    value = obj.get(key)
    if value is None or isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


def _opt_str(obj: Mapping[str, Any], key: str) -> str | None:
    value = obj.get(key)
    return value if isinstance(value, str) else None


def _parse_call(raw: Any) -> CallIdentity | None:
    if raw is None:
        return None
    # The identity is an optional addition: a malformed one must not end the call.
    try:
        if not isinstance(raw, Mapping):
            raise ProtocolError("call must be an object")
        return CallIdentity(
            carrier=_need(raw, "carrier", str, "session.start.call"),
            carrier_call_id=_need(raw, "carrier_call_id", str, "session.start.call"),
            from_number=_opt_str(raw, "from"),
            to_number=_opt_str(raw, "to"),
            pii_redacted=raw.get("pii_redacted") is True,
        )
    except ProtocolError as error:
        _log.debug("session.start: ignoring a malformed call identity: %s", error)
        return None


def _error_code(value: Any) -> ErrorCode | str:
    try:
        return ErrorCode(value)
    except ValueError:
        return str(value)


def _parse_metrics(raw: Any) -> Metrics:
    if not isinstance(raw, Mapping):
        return Metrics()
    return Metrics(
        end_of_turn_ms=_opt_number(raw, "end_of_turn_ms"),
        stt_final_ms=_opt_number(raw, "stt_final_ms"),
        llm_ttft_ms=_opt_number(raw, "llm_ttft_ms"),
        tts_ttfb_ms=_opt_number(raw, "tts_ttfb_ms"),
        response_ms=_opt_number(raw, "response_ms"),
    )


def _count(obj: Mapping[str, Any], key: str) -> int:
    value = obj.get(key)
    if isinstance(value, bool) or not isinstance(value, int):
        return 0
    return value


def _parse_token_usage(raw: Any) -> TokenUsage | None:
    # An optional addition: a malformed one is ignored, the event still counts.
    if not isinstance(raw, Mapping):
        return None
    return TokenUsage(
        input_tokens=_count(raw, "input_tokens"),
        cached_input_tokens=_count(raw, "cached_input_tokens"),
        output_tokens=_count(raw, "output_tokens"),
    )


def _parse_stage_latency(raw: Any) -> StageLatency | None:
    if not isinstance(raw, Mapping):
        return None
    avg, worst = _opt_number(raw, "avg_ms"), _opt_number(raw, "max_ms")
    if avg is None or worst is None:
        return None
    return StageLatency(avg_ms=avg, max_ms=worst)


def _parse_latency_summary(raw: Any) -> LatencySummary:
    if not isinstance(raw, Mapping):
        return LatencySummary()
    return LatencySummary(
        end_of_turn=_parse_stage_latency(raw.get("end_of_turn")),
        stt_final=_parse_stage_latency(raw.get("stt_final")),
        llm_ttft=_parse_stage_latency(raw.get("llm_ttft")),
        tts_ttfb=_parse_stage_latency(raw.get("tts_ttfb")),
        response=_parse_stage_latency(raw.get("response")),
    )


def _parse_usage_summary(raw: Any) -> UsageSummary:
    if not isinstance(raw, Mapping):
        return UsageSummary()
    llm, stt, tts = raw.get("llm"), raw.get("stt"), raw.get("tts")
    return UsageSummary(
        llm=LlmUsage(
            provider=str(llm.get("provider", "")),
            model=str(llm.get("model", "")),
            input_tokens=_count(llm, "input_tokens"),
            cached_input_tokens=_count(llm, "cached_input_tokens"),
            output_tokens=_count(llm, "output_tokens"),
            responses_without_usage=_count(llm, "responses_without_usage"),
        )
        if isinstance(llm, Mapping)
        else None,
        stt=SttUsage(
            provider=str(stt.get("provider", "")),
            model=str(stt.get("model", "")),
            audio_seconds=_opt_number(stt, "audio_seconds") or 0.0,
        )
        if isinstance(stt, Mapping)
        else None,
        tts=TtsUsage(
            provider=str(tts.get("provider", "")),
            model=str(tts.get("model", "")),
            characters=_count(tts, "characters"),
        )
        if isinstance(tts, Mapping)
        else None,
    )


def _parse_body(kind: str, obj: Mapping[str, Any], env: dict[str, int]) -> Message:
    what = kind
    epoch = lambda: _need(obj, "epoch", int, what)  # noqa: E731
    text = lambda: _need(obj, "text", str, what)  # noqa: E731
    if kind == "session.start":
        caps = obj.get("capabilities")
        caps = caps if isinstance(caps, Mapping) else {}
        versions = _need(obj, "versions", list, what)
        if not all(isinstance(v, int) and not isinstance(v, bool) for v in versions):
            raise ProtocolError(f"{what}: versions must be integers")
        raw_resume = obj.get("resume")
        resume = None
        if raw_resume is not None:
            if not isinstance(raw_resume, Mapping):
                raise ProtocolError(f"{what}: resume must be an object")
            resume = ResumeInfo(
                attempt=_need(raw_resume, "attempt", int, "session.start.resume"),
                previous_seq=_need(raw_resume, "previous_seq", int, "session.start.resume"),
                down_ms=_need(raw_resume, "down_ms", int, "session.start.resume"),
            )
        return SessionStart(
            **env,
            protocol=_need(obj, "protocol", str, what),
            versions=tuple(versions),
            session_id=_need(obj, "session_id", str, what),
            agent=_need(obj, "agent", str, what),
            capabilities=Capabilities(
                transfer=bool(caps.get("transfer", False)),
                tools=bool(caps.get("tools", False)),
            ),
            resume=resume,
            call=_parse_call(obj.get("call")),
        )
    if kind == "session.end":
        return SessionEnd(**env, reason=_need(obj, "reason", str, what))
    if kind == "session.usage":
        return SessionUsage(
            **env,
            duration_ms=_need(obj, "duration_ms", int, what),
            turns=_need(obj, "turns", int, what),
            interrupted_turns=_need(obj, "interrupted_turns", int, what),
            latency=_parse_latency_summary(obj.get("latency")),
            usage=_parse_usage_summary(obj.get("usage")),
        )
    if kind == "turn.user_started":
        return TurnUserStarted(**env, epoch=epoch())
    if kind == "turn.user_ended":
        return TurnUserEnded(**env, epoch=epoch())
    if kind == "turn.bot_started":
        return TurnBotStarted(**env, epoch=epoch())
    if kind == "turn.bot_ended":
        return TurnBotEnded(**env, epoch=epoch())
    if kind == "turn.interrupted":
        return TurnInterrupted(**env, epoch=epoch())
    if kind == "turn.metrics":
        return TurnMetrics(**env, epoch=epoch(), metrics=_parse_metrics(obj.get("metrics")))
    if kind == "user.transcript":
        return UserTranscript(**env, text=text(), final=_need(obj, "final", bool, what))
    if kind == "user.text":
        return UserText(**env, epoch=epoch(), text=text())
    if kind == "bot.llm_started":
        return BotLlmStarted(**env, epoch=epoch())
    if kind == "bot.text":
        return BotText(**env, epoch=epoch(), text=text())
    if kind == "bot.llm_stopped":
        return BotLlmStopped(**env, epoch=epoch(), usage=_parse_token_usage(obj.get("usage")))
    if kind == "bot.tts_text":
        return BotTtsText(**env, epoch=epoch(), text=text())
    if kind == "bot.tts_stopped":
        return BotTtsStopped(**env, epoch=epoch())
    if kind == "language.switched":
        return LanguageSwitched(
            **env,
            from_language=_need(obj, "from", str, what),
            to=_need(obj, "to", str, what),
        )
    if kind == "error":
        return ErrorEvent(
            **env,
            stage=_need(obj, "stage", str, what),
            message=_need(obj, "message", str, what),
            fatal=_need(obj, "fatal", bool, what),
        )
    if kind == "events.lost":
        return EventsLost(**env, count=_need(obj, "count", int, what))
    if kind == "tool.call":
        tool_kind = _need(obj, "kind", str, what)
        if tool_kind not in ("read_only", "side_effect"):
            raise ProtocolError(f"{what}: unknown tool kind {tool_kind!r}")
        return ToolCall(
            **env,
            id=_need(obj, "id", int, what),
            call_id=_need(obj, "call_id", str, what),
            name=_need(obj, "name", str, what),
            arguments=_need(obj, "arguments", dict, what),
            kind=tool_kind,
            epoch=epoch(),
            idempotency_key=_need(obj, "idempotency_key", str, what),
            timeout_ms=_need(obj, "timeout_ms", int, what),
        )
    if kind == "tool.cancel":
        return ToolCancel(
            **env, id=_need(obj, "id", int, what), reason=_need(obj, "reason", str, what)
        )
    if kind == "command.result":
        error = obj.get("error")
        wire_error = None
        if isinstance(error, Mapping):
            wire_error = WireError(
                code=_error_code(error.get("code", "")), message=str(error.get("message", ""))
            )
        return CommandResult(
            **env,
            id=_need(obj, "id", str, what),
            ok=_need(obj, "ok", bool, what),
            error=wire_error,
        )
    return UnknownMessage(**env, type=kind, raw=dict(obj))


def parse_server_message(data: str | bytes | Mapping[str, Any]) -> Message:
    """Parse one server message (a JSON text or an already decoded object).

    Fields the SDK does not know are ignored; an unknown ``type`` gives an
    :class:`UnknownMessage`.

    Raises:
        ProtocolError: not JSON, not an object, no ``type``, or a required field is missing
            or has the wrong type.
    """
    if isinstance(data, Mapping):
        obj: Any = data
    else:
        if isinstance(data, (bytes, bytearray)):
            raise ProtocolError("binary frames are not part of the protocol")
        try:
            obj = json.loads(data)
        except ValueError as exc:
            raise ProtocolError(f"not JSON: {exc}") from exc
    if not isinstance(obj, Mapping):
        raise ProtocolError("a message must be a JSON object")
    kind = obj.get("type")
    if not isinstance(kind, str):
        raise ProtocolError("missing string field 'type'")
    env = {
        "seq": obj["seq"] if isinstance(obj.get("seq"), int) else 0,
        "t_ms": obj["t_ms"] if isinstance(obj.get("t_ms"), int) else 0,
    }
    return _parse_body(kind, obj, env)


# ---------------------------------------------------------------------------
# Agent -> server
# ---------------------------------------------------------------------------

_TOOL_NAME = re.compile(r"[A-Za-z0-9_-]{1,64}")
_E164 = re.compile(r"\+[0-9]{8,15}")


def validate_tool_name(name: str) -> str:
    """Return ``name`` if it is a legal tool name (1 to 64 of ``A-Z a-z 0-9 _ -``)."""
    if not _TOOL_NAME.fullmatch(name):
        raise ValueError(f"illegal tool name {name!r}: 1 to 64 of A-Z a-z 0-9 _ -")
    return name


def validate_phone_number(number: str) -> str:
    """Return ``number`` if it is E.164 as the protocol wants it (``+`` and 8 to 15 digits)."""
    if not _E164.fullmatch(number):
        raise ProtocolError(
            f"invalid destination {number!r}: E.164 is '+' and 8 to 15 digits",
            ErrorCode.INVALID_ARGUMENT,
        )
    return number


def _bounded(value: str | None, limit: int, what: str) -> None:
    if value is not None and len(value.encode("utf-8")) > limit:
        raise ProtocolError(f"{what} is longer than {limit} bytes", ErrorCode.INVALID_ARGUMENT)


def _put(out: dict[str, Any], **fields: Any) -> dict[str, Any]:
    out.update({k: v for k, v in fields.items() if v is not None})
    return out


def _command(
    kind: str, id: str | None, re_: int | None, **fields: Any
) -> dict[str, Any]:
    if id is not None and (not id or len(id.encode("utf-8")) > MAX_COMMAND_ID_BYTES):
        raise ProtocolError(
            f"command id must be 1 to {MAX_COMMAND_ID_BYTES} bytes", ErrorCode.INVALID_ARGUMENT
        )
    return _put({"type": kind}, id=id, re=re_, **fields)


def encode(message: Mapping[str, Any]) -> str:
    """Serialize an agent message as one compact JSON text.

    Raises:
        ProtocolError: ``too_large`` when it exceeds :data:`MAX_MESSAGE_BYTES`.
    """
    text = json.dumps(message, separators=(",", ":"), ensure_ascii=False, allow_nan=False)
    if len(text.encode("utf-8")) > MAX_MESSAGE_BYTES:
        raise ProtocolError(
            f"message is larger than {MAX_MESSAGE_BYTES} bytes", ErrorCode.TOO_LARGE
        )
    return text


def session_accept(
    version: int,
    *,
    tools: Sequence[Mapping[str, Any]] = (),
    system_prompt: str | None = None,
    context: Sequence[Mapping[str, str]] = (),
    greeting: str | None = None,
    events: Sequence[str] | None = None,
    resume: bool = False,
) -> dict[str, Any]:
    """Build ``session.accept``. ``events=None`` subscribes to every event; ``[]`` to none.
    ``resume=True`` promises to keep the call's state after a lost link (and is required in the
    answer to a resuming ``session.start``)."""
    if len(tools) > MAX_TOOLS:
        raise ProtocolError(f"more than {MAX_TOOLS} tools", ErrorCode.TOO_LARGE)
    if len(context) > MAX_CONTEXT_MESSAGES:
        raise ProtocolError(
            f"more than {MAX_CONTEXT_MESSAGES} context messages", ErrorCode.TOO_LARGE
        )
    _bounded(system_prompt, MAX_SYSTEM_PROMPT_BYTES, "system_prompt")
    _bounded(greeting, MAX_LINE_BYTES, "greeting")
    message: dict[str, Any] = {"type": "session.accept", "version": version, "tools": list(tools)}
    _put(message, system_prompt=system_prompt, greeting=greeting)
    if context:
        message["context"] = [dict(m) for m in context]
    if events is not None:
        message["events"] = list(events)
    if resume:
        message["resume"] = True
    return message


def session_reject(
    code: ErrorCode | str | None = None, message: str | None = None
) -> dict[str, Any]:
    """Build ``session.reject``: the agent does not take this call."""
    return _put(
        {"type": "session.reject"},
        code=code.value if isinstance(code, ErrorCode) else code,
        message=message,
    )


def say(
    text: str, *, interrupt: bool = False, id: str | None = None, re: int | None = None
) -> dict[str, Any]:
    """Build ``say``: the bot speaks ``text`` verbatim, without an LLM request."""
    if not text.strip():
        raise ProtocolError("say.text is empty", ErrorCode.INVALID_ARGUMENT)
    _bounded(text, MAX_LINE_BYTES, "say.text")
    message = _command("say", id, re, text=text)
    if interrupt:
        message["interrupt"] = True
    return message


def end(
    *,
    say: str | None = None,
    reason: str | None = None,
    id: str | None = None,
    re: int | None = None,
) -> dict[str, Any]:
    """Build ``end``: speak the optional farewell ``say``, then end the call."""
    if say is not None and not say.strip():
        raise ProtocolError("end.say is empty", ErrorCode.INVALID_ARGUMENT)
    _bounded(say, MAX_LINE_BYTES, "end.say")
    _bounded(reason, MAX_NOTE_BYTES, "end.reason")
    return _command("end", id, re, say=say, reason=reason)


def transfer(
    destination: str,
    *,
    mode: Literal["cold", "warm"] | None = None,
    summary: str | None = None,
    reason: str | None = None,
    id: str | None = None,
    re: int | None = None,
) -> dict[str, Any]:
    """Build ``transfer`` (E.164 ``destination``; ``mode`` ``cold`` by default).

    Always give a transfer an ``id``: a repeat with the same ``id`` carries the same carrier
    idempotency key, a transfer without one is a new transfer every time.
    """
    validate_phone_number(destination)
    if mode not in (None, "cold", "warm"):
        raise ProtocolError("mode must be 'cold' or 'warm'", ErrorCode.INVALID_ARGUMENT)
    _bounded(summary, MAX_NOTE_BYTES, "transfer.summary")
    _bounded(reason, MAX_NOTE_BYTES, "transfer.reason")
    return _command(
        "transfer", id, re, destination=destination, mode=mode, summary=summary, reason=reason
    )


def context_update(
    *,
    system_prompt: str | None = None,
    append: Sequence[Mapping[str, str]] = (),
    id: str | None = None,
    re: int | None = None,
) -> dict[str, Any]:
    """Build ``context.update``: replace the system prompt and/or append messages
    (roles ``system``, ``user``, ``assistant``; at most 32; all or nothing)."""
    if len(append) > MAX_CONTEXT_MESSAGES:
        raise ProtocolError(
            f"more than {MAX_CONTEXT_MESSAGES} messages", ErrorCode.TOO_LARGE
        )
    for item in append:
        if item.get("role") not in ("system", "user", "assistant"):
            raise ProtocolError(
                f"unknown role {item.get('role')!r}", ErrorCode.INVALID_ARGUMENT
            )
        if not str(item.get("text", "")).strip():
            raise ProtocolError("a context message is empty", ErrorCode.INVALID_ARGUMENT)
    _bounded(system_prompt, MAX_SYSTEM_PROMPT_BYTES, "system_prompt")
    message = _command("context.update", id, re, system_prompt=system_prompt)
    if append:
        message["append"] = [{"role": m["role"], "text": m["text"]} for m in append]
    return message


def tools_set(
    enabled: Sequence[str] | None,
    *,
    id: str | None = None,
    re: int | None = None,
) -> dict[str, Any]:
    """Build ``tools.set``: offer only the ``enabled`` tools (local and remote); ``None``: all."""
    message = _command("tools.set", id, re)
    message["enabled"] = None if enabled is None else list(enabled)
    return message


def tool_result(
    id: int, *, result: Any = None, error: str | None = None
) -> dict[str, Any]:
    """Build ``tool.result`` answering ``tool.call`` ``id``.

    A string ``result`` reaches the LLM as it is, any other JSON in compact form; ``error``
    wins over ``result`` and reaches the LLM as a ``tool_failed`` error result.
    """
    message: dict[str, Any] = {"type": "tool.result", "id": id}
    if error is not None:
        message["error"] = error
    else:
        message["result"] = result
    return message
