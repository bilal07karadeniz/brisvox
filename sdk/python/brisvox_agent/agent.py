"""The async agent API: an :class:`Agent` is a WebSocket server that brisvox connects to.

One connection is one call. The agent answers the handshake, then runs three things per call:

* a **reader** that never blocks on user code: it resolves command results, starts tool calls
  and queues events;
* a **dispatcher** that runs the event handlers one at a time, in the server's order
  (a bounded queue; when a slow handler lets it fill up, the newest events are dropped and
  reported to the handlers as a synthetic :class:`~brisvox_agent.EventsLost`, mirroring what the
  server does on its side);
* one **task per tool call**, so tools run concurrently and a slow event handler never delays a
  tool result.

Everything that waits on the network or on the server has a timeout.

**Reconnecting** (opt-in, ``Agent(resume=True)``): the agent then declares ``resume`` in
``session.accept``, and when a call's link is lost the :class:`Session` is *parked* instead of
ended: its ``data``, queued events and handler state stay for ``resume_window`` seconds. The server
reconnects with a ``session.start`` carrying ``resume``; the agent re-attaches the parked session
to the new link (no greeting, no ``on_session_start``), runs the ``on_resumed`` handlers and goes
on. A parked session nobody resumes ends cleanly after the window (``on_session_end`` with reason
``link_closed``).
"""

from __future__ import annotations

import asyncio
import contextvars
import hmac
import inspect
import ipaddress
import logging
import os
import ssl as ssl_module
import sys
from collections.abc import Awaitable, Callable, Iterable, Mapping, Sequence
from dataclasses import dataclass, field
from http import HTTPStatus
from typing import Any, Literal, TypeVar

from websockets.asyncio.server import Server, ServerConnection, serve
from websockets.exceptions import ConnectionClosed

from . import protocol
from .protocol import (
    CommandResult,
    ErrorCode,
    Event,
    EventsLost,
    ProtocolError,
    Resumed,
    SessionEnd,
    SessionStart,
    ToolCall,
    ToolCancel,
    UnknownMessage,
)
from .tools import Tool, ToolContext, ToolError, to_json

__all__ = [
    "Agent",
    "AgentServer",
    "Accept",
    "ContextMessage",
    "Reject",
    "Session",
    "SessionClosed",
    "CommandError",
    "CommandTimeout",
]

log = logging.getLogger("brisvox_agent")

E = TypeVar("E", bound=Event)
F = TypeVar("F", bound=Callable[..., Any])


def is_loopback_host(host: str) -> bool:
    """Whether binding ``host`` is reachable only from this machine (``localhost``, ``127.0.0.0/8``,
    ``::1``, or an IPv4-mapped loopback address)."""
    name = host.strip().strip("[]").lower()
    if name == "localhost" or name.endswith(".localhost"):
        return True
    try:
        address = ipaddress.ip_address(name)
    except ValueError:
        return False
    if isinstance(address, ipaddress.IPv6Address) and address.ipv4_mapped is not None:
        address = address.ipv4_mapped
    return address.is_loopback


LOOPBACK_HINT = (
    "loopback: reachable only from this machine; "
    "bind 0.0.0.0 or :: for a server in another container"
)


def listening_message(name: str, host: str, url: str) -> str:
    """The startup line of :meth:`Agent.run`: the address the agent bound, with a hint when it is
    a loopback address."""
    message = f"agent {name!r} listening on {url}"
    if is_loopback_host(host):
        message += f" ({LOOPBACK_HINT})"
    return message

_AUTO: Any = object()
"""Sentinel: take ``re`` from the event the current handler is reacting to."""

_current_re: contextvars.ContextVar[int | None] = contextvars.ContextVar(
    "brisvox_agent_re", default=None
)

_TURN_EVENTS = (
    "turn.user_started",
    "turn.user_ended",
    "turn.bot_started",
    "turn.bot_ended",
    "turn.interrupted",
    "turn.metrics",
)


class SessionClosed(ConnectionError):
    """The call's link is closed: nothing more can be sent."""


class CommandError(Exception):
    """A command failed: the server answered ``command.result`` with ``ok: false``, or the
    command broke a protocol rule before it was sent.

    ``code`` is an :class:`~brisvox_agent.ErrorCode` (or the raw string of a code this SDK does
    not know yet).
    """

    def __init__(self, code: ErrorCode | str, message: str) -> None:
        super().__init__(f"{getattr(code, 'value', code)}: {message}")
        self.code = code
        self.message = message


class CommandTimeout(CommandError, TimeoutError):
    """No ``command.result`` came in time. The command may still have been applied; for a
    ``transfer`` that is ``outcome_unknown``: retry only with the same ``id``."""

    def __init__(self, command_id: str, timeout: float) -> None:
        super().__init__(
            ErrorCode.OUTCOME_UNKNOWN,
            f"no result for command {command_id!r} within {timeout:g}s; "
            "it may or may not have been applied",
        )
        self.command_id = command_id


class Reject(Exception):
    """Raise from the ``on_session_start`` handler to decline the call (``session.reject``).

    The call then runs on the server's agent definition alone.
    """

    def __init__(self, code: ErrorCode | str = ErrorCode.UNAVAILABLE, message: str = "") -> None:
        super().__init__(message or str(getattr(code, "value", code)))
        self.code = code
        self.message = message


@dataclass(frozen=True, slots=True)
class ContextMessage:
    """A conversation message: ``role`` is ``system``, ``user`` or ``assistant``."""

    role: Literal["system", "user", "assistant"]
    text: str


def _wire_messages(items: Iterable[ContextMessage | Mapping[str, str] | tuple[str, str]]) -> list[dict[str, str]]:
    out: list[dict[str, str]] = []
    for item in items:
        if isinstance(item, ContextMessage):
            out.append({"role": item.role, "text": item.text})
        elif isinstance(item, Mapping):
            out.append({"role": item["role"], "text": item["text"]})
        else:
            role, text = item
            out.append({"role": role, "text": text})
    return out


_UNSET: Any = object()


@dataclass(slots=True)
class Accept:
    """What an ``on_session_start`` handler may change for this call: any field left unset keeps
    the agent's default."""

    system_prompt: str | None = _UNSET
    context: Sequence[ContextMessage | Mapping[str, str] | tuple[str, str]] = _UNSET
    greeting: str | None = _UNSET
    events: Sequence[str] | None = _UNSET
    """Event types to receive (``None``: all; ``[]``: none)."""
    tools: Sequence[str] | None = _UNSET
    """Names of the declared tools to offer on this call (``None``: all of them)."""


@dataclass(slots=True)
class _Pending:
    future: asyncio.Future[CommandResult]


class Session:
    """One call, seen from the agent: its identity, the commands to act on it, and a ``data``
    dict for the handlers' own per-call state.

    Created by the :class:`Agent` when a call connects; handlers and tools receive it.
    """

    def __init__(
        self,
        agent: "Agent",
        ws: ServerConnection,
        start: SessionStart,
        version: int,
    ) -> None:
        self._agent = agent
        self._ws = ws
        self.start = start
        """The ``session.start`` message (versions offered, capabilities, ...)."""
        self.version = version
        """The protocol version picked for this call."""
        self.data: dict[str, Any] = {}
        """Free space for the handlers of this call."""
        self.end_reason: str | None = None
        """The ``session.end`` reason once the call is over."""
        self._pending: dict[str, _Pending] = {}
        self._next_id = 0
        self._closed = asyncio.Event()
        self._tool_tasks: dict[int, tuple[asyncio.Task[None], ToolContext]] = {}
        self._queue: asyncio.Queue[Event | None] = asyncio.Queue(agent.event_queue)
        self._lost = 0
        # Events put on the queue / delivered so far, and the number of queued items that precede
        # the first drop of the current gap: EventsLost is reported once that many are delivered,
        # so it lands where the gap is (before the next event that fits after it).
        self._enqueued = 0
        self._delivered = 0
        self._gap_after = 0
        # The session.end (the server's, or a synthetic link_closed one): never dropped. It is
        # queued behind everything else; if the queue is full it waits (`_end_pending`) for room.
        self._end: SessionEnd | None = None
        self._end_pending = False
        self._end_started = False
        # Reconnecting (Agent(resume=True)): the dispatcher outlives a link, a parked session waits
        # for the server to come back until `_park_deadline` (loop time).
        self._dispatcher: asyncio.Task[None] | None = None
        self._events_sub: list[str] | None = None
        self._parked = False
        self._resuming = False
        self._park_deadline = 0.0
        self._park_timer: asyncio.TimerHandle | None = None
        # The `seq` of the last server message read on the current (or last) link: what a
        # resuming `session.start.resume.previous_seq` must continue from.
        self._last_seq = start.seq
        self.resumes = 0
        """How many times the call's link was taken up again after a loss."""

    # -- identity -------------------------------------------------------------------------------

    @property
    def id(self) -> str:
        """The server's id of the call."""
        return self.start.session_id

    @property
    def agent_name(self) -> str:
        """The name of the agent definition the call runs."""
        return self.start.agent

    @property
    def capabilities(self) -> protocol.Capabilities:
        """What the server can do on this call (``transfer``, ``tools``)."""
        return self.start.capabilities

    @property
    def call(self) -> protocol.CallIdentity | None:
        """Who is on the line: the carrier, the carrier's call id and the caller's and called
        numbers. ``None`` unless the agent definition opted in (``[agent_link] caller_identity =
        true`` or ``BRISVOX_AGENT_CALLER_IDENTITY=1``) and the call comes through a carrier
        (Twilio, Telnyx, SIP). The numbers are ``None`` when the server redacts phone numbers
        (``call.pii_redacted``). Personal data: keep it out of your logs."""
        return self.start.call

    @property
    def closed(self) -> bool:
        """Whether the link is closed (also true while the session is :attr:`parked`)."""
        return self._closed.is_set()

    @property
    def parked(self) -> bool:
        """Whether the link was lost and the session waits for the server to resume the call
        (only with ``Agent(resume=True)``). Commands raise :class:`SessionClosed` meanwhile."""
        return self._parked

    async def wait_closed(self) -> None:
        """Wait until the link is closed (a lost link counts; a resumed one is open again)."""
        await self._closed.wait()

    # -- commands -------------------------------------------------------------------------------

    async def say(
        self,
        text: str,
        *,
        interrupt: bool = False,
        id: str | None = None,
        re: int | None = _AUTO,
        wait: bool = True,
        timeout: float | None = None,
    ) -> CommandResult | asyncio.Future[CommandResult]:
        """The bot speaks ``text`` verbatim (a *bot line*, no LLM request).

        The line waits (in order) while the user speaks or a response runs. With
        ``interrupt=True`` it cuts a running response, like a barge-in (never the user).

        With ``wait=True`` (the default) this returns the :class:`CommandResult` once the server
        has queued the line, and raises :class:`CommandError` if it refused (``queue_full``,
        ``call_ended``, ``invalid_argument``). With ``wait=False`` it returns a future of the
        result at once.
        """
        return await self._command(
            lambda cid, r: protocol.say(text, interrupt=interrupt, id=cid, re=r),
            id=id,
            re=re,
            wait=wait,
            timeout=timeout if timeout is not None else self._agent.command_timeout,
        )

    async def end(
        self,
        say: str | None = None,
        *,
        reason: str | None = None,
        id: str | None = None,
        re: int | None = _AUTO,
        wait: bool = True,
        timeout: float | None = None,
    ) -> CommandResult | asyncio.Future[CommandResult]:
        """End the call: speak the optional farewell ``say``, wait until the bot is silent,
        then hang up (the server bounds this at 30 s). The result comes when the call is being
        ended; ``session.end`` (reason ``ended_by_agent``) follows."""
        return await self._command(
            lambda cid, r: protocol.end(say=say, reason=reason, id=cid, re=r),
            id=id,
            re=re,
            wait=wait,
            timeout=timeout if timeout is not None else self._agent.command_timeout + 35.0,
        )

    async def transfer(
        self,
        destination: str,
        *,
        mode: Literal["cold", "warm"] | None = None,
        summary: str | None = None,
        reason: str | None = None,
        id: str | None = None,
        re: int | None = _AUTO,
        wait: bool = True,
        timeout: float | None = None,
    ) -> CommandResult | asyncio.Future[CommandResult]:
        """Transfer the caller to the E.164 number ``destination`` through the carrier.

        Needs ``session.capabilities.transfer`` (else the server answers ``unsupported``).
        The command always carries an ``id`` (generated when you pass none), because the
        carrier's idempotency key derives from it: after a :class:`CommandTimeout` (outcome
        unknown) or a lost result, repeat the transfer **with the same** ``id`` and it cannot
        start twice.
        """
        if id is None:
            id = self._new_id()
        return await self._command(
            lambda cid, r: protocol.transfer(
                destination, mode=mode, summary=summary, reason=reason, id=cid, re=r
            ),
            id=id,
            re=re,
            wait=wait,
            timeout=timeout if timeout is not None else self._agent.command_timeout + 50.0,
        )

    async def update_context(
        self,
        *,
        system_prompt: str | None = None,
        append: Sequence[ContextMessage | Mapping[str, str] | tuple[str, str]] = (),
        id: str | None = None,
        re: int | None = _AUTO,
        wait: bool = True,
        timeout: float | None = None,
    ) -> CommandResult | asyncio.Future[CommandResult]:
        """Edit the conversation from the next response on: replace the system prompt and/or
        append up to 32 messages (all of the command or nothing)."""
        return await self._command(
            lambda cid, r: protocol.context_update(
                system_prompt=system_prompt, append=_wire_messages(append), id=cid, re=r
            ),
            id=id,
            re=re,
            wait=wait,
            timeout=timeout if timeout is not None else self._agent.command_timeout,
        )

    async def set_tools(
        self,
        enabled: Sequence[str] | None,
        *,
        id: str | None = None,
        re: int | None = _AUTO,
        wait: bool = True,
        timeout: float | None = None,
    ) -> CommandResult | asyncio.Future[CommandResult]:
        """Offer only the tools named in ``enabled`` (local and remote) to the LLM from the next
        response on; ``None`` offers all of them again."""
        return await self._command(
            lambda cid, r: protocol.tools_set(enabled, id=cid, re=r),
            id=id,
            re=re,
            wait=wait,
            timeout=timeout if timeout is not None else self._agent.command_timeout,
        )

    # -- internals ------------------------------------------------------------------------------

    def _attach(self, ws: ServerConnection) -> None:
        """Move the session onto a resumed link."""
        if self._park_timer is not None:
            self._park_timer.cancel()
            self._park_timer = None
        self._parked = False
        self._resuming = False
        self._ws = ws
        self.resumes += 1
        self._closed.clear()

    def _new_id(self) -> str:
        self._next_id += 1
        return f"c{self._next_id}"

    async def _send(self, message: Mapping[str, Any]) -> None:
        """Write one message, bounded by the agent's write timeout."""
        text = protocol.encode(message)
        if self.closed:
            raise SessionClosed("the link is closed")
        try:
            async with asyncio.timeout(self._agent.write_timeout):
                await self._ws.send(text)
        except ConnectionClosed as exc:
            raise SessionClosed(f"the link closed: {exc}") from exc
        except TimeoutError as exc:
            raise SessionClosed("a write timed out; the link is considered down") from exc

    async def _command(
        self,
        build: Callable[[str | None, int | None], dict[str, Any]],
        *,
        id: str | None,
        re: int | None,
        wait: bool,
        timeout: float,
    ) -> Any:
        if re is _AUTO:
            re = _current_re.get()
        # Without waiting there is nobody to read the result, but an id is still sent so a
        # failure is logged instead of lost.
        command_id = id if id is not None else self._new_id()
        if command_id in self._pending:
            raise ValueError(f"command id {command_id!r} is already waiting for its result")
        try:
            message = build(command_id, re)
        except ProtocolError as exc:
            raise CommandError(exc.code, str(exc)) from exc
        future: asyncio.Future[CommandResult] = asyncio.get_running_loop().create_future()
        self._pending[command_id] = _Pending(future)
        try:
            await self._send(message)
        except BaseException:
            self._pending.pop(command_id, None)
            raise
        if not wait:
            future.add_done_callback(_log_unwaited(message["type"], command_id))
            return future
        try:
            result = await asyncio.wait_for(future, timeout)
        except TimeoutError:
            self._pending.pop(command_id, None)
            raise CommandTimeout(command_id, timeout) from None
        if not result.ok:
            err = result.error
            raise CommandError(
                err.code if err else ErrorCode.BAD_MESSAGE, err.message if err else "failed"
            )
        return result

    def _resolve(self, result: CommandResult) -> None:
        pending = self._pending.pop(result.id, None)
        if pending is not None and not pending.future.done():
            pending.future.set_result(result)

    def _fail_pending(self, exc: BaseException) -> None:
        pending, self._pending = self._pending, {}
        for item in pending.values():
            if not item.future.done():
                item.future.set_exception(exc)

    def _finish(self, end: SessionEnd) -> None:
        """Queue the session end (once); when the queue is full the dispatcher or the link-close
        path queues it as soon as there is room. Nothing is queued behind it."""
        if self._end is not None:
            return
        self._end = end
        try:
            self._queue.put_nowait(end)
        except asyncio.QueueFull:
            self._end_pending = True

    def _enqueue(self, event: Event | CommandResult) -> None:
        if self._end is not None:
            return  # the call is over; nothing is delivered after session.end
        try:
            self._queue.put_nowait(event)
        except asyncio.QueueFull:
            # Overflow policy: drop the newest and count it (the server does the same). The first
            # drop of a gap remembers how many items were queued ahead of it.
            if self._lost == 0:
                self._gap_after = self._enqueued
            self._lost += 1
        else:
            self._enqueued += 1


def _log_unwaited(kind: str, command_id: str) -> Callable[[asyncio.Future[CommandResult]], None]:
    def done(future: asyncio.Future[CommandResult]) -> None:
        if future.cancelled():
            return
        exc = future.exception()
        if exc is not None:
            log.debug("command %s %s was not answered: %s", kind, command_id, exc)
            return
        result = future.result()
        if not result.ok and result.error is not None:
            log.warning(
                "command %s %s failed: %s: %s",
                kind,
                command_id,
                getattr(result.error.code, "value", result.error.code),
                result.error.message,
            )

    return done


Handler = Callable[[Session, Any], Awaitable[None] | None]


class Agent:
    """A brisvox agent: the application logic of a call.

    Declare tools and handlers with decorators, then ``agent.run()`` (blocking) or
    ``async with agent.serve(...)`` (embedding)::

        agent = Agent("luigi", system_prompt="You are Luigi's host.", greeting="Hi!")

        @agent.tool(kind="side_effect")
        async def book_table(time: str, party_size: int = 2) -> str:
            \"\"\"Book a table.\"\"\"
            ...

        @agent.on_user_text
        async def heard(session, event):
            ...

        agent.run(port=8765)

    Args:
        name: The agent's name (for logs only; the server names the definition itself).
        system_prompt: Replaces the agent definition's system prompt (at most 64 KiB).
        context: Messages the conversation starts with (at most 32).
        greeting: A line the bot speaks at once.
        events: ``"auto"`` (default) subscribes only to the event types you registered handlers
            for; ``None`` to every event; a sequence of types to exactly those. The
            always-delivered messages (``session.*``, ``tool.*``, ``command.result``,
            ``events.lost``, ``error``) are not subscribable.
        handshake_timeout: Budget in seconds for ``session.start`` to arrive and the answer to
            be written (every handshake write is bounded by it). The server gives up after 2 s
            from connecting, so keep it below that.
        command_timeout: Default wait for a ``command.result``, in seconds.
        write_timeout: Bound on one WebSocket write, in seconds.
        side_effect_grace: How long a ``side_effect`` tool may keep running after the link
            closed, in seconds, before it is cancelled. Tools see ``ctx.cancelled`` (reason
            ``"link_closed"``) at once; ``read_only`` tools are cancelled immediately.
        event_queue: Events queued per call for the handlers (newest dropped when full).
        max_sessions: Calls served at once; more are rejected with ``unavailable``.
        versions: Protocol versions to offer back (default: all this SDK speaks).
        resume: Opt in to reconnecting. The agent declares ``resume`` in ``session.accept``; when a
            call's link is lost the session is parked (``Session.data``, queued events and the
            handlers' view of the call are kept) and the server's reconnection with
            ``session.start.resume`` re-attaches it, with no greeting and no ``on_session_start``.
            Handle the gap in ``on_resumed``. Off by default: an agent that keeps no state across
            a lost link must not declare it.
        resume_window: How long a parked session waits for the server, in seconds (default 15,
            the server's own reconnect window). After it the call ends for the handlers
            (``on_session_end`` with reason ``link_closed``) and a later resumption is rejected
            with ``unknown_session``.
    """

    def __init__(
        self,
        name: str = "agent",
        *,
        system_prompt: str | None = None,
        context: Sequence[ContextMessage | Mapping[str, str] | tuple[str, str]] = (),
        greeting: str | None = None,
        events: Sequence[str] | None | Literal["auto"] = "auto",
        handshake_timeout: float = 1.5,
        command_timeout: float = 10.0,
        write_timeout: float = 10.0,
        side_effect_grace: float = 30.0,
        event_queue: int = 1024,
        max_sessions: int = 256,
        versions: Sequence[int] = protocol.SUPPORTED_VERSIONS,
        resume: bool = False,
        resume_window: float = 15.0,
    ) -> None:
        if event_queue < 1 or max_sessions < 1:
            raise ValueError("event_queue and max_sessions must be at least 1")
        if resume_window <= 0:
            raise ValueError("resume_window must be positive")
        unknown = [v for v in versions if v not in protocol.SUPPORTED_VERSIONS]
        if unknown or not versions:
            raise ValueError(f"this SDK speaks versions {protocol.SUPPORTED_VERSIONS}, not {list(versions)}")
        self.name = name
        self.system_prompt = system_prompt
        self.context = list(context)
        self.greeting = greeting
        self.events = events
        self.handshake_timeout = handshake_timeout
        self.command_timeout = command_timeout
        self.write_timeout = write_timeout
        self.side_effect_grace = side_effect_grace
        self.event_queue = event_queue
        self.max_sessions = max_sessions
        self.versions = tuple(versions)
        self.resume = resume
        self.resume_window = resume_window
        self._sessions: dict[str, Session] = {}
        self._draining = False
        self._tools: dict[str, Tool] = {}
        self._handlers: dict[str, list[Handler]] = {}
        self._start_handler: Callable[[Session], Awaitable[Accept | None] | Accept | None] | None = None
        self._active = 0
        self._background: set[asyncio.Task[Any]] = set()

    # -- declaration ----------------------------------------------------------------------------

    def tool(
        self,
        func: F | None = None,
        *,
        name: str | None = None,
        description: str | None = None,
        kind: Literal["read_only", "side_effect"] = "read_only",
        timeout_ms: int | None = None,
        filler_after_ms: int | None = None,
        confirm: bool = False,
        repeatable: bool = False,
    ) -> F | Callable[[F], F]:
        """Declare a tool (decorator, with or without arguments).

        The JSON schema comes from the type hints, the description from the docstring (or
        ``description=``). ``kind="side_effect"`` for anything that changes the world: it never
        runs speculatively and never twice for one call; use ``ctx.idempotency_key`` (declare a
        ``ctx: ToolContext`` parameter) against APIs that deduplicate. See
        :meth:`brisvox_agent.Tool.from_function` for the options.

        Raises:
            ValueError: duplicate or illegal name, more than 64 tools, bad timeout.
            brisvox_agent.schema.SchemaError: a parameter type has no JSON Schema.
        """

        def register(function: F) -> F:
            declared = Tool.from_function(
                function,
                name=name,
                description=description,
                kind=kind,
                timeout_ms=timeout_ms,
                filler_after_ms=filler_after_ms,
                confirm=confirm,
                repeatable=repeatable,
            )
            if declared.name in self._tools:
                raise ValueError(f"tool {declared.name!r} is declared twice")
            if len(self._tools) >= protocol.MAX_TOOLS:
                raise ValueError(f"at most {protocol.MAX_TOOLS} tools")
            self._tools[declared.name] = declared
            return function

        return register(func) if func is not None else register

    @property
    def tools(self) -> Mapping[str, Tool]:
        """The declared tools by name."""
        return dict(self._tools)

    def on(self, event: type[Event] | str) -> Callable[[F], F]:
        """Register an event handler by event class (``UserText``) or wire type
        (``"user.text"``). The handler is ``async def (session, event)`` (a plain function works
        too); handlers of one call run one at a time, in the order of the events. A handler that
        raises is logged and the call goes on."""
        kind = event if isinstance(event, str) else event.TYPE
        if kind not in _ALL_EVENT_TYPES:
            raise ValueError(f"unknown event type {kind!r}")

        def register(function: F) -> F:
            self._handlers.setdefault(kind, []).append(function)
            return function

        return register

    def on_session_start(self, func: F) -> F:
        """Register the handshake handler: ``async def (session) -> Accept | None``.

        It runs before ``session.accept`` is sent, inside the handshake budget. Return an
        :class:`Accept` to change the prompt, context, greeting, events or tools for this call,
        or raise :class:`Reject` to decline it. One handler per agent.
        """
        if self._start_handler is not None:
            raise ValueError("on_session_start is already registered")
        self._start_handler = func
        return func

    def on_session_end(self, func: F) -> F:
        """``async def (session, event: SessionEnd)``: the call is over. Runs exactly once per
        call, after all earlier events were handled; if the link dropped without a
        ``session.end`` the reason is ``"link_closed"``."""
        return self.on("session.end")(func)

    def on_session_usage(self, func: F) -> F:
        """``async def (session, event: SessionUsage)``: what the call used and how fast it was
        (``duration_ms``, ``turns``, ``latency``, ``usage`` with the LLM tokens, STT audio seconds
        and TTS characters), once, right before the session end. Units, not money. Always sent
        by the server (no subscription needed); not sent when the link was down at the end, and
        delivered like any other event (a handler that fell far behind can lose it, see
        :meth:`on_events_lost`)."""
        return self.on("session.usage")(func)

    def on_transcript(self, func: F) -> F:
        """``async def (session, event: UserTranscript)``: STT segments (interim and final)."""
        return self.on("user.transcript")(func)

    def on_user_text(self, func: F) -> F:
        """``async def (session, event: UserText)``: the whole user turn the LLM answers."""
        return self.on("user.text")(func)

    def on_bot_text(self, func: F) -> F:
        """``async def (session, event: BotText)``: streamed LLM deltas (a bot line is one)."""
        return self.on("bot.text")(func)

    def on_turn(self, func: F) -> F:
        """``async def (session, event)``: every ``turn.*`` event (user started/ended, bot
        started/ended, interrupted, metrics)."""
        for kind in _TURN_EVENTS:
            self.on(kind)(func)
        return func

    def on_resumed(self, func: F) -> F:
        """``async def (session, event: Resumed)``: the call's lost link was taken up again
        (``attempt``, ``previous_seq``, ``down_ms``). Runs after the events queued before the
        loss; the server then reports what it could not deliver as ``events.lost``. Needs
        ``Agent(resume=True)``."""
        return self.on("session.resumed")(func)

    def on_error(self, func: F) -> F:
        """``async def (session, event: ErrorEvent)``: a stage of the call reported an error."""
        return self.on("error")(func)

    def on_events_lost(self, func: F) -> F:
        """``async def (session, event: EventsLost)``: events were dropped, by the server or by
        this SDK's own queue."""
        return self.on("events.lost")(func)

    # -- serving --------------------------------------------------------------------------------

    def serve(
        self,
        host: str = "127.0.0.1",
        port: int = 8765,
        *,
        token: str | None | Literal["env"] = "env",
        ssl: ssl_module.SSLContext | None = None,
    ) -> "AgentServer":
        """The WebSocket server for the agent: ``async with agent.serve(...) as server``.

        ``token`` is the bearer token the brisvox server must present
        (``Authorization: Bearer <token>``); the default ``"env"`` reads
        ``BRISVOX_AGENT_TOKEN`` (unset or empty: no authentication, so bind a loopback address
        or a private network then). ``port=0`` picks a free port (``server.port``).
        """
        if token == "env":
            token = os.environ.get("BRISVOX_AGENT_TOKEN") or None
        return AgentServer(self, host, port, token=token, ssl=ssl)

    def run(
        self,
        host: str = "127.0.0.1",
        port: int = 8765,
        *,
        token: str | None | Literal["env"] = "env",
        ssl: ssl_module.SSLContext | None = None,
        on_ready: Callable[["AgentServer"], None] | None = None,
    ) -> None:
        """Serve until interrupted (Ctrl+C). Blocking; use :meth:`serve` inside an event loop.

        ``on_ready`` is called with the listening server (its ``url`` holds the bound port). The
        address it bound is logged at INFO, or printed to stderr when logging is not set up; with a
        loopback ``host`` (the default) the line says the agent is reachable only from this
        machine, and that a server in another container needs ``0.0.0.0`` or ``::``.
        """

        async def main() -> None:
            async with self.serve(host, port, token=token, ssl=ssl) as server:
                message = listening_message(self.name, server.host, server.url)
                if log.isEnabledFor(logging.INFO):
                    log.info("%s", message)
                else:
                    # Logging is not set up (the default shows warnings only): the address is
                    # what a deployer needs, so the blocking entry point says it anyway.
                    print(message, file=sys.stderr, flush=True)
                if on_ready is not None:
                    on_ready(server)
                await server.serve_forever()

        try:
            asyncio.run(main())
        except KeyboardInterrupt:
            pass

    # -- one call -------------------------------------------------------------------------------

    def _negotiate(self, offered: Iterable[int]) -> int | None:
        common = set(offered) & set(self.versions)
        return max(common) if common else None

    def _subscription(self) -> list[str] | None:
        if self.events == "auto":
            return [kind for kind in protocol.EVENT_TYPES if self._handlers.get(kind)]
        return None if self.events is None else list(self.events)

    async def handle(self, ws: ServerConnection) -> None:
        """Serve one connection (one call) to its end. :class:`AgentServer` calls this; call it
        yourself to embed the agent in another WebSocket server."""
        loop = asyncio.get_running_loop()
        deadline = loop.time() + self.handshake_timeout
        self._active += 1
        try:
            await self._serve_call(ws, deadline)
        finally:
            self._active -= 1

    async def _serve_call(self, ws: ServerConnection, deadline: float) -> None:
        try:
            session = await self._handshake(ws, deadline)
        except ConnectionClosed:
            return
        except Exception:
            log.exception("handshake failed")
            await _close(ws, 1011, "internal error")
            return
        if session is None:
            return
        if session._dispatcher is None:  # a resumed session keeps the dispatcher of its first link
            session._dispatcher = asyncio.create_task(
                self._dispatch(session), name=f"brisvox-dispatch-{session.id}"
            )
        try:
            await self._read(session)
        except ConnectionClosed:
            pass
        except Exception:
            log.exception("session %s: reader failed", session.id)
        finally:
            if session._ws is not ws:
                # A resumed link took the session over (this one was half-open): not ours any more.
                await _close(ws, 1000, "")
            else:
                # Nothing awaits between the loss and the decision to park, so a resumption
                # cannot slip in between.
                self._link_lost(session)
                if self._may_park(session):
                    self._park(session)
                else:
                    await self._finalize(session)
                await _close(ws, 1000, "")

    # -- reconnecting ---------------------------------------------------------------------------

    def _link_lost(self, session: Session) -> None:
        """The link is gone: commands fail, running tools hear it."""
        session._closed.set()
        session._fail_pending(SessionClosed("the link closed before the command was answered"))
        self._wind_down_tools(session)

    def _parked_count(self) -> int:
        """Parked calls (no connection of their own, so not part of ``_active``)."""
        return sum(1 for s in self._sessions.values() if s._parked and not s._resuming)

    def _may_park(self, session: Session) -> bool:
        """A lost link is only waited out for a resumable agent whose call did not end."""
        return self.resume and not self._draining and session._end is None

    def _spawn(self, coro: Awaitable[None], name: str) -> None:
        task = asyncio.ensure_future(coro)
        task.set_name(name)
        self._background.add(task)
        task.add_done_callback(self._background.discard)

    def _park(self, session: Session) -> None:
        session._parked = True
        session._park_deadline = asyncio.get_running_loop().time() + self.resume_window
        self._arm_park(session)
        log.info("session %s: link lost; waiting %.1fs for the server to resume", session.id, self.resume_window)

    def _arm_park(self, session: Session) -> None:
        loop = asyncio.get_running_loop()
        if session._park_timer is not None:  # never two timers: a stale one would end a later window
            session._park_timer.cancel()
            session._park_timer = None
        delay = max(0.0, session._park_deadline - loop.time())
        session._park_timer = loop.call_later(
            delay,
            lambda: self._spawn(self._expire(session), f"brisvox-expire-{session.id}"),
        )

    async def _expire(self, session: Session) -> None:
        """The window ran out (or the server stops): the call ends for the handlers."""
        if not session._parked or session._resuming:
            return
        session._parked = False
        session._park_timer = None
        log.info("session %s: not resumed in time; the call ends", session.id)
        await self._finalize(session)

    async def _expire_all(self) -> None:
        """Server shutdown: end every parked call now."""
        for session in list(self._sessions.values()):
            if session._parked:
                if session._park_timer is not None:
                    session._park_timer.cancel()
                    session._park_timer = None
                session._resuming = False
                await self._expire(session)

    async def _finalize(self, session: Session) -> None:
        """The call is over for the handlers: queue the end behind everything else, wait for the
        handlers to finish (bounded), and forget the session."""
        if self._sessions.get(session.id) is session:
            del self._sessions[session.id]
        dispatcher = session._dispatcher
        session._finish(SessionEnd(reason="link_closed"))  # no-op when the server's arrived
        end = session._end
        if dispatcher is None:
            return
        try:
            async with asyncio.timeout(5.0):
                # The end is queued last and waits for room: on_session_end runs exactly once,
                # even when the handler queue overflowed.
                if session._end_pending:
                    session._end_pending = False
                    await session._queue.put(end)
                await session._queue.put(None)
                await dispatcher
        except TimeoutError:
            log.warning("session %s: handlers did not finish in 5s; cancelled", session.id)
            dispatcher.cancel()
            await asyncio.gather(dispatcher, return_exceptions=True)
            if not session._end_started:
                # The handlers hogged the whole budget; the end still gets its turn.
                try:
                    async with asyncio.timeout(1.0):
                        await self._deliver(session, end)
                except TimeoutError:
                    log.warning("session %s: on_session_end did not finish in 1s", session.id)

    async def _resume(
        self,
        ws: ServerConnection,
        start: SessionStart,
        version: int,
        send: Callable[[dict[str, Any]], Awaitable[None]],
        write_deadline: Callable[[], float],
    ) -> Session | None:
        """Answer a ``session.start`` that resumes a call: re-attach the parked session."""
        resume = start.resume
        if resume is None:  # pragma: no cover - the caller checked
            return None
        session = None if self._draining else self._sessions.get(start.session_id)
        if session is None or session._end is not None or session._resuming:
            log.warning("session %s: resumption of a call this agent does not know", start.session_id)
            await send(protocol.session_reject(ErrorCode.UNKNOWN_SESSION, "no such call"))
            await _close(ws, 1000, "")
            return None
        # From here the expiry timer must not end the session under the answer being written.
        session._resuming = True
        message = protocol.session_accept(version, events=session._events_sub, resume=True)
        try:
            async with asyncio.timeout_at(write_deadline()):
                await ws.send(protocol.encode(message))
        except BaseException as exc:
            session._resuming = False
            if session._parked:
                self._arm_park(session)  # the same deadline: the window is not extended
            if isinstance(exc, TimeoutError):
                raise ConnectionError("session.accept write timed out") from None
            raise
        old = session._ws
        if not session.closed:
            # The server resumed while this side still holds the old link (a half-open one).
            self._link_lost(session)
            self._spawn(_close(old, 1000, "superseded by a resumed link"), "brisvox-close-old")
        seen_seq = session._last_seq  # what this side read on the lost link
        session._attach(ws)
        session._last_seq = start.seq
        session.version = version
        log.info(
            "session %s: resumed (attempt %d, down %d ms)",
            session.id, resume.attempt, resume.down_ms,
        )
        if self._handlers.get("session.resumed"):
            session._enqueue(
                Resumed(
                    seq=start.seq,
                    t_ms=start.t_ms,
                    attempt=resume.attempt,
                    previous_seq=resume.previous_seq,
                    seen_seq=seen_seq,
                    down_ms=resume.down_ms,
                )
            )
        return session

    async def _handshake(self, ws: ServerConnection, deadline: float) -> Session | None:
        try:
            async with asyncio.timeout_at(deadline):
                first = await ws.recv()
        except TimeoutError:
            log.warning("no session.start within %.1fs", self.handshake_timeout)
            await _close(ws, 1008, "no session.start")
            return None
        try:
            start = protocol.parse_server_message(first)  # type: ignore[arg-type]
        except ProtocolError as exc:
            log.warning("bad first message: %s", exc)
            await _close(ws, 1002, "expected session.start")
            return None
        if not isinstance(start, SessionStart) or start.protocol != protocol.PROTOCOL:
            log.warning("first message was not a %s session.start", protocol.PROTOCOL)
            await _close(ws, 1002, "expected session.start")
            return None

        loop = asyncio.get_running_loop()

        def write_deadline() -> float:
            # A handshake write is bounded by the handshake budget and by one write timeout.
            return min(deadline, loop.time() + self.write_timeout)

        async def send(message: dict[str, Any]) -> None:
            """A rejection: best effort. Past the budget the server has given up anyway."""
            try:
                async with asyncio.timeout_at(write_deadline()):
                    await ws.send(protocol.encode(message))
            except TimeoutError:
                log.debug("session %s: rejection not written within the handshake budget", start.session_id)

        version = self._negotiate(start.versions)
        if version is None:
            log.warning(
                "session %s offers versions %s, this agent speaks %s",
                start.session_id, list(start.versions), list(self.versions),
            )
            await send(
                protocol.session_reject(
                    ErrorCode.UNSUPPORTED_VERSION,
                    f"this agent speaks versions {list(self.versions)}",
                )
            )
            await _close(ws, 1000, "")
            return None
        if start.resume is not None:
            return await self._resume(ws, start, version, send, write_deadline)
        # Connected calls (this one included) plus the parked ones, which hold state too.
        if self._active + self._parked_count() > self.max_sessions:
            await send(protocol.session_reject(ErrorCode.UNAVAILABLE, "too many calls"))
            await _close(ws, 1000, "")
            return None

        session = Session(self, ws, start, version)
        accept = Accept()
        if self._start_handler is not None:
            try:
                async with asyncio.timeout_at(deadline):
                    returned = self._start_handler(session)
                    if inspect.isawaitable(returned):
                        returned = await returned
                if isinstance(returned, Accept):
                    accept = returned
                elif returned is not None:
                    raise TypeError("on_session_start must return an Accept or None")
            except Reject as rejection:
                await send(protocol.session_reject(rejection.code, rejection.message or None))
                await _close(ws, 1000, "")
                return None
            except TimeoutError:
                log.warning("session %s: on_session_start exceeded the handshake budget", start.session_id)
                await send(protocol.session_reject(ErrorCode.UNAVAILABLE, "handshake budget exceeded"))
                await _close(ws, 1000, "")
                return None
            except Exception:
                log.exception("session %s: on_session_start raised; rejecting", start.session_id)
                await send(protocol.session_reject(ErrorCode.UNAVAILABLE, "agent error"))
                await _close(ws, 1000, "")
                return None

        def pick(override: Any, default: Any) -> Any:
            return default if override is _UNSET else override

        names = pick(accept.tools, None)
        if names is None:
            tools = list(self._tools.values())
        else:
            missing = [n for n in names if n not in self._tools]
            if missing:
                raise ValueError(f"Accept.tools names undeclared tools: {missing}")
            tools = [self._tools[n] for n in names]
        events = pick(accept.events, self._subscription())
        message = protocol.session_accept(
            version,
            tools=[t.declaration() for t in tools],
            system_prompt=pick(accept.system_prompt, self.system_prompt),
            context=_wire_messages(pick(accept.context, self.context)),
            greeting=pick(accept.greeting, self.greeting),
            events=events,
            resume=self.resume,
        )
        try:
            async with asyncio.timeout_at(write_deadline()):
                await ws.send(protocol.encode(message))
        except TimeoutError:
            raise ConnectionError("session.accept write timed out") from None
        if self.resume:
            session._events_sub = None if events is None else list(events)
            self._sessions[session.id] = session
        return session

    async def _read(self, session: Session) -> None:
        async for raw in session._ws:
            if isinstance(raw, (bytes, bytearray)):
                log.warning("session %s: binary frame ignored", session.id)
                continue
            try:
                message = protocol.parse_server_message(raw)
            except ProtocolError as exc:
                log.warning("session %s: unreadable server message: %s", session.id, exc)
                continue
            session._last_seq = message.seq
            if isinstance(message, ToolCall):
                self._start_tool(session, message)
            elif isinstance(message, ToolCancel):
                self._cancel_tool(session, message)
            elif isinstance(message, CommandResult):
                session._resolve(message)
                if self._handlers.get("command.result"):
                    session._enqueue(message)
            elif isinstance(message, SessionStart):
                log.warning("session %s: second session.start ignored", session.id)
            elif isinstance(message, UnknownMessage):
                log.debug("session %s: unknown message type %r ignored", session.id, message.type)
            elif isinstance(message, Event):
                if isinstance(message, SessionEnd):
                    # Never dropped: kept aside and queued last when the link closes.
                    session.end_reason = message.reason
                    session._finish(message)
                elif self._handlers.get(message.TYPE):
                    session._enqueue(message)
                elif isinstance(message, protocol.ErrorEvent):
                    log.warning(
                        "session %s: server reported an error in %s: %s",
                        session.id, message.stage, message.message,
                    )

    async def _dispatch(self, session: Session) -> None:
        while True:
            event = await session._queue.get()
            if event is None:
                return
            # EventsLost goes in front of the next event that fits after the gap, i.e. once the
            # events queued ahead of the first drop were delivered (the server's rule too). The
            # session end is not counted: it is queued after everything else.
            if session._end_pending:
                # A slot was just freed and nothing else is queued behind the end.
                session._end_pending = False
                if session._end is not None:
                    session._queue.put_nowait(session._end)
            if session._lost and session._delivered >= session._gap_after:
                lost, session._lost = session._lost, 0
                log.warning("session %s: handlers fell behind, %d events dropped", session.id, lost)
                await self._deliver(session, EventsLost(count=lost))
            if event is session._end:
                session._end_started = True
            else:
                session._delivered += 1
            await self._deliver(session, event)

    async def _deliver(self, session: Session, event: Event | CommandResult) -> None:
        token = _current_re.set(event.seq or None)
        try:
            for handler in self._handlers.get(event.TYPE, ()):
                try:
                    returned = handler(session, event)
                    if inspect.isawaitable(returned):
                        await returned
                except SessionClosed:
                    log.debug("session %s: handler for %s hit a closed link", session.id, event.TYPE)
                except asyncio.CancelledError:
                    raise
                except Exception:
                    log.exception("session %s: handler for %s raised", session.id, event.TYPE)
        finally:
            _current_re.reset(token)

    # -- tools ----------------------------------------------------------------------------------

    def _start_tool(self, session: Session, call: ToolCall) -> None:
        ctx = ToolContext(
            session=session,
            call_id=call.call_id,
            request_id=call.id,
            name=call.name,
            kind=call.kind,
            epoch=call.epoch,
            idempotency_key=call.idempotency_key,
            timeout_ms=call.timeout_ms,
        )
        task = asyncio.create_task(
            self._run_tool(session, call, ctx), name=f"brisvox-tool-{call.name}"
        )
        # Registered before the task first runs, so a tool.cancel right behind the tool.call
        # finds it. A strong reference also keeps side_effect tasks alive past the link.
        session._tool_tasks[call.id] = (task, ctx)
        self._background.add(task)

        def finished(done: asyncio.Task[None]) -> None:
            self._background.discard(done)
            session._tool_tasks.pop(call.id, None)

        task.add_done_callback(finished)

    def _wind_down_tools(self, session: Session) -> None:
        """The link is gone: tell every running tool (``ctx.cancelled``, reason ``link_closed``),
        cancel the read_only ones, and bound the side_effect ones by ``side_effect_grace`` so a hung
        call cannot outlive the session forever."""
        loop = asyncio.get_running_loop()
        for task, ctx in list(session._tool_tasks.values()):
            if not ctx.cancelled.is_set():
                ctx.cancel_reason = "link_closed"
                ctx.cancelled.set()
            if ctx.kind == "read_only":
                task.cancel()
            else:
                handle = loop.call_later(self.side_effect_grace, task.cancel)
                task.add_done_callback(lambda _done, h=handle: h.cancel())

    def _cancel_tool(self, session: Session, cancel: ToolCancel) -> None:
        entry = session._tool_tasks.get(cancel.id)
        if entry is None:
            return
        task, ctx = entry
        ctx.cancel_reason = cancel.reason
        ctx.cancelled.set()
        if ctx.kind == "read_only":
            task.cancel()
        # side_effect: advice only. The tool finishes; its result becomes a late note.

    async def _run_tool(self, session: Session, call: ToolCall, ctx: ToolContext) -> None:
        tool = self._tools.get(call.name)
        if tool is None:
            await self._answer(
                session, protocol.tool_result(call.id, error=f"unknown tool {call.name!r}")
            )
            return
        try:
            value = await tool.run(call.arguments, ctx)
            reply = protocol.tool_result(call.id, result=to_json(value))
        except asyncio.CancelledError:
            if ctx.cancelled.is_set() or session.closed:
                log.debug("tool %s (%d) cancelled: %s", call.name, call.id, ctx.cancel_reason)
                return  # the server stopped waiting; a result would be ignored
            raise
        except ToolError as exc:
            reply = protocol.tool_result(call.id, error=str(exc) or "tool failed")
        except Exception as exc:
            log.exception("tool %s (%d) failed", call.name, call.id)
            reply = protocol.tool_result(call.id, error=f"tool failed: {type(exc).__name__}")
        await self._answer(session, reply)

    async def _answer(self, session: Session, reply: dict[str, Any]) -> None:
        try:
            await session._send(reply)
        except ProtocolError:
            await self._answer_quietly(
                session, protocol.tool_result(reply["id"], error="tool result is too large")
            )
        except ValueError:
            # Not strict JSON (NaN / Infinity slipped through): the server still gets an answer.
            await self._answer_quietly(
                session, protocol.tool_result(reply["id"], error="tool result is not valid JSON")
            )
        except SessionClosed:
            log.debug("session %s: tool result dropped, the link is closed", session.id)

    async def _answer_quietly(self, session: Session, reply: dict[str, Any]) -> None:
        try:
            await session._send(reply)
        except (SessionClosed, ProtocolError, ValueError):
            pass


_ALL_EVENT_TYPES = frozenset(
    {
        "session.end",
        "session.usage",
        "session.resumed",
        "error",
        "events.lost",
        "command.result",
        *protocol.EVENT_TYPES,
    }
)


async def _close(ws: ServerConnection, code: int, reason: str) -> None:
    try:
        async with asyncio.timeout(2.0):
            await ws.close(code, reason)
    except (TimeoutError, ConnectionClosed, OSError):
        pass


class AgentServer:
    """The listening side of an :class:`Agent` (``async with agent.serve(...)``).

    Rejects handshakes without the right bearer token with HTTP 401 before any WebSocket
    frame is exchanged.
    """

    def __init__(
        self,
        agent: Agent,
        host: str,
        port: int,
        *,
        token: str | None,
        ssl: ssl_module.SSLContext | None,
    ) -> None:
        self.agent = agent
        self.host = host
        self._port = port
        self._token = token
        self._ssl = ssl
        self._server: Server | None = None

    @property
    def port(self) -> int:
        """The bound port (useful with ``port=0``)."""
        if self._server is not None:
            for sock in self._server.sockets:
                return int(sock.getsockname()[1])
        return self._port

    @property
    def url(self) -> str:
        """The ``ws://`` (or ``wss://``) URL for ``BRISVOX_AGENT_URL``."""
        scheme = "wss" if self._ssl else "ws"
        host = f"[{self.host}]" if ":" in self.host else self.host
        return f"{scheme}://{host}:{self.port}"

    def _authorize(self, connection: ServerConnection, request: Any) -> Any:
        if self._token is None:
            return None
        header = request.headers.get("Authorization", "")
        scheme, _, presented = header.partition(" ")
        if scheme.lower() == "bearer" and hmac.compare_digest(
            presented.strip().encode(), self._token.encode()
        ):
            return None
        return connection.respond(HTTPStatus.UNAUTHORIZED, "unauthorized\n")

    async def start(self) -> "AgentServer":
        """Start listening."""
        self.agent._draining = False
        self._server = await serve(
            self.agent.handle,
            self.host,
            self._port,
            ssl=self._ssl,
            process_request=self._authorize,
            max_size=2 * protocol.MAX_MESSAGE_BYTES,
            compression=None,
            open_timeout=self.agent.handshake_timeout + 3.0,
            logger=logging.getLogger("brisvox_agent.websockets"),
        )
        return self

    async def serve_forever(self) -> None:
        """Run until cancelled."""
        if self._server is None:
            await self.start()
        assert self._server is not None
        await self._server.serve_forever()

    async def close(self) -> None:
        """Stop listening and close the live calls' links. Calls parked for a resumption end now."""
        if self._server is not None:
            self.agent._draining = True
            self._server.close()
            await self._server.wait_closed()
            self._server = None
            await self.agent._expire_all()

    async def __aenter__(self) -> "AgentServer":
        return await self.start()

    async def __aexit__(self, *exc: object) -> None:
        await self.close()
