"""Brisvox agent SDK: write the application logic of a voice call in Python.

The brisvox server connects out to your agent over a WebSocket (protocol version 1,
``docs/AGENT_PROTOCOL.md``); this package is the agent side::

    from brisvox_agent import Agent

    agent = Agent("luigi", system_prompt="You are Luigi's host.", greeting="Hi!")

    @agent.tool
    async def opening_hours(day: str) -> str:
        \"\"\"Opening hours of the restaurant on a weekday.\"\"\"
        return "12:00 to 22:00"

    agent.run(port=8765)
"""

from .agent import (
    Accept,
    Agent,
    AgentServer,
    CommandError,
    CommandTimeout,
    ContextMessage,
    Reject,
    Session,
    SessionClosed,
)
from .protocol import (
    PROTOCOL,
    SUPPORTED_VERSIONS,
    BotLlmStarted,
    BotLlmStopped,
    BotText,
    BotTtsStopped,
    BotTtsText,
    CallIdentity,
    Capabilities,
    CommandResult,
    ErrorCode,
    ErrorEvent,
    Event,
    EventsLost,
    LatencySummary,
    LlmUsage,
    ResumeInfo,
    Resumed,
    Metrics,
    ProtocolError,
    SessionEnd,
    SessionStart,
    SessionUsage,
    StageLatency,
    SttUsage,
    TokenUsage,
    ToolCall,
    ToolCancel,
    TtsUsage,
    TurnBotEnded,
    TurnBotStarted,
    TurnInterrupted,
    TurnMetrics,
    TurnUserEnded,
    TurnUserStarted,
    UnknownMessage,
    UsageSummary,
    UserText,
    UserTranscript,
    WireError,
)
from .schema import SchemaError
from .tools import Tool, ToolArgumentError, ToolContext, ToolError

__version__ = "0.1.1"

__all__ = [
    "Accept",
    "Agent",
    "AgentServer",
    "BotLlmStarted",
    "BotLlmStopped",
    "BotText",
    "BotTtsStopped",
    "BotTtsText",
    "CallIdentity",
    "Capabilities",
    "CommandError",
    "CommandResult",
    "CommandTimeout",
    "ContextMessage",
    "ErrorCode",
    "ErrorEvent",
    "Event",
    "EventsLost",
    "LatencySummary",
    "LlmUsage",
    "Metrics",
    "PROTOCOL",
    "ProtocolError",
    "Reject",
    "ResumeInfo",
    "Resumed",
    "SUPPORTED_VERSIONS",
    "SchemaError",
    "Session",
    "SessionClosed",
    "SessionEnd",
    "SessionStart",
    "SessionUsage",
    "StageLatency",
    "SttUsage",
    "TokenUsage",
    "Tool",
    "ToolArgumentError",
    "ToolCall",
    "ToolCancel",
    "ToolContext",
    "ToolError",
    "TtsUsage",
    "TurnBotEnded",
    "TurnBotStarted",
    "TurnInterrupted",
    "TurnMetrics",
    "TurnUserEnded",
    "TurnUserStarted",
    "UnknownMessage",
    "UsageSummary",
    "UserText",
    "UserTranscript",
    "WireError",
    "__version__",
]
