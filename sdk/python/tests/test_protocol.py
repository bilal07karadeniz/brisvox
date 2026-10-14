"""Message (de)serialization against the examples and tables of docs/AGENT_PROTOCOL.md."""

from __future__ import annotations

import json
import re
import unittest
from pathlib import Path

from brisvox_agent import protocol
from brisvox_agent.protocol import (
    BotText,
    CommandResult,
    ErrorCode,
    ErrorEvent,
    EventsLost,
    Metrics,
    ProtocolError,
    SessionEnd,
    SessionStart,
    ToolCall,
    ToolCancel,
    TurnMetrics,
    UnknownMessage,
    UserTranscript,
    parse_server_message,
)

DOC = Path(__file__).resolve().parents[3] / "docs" / "AGENT_PROTOCOL.md"
DOC_TEXT = DOC.read_text(encoding="utf-8") if DOC.exists() else ""

# The two JSON examples of the doc ("Version negotiation and handshake").
DOC_SESSION_START = """{"seq":1,"t_ms":0,"type":"session.start","protocol":"brisvox.agent","versions":[1],
 "session_id":"42","agent":"luigi","capabilities":{"transfer":true,"tools":true}}"""
DOC_SESSION_ACCEPT = """{"type":"session.accept","version":1,
 "system_prompt":"You are Luigi's host.",
 "context":[{"role":"system","text":"The caller is a regular."}],
 "greeting":"Hi, Luigi's here. How can I help?",
 "events":["user.text","turn.bot_ended"],
 "tools":[{"name":"book_table","description":"Books a table.",
           "parameters":{"type":"object","properties":{"time":{"type":"string"}},"required":["time"]},
           "kind":"side_effect","timeout_ms":4000}]}"""


class DocExamples(unittest.TestCase):
    def test_session_start_parses(self) -> None:
        message = parse_server_message(DOC_SESSION_START)
        self.assertIsInstance(message, SessionStart)
        assert isinstance(message, SessionStart)
        self.assertEqual((message.seq, message.t_ms), (1, 0))
        self.assertEqual(message.protocol, protocol.PROTOCOL)
        self.assertEqual(message.versions, (1,))
        self.assertEqual((message.session_id, message.agent), ("42", "luigi"))
        self.assertTrue(message.capabilities.transfer)
        self.assertTrue(message.capabilities.tools)

    def test_session_start_without_call_has_no_identity(self) -> None:
        message = parse_server_message(DOC_SESSION_START)
        assert isinstance(message, SessionStart)
        self.assertIsNone(message.call)

    def test_session_start_parses_the_caller_identity(self) -> None:
        body = {
            "seq": 1,
            "t_ms": 0,
            "type": "session.start",
            "protocol": "brisvox.agent",
            "versions": [1],
            "session_id": "42",
            "agent": "luigi",
            "capabilities": {"transfer": True, "tools": True},
        }
        full = parse_server_message(
            {
                **body,
                "call": {
                    "carrier": "twilio",
                    "carrier_call_id": "CA1",
                    "from": "+15551230001",
                    "to": "+15551230002",
                },
            }
        )
        assert isinstance(full, SessionStart)
        self.assertEqual(
            full.call,
            protocol.CallIdentity(
                carrier="twilio",
                carrier_call_id="CA1",
                from_number="+15551230001",
                to_number="+15551230002",
            ),
        )
        assert full.call is not None
        self.assertFalse(full.call.pii_redacted)
        # Numbers withheld by the server's PII redaction.
        redacted = parse_server_message(
            {
                **body,
                "call": {"carrier": "sip", "carrier_call_id": "abc@h", "pii_redacted": True},
            }
        )
        assert isinstance(redacted, SessionStart)
        self.assertEqual(
            redacted.call,
            protocol.CallIdentity(carrier="sip", carrier_call_id="abc@h", pii_redacted=True),
        )
        assert redacted.call is not None
        self.assertEqual((redacted.call.from_number, redacted.call.to_number), (None, None))
        # The identity is optional: a malformed one is ignored (and logged at debug), the call
        # goes on without it.
        for bad in ("yes", {"carrier": "twilio"}, {"carrier": 1, "carrier_call_id": "x"}):
            with self.subTest(call=bad):
                with self.assertLogs("brisvox_agent", level="DEBUG"):
                    message = parse_server_message({**body, "call": bad})
                self.assertIsNone(message.call)
        # The numbers stay out of repr() (and so out of a log line).
        assert full.call is not None
        self.assertNotIn("1555", repr(full.call))
        self.assertIn("CA1", repr(full.call))

    def test_session_accept_matches_the_doc_example(self) -> None:
        built = protocol.session_accept(
            1,
            system_prompt="You are Luigi's host.",
            context=[{"role": "system", "text": "The caller is a regular."}],
            greeting="Hi, Luigi's here. How can I help?",
            events=["user.text", "turn.bot_ended"],
            tools=[
                {
                    "name": "book_table",
                    "description": "Books a table.",
                    "parameters": {
                        "type": "object",
                        "properties": {"time": {"type": "string"}},
                        "required": ["time"],
                    },
                    "kind": "side_effect",
                    "timeout_ms": 4000,
                }
            ],
        )
        self.assertEqual(built, json.loads(DOC_SESSION_ACCEPT))
        self.assertEqual(json.loads(protocol.encode(built)), built)

    @unittest.skipUnless(DOC_TEXT, "docs/AGENT_PROTOCOL.md is not next to the SDK")
    def test_the_docs_own_json_blocks_are_what_this_test_hardcodes(self) -> None:
        """Guards the two hardcoded examples above against drifting from the doc."""
        block = re.search(r"```json\n(.*?)```", DOC_TEXT, re.S)
        assert block is not None
        start, accept = block.group(1).strip().split("\n\n")
        self.assertEqual(json.loads(start), json.loads(DOC_SESSION_START))
        self.assertEqual(json.loads(accept), json.loads(DOC_SESSION_ACCEPT))

    @unittest.skipUnless(DOC_TEXT, "docs/AGENT_PROTOCOL.md is not next to the SDK")
    def test_error_codes_are_exactly_the_docs_table(self) -> None:
        section = DOC_TEXT.split("## Error codes", 1)[1].split("\n## ", 1)[0]
        documented = set(re.findall(r"^\| `([a-z_]+)` \|", section, re.M))
        self.assertEqual(documented, {code.value for code in ErrorCode})

    @unittest.skipUnless(DOC_TEXT, "docs/AGENT_PROTOCOL.md is not next to the SDK")
    def test_event_and_command_names_are_the_docs_tables(self) -> None:
        events = DOC_TEXT.split("### Events", 1)[1].split("### Tool calls", 1)[0]
        documented = set(re.findall(r"^\| `([a-z_.]+)` \|", events, re.M)) - {"type"}
        subscribable = documented - {"error", "events.lost", "session.end"}
        self.assertEqual(subscribable, set(protocol.EVENT_TYPES))
        # Every documented server message type parses into a typed class, none into Unknown.
        for kind in documented:
            message = parse_server_message(_sample(kind))
            self.assertNotIsInstance(message, UnknownMessage, kind)
            self.assertEqual(message.TYPE, kind)

    @unittest.skipUnless(DOC_TEXT, "docs/AGENT_PROTOCOL.md is not next to the SDK")
    def test_limits_match_the_docs(self) -> None:
        self.assertIn("**64 KiB**", DOC_TEXT)
        self.assertEqual(protocol.MAX_MESSAGE_BYTES, 64 * 1024)
        self.assertIn("at most 4 KiB", DOC_TEXT)
        self.assertEqual(protocol.MAX_LINE_BYTES, 4096)
        self.assertIn("At most 64.", DOC_TEXT)
        self.assertEqual(protocol.MAX_TOOLS, 64)
        self.assertIn("at most 32", DOC_TEXT)
        self.assertEqual(protocol.MAX_CONTEXT_MESSAGES, 32)
        self.assertIn("at most 30 000", DOC_TEXT)
        self.assertEqual(protocol.MAX_TOOL_TIMEOUT_MS, 30_000)


def _sample(kind: str) -> str:
    """A well-formed server message of ``kind`` with the doc's fields."""
    extra: dict[str, object] = {
        "session.start": {**{"protocol": "brisvox.agent", "versions": [1], "session_id": "1",
                             "agent": "a"}, "capabilities": {"transfer": True, "tools": True}},
        "session.end": {"reason": "hangup"},
        "user.transcript": {"text": "hi", "final": False},
        "user.text": {"epoch": 1, "text": "hi"},
        "bot.text": {"epoch": 1, "text": "hi"},
        "bot.tts_text": {"epoch": 1, "text": "hi"},
        "turn.metrics": {"epoch": 1, "metrics": {"response_ms": 400.5}},
        "error": {"stage": "llm", "message": "boom", "fatal": False},
        "events.lost": {"count": 3},
        "tool.call": {"id": 1, "call_id": "c", "name": "t", "arguments": {}, "kind": "read_only",
                      "epoch": 1, "idempotency_key": "00ff00ff00ff00ff", "timeout_ms": 5000},
        "tool.cancel": {"id": 1, "reason": "timeout"},
        "command.result": {"id": "x", "ok": True},
    }.get(kind, {"epoch": 1})
    return json.dumps({"seq": 7, "t_ms": 99, "type": kind, **extra})  # type: ignore[arg-type]


class ServerMessages(unittest.TestCase):
    def test_envelope_and_fields(self) -> None:
        message = parse_server_message(_sample("user.text"))
        self.assertEqual((message.seq, message.t_ms), (7, 99))
        self.assertEqual(message.text, "hi")  # type: ignore[attr-defined]

    def test_transcript_final_flag(self) -> None:
        message = parse_server_message(_sample("user.transcript"))
        assert isinstance(message, UserTranscript)
        self.assertFalse(message.final)

    def test_metrics_absent_values_are_none(self) -> None:
        message = parse_server_message(_sample("turn.metrics"))
        assert isinstance(message, TurnMetrics)
        self.assertEqual(message.metrics, Metrics(response_ms=400.5))
        bare = parse_server_message('{"type":"turn.metrics","epoch":2,"metrics":{}}')
        assert isinstance(bare, TurnMetrics)
        self.assertEqual(bare.metrics, Metrics())

    def test_unknown_fields_are_ignored(self) -> None:
        message = parse_server_message('{"seq":2,"t_ms":1,"type":"bot.text","epoch":1,"text":"a","new":[1]}')
        assert isinstance(message, BotText)
        self.assertEqual(message.text, "a")

    def test_unknown_type_is_kept_not_fatal(self) -> None:
        message = parse_server_message('{"seq":2,"t_ms":1,"type":"future.thing","x":1}')
        assert isinstance(message, UnknownMessage)
        self.assertEqual(message.type, "future.thing")
        self.assertEqual(message.raw["x"], 1)

    def test_tool_call(self) -> None:
        message = parse_server_message(_sample("tool.call"))
        assert isinstance(message, ToolCall)
        self.assertEqual((message.id, message.name, message.kind), (1, "t", "read_only"))
        self.assertEqual(message.idempotency_key, "00ff00ff00ff00ff")

    def test_tool_cancel_reasons(self) -> None:
        for reason in ("interrupted", "timeout", "abandoned"):
            message = parse_server_message(
                json.dumps({"seq": 1, "t_ms": 1, "type": "tool.cancel", "id": 9, "reason": reason})
            )
            assert isinstance(message, ToolCancel)
            self.assertEqual(message.reason, reason)

    def test_command_result_error_codes(self) -> None:
        text = ('{"seq":3,"t_ms":4,"type":"command.result","id":"s1","ok":false,'
                '"error":{"code":"queue_full","message":"full"}}')
        message = parse_server_message(text)
        assert isinstance(message, CommandResult)
        self.assertFalse(message.ok)
        assert message.error is not None
        self.assertIs(message.error.code, ErrorCode.QUEUE_FULL)
        # A code added within version 1 survives as a string.
        odd = parse_server_message(text.replace("queue_full", "brand_new"))
        assert isinstance(odd, CommandResult) and odd.error is not None
        self.assertEqual(odd.error.code, "brand_new")

    def test_error_event_and_events_lost_and_end(self) -> None:
        self.assertIsInstance(parse_server_message(_sample("error")), ErrorEvent)
        lost = parse_server_message(_sample("events.lost"))
        assert isinstance(lost, EventsLost)
        self.assertEqual(lost.count, 3)
        end = parse_server_message(_sample("session.end"))
        assert isinstance(end, SessionEnd)
        self.assertEqual(end.reason, "hangup")

    def test_malformed_messages_raise_bad_message(self) -> None:
        for text in (
            "not json",
            "[]",
            '"x"',
            "{}",
            '{"type":5}',
            '{"type":"bot.text","epoch":1}',  # missing text
            '{"type":"bot.text","epoch":"1","text":"a"}',  # wrong type
            '{"type":"bot.text","epoch":true,"text":"a"}',  # bool is not an int
            '{"type":"user.transcript","text":"a"}',  # missing final
            '{"type":"tool.call","id":1,"call_id":"c","name":"t","arguments":[],"kind":"read_only",'
            '"epoch":1,"idempotency_key":"k","timeout_ms":1}',  # arguments must be an object
            '{"type":"tool.call","id":1,"call_id":"c","name":"t","arguments":{},"kind":"weird",'
            '"epoch":1,"idempotency_key":"k","timeout_ms":1}',
        ):
            with self.subTest(text=text):
                with self.assertRaises(ProtocolError) as caught:
                    parse_server_message(text)
                self.assertIs(caught.exception.code, ErrorCode.BAD_MESSAGE)

    def test_binary_frames_are_refused(self) -> None:
        with self.assertRaises(ProtocolError):
            parse_server_message(b'{"type":"session.end","reason":"x"}')


class AgentMessages(unittest.TestCase):
    def test_say(self) -> None:
        self.assertEqual(protocol.say("hello"), {"type": "say", "text": "hello"})
        self.assertEqual(
            protocol.say("hello", interrupt=True, id="s1", re=12),
            {"type": "say", "id": "s1", "re": 12, "text": "hello", "interrupt": True},
        )

    def test_end(self) -> None:
        self.assertEqual(protocol.end(), {"type": "end"})
        self.assertEqual(
            protocol.end(say="Bye", reason="done", id="e1"),
            {"type": "end", "id": "e1", "say": "Bye", "reason": "done"},
        )

    def test_transfer(self) -> None:
        self.assertEqual(
            protocol.transfer("+4930123456", mode="warm", summary="s", reason="r", id="t1"),
            {"type": "transfer", "id": "t1", "destination": "+4930123456", "mode": "warm",
             "summary": "s", "reason": "r"},
        )

    def test_context_update_and_tools_set(self) -> None:
        self.assertEqual(
            protocol.context_update(system_prompt="p", append=[{"role": "user", "text": "u"}], id="c"),
            {"type": "context.update", "id": "c", "system_prompt": "p",
             "append": [{"role": "user", "text": "u"}]},
        )
        self.assertEqual(
            protocol.tools_set(["a"], id="t"), {"type": "tools.set", "id": "t", "enabled": ["a"]}
        )
        # null means "all tools again" and must be on the wire, not omitted.
        self.assertEqual(protocol.tools_set(None), {"type": "tools.set", "enabled": None})

    def test_tool_result(self) -> None:
        self.assertEqual(protocol.tool_result(3, result="ok"), {"type": "tool.result", "id": 3, "result": "ok"})
        self.assertEqual(protocol.tool_result(3, result={"a": 1}), {"type": "tool.result", "id": 3, "result": {"a": 1}})
        self.assertEqual(protocol.tool_result(3, error="bad"), {"type": "tool.result", "id": 3, "error": "bad"})

    def test_session_reject(self) -> None:
        self.assertEqual(
            protocol.session_reject(ErrorCode.UNSUPPORTED_VERSION, "no"),
            {"type": "session.reject", "code": "unsupported_version", "message": "no"},
        )

    def test_rules_of_the_doc_are_checked_before_sending(self) -> None:
        cases = [
            (lambda: protocol.say(""), ErrorCode.INVALID_ARGUMENT),
            (lambda: protocol.say("x" * 4097), ErrorCode.INVALID_ARGUMENT),
            (lambda: protocol.say("é" * 2049), ErrorCode.INVALID_ARGUMENT),  # bytes, not chars
            (lambda: protocol.say("x", id="i" * 65), ErrorCode.INVALID_ARGUMENT),
            (lambda: protocol.transfer("0049123456"), ErrorCode.INVALID_ARGUMENT),
            (lambda: protocol.transfer("+1234567"), ErrorCode.INVALID_ARGUMENT),  # 7 digits
            (lambda: protocol.transfer("+1234567890123456"), ErrorCode.INVALID_ARGUMENT),  # 16
            (lambda: protocol.transfer("+49301234", summary="s" * 513), ErrorCode.INVALID_ARGUMENT),
            (lambda: protocol.transfer("+49301234", mode="hot"), ErrorCode.INVALID_ARGUMENT),  # type: ignore[arg-type]
            (lambda: protocol.end(reason="r" * 513), ErrorCode.INVALID_ARGUMENT),
            (lambda: protocol.context_update(append=[{"role": "robot", "text": "x"}]), ErrorCode.INVALID_ARGUMENT),
            (lambda: protocol.context_update(append=[{"role": "user", "text": " "}]), ErrorCode.INVALID_ARGUMENT),
            (lambda: protocol.context_update(append=[{"role": "user", "text": "x"}] * 33), ErrorCode.TOO_LARGE),
            (lambda: protocol.session_accept(1, tools=[{}] * 65), ErrorCode.TOO_LARGE),
            (lambda: protocol.encode({"type": "x", "p": "a" * 70_000}), ErrorCode.TOO_LARGE),
        ]
        for build, code in cases:
            with self.subTest(code=code):
                with self.assertRaises(ProtocolError) as caught:
                    build()
                self.assertIs(caught.exception.code, code)

    def test_boundaries_are_accepted(self) -> None:
        protocol.say("x" * 4096)
        protocol.transfer("+12345678")
        protocol.transfer("+123456789012345")
        protocol.say("x", id="i" * 64)
        protocol.context_update(append=[{"role": "user", "text": "x"}] * 32)

    def test_tool_names(self) -> None:
        protocol.validate_tool_name("book_table-2")
        for bad in ("", "a b", "é", "x" * 65, "a.b"):
            with self.assertRaises(ValueError):
                protocol.validate_tool_name(bad)

    def test_nan_cannot_reach_the_wire(self) -> None:
        with self.assertRaises(ValueError):
            protocol.encode(protocol.tool_result(1, result=float("nan")))


if __name__ == "__main__":
    unittest.main()
