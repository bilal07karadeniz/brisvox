"""JSON Schema for tool parameters, derived from the type hints of a Python function.

Supported hints: ``str``, ``int``, ``float``, ``bool``, ``list[T]``/``Sequence[T]``/``set[T]``,
``tuple[T, ...]``, ``dict[str, T]``, ``T | None`` / ``Optional[T]``, ``Literal[...]``,
``enum.Enum`` subclasses, dataclasses (nested objects), ``datetime.date`` / ``datetime.datetime``
(strings in ISO 8601), ``Any``, and ``Annotated[T, "description"]`` (a string in the metadata
becomes the ``description``). A parameter without a default is ``required``; the value of a
default is copied into the schema when it is JSON-serializable.

Descriptions of parameters also come from an ``Args:`` section of a Google-style docstring.

The schema a tool declares must be an object schema without remote ``$ref``; nested dataclasses
are inlined, so the output never contains a ``$ref`` at all.
"""

from __future__ import annotations

import dataclasses
import datetime
import enum
import inspect
import re
import types
import typing
from collections.abc import Mapping, Sequence, Set
from typing import Annotated, Any, Callable, Literal, Union, get_args, get_origin, get_type_hints

__all__ = ["SchemaError", "schema_for_hint", "parameters_schema", "describe_function"]

_NONE = type(None)


class SchemaError(TypeError):
    """A type hint that cannot be expressed as a JSON Schema."""


def _describe(metadata: Sequence[Any]) -> str | None:
    for item in metadata:
        if isinstance(item, str):
            return item
    return None


def schema_for_hint(hint: Any, _seen: tuple[Any, ...] = ()) -> dict[str, Any]:
    """The JSON Schema of the type hint ``hint``.

    Raises:
        SchemaError: for a hint without a JSON image (arbitrary classes, callables, ...).
    """
    if hint is Any or hint is inspect.Parameter.empty:
        return {}
    origin = get_origin(hint)
    args = get_args(hint)

    if origin is Annotated:
        inner = schema_for_hint(args[0], _seen)
        description = _describe(args[1:])
        if description:
            inner = {**inner, "description": description}
        return inner

    if origin is Union or origin is types.UnionType:
        members = [a for a in args if a is not _NONE]
        schemas = [schema_for_hint(a, _seen) for a in members]
        if len(members) != len(args):
            schemas.append({"type": "null"})
        if len(schemas) == 1:
            return schemas[0]
        return {"anyOf": schemas}

    if origin is Literal:
        values = list(args)
        for v in values:
            if v is not None and not isinstance(v, (str, int, float, bool)):
                raise SchemaError(f"Literal value {v!r} is not a JSON scalar")
        return {"enum": values}

    if hint is str:
        return {"type": "string"}
    if hint is bool:
        return {"type": "boolean"}
    if hint is int:
        return {"type": "integer"}
    if hint is float:
        return {"type": "number"}
    if hint is _NONE or hint is None:
        return {"type": "null"}
    if hint is datetime.datetime:
        return {"type": "string", "format": "date-time"}
    if hint is datetime.date:
        return {"type": "string", "format": "date"}

    if isinstance(hint, type) and issubclass(hint, enum.Enum):
        values = [member.value for member in hint]
        return {"enum": values}

    if origin in (list, set, frozenset) or origin in (Sequence, Set, typing.Sequence):
        item = schema_for_hint(args[0], _seen) if args else {}
        out: dict[str, Any] = {"type": "array", "items": item}
        if origin in (set, frozenset, Set):
            out["uniqueItems"] = True
        return out
    if origin is tuple:
        if len(args) == 2 and args[1] is Ellipsis:
            return {"type": "array", "items": schema_for_hint(args[0], _seen)}
        if args:
            prefix = [schema_for_hint(a, _seen) for a in args]
            return {
                "type": "array",
                "prefixItems": prefix,
                "minItems": len(prefix),
                "maxItems": len(prefix),
            }
        return {"type": "array"}
    if hint in (list, set, frozenset, tuple, Sequence):
        return {"type": "array"}
    if origin in (dict, Mapping) or hint in (dict, Mapping):
        if args and args[0] is not str:
            raise SchemaError("only dict[str, T] has a JSON Schema")
        out = {"type": "object"}
        if len(args) == 2:
            value = schema_for_hint(args[1], _seen)
            out["additionalProperties"] = value if value else True
        return out

    if dataclasses.is_dataclass(hint) and isinstance(hint, type):
        if hint in _seen:
            raise SchemaError(f"recursive dataclass {hint.__name__} has no inline schema")
        return _object_schema(
            {f.name: f for f in dataclasses.fields(hint)},
            get_type_hints(hint, include_extras=True),
            _seen + (hint,),
            docs=_arg_docs(hint.__doc__ or ""),
        )

    raise SchemaError(f"no JSON Schema for the type hint {hint!r}")


def _object_schema(
    fields: Mapping[str, dataclasses.Field[Any]],
    hints: Mapping[str, Any],
    seen: tuple[Any, ...],
    docs: Mapping[str, str],
) -> dict[str, Any]:
    properties: dict[str, Any] = {}
    required: list[str] = []
    for name, f in fields.items():
        if not f.init:
            continue
        prop = schema_for_hint(hints.get(name, Any), seen)
        if name in docs and "description" not in prop:
            prop = {**prop, "description": docs[name]}
        if f.default is not dataclasses.MISSING:
            _set_default(prop, f.default)
        elif f.default_factory is dataclasses.MISSING:
            required.append(name)
        properties[name] = prop
    out: dict[str, Any] = {"type": "object", "properties": properties}
    if required:
        out["required"] = required
    return out


def _set_default(prop: dict[str, Any], default: Any) -> None:
    if isinstance(default, enum.Enum):
        default = default.value
    if isinstance(default, (str, int, float, bool)) or default is None:
        prop["default"] = default


_ARGS_HEADER = re.compile(r"^\s*(Args|Arguments|Parameters|Attributes)\s*:\s*$")
_ARG_LINE = re.compile(r"^(\s+)(\*{0,2}\w+)\s*(?:\([^)]*\))?\s*:\s*(.*)$")


def _arg_docs(doc: str) -> dict[str, str]:
    """``{name: description}`` from the ``Args:`` section of a Google-style docstring."""
    out: dict[str, str] = {}
    lines = inspect.cleandoc(doc).splitlines()
    in_args = False
    indent = 0
    current: str | None = None
    for line in lines:
        if _ARGS_HEADER.match(line):
            in_args, current = True, None
            continue
        if not in_args:
            continue
        if not line.strip():
            current = None
            continue
        match = _ARG_LINE.match(line)
        if match and (current is None or len(match.group(1)) <= indent):
            indent = len(match.group(1))
            current = match.group(2).lstrip("*")
            out[current] = match.group(3).strip()
        elif current is not None and len(line) - len(line.lstrip()) > indent:
            out[current] = (out[current] + " " + line.strip()).strip()
        elif not line.startswith((" ", "\t")):
            in_args = False  # a new unindented section ends the Args block
    return out


def describe_function(func: Callable[..., Any]) -> str:
    """The first paragraph of the docstring of ``func`` (what the LLM is told the tool does)."""
    doc = inspect.getdoc(func) or ""
    first = doc.split("\n\n", 1)[0]
    if _ARGS_HEADER.match(first.splitlines()[0] if first else ""):
        return ""
    return " ".join(first.split())


def parameters_schema(
    func: Callable[..., Any], *, skip: frozenset[str] = frozenset()
) -> dict[str, Any]:
    """The ``parameters`` object schema of ``func`` from its signature and type hints.

    ``skip`` names parameters that are not tool arguments (such as the context parameter).

    Raises:
        SchemaError: a parameter has no JSON image, or is ``*args``/``**kwargs``/positional-only.
    """
    signature = inspect.signature(func)
    try:
        hints = get_type_hints(func, include_extras=True)
    except Exception as exc:  # unresolvable forward reference
        raise SchemaError(f"cannot resolve the type hints of {func.__name__}: {exc}") from exc
    docs = _arg_docs(inspect.getdoc(func) or "")
    properties: dict[str, Any] = {}
    required: list[str] = []
    for name, param in signature.parameters.items():
        if name in skip:
            continue
        if param.kind in (param.VAR_POSITIONAL, param.VAR_KEYWORD, param.POSITIONAL_ONLY):
            raise SchemaError(
                f"{func.__name__}: parameter {name!r} cannot be a tool argument "
                "(use plain or keyword-only parameters)"
            )
        try:
            prop = schema_for_hint(hints.get(name, Any))
        except SchemaError as exc:
            raise SchemaError(f"{func.__name__}({name}): {exc}") from exc
        if name in docs and "description" not in prop:
            prop = {**prop, "description": docs[name]}
        if param.default is inspect.Parameter.empty:
            required.append(name)
        else:
            _set_default(prop, param.default)
        properties[name] = prop
    out: dict[str, Any] = {"type": "object", "properties": properties}
    if required:
        out["required"] = required
    return out
