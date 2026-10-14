"""JSON Schema from type hints, argument conversion and the tool declaration."""

from __future__ import annotations

import asyncio
import datetime
import enum
import json
import unittest
from dataclasses import dataclass, field
from typing import Annotated, Any, Literal, Optional

from brisvox_agent import SchemaError, Tool, ToolArgumentError, ToolContext
from brisvox_agent.schema import describe_function, parameters_schema, schema_for_hint
from brisvox_agent.tools import coerce, to_json


class Color(enum.Enum):
    RED = "red"
    BLUE = "blue"


@dataclass
class Guest:
    """A guest.

    Attributes:
        name: Full name.
    """

    name: str
    age: int | None = None
    tags: list[str] = field(default_factory=list)


class Schemas(unittest.TestCase):
    def test_scalars(self) -> None:
        self.assertEqual(schema_for_hint(str), {"type": "string"})
        self.assertEqual(schema_for_hint(int), {"type": "integer"})
        self.assertEqual(schema_for_hint(float), {"type": "number"})
        self.assertEqual(schema_for_hint(bool), {"type": "boolean"})
        self.assertEqual(schema_for_hint(Any), {})

    def test_containers_and_unions(self) -> None:
        self.assertEqual(schema_for_hint(list[int]), {"type": "array", "items": {"type": "integer"}})
        self.assertEqual(
            schema_for_hint(dict[str, float]),
            {"type": "object", "additionalProperties": {"type": "number"}},
        )
        self.assertEqual(
            schema_for_hint(str | None), {"anyOf": [{"type": "string"}, {"type": "null"}]}
        )
        self.assertEqual(
            schema_for_hint(Optional[int]), {"anyOf": [{"type": "integer"}, {"type": "null"}]}
        )
        self.assertEqual(
            schema_for_hint(tuple[int, ...]), {"type": "array", "items": {"type": "integer"}}
        )
        self.assertEqual(schema_for_hint(set[str])["uniqueItems"], True)

    def test_literal_enum_dates_annotated(self) -> None:
        self.assertEqual(schema_for_hint(Literal["a", "b"]), {"enum": ["a", "b"]})
        self.assertEqual(schema_for_hint(Color), {"enum": ["red", "blue"]})
        self.assertEqual(schema_for_hint(datetime.date), {"type": "string", "format": "date"})
        self.assertEqual(
            schema_for_hint(datetime.datetime), {"type": "string", "format": "date-time"}
        )
        self.assertEqual(
            schema_for_hint(Annotated[int, "party size"]),
            {"type": "integer", "description": "party size"},
        )

    def test_dataclass_is_inlined_without_refs(self) -> None:
        schema = schema_for_hint(Guest)
        self.assertEqual(schema["required"], ["name"])
        self.assertEqual(schema["properties"]["name"]["description"], "Full name.")
        self.assertEqual(schema["properties"]["tags"], {"type": "array", "items": {"type": "string"}})
        self.assertNotIn("$ref", json.dumps(schema))

    def test_unsupported_hints_are_refused(self) -> None:
        class Plain:
            pass

        for hint in (Plain, dict[int, str]):
            with self.assertRaises(SchemaError):
                schema_for_hint(hint)

    def test_parameters_from_a_signature(self) -> None:
        def book_table(
            time: str,
            party_size: int = 2,
            seat: Literal["bar", "table"] = "table",
            notes: str | None = None,
        ) -> str:
            """Book a table.

            More detail the LLM does not need.

            Args:
                time: When, as HH:MM.
                party_size: How many people
                    (continued line).
            """
            return ""

        schema = parameters_schema(book_table)
        self.assertEqual(schema["type"], "object")
        self.assertEqual(schema["required"], ["time"])
        props = schema["properties"]
        self.assertEqual(props["time"], {"type": "string", "description": "When, as HH:MM."})
        self.assertEqual(
            props["party_size"],
            {"type": "integer", "description": "How many people (continued line).", "default": 2},
        )
        self.assertEqual(props["seat"]["default"], "table")
        self.assertIsNone(props["notes"]["default"])
        self.assertEqual(describe_function(book_table), "Book a table.")

    def test_variadics_are_refused(self) -> None:
        def bad(*args: int) -> None: ...

        with self.assertRaises(SchemaError):
            parameters_schema(bad)


class Conversion(unittest.TestCase):
    def test_enum_dataclass_and_dates(self) -> None:
        self.assertIs(coerce("red", Color), Color.RED)
        guest = coerce({"name": "Ann", "tags": ["a"], "extra": 1}, Guest)
        self.assertEqual(guest, Guest("Ann", None, ["a"]))
        self.assertEqual(coerce("2026-10-13", datetime.date), datetime.date(2026, 10, 13))
        self.assertEqual(
            coerce("2026-10-13T12:30:00", datetime.datetime), datetime.datetime(2026, 10, 13, 12, 30)
        )

    def test_numbers_and_bools_are_strict(self) -> None:
        self.assertEqual(coerce(3.0, int), 3)  # JSON 3.0 is a whole number
        self.assertEqual(coerce(3, float), 3.0)
        for value, hint in ((True, int), (1, bool), ("1", int), (2.5, int), (1, str)):
            with self.assertRaises((TypeError, ValueError)):
                coerce(value, hint)

    def test_optional_and_literal_and_containers(self) -> None:
        self.assertIsNone(coerce(None, str | None))
        self.assertEqual(coerce("x", str | None), "x")
        self.assertEqual(coerce("a", Literal["a", "b"]), "a")
        with self.assertRaises(ValueError):
            coerce("c", Literal["a", "b"])
        self.assertEqual(coerce([1, 2], list[int]), [1, 2])
        self.assertEqual(coerce([1, 2], set[int]), {1, 2})
        self.assertEqual(coerce([1, "x"], tuple[int, str]), (1, "x"))
        self.assertEqual(coerce({"a": 1}, dict[str, int]), {"a": 1})
        with self.assertRaises(TypeError):
            coerce([1, "x"], list[int])

    def test_results_become_json(self) -> None:
        self.assertEqual(to_json("text"), "text")
        self.assertEqual(to_json(Guest("A", 3)), {"name": "A", "age": 3, "tags": []})
        self.assertEqual(to_json({"c": Color.RED, "d": datetime.date(2026, 1, 2)}), {"c": "red", "d": "2026-01-02"})
        self.assertEqual(to_json({3, 1, 2}), [1, 2, 3])
        with self.assertRaises(TypeError):
            to_json(object())


class Declaration(unittest.TestCase):
    def test_declaration_is_the_wire_shape(self) -> None:
        def lookup(city: str, ctx: ToolContext) -> str:
            """Look up the weather."""
            return city

        tool = Tool.from_function(lookup, kind="side_effect", timeout_ms=4000, filler_after_ms=0)
        self.assertEqual(
            tool.declaration(),
            {
                "name": "lookup",
                "description": "Look up the weather.",
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"],
                },
                "kind": "side_effect",
                "timeout_ms": 4000,
                "filler_after_ms": 0,
            },
        )

    def test_confirm_is_declared_only_when_set_and_needs_a_side_effect_tool(self) -> None:
        def transfer(number: str) -> str:
            """Transfer the call."""
            return number

        held = Tool.from_function(transfer, kind="side_effect", confirm=True).declaration()
        self.assertIs(held["confirm"], True)
        plain = Tool.from_function(transfer, kind="side_effect").declaration()
        self.assertNotIn("confirm", plain)
        with self.assertRaises(ValueError):
            Tool.from_function(transfer, confirm=True)  # read_only by default

    def test_repeatable_is_declared_only_when_set_and_needs_a_side_effect_tool(self) -> None:
        def press(digit: str) -> str:
            """Press a key."""
            return digit

        again = Tool.from_function(press, kind="side_effect", repeatable=True)
        self.assertIs(again.repeatable, True)
        self.assertIs(again.declaration()["repeatable"], True)
        plain = Tool.from_function(press, kind="side_effect")
        self.assertIs(plain.repeatable, False)
        self.assertNotIn("repeatable", plain.declaration())
        with self.assertRaises(ValueError):
            Tool.from_function(press, repeatable=True)  # read_only by default
        with self.assertRaises(ValueError):
            Tool.from_function(press, kind="read_only", repeatable=True)

    def test_rules_of_the_doc_hold_at_declaration_time(self) -> None:
        def ok() -> str:
            """Doc."""
            return ""

        def nodoc() -> str:
            return ""

        Tool.from_function(ok, name="a-b_9")
        for kwargs in (
            {"name": "has space"},
            {"name": "x" * 65},
            {"kind": "write"},
            {"timeout_ms": 30_001},
            {"timeout_ms": 0},
            {"filler_after_ms": -1},
        ):
            with self.subTest(**kwargs), self.assertRaises(ValueError):
                Tool.from_function(ok, **kwargs)  # type: ignore[arg-type]
        with self.assertRaises(ValueError):
            Tool.from_function(nodoc)
        Tool.from_function(nodoc, description="Described by argument.")

    def test_run_converts_arguments_and_injects_the_context(self) -> None:
        async def go() -> None:
            seen: dict[str, Any] = {}

            async def paint(color: Color, guest: Guest, ctx: ToolContext) -> str:
                """Paint."""
                seen.update(color=color, guest=guest, ctx=ctx)
                return "done"

            tool = Tool.from_function(paint)
            self.assertNotIn("ctx", tool.parameters["properties"])
            ctx = _ctx()
            result = await tool.run({"color": "blue", "guest": {"name": "Z"}}, ctx)
            self.assertEqual(result, "done")
            self.assertIs(seen["color"], Color.BLUE)
            self.assertEqual(seen["guest"], Guest("Z"))
            self.assertIs(seen["ctx"], ctx)
            with self.assertRaises(ToolArgumentError):
                await tool.run({"color": "green", "guest": {"name": "Z"}}, ctx)
            with self.assertRaises(ToolArgumentError):
                await tool.run({"color": "red"}, ctx)

        asyncio.run(go())

    def test_blocking_functions_do_not_stall_the_event_loop(self) -> None:
        import time

        async def go() -> None:
            def slow() -> str:
                """Blocks."""
                time.sleep(0.2)
                return "ok"

            tool = Tool.from_function(slow)
            ticks = 0

            async def ticker() -> None:
                nonlocal ticks
                while True:
                    await asyncio.sleep(0.01)
                    ticks += 1

            task = asyncio.create_task(ticker())
            result = await tool.run({}, _ctx())
            task.cancel()
            self.assertEqual(result, "ok")
            self.assertGreater(ticks, 5)

        asyncio.run(go())


def _ctx() -> ToolContext:
    return ToolContext(
        session=None,  # type: ignore[arg-type]
        call_id="c",
        request_id=1,
        name="t",
        kind="read_only",
        epoch=1,
        idempotency_key="0" * 16,
        timeout_ms=5000,
    )


if __name__ == "__main__":
    unittest.main()
