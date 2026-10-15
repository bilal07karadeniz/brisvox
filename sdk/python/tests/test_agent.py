"""The Agent against a fake brisvox server that replays the sequences of
docs/AGENT_PROTOCOL.md over a real WebSocket on 127.0.0.1."""

from __future__ import annotations

import asyncio
import contextlib
import io
import json
import logging
import unittest
from typing import Any

from websockets.asyncio.client import connect
from websockets.exceptions import InvalidStatus

from brisvox_agent import (
    Accept,
    Agent,
    CommandError,
    CommandTimeout,
    ErrorCode,
    EventsLost,
    Reject,
    SessionClosed,
    SessionEnd,
    ToolContext,
    ToolError,
    UserText,
)
from fake_server import START_BODY, FakeServer

logging.getLogger("brisvox_agent").setLevel(logging.CRITICAL)


class StartupMessageTests(unittest.TestCase):
    """The address ``Agent.run`` announces, and the hint on a loopback bind."""

    def test_loopback_binds_are_recognised(self) -> None:
        from brisvox_agent.agent import is_loopback_host

        for host in ("127.0.0.1", "127.1.2.3", "::1", "[::1]", "localhost", "::ffff:127.0.0.1"):
            self.assertTrue(is_loopback_host(host), host)
        for host in ("0.0.0.0", "::", "192.168.1.5", "10.0.0.2", "agent.internal", "", "fd00::1"):
            self.assertFalse(is_loopback_host(host), host)

    def test_the_line_names_the_url_and_hints_only_on_loopback(self) -> None:
        from brisvox_agent.agent import listening_message

        local = listening_message("luigi", "127.0.0.1", "ws://127.0.0.1:8765")
        self.assertIn("'luigi' listening on ws://127.0.0.1:8765", local)
        self.assertIn("reachable only from this machine", local)
        self.assertIn("0.0.0.0 or ::", local)
        open_ = listening_message("luigi", "::", "ws://[::]:8765")
        self.assertIn("listening on ws://[::]:8765", open_)
        self.assertNotIn("loopback", open_)

    def _run_once(self, host: str | None) -> None:
        def stop(_server: Any) -> None:
            raise KeyboardInterrupt

        agent = Agent("luigi", system_prompt="Hi.")
        if host is None:
            agent.run(port=0, token=None, on_ready=stop)
        else:
            agent.run(host, 0, token=None, on_ready=stop)

    def test_run_prints_the_address_when_logging_is_not_set_up(self) -> None:
        # The module sets the logger to CRITICAL above: INFO is not enabled, like a script that
        # never configured logging.
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            self._run_once(None)
        self.assertIn("listening on ws://127.0.0.1:", err.getvalue())
        self.assertIn("reachable only from this machine", err.getvalue())

    def test_run_logs_the_address_when_info_is_enabled_and_prints_nothing(self) -> None:
        err = io.StringIO()
        with self.assertLogs("brisvox_agent", level="INFO") as logs, contextlib.redirect_stderr(err):
            self._run_once("0.0.0.0")
        self.assertTrue(any("listening on ws://0.0.0.0:" in line for line in logs.output))
        self.assertFalse(any("loopback" in line for line in logs.output))
        self.assertEqual(err.getvalue(), "")


class AgentCase(unittest.IsolatedAsyncioTestCase):
    """Starts ``self.agent`` on a free port per test and connects fake servers to it."""

    agent: Agent
    token: str | None = None

    def make_agent(self) -> Agent:
        return Agent("luigi", system_prompt="You are Luigi's host.", greeting="Hi!")

    async def asyncSetUp(self) -> None:
        self.agent = self.make_agent()
        self.setup_agent(self.agent)
        self.server = self.agent.serve("127.0.0.1", 0, token=self.token)
        await self.server.start()
        self.addAsyncCleanup(self.server.close)
        self.fakes: list[FakeServer] = []

    def setup_agent(self, agent: Agent) -> None:
        """Declare tools and handlers of the test."""

    async def call(self, **start: Any) -> tuple[FakeServer, dict[str, Any]]:
        fake = await FakeServer.connect(self.server.url, token=self.token)
        self.fakes.append(fake)
        self.addAsyncCleanup(fake.close)
        answer = await fake.start(**start)
        return fake, answer

    async def settle(self, times: int = 5) -> None:
        for _ in range(times):
            await asyncio.sleep(0.01)


class Handshake(AgentCase):
    def setup_agent(self, agent: Agent) -> None:
        @agent.tool(kind="side_effect", timeout_ms=4000)
        async def book_table(time: str) -> str:
            """Books a table."""
            return "booked"

        @agent.on_user_text
        async def heard(session, event) -> None:  # noqa: ANN001
            pass

        @agent.on_turn
        async def turn(session, event) -> None:  # noqa: ANN001
            pass

    async def test_accept_carries_prompt_greeting_tools_and_auto_events(self) -> None:
        _, accept = await self.call()
        self.assertEqual(accept["type"], "session.accept")
        self.assertEqual(accept["version"], 1)
        self.assertEqual(accept["system_prompt"], "You are Luigi's host.")
        self.assertEqual(accept["greeting"], "Hi!")
        self.assertEqual(
            accept["tools"],
            [
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
        # Only the types with a handler: user.text and the six turn.* events.
        self.assertEqual(
            sorted(accept["events"]),
            sorted(
                [
                    "user.text",
                    "turn.user_started",
                    "turn.user_ended",
                    "turn.bot_started",
                    "turn.bot_ended",
                    "turn.interrupted",
                    "turn.metrics",
                ]
            ),
        )

    async def test_the_highest_common_version_is_picked(self) -> None:
        _, accept = await self.call(versions=[3, 2, 1])
        self.assertEqual(accept["type"], "session.accept")
        self.assertEqual(accept["version"], 1)

    async def test_no_common_version_is_rejected_with_unsupported_version(self) -> None:
        fake, answer = await self.call(versions=[2, 3])
        self.assertEqual(answer["type"], "session.reject")
        self.assertEqual(answer["code"], "unsupported_version")
        self.assertTrue(answer["message"])
        with self.assertRaises(Exception):  # the agent closes the link after rejecting
            await fake.recv()

    async def test_a_wrong_protocol_or_first_message_closes_the_link(self) -> None:
        fake = await FakeServer.connect(self.server.url)
        await fake.send({"type": "turn.user_started", "epoch": 1})
        with self.assertRaises(Exception):
            await fake.recv()
        other = await FakeServer.connect(self.server.url)
        await other.send({"type": "session.start", "protocol": "other", "versions": [1],
                          "session_id": "1", "agent": "a", "capabilities": {}})
        with self.assertRaises(Exception):
            await other.recv()


class NoHandlers(AgentCase):
    async def test_no_handlers_means_no_event_subscription(self) -> None:
        _, accept = await self.call()
        self.assertEqual(accept["events"], [])
        self.assertEqual(accept["tools"], [])


class CallIdentitySession(AgentCase):
    """``session.call``: who is on the line, when the server says."""

    def setup_agent(self, agent: Agent) -> None:
        self.seen: list[Any] = []

        @agent.on_session_start
        async def start(session) -> None:  # noqa: ANN001
            self.seen.append(session.call)

    async def test_absent_unless_the_server_sends_it(self) -> None:
        await self.call()
        self.assertEqual(self.seen, [None])

    async def test_the_session_exposes_the_identity(self) -> None:
        await self.call(
            call={
                "carrier": "telnyx",
                "carrier_call_id": "v3:abc",
                "from": "+15551230001",
                "to": "+15551230002",
            }
        )
        (identity,) = self.seen
        self.assertEqual(identity.carrier, "telnyx")
        self.assertEqual(identity.carrier_call_id, "v3:abc")
        self.assertEqual(identity.from_number, "+15551230001")
        self.assertEqual(identity.to_number, "+15551230002")
        self.assertFalse(identity.pii_redacted)


class StartHandler(AgentCase):
    def setup_agent(self, agent: Agent) -> None:
        @agent.tool
        def alpha() -> str:
            """A."""
            return "a"

        @agent.tool
        def beta() -> str:
            """B."""
            return "b"

        @agent.on_session_start
        async def start(session) -> Accept | None:  # noqa: ANN001
            if session.id == "nope":
                raise Reject(ErrorCode.UNAVAILABLE, "closed today")
            if session.id == "boom":
                raise RuntimeError("bug")
            if session.id == "slow":
                await asyncio.sleep(10)
            if session.id == "vip":
                return Accept(
                    system_prompt="VIP prompt",
                    greeting=None,
                    context=[("system", "regular"), {"role": "user", "text": "hi"}],
                    tools=["beta"],
                    events=["user.text"],
                )
            return None

    async def test_defaults_without_overrides(self) -> None:
        _, accept = await self.call()
        self.assertEqual([t["name"] for t in accept["tools"]], ["alpha", "beta"])
        self.assertEqual(accept["system_prompt"], "You are Luigi's host.")

    async def test_accept_overrides_per_call(self) -> None:
        _, accept = await self.call(session_id="vip")
        self.assertEqual(accept["system_prompt"], "VIP prompt")
        self.assertNotIn("greeting", accept)
        self.assertEqual(
            accept["context"],
            [{"role": "system", "text": "regular"}, {"role": "user", "text": "hi"}],
        )
        self.assertEqual([t["name"] for t in accept["tools"]], ["beta"])
        self.assertEqual(accept["events"], ["user.text"])

    async def test_reject_sends_session_reject(self) -> None:
        _, answer = await self.call(session_id="nope")
        self.assertEqual(
            answer, {"type": "session.reject", "code": "unavailable", "message": "closed today"}
        )

    async def test_a_raising_handler_rejects_instead_of_hanging(self) -> None:
        _, answer = await self.call(session_id="boom")
        self.assertEqual(answer["type"], "session.reject")
        self.assertEqual(answer["code"], "unavailable")

    async def test_a_slow_handler_is_cut_at_the_handshake_budget(self) -> None:
        self.agent.handshake_timeout = 0.3
        started = asyncio.get_running_loop().time()
        _, answer = await self.call(session_id="slow")
        self.assertEqual(answer["type"], "session.reject")
        self.assertLess(asyncio.get_running_loop().time() - started, 2.0)


class Events(AgentCase):
    def setup_agent(self, agent: Agent) -> None:
        self.seen: list[Any] = []
        self.ended = asyncio.Event()
        self.slow_gate = asyncio.Event()

        @agent.on_user_text
        async def heard(session, event: UserText) -> None:  # noqa: ANN001
            self.seen.append(("user", event.text, event.seq))
            if event.text == "slow":
                await self.slow_gate.wait()
            if event.text == "hours":
                await session.say("We open at noon.")  # `re` is taken from the event

        @agent.on_bot_text
        async def bot(session, event) -> None:  # noqa: ANN001
            self.seen.append(("bot", event.text))

        @agent.on_transcript
        def transcript(session, event) -> None:  # noqa: ANN001  (a plain function works too)
            self.seen.append(("transcript", event.text, event.final))

        @agent.on_error
        async def error(session, event) -> None:  # noqa: ANN001
            self.seen.append(("error", event.stage))
            raise RuntimeError("a failing handler must not kill the call")

        @agent.on_events_lost
        async def lost(session, event: EventsLost) -> None:  # noqa: ANN001
            self.seen.append(("lost", event.count))

        @agent.on_session_usage
        async def usage(session, event) -> None:  # noqa: ANN001
            self.seen.append(("usage", event.turns, event.usage.llm.input_tokens))

        @agent.on_session_end
        async def end(session, event: SessionEnd) -> None:  # noqa: ANN001
            self.seen.append(("end", event.reason, session.end_reason))
            self.ended.set()

    async def test_events_arrive_in_order_and_a_failing_handler_is_survived(self) -> None:
        fake, _ = await self.call()
        await fake.send({"type": "user.transcript", "text": "hel", "final": False})
        await fake.send({"type": "error", "stage": "llm", "message": "x", "fatal": False})
        await fake.send({"type": "bot.text", "epoch": 1, "text": "Sure."})
        await fake.send({"type": "user.text", "epoch": 1, "text": "hello"})
        await fake.send({"type": "session.end", "reason": "hangup"})
        await asyncio.wait_for(self.ended.wait(), 5)
        self.assertEqual(
            [s[0] for s in self.seen], ["transcript", "error", "bot", "user", "end"]
        )
        self.assertEqual(self.seen[0], ("transcript", "hel", False))
        self.assertEqual(self.seen[-1], ("end", "hangup", "hangup"))

    async def test_session_usage_is_delivered_before_the_session_end_without_a_subscription(
        self,
    ) -> None:
        fake, accept = await self.call()
        # `session.usage` is always sent: it is not a subscribable event.
        self.assertNotIn("session.usage", accept["events"])
        await fake.send({
            "type": "session.usage", "duration_ms": 5000, "turns": 2, "interrupted_turns": 0,
            "latency": {}, "usage": {"llm": {"provider": "openai", "model": "m", "input_tokens": 77,
                                             "cached_input_tokens": 0, "output_tokens": 3,
                                             "responses_without_usage": 0}},
        })
        await fake.send({"type": "session.end", "reason": "hangup"})
        await asyncio.wait_for(self.ended.wait(), 5)
        self.assertEqual([s[0] for s in self.seen], ["usage", "end"])
        self.assertEqual(self.seen[0], ("usage", 2, 77))

    async def test_say_reacts_to_the_event_with_re_and_waits_for_the_result(self) -> None:
        fake, _ = await self.call()
        seq = await fake.send({"type": "user.text", "epoch": 1, "text": "hours"})
        command = await fake.recv_type("say")
        self.assertEqual(command["text"], "We open at noon.")
        self.assertEqual(command["re"], seq)
        self.assertIn("id", command)
        self.assertNotIn("interrupt", command)
        await fake.ok(command["id"])

    async def test_a_slow_handler_does_not_block_command_results_or_tools(self) -> None:
        # The first event parks its handler; the reader must keep serving the link.
        @self.agent.tool
        async def ping() -> str:
            """Ping."""
            return "pong"

        fake, _ = await self.call()
        await fake.send({"type": "user.text", "epoch": 1, "text": "slow"})
        await fake.send({"type": "tool.call", "id": 5, "call_id": "c", "name": "ping", "arguments": {},
                         "kind": "read_only", "epoch": 1, "idempotency_key": "ab" * 8, "timeout_ms": 5000})
        reply = await fake.recv_type("tool.result")
        self.assertEqual(reply, {"type": "tool.result", "id": 5, "result": "pong"})
        self.slow_gate.set()

    async def test_the_link_dropping_still_ends_the_session_once(self) -> None:
        fake, _ = await self.call()
        await fake.close()
        await asyncio.wait_for(self.ended.wait(), 5)
        self.assertEqual(self.seen[-1], ("end", "link_closed", None))
        self.assertEqual([s[0] for s in self.seen].count("end"), 1)

    async def test_a_full_handler_queue_drops_the_newest_and_reports_events_lost(self) -> None:
        self.agent.event_queue = 2
        fake, _ = await self.call()
        await fake.send({"type": "user.text", "epoch": 1, "text": "slow"})  # parks the dispatcher
        await self.settle()
        for n in range(5):  # 2 fit, 3 are dropped
            await fake.send({"type": "bot.text", "epoch": 1, "text": f"b{n}"})
        await self.settle()
        self.slow_gate.set()
        await fake.send({"type": "session.end", "reason": "hangup"})
        await asyncio.wait_for(self.ended.wait(), 5)
        names = [s[0] for s in self.seen]
        self.assertIn(("lost", 3), self.seen)
        self.assertEqual(names.count("bot"), 2)

    async def test_session_end_is_never_dropped_by_a_full_handler_queue(self) -> None:
        self.agent.event_queue = 2
        fake, _ = await self.call()
        await fake.send({"type": "user.text", "epoch": 1, "text": "slow"})  # parks the dispatcher
        await self.settle()
        for n in range(5):  # 2 fit, 3 are dropped
            await fake.send({"type": "user.text", "epoch": 1, "text": f"u{n}"})
        await fake.send({"type": "session.end", "reason": "hangup"})
        await self.settle()
        self.assertEqual([s[0] for s in self.seen], ["user"])  # the handler is still parked
        self.slow_gate.set()
        await asyncio.wait_for(self.ended.wait(), 5)
        self.assertEqual(self.seen[-1], ("end", "hangup", "hangup"))
        self.assertEqual([s[0] for s in self.seen].count("end"), 1)

    async def test_events_lost_lands_where_the_gap_is(self) -> None:
        self.agent.event_queue = 2
        fake, _ = await self.call()
        await fake.send({"type": "user.text", "epoch": 1, "text": "slow"})  # parks the dispatcher
        await self.settle()
        for n in range(5):  # u0 and u1 fit, u2..u4 are dropped
            await fake.send({"type": "user.text", "epoch": 1, "text": f"u{n}"})
        await self.settle()
        self.slow_gate.set()
        await fake.send({"type": "session.end", "reason": "hangup"})
        await asyncio.wait_for(self.ended.wait(), 5)
        self.assertEqual(
            [s[:2] for s in self.seen],
            [("user", "slow"), ("user", "u0"), ("user", "u1"), ("lost", 3), ("end", "hangup")],
        )

    async def test_events_lost_precedes_the_next_event_that_fits_after_the_gap(self) -> None:
        self.agent.event_queue = 2
        fake, _ = await self.call()
        await fake.send({"type": "user.text", "epoch": 1, "text": "slow"})
        await self.settle()
        for n in range(4):  # u0, u1 fit; u2, u3 dropped
            await fake.send({"type": "user.text", "epoch": 1, "text": f"u{n}"})
        await self.settle()
        self.slow_gate.set()
        await self.settle(10)  # drained: room again
        await fake.send({"type": "user.text", "epoch": 1, "text": "after"})
        await fake.send({"type": "session.end", "reason": "hangup"})
        await asyncio.wait_for(self.ended.wait(), 5)
        self.assertEqual(
            [s[:2] for s in self.seen],
            [("user", "slow"), ("user", "u0"), ("user", "u1"), ("lost", 2), ("user", "after"),
             ("end", "hangup")],
        )

    async def test_unknown_types_and_malformed_messages_do_not_break_the_link(self) -> None:
        fake, _ = await self.call()
        await fake.send({"type": "future.thing", "x": 1})
        await fake.ws.send("not json")
        await fake.ws.send(b"\x00\x01")
        await fake.send({"type": "bot.text", "epoch": 1})  # missing text
        await fake.send({"type": "bot.text", "epoch": 1, "text": "still here"})
        await fake.send({"type": "session.end", "reason": "hangup"})
        await asyncio.wait_for(self.ended.wait(), 5)
        self.assertEqual([s for s in self.seen if s[0] == "bot"], [("bot", "still here")])


class Commands(AgentCase):
    def setup_agent(self, agent: Agent) -> None:
        self.sessions: asyncio.Queue[Any] = asyncio.Queue()

        @agent.on_session_start
        async def start(session) -> None:  # noqa: ANN001
            self.sessions.put_nowait(session)

    async def new(self) -> tuple[FakeServer, Any]:
        fake, _ = await self.call()
        return fake, await asyncio.wait_for(self.sessions.get(), 5)

    async def test_every_command_has_its_doc_shape(self) -> None:
        fake, session = await self.new()
        tasks = [
            asyncio.create_task(session.say("hi", interrupt=True, re=4)),
            asyncio.create_task(session.update_context(system_prompt="p", append=[("user", "u")])),
            asyncio.create_task(session.set_tools(["a"])),
            asyncio.create_task(session.set_tools(None)),
            asyncio.create_task(session.end("Bye", reason="done")),
            asyncio.create_task(session.transfer("+4930123456", mode="warm", summary="s", id="tr1")),
        ]
        commands = {}
        for _ in tasks:
            command = await fake.recv()
            commands[command["type"] + str(command.get("enabled", "-"))] = command
            await fake.ok(command["id"])
        for task in tasks:
            result = await task
            self.assertTrue(result.ok)
        say = commands["say-"]
        self.assertEqual((say["text"], say["interrupt"], say["re"]), ("hi", True, 4))
        self.assertEqual(
            commands["context.update-"]["append"], [{"role": "user", "text": "u"}]
        )
        self.assertEqual(commands["tools.set['a']"]["enabled"], ["a"])
        self.assertIsNone(commands["tools.setNone"]["enabled"])
        self.assertEqual(commands["end-"]["say"], "Bye")
        transfer = commands["transfer-"]
        self.assertEqual(transfer["id"], "tr1")
        self.assertEqual(transfer["destination"], "+4930123456")
        self.assertEqual(transfer["mode"], "warm")

    async def test_ids_are_unique_and_a_transfer_always_has_one(self) -> None:
        fake, session = await self.new()
        first = asyncio.create_task(session.transfer("+4930123456"))
        t1 = await fake.recv()
        await fake.ok(t1["id"])
        await first
        second = asyncio.create_task(session.say("x"))
        s1 = await fake.recv()
        await fake.ok(s1["id"])
        await second
        self.assertNotEqual(t1["id"], s1["id"])

    async def test_a_refusal_raises_command_error_with_the_code(self) -> None:
        fake, session = await self.new()
        task = asyncio.create_task(session.transfer("+4930123456"))
        command = await fake.recv()
        await fake.fail(command["id"], "unsupported", "no call-control")
        with self.assertRaises(CommandError) as caught:
            await task
        self.assertIs(caught.exception.code, ErrorCode.UNSUPPORTED)
        self.assertEqual(caught.exception.message, "no call-control")

    async def test_rules_are_checked_before_anything_is_sent(self) -> None:
        fake, session = await self.new()
        with self.assertRaises(CommandError) as caught:
            await session.say("")
        self.assertIs(caught.exception.code, ErrorCode.INVALID_ARGUMENT)
        with self.assertRaises(CommandError) as caught:
            await session.transfer("0049301234")
        self.assertIs(caught.exception.code, ErrorCode.INVALID_ARGUMENT)
        with self.assertRaises(CommandError) as caught:
            await session.say("x" * 5000)
        self.assertIs(caught.exception.code, ErrorCode.INVALID_ARGUMENT)
        with self.assertRaises(asyncio.TimeoutError):
            await fake.recv(timeout=0.2)  # nothing went out

    async def test_no_result_in_time_raises_command_timeout_and_keeps_the_id(self) -> None:
        fake, session = await self.new()
        with self.assertRaises(CommandTimeout) as caught:
            await session.transfer("+4930123456", id="tr-9", timeout=0.1)
        self.assertEqual(caught.exception.command_id, "tr-9")
        self.assertIs(caught.exception.code, ErrorCode.OUTCOME_UNKNOWN)
        first = await fake.recv()
        # Retrying with the same id is allowed and carries the same id on the wire.
        retry = asyncio.create_task(session.transfer("+4930123456", id="tr-9"))
        again = await fake.recv()
        self.assertEqual(again["id"], first["id"])
        await fake.ok("tr-9")
        self.assertTrue((await retry).ok)

    async def test_wait_false_returns_a_future(self) -> None:
        fake, session = await self.new()
        future = await session.say("hi", wait=False)
        command = await fake.recv()
        self.assertFalse(future.done())
        await fake.ok(command["id"])
        self.assertTrue((await asyncio.wait_for(future, 2)).ok)

    async def test_a_failed_unwaited_command_does_not_leak_an_exception(self) -> None:
        fake, session = await self.new()
        future = await session.say("hi", wait=False)
        command = await fake.recv()
        await fake.fail(command["id"], "queue_full")
        result = await asyncio.wait_for(future, 2)
        self.assertFalse(result.ok)

    async def test_pending_commands_fail_when_the_link_closes(self) -> None:
        fake, session = await self.new()
        task = asyncio.create_task(session.say("hi"))
        await fake.recv()
        await fake.close()
        with self.assertRaises(SessionClosed):
            await task
        await session.wait_closed()
        with self.assertRaises(SessionClosed):
            await session.say("again")

    async def test_a_duplicate_waiting_id_is_refused(self) -> None:
        fake, session = await self.new()
        first = asyncio.create_task(session.say("a", id="same"))
        await fake.recv()
        with self.assertRaises(ValueError):
            await session.say("b", id="same")
        await fake.ok("same")
        await first


class Tools(AgentCase):
    def setup_agent(self, agent: Agent) -> None:
        self.cancelled = asyncio.Event()
        self.finished = asyncio.Event()
        self.keys: list[str] = []
        self.sessions: list[Any] = []

        @agent.tool
        async def echo(text: str, times: int = 1) -> dict:
            """Echo."""
            return {"text": text * times}

        @agent.tool
        def plain(a: int, b: int) -> int:
            """Add."""
            return a + b

        @agent.tool
        async def refuses() -> str:
            """Refuses."""
            raise ToolError("no tables left")

        @agent.tool
        async def explodes() -> str:
            """Explodes."""
            raise RuntimeError("secret internal detail")

        @agent.tool
        async def hangs(ctx: ToolContext) -> str:
            """Hangs until cancelled."""
            try:
                await asyncio.sleep(30)
            except asyncio.CancelledError:
                self.cancelled.set()
                raise
            return "never"

        @agent.tool(kind="side_effect")
        async def book(time: str, ctx: ToolContext) -> str:
            """Books."""
            self.keys.append(ctx.idempotency_key)
            await ctx.cancelled.wait() if time == "wait" else None
            self.finished.set()
            return f"booked {time} ({ctx.cancel_reason})"

        @agent.tool(kind="side_effect")
        async def stubborn(ctx: ToolContext) -> str:
            """Ignores ctx.cancelled and hangs."""
            try:
                await asyncio.sleep(30)
            except asyncio.CancelledError:
                self.cancelled.set()
                raise
            return "never"

        @agent.tool
        async def nan_mean() -> float:
            """Mean of nothing."""
            return float("nan")

        @agent.tool
        async def nested_inf() -> dict:
            """Infinity inside a result."""
            return {"values": [1.0, float("inf")]}

        @agent.on_session_start
        async def remember(session) -> None:  # noqa: ANN001
            self.sessions.append(session)

        @agent.tool
        async def huge() -> str:
            """Huge."""
            return "x" * 100_000

        @agent.tool
        async def unserializable() -> object:
            """Bad."""
            return object()

    def tool_call(self, id: int, name: str, arguments: dict[str, Any], kind: str = "read_only") -> dict[str, Any]:
        return {"type": "tool.call", "id": id, "call_id": f"call-{id}", "name": name,
                "arguments": arguments, "kind": kind, "epoch": 1,
                "idempotency_key": "0123456789abcdef", "timeout_ms": 5000}

    async def test_results_for_async_and_plain_functions(self) -> None:
        fake, _ = await self.call()
        await fake.send(self.tool_call(1, "echo", {"text": "ab", "times": 2}))
        await fake.send(self.tool_call(2, "plain", {"a": 2, "b": 3}))
        replies = {r["id"]: r for r in [await fake.recv_type("tool.result"), await fake.recv_type("tool.result")]}
        self.assertEqual(replies[1], {"type": "tool.result", "id": 1, "result": {"text": "abab"}})
        self.assertEqual(replies[2], {"type": "tool.result", "id": 2, "result": 5})

    async def test_errors_reach_the_llm_without_internal_details(self) -> None:
        fake, _ = await self.call()
        await fake.send(self.tool_call(1, "refuses", {}))
        await fake.send(self.tool_call(2, "explodes", {}))
        await fake.send(self.tool_call(3, "nonexistent", {}))
        await fake.send(self.tool_call(4, "echo", {"text": 5}))  # wrong type
        await fake.send(self.tool_call(5, "unserializable", {}))
        replies = {}
        for _ in range(5):
            r = await fake.recv_type("tool.result")
            replies[r["id"]] = r
        self.assertEqual(replies[1]["error"], "no tables left")
        self.assertEqual(replies[2]["error"], "tool failed: RuntimeError")
        self.assertNotIn("secret", str(replies[2]))
        self.assertIn("unknown tool", replies[3]["error"])
        self.assertIn("text", replies[4]["error"])
        self.assertEqual(replies[5]["error"], "tool failed: TypeError")  # an error, not a hang
        self.assertNotIn("result", replies[5])

    async def test_an_oversized_result_becomes_an_error(self) -> None:
        fake, _ = await self.call()
        await fake.send(self.tool_call(1, "huge", {}))
        reply = await fake.recv_type("tool.result")
        self.assertEqual(reply, {"type": "tool.result", "id": 1, "error": "tool result is too large"})

    async def test_read_only_tools_are_cancelled_by_tool_cancel(self) -> None:
        fake, _ = await self.call()
        await fake.send(self.tool_call(1, "hangs", {}))
        await self.settle()
        await fake.send({"type": "tool.cancel", "id": 1, "reason": "interrupted"})
        await asyncio.wait_for(self.cancelled.wait(), 5)
        # No result is sent for a cancelled call: the next tool.result is call 2's.
        await fake.send(self.tool_call(2, "plain", {"a": 1, "b": 1}))
        reply = await fake.recv_type("tool.result")
        self.assertEqual(reply["id"], 2)

    async def test_a_cancel_right_behind_the_call_still_finds_the_tool(self) -> None:
        fake, _ = await self.call()
        await fake.send(self.tool_call(1, "hangs", {}))
        await fake.send({"type": "tool.cancel", "id": 1, "reason": "timeout"})
        await fake.send(self.tool_call(2, "plain", {"a": 1, "b": 1}))
        reply = await fake.recv_type("tool.result")
        self.assertEqual(reply["id"], 2)  # id 1 produced nothing and nothing is left running
        await self.settle()
        self.assertEqual(self.agent._background, set())  # the cancelled task left nothing behind

    async def test_side_effect_tools_are_not_cancelled_and_still_answer(self) -> None:
        fake, _ = await self.call()
        await fake.send(self.tool_call(1, "book", {"time": "wait"}, kind="side_effect"))
        await self.settle()
        await fake.send({"type": "tool.cancel", "id": 1, "reason": "timeout"})
        reply = await fake.recv_type("tool.result")
        self.assertEqual(reply, {"type": "tool.result", "id": 1, "result": "booked wait (timeout)"})
        self.assertEqual(self.keys, ["0123456789abcdef"])

    async def test_non_finite_numbers_are_answered_with_an_error(self) -> None:
        fake, _ = await self.call()
        await fake.send(self.tool_call(1, "nan_mean", {}))
        await fake.send(self.tool_call(2, "nested_inf", {}))
        replies = {}
        for _ in range(2):
            r = await fake.recv_type("tool.result", timeout=1.5)
            replies[r["id"]] = r
        for n in (1, 2):
            self.assertIn("error", replies[n])
            self.assertNotIn("result", replies[n])

    async def test_a_result_that_fails_to_encode_is_still_answered(self) -> None:
        # The safety net behind to_json: whatever protocol.encode rejects gets an error reply.
        fake, _ = await self.call()
        await self.agent._answer(
            self.sessions[0], {"type": "tool.result", "id": 9, "result": float("nan")}
        )
        reply = await fake.recv_type("tool.result", timeout=1.5)
        self.assertEqual(
            reply, {"type": "tool.result", "id": 9, "error": "tool result is not valid JSON"}
        )

    async def test_a_side_effect_tool_is_told_when_the_link_closes_and_can_finish(self) -> None:
        fake, _ = await self.call()
        await fake.send(self.tool_call(1, "book", {"time": "wait"}, kind="side_effect"))
        await self.settle()
        self.assertFalse(self.finished.is_set())
        await fake.close()
        await asyncio.wait_for(self.finished.wait(), 5)  # ctx.cancelled was set
        await self.settle(10)
        self.assertEqual(self.agent._background, set())  # it finished, nothing leaks

    async def test_a_side_effect_tool_that_ignores_the_link_closing_is_bounded(self) -> None:
        self.agent.side_effect_grace = 0.05
        fake, _ = await self.call()
        await fake.send(self.tool_call(1, "stubborn", {}, kind="side_effect"))
        await self.settle()
        self.assertFalse(self.cancelled.is_set())
        await fake.close()
        await asyncio.wait_for(self.cancelled.wait(), 5)  # cancelled after the grace period
        await self.settle(10)
        self.assertEqual(self.agent._background, set())

    async def test_a_side_effect_tool_survives_the_link_closing_within_its_grace(self) -> None:
        self.agent.side_effect_grace = 30.0
        fake, _ = await self.call()
        await fake.send(self.tool_call(1, "stubborn", {}, kind="side_effect"))
        await self.settle()
        await fake.close()
        await self.settle(10)
        self.assertFalse(self.cancelled.is_set())  # not killed at once
        self.assertTrue(self.agent._background)
        for task in list(self.agent._background):
            task.cancel()
        await self.settle()


class Token(AgentCase):
    token = "s3cret"

    async def test_a_bearer_token_is_required(self) -> None:
        for headers in (None, {"Authorization": "Bearer wrong"}, {"Authorization": "Basic s3cret"}):
            with self.assertRaises(InvalidStatus) as caught:
                await connect(self.server.url, additional_headers=headers, open_timeout=5)
            self.assertEqual(caught.exception.response.status_code, 401)
        fake, answer = await self.call()
        self.assertEqual(answer["type"], "session.accept")


class Limits(AgentCase):
    async def test_more_calls_than_max_sessions_are_rejected(self) -> None:
        self.agent.max_sessions = 1
        first, answer = await self.call()
        self.assertEqual(answer["type"], "session.accept")
        _, second = await self.call(session_id="43")
        self.assertEqual(second["type"], "session.reject")
        self.assertEqual(second["code"], "unavailable")
        # A call that ended frees its slot.
        await first.close()
        await self.settle(20)
        _, third = await self.call(session_id="44")
        self.assertEqual(third["type"], "session.accept")

    async def test_each_call_is_isolated(self) -> None:
        a, _ = await self.call(session_id="a")
        b, _ = await self.call(session_id="b")
        await a.close()
        await b.send({"type": "bot.text", "epoch": 1, "text": "x"})
        await self.settle()
        self.assertFalse(b.ws.close_code)  # b's link is unaffected by a's close


class HandshakeBudget(unittest.IsolatedAsyncioTestCase):
    async def test_the_accept_write_is_bounded_by_the_handshake_budget(self) -> None:
        class StuckSocket:
            """Delivers session.start, then never takes another byte."""

            async def recv(self) -> str:
                return json.dumps({"seq": 1, "t_ms": 0, "type": "session.start", **START_BODY})

            async def send(self, text: str) -> None:
                await asyncio.sleep(30)

            async def close(self, *args: Any, **kwargs: Any) -> None:
                pass

        agent = Agent("luigi", handshake_timeout=0.2, write_timeout=10.0)
        loop = asyncio.get_running_loop()
        started = loop.time()
        with self.assertRaises(ConnectionError):
            await agent._handshake(StuckSocket(), loop.time() + agent.handshake_timeout)  # type: ignore[arg-type]
        self.assertLess(loop.time() - started, 2.0)


class Declaration(unittest.TestCase):
    def test_duplicate_and_illegal_tools_fail_at_decoration_time(self) -> None:
        agent = Agent()

        @agent.tool
        def one() -> str:
            """One."""
            return ""

        with self.assertRaises(ValueError):
            agent.tool(one)
        with self.assertRaises(ValueError):

            @agent.tool(name="bad name")
            def two() -> str:
                """Two."""
                return ""

    def test_at_most_64_tools(self) -> None:
        agent = Agent()
        for n in range(64):
            agent.tool(lambda: "", name=f"t{n}", description="d")
        with self.assertRaises(ValueError):
            agent.tool(lambda: "", name="t64", description="d")

    def test_unknown_event_type_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            Agent().on("tool.call")
        with self.assertRaises(ValueError):
            Agent().on("nonsense")

    def test_unsupported_versions_are_refused(self) -> None:
        with self.assertRaises(ValueError):
            Agent(versions=[2])


if __name__ == "__main__":
    unittest.main()
