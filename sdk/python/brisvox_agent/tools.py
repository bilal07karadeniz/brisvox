"""Typed remote tools: a Python function the LLM of the call may call.

A tool is declared with ``@agent.tool`` (see :class:`brisvox_agent.Agent`). Its JSON schema comes
from the function's type hints (:mod:`brisvox_agent.schema`); its arguments reach the function
already converted to the hinted types (enums, dataclasses, ``datetime``).

A function may ask for a :class:`ToolContext` by declaring a parameter annotated with it
(or named ``ctx``); it is not part of the schema.
"""

from __future__ import annotations

import asyncio
import dataclasses
import datetime
import enum
import inspect
import json
import math
import types
import typing
from collections.abc import Mapping, Sequence, Set
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Annotated, Any, Callable, Literal, Union, get_args, get_origin

from . import protocol, schema

if TYPE_CHECKING:
    from .agent import Session

__all__ = ["Tool", "ToolContext", "ToolError", "ToolArgumentError"]

ToolKind = Literal["read_only", "side_effect"]


class ToolError(Exception):
    """Raise from a tool to fail it with a message the LLM gets verbatim.

    Any other exception fails the tool too, but the LLM only learns the exception's type name
    (the details, which may be internal, go to the log).
    """


class ToolArgumentError(ToolError):
    """The arguments of a ``tool.call`` do not fit the function (the server validates them
    against the declared schema, so this is rare)."""


@dataclass(slots=True)
class ToolContext:
    """What a tool may want to know about the call it runs for.

    ``idempotency_key`` is stable for the tool call of the call (16 hex digits): pass it to APIs
    that deduplicate requests, so a ``side_effect`` tool never acts twice.
    """

    session: "Session"
    call_id: str
    request_id: int
    name: str
    kind: ToolKind
    epoch: int
    idempotency_key: str
    timeout_ms: int
    cancelled: asyncio.Event = field(default_factory=asyncio.Event)
    """Set when the server sent ``tool.cancel``."""
    cancel_reason: str | None = None
    """``interrupted``, ``timeout`` or ``abandoned`` once :attr:`cancelled` is set."""


class Tool:
    """A declared tool: its wire declaration and the function behind it.

    Build it with :meth:`from_function`; the agent keeps one per ``@agent.tool``.
    """

    __slots__ = (
        "name",
        "description",
        "parameters",
        "kind",
        "timeout_ms",
        "filler_after_ms",
        "confirm",
        "repeatable",
        "func",
        "_ctx_param",
        "_hints",
        "_signature",
    )

    def __init__(
        self,
        func: Callable[..., Any],
        *,
        name: str,
        description: str,
        parameters: dict[str, Any],
        kind: ToolKind,
        timeout_ms: int | None,
        filler_after_ms: int | None,
        ctx_param: str | None,
        confirm: bool = False,
        repeatable: bool = False,
    ) -> None:
        self.func = func
        self.name = name
        self.description = description
        self.parameters = parameters
        self.kind: ToolKind = kind
        self.timeout_ms = timeout_ms
        self.filler_after_ms = filler_after_ms
        self.confirm = confirm
        self.repeatable = repeatable
        self._ctx_param = ctx_param
        self._signature = inspect.signature(func)
        self._hints = typing.get_type_hints(func, include_extras=True)

    @classmethod
    def from_function(
        cls,
        func: Callable[..., Any],
        *,
        name: str | None = None,
        description: str | None = None,
        kind: ToolKind = "read_only",
        timeout_ms: int | None = None,
        filler_after_ms: int | None = None,
        confirm: bool = False,
        repeatable: bool = False,
    ) -> "Tool":
        """Declare ``func`` as a tool.

        Args:
            func: A plain or ``async`` function. Without ``description`` the first paragraph of
                its docstring is used.
            name: Defaults to the function's name.
            kind: ``read_only`` (may run speculatively, cancelled on barge-in or timeout) or
                ``side_effect`` (never speculative, never run twice for one call).
            timeout_ms: The server's wait for the result (default 5000, at most 30000).
            filler_after_ms: The bot says a filler phrase after this long without a result
                (default 700; 0 disables).
            confirm: Hold the tool for the user's confirmation (``side_effect`` tools only): the
                server does not call it when the model asks; it tells the model to ask the user
                and calls the tool only after the user's next reply confirms. Use it for a
                transfer, a payment, a booking.
            repeatable: Allow an identical call (same arguments) to run again in a later
                response of the same turn (``side_effect`` tools only). By default the server
                refuses such a repeat as ``already_done`` once the first one succeeded, since a
                model that read the result and asks again is repeating itself (a second
                booking). Set it for a tool where doing the same thing twice is legitimate,
                such as pressing the same IVR digit twice.

        Raises:
            ValueError: an illegal name, kind or timeout, or ``confirm`` or ``repeatable`` on a
                read-only tool.
            schema.SchemaError: a parameter has no JSON Schema.
        """
        tool_name = protocol.validate_tool_name(name or func.__name__)
        if kind not in ("read_only", "side_effect"):
            raise ValueError(f"tool kind must be 'read_only' or 'side_effect', not {kind!r}")
        if timeout_ms is not None and not 0 < timeout_ms <= protocol.MAX_TOOL_TIMEOUT_MS:
            raise ValueError(f"timeout_ms must be 1..{protocol.MAX_TOOL_TIMEOUT_MS}")
        if filler_after_ms is not None and filler_after_ms < 0:
            raise ValueError("filler_after_ms must be 0 or more")
        if confirm and kind != "side_effect":
            raise ValueError("confirm=True needs kind='side_effect' (a read-only tool changes nothing)")
        if repeatable and kind != "side_effect":
            raise ValueError(
                "repeatable=True needs kind='side_effect' (a read-only tool may always repeat)"
            )
        text = (description if description is not None else schema.describe_function(func)).strip()
        if not text:
            raise ValueError(
                f"tool {tool_name!r} needs a description (a docstring or description=...)"
            )
        ctx_param = _find_ctx_param(func)
        skip = frozenset({ctx_param}) if ctx_param else frozenset()
        parameters = schema.parameters_schema(func, skip=skip)
        return cls(
            func,
            name=tool_name,
            description=text,
            parameters=parameters,
            kind=kind,
            timeout_ms=timeout_ms,
            filler_after_ms=filler_after_ms,
            ctx_param=ctx_param,
            confirm=confirm,
            repeatable=repeatable,
        )

    def declaration(self) -> dict[str, Any]:
        """The entry of ``session.accept.tools``."""
        out: dict[str, Any] = {
            "name": self.name,
            "description": self.description,
            "parameters": self.parameters,
            "kind": self.kind,
        }
        if self.timeout_ms is not None:
            out["timeout_ms"] = self.timeout_ms
        if self.filler_after_ms is not None:
            out["filler_after_ms"] = self.filler_after_ms
        if self.confirm:
            out["confirm"] = True
        if self.repeatable:
            out["repeatable"] = True
        return out

    def bind(self, arguments: Mapping[str, Any], ctx: ToolContext) -> tuple[tuple[()], dict[str, Any]]:
        """Convert the JSON ``arguments`` to the hinted types and add the context.

        Raises:
            ToolArgumentError: a required argument is missing or a value does not convert.
        """
        kwargs: dict[str, Any] = {}
        for name, param in self._signature.parameters.items():
            if name == self._ctx_param:
                kwargs[name] = ctx
            elif name in arguments:
                try:
                    kwargs[name] = coerce(arguments[name], self._hints.get(name, Any))
                except (TypeError, ValueError, KeyError) as exc:
                    raise ToolArgumentError(f"argument {name!r}: {exc}") from exc
            elif param.default is inspect.Parameter.empty:
                raise ToolArgumentError(f"missing argument {name!r}")
        return (), kwargs

    async def run(self, arguments: Mapping[str, Any], ctx: ToolContext) -> Any:
        """Call the function (``async`` functions are awaited; plain functions run on the
        default executor thread, so a blocking tool never stalls the event loop)."""
        args, kwargs = self.bind(arguments, ctx)
        if inspect.iscoroutinefunction(self.func):
            return await self.func(*args, **kwargs)
        loop = asyncio.get_running_loop()
        import contextvars
        import functools

        run_ctx = contextvars.copy_context()
        result = await loop.run_in_executor(
            None, functools.partial(run_ctx.run, self.func, *args, **kwargs)
        )
        if inspect.isawaitable(result):
            return await result
        return result


def _find_ctx_param(func: Callable[..., Any]) -> str | None:
    try:
        hints = typing.get_type_hints(func, include_extras=True)
    except Exception:  # surfaced later by the schema step
        hints = {}
    for name in inspect.signature(func).parameters:
        hint = hints.get(name)
        if get_origin(hint) is Annotated:
            hint = get_args(hint)[0]
        if hint is ToolContext or name == "ctx":
            return name
    return None


def coerce(value: Any, hint: Any) -> Any:
    """Convert the JSON ``value`` to the type ``hint`` (the inverse of the schema mapping).

    Values the function can take as they are pass through; ``TypeError``/``ValueError`` signal
    a value that cannot be converted.
    """
    if hint is Any or hint is inspect.Parameter.empty:
        return value
    origin = get_origin(hint)
    args = get_args(hint)
    if origin is Annotated:
        return coerce(value, args[0])
    if origin is Union or origin is types.UnionType:
        if value is None and type(None) in args:
            return None
        errors: list[str] = []
        for member in args:
            if member is type(None):
                continue
            try:
                return coerce(value, member)
            except (TypeError, ValueError, KeyError) as exc:
                errors.append(str(exc))
        raise ValueError("; ".join(errors) or "no member of the union fits")
    if origin is Literal:
        if value not in args:
            raise ValueError(f"{value!r} is not one of {list(args)}")
        return value
    if hint is bool:
        if not isinstance(value, bool):
            raise TypeError(f"{value!r} is not a boolean")
        return value
    if hint is int:
        if isinstance(value, bool) or not isinstance(value, int):
            if isinstance(value, float) and value.is_integer():
                return int(value)
            raise TypeError(f"{value!r} is not an integer")
        return value
    if hint is float:
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise TypeError(f"{value!r} is not a number")
        return float(value)
    if hint is str:
        if not isinstance(value, str):
            raise TypeError(f"{value!r} is not a string")
        return value
    if hint is datetime.datetime:
        return datetime.datetime.fromisoformat(_str(value))
    if hint is datetime.date:
        return datetime.date.fromisoformat(_str(value))
    if isinstance(hint, type) and issubclass(hint, enum.Enum):
        return hint(value)
    if origin in (list, Sequence) or hint in (list, Sequence):
        _need_list(value)
        item = args[0] if args else Any
        return [coerce(v, item) for v in value]
    if origin in (set, frozenset, Set) or hint in (set, frozenset):
        _need_list(value)
        item = args[0] if args else Any
        container = frozenset if origin is frozenset or hint is frozenset else set
        return container(coerce(v, item) for v in value)
    if origin is tuple or hint is tuple:
        _need_list(value)
        if len(args) == 2 and args[1] is Ellipsis:
            return tuple(coerce(v, args[0]) for v in value)
        if args:
            if len(args) != len(value):
                raise ValueError(f"expected {len(args)} items, got {len(value)}")
            return tuple(coerce(v, a) for v, a in zip(value, args))
        return tuple(value)
    if origin in (dict, Mapping) or hint in (dict, Mapping):
        if not isinstance(value, dict):
            raise TypeError(f"{value!r} is not an object")
        if len(args) == 2:
            return {k: coerce(v, args[1]) for k, v in value.items()}
        return value
    if dataclasses.is_dataclass(hint) and isinstance(hint, type):
        if not isinstance(value, dict):
            raise TypeError(f"{value!r} is not an object")
        hints = typing.get_type_hints(hint, include_extras=True)
        init: dict[str, Any] = {}
        for f in dataclasses.fields(hint):
            if f.init and f.name in value:
                init[f.name] = coerce(value[f.name], hints.get(f.name, Any))
        return hint(**init)
    return value


def _str(value: Any) -> str:
    if not isinstance(value, str):
        raise TypeError(f"{value!r} is not a string")
    return value


def _need_list(value: Any) -> None:
    if not isinstance(value, list):
        raise TypeError(f"{value!r} is not an array")


def to_json(value: Any) -> Any:
    """Make a tool's return value JSON-serializable: dataclasses become objects, enums their
    values, ``datetime`` ISO strings, sets sorted lists. Strings stay strings (the LLM gets a
    string result as it is). Raises ``TypeError`` for anything else that is not JSON and
    ``ValueError`` for a NaN or infinite float (JSON has neither)."""
    if isinstance(value, float) and not math.isfinite(value):
        raise ValueError(f"a tool returned {value!r}, which is not valid JSON")
    if value is None or isinstance(value, (str, bool, int, float)):
        return value
    if isinstance(value, enum.Enum):
        return to_json(value.value)
    if isinstance(value, (datetime.datetime, datetime.date)):
        return value.isoformat()
    if dataclasses.is_dataclass(value) and not isinstance(value, type):
        return {f.name: to_json(getattr(value, f.name)) for f in dataclasses.fields(value)}
    if isinstance(value, Mapping):
        return {str(k): to_json(v) for k, v in value.items()}
    if isinstance(value, (set, frozenset)):
        return sorted((to_json(v) for v in value), key=lambda v: json.dumps(v, sort_keys=True))
    if isinstance(value, (list, tuple)):
        return [to_json(v) for v in value]
    raise TypeError(f"a tool returned {type(value).__name__}, which is not JSON")
