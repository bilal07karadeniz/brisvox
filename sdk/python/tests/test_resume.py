"""Reconnecting: ``Agent(resume=True)`` declares ``resume``, parks a call whose link was
lost and re-attaches it when the server's ``session.start.resume`` arrives (docs/AGENT_PROTOCOL.md,
"Reconnecting")."""

from __future__ import annotations

import asyncio
import logging
import unittest
from collections.abc import Callable
from typing import Any

from brisvox_agent import (
    Accept,
    Agent,
    ErrorCode,
    ProtocolError,
    Resumed,
    ResumeInfo,
    SessionClosed,
    SessionEnd,
    SessionStart,
    ToolContext,
    UserText,
    protocol,
)
from fake_server import START_BODY, FakeServer

logging.getLogger("brisvox_agent").setLevel(logging.CRITICAL)

RESUME = {"attempt": 1, "previous_seq": 7, "down_ms": 120}


async def until(what: str, cond: Callable[[], bool], timeout: float = 5.0) -> None:
    async with asyncio.timeout(timeout):
        while not cond():
            await asyncio.sleep(0.005)


class ResumeCase(unittest.IsolatedAsyncioTestCase):
    window = 15.0

    def make_agent(self) -> Agent:
        return Agent("luigi", greeting="Hi!", resume=True, resume_window=self.window)

    async def asyncSetUp(self) -> None:
        self.agent = self.make_agent()
        self.log: list[tuple[Any, ...]] = []
        self.sessions: list[Any] = []
        self.ended = asyncio.Event()
        self.resumed_events: list[Resumed] = []
        agent = self.agent

        @agent.on_session_start
        async def start(session) -> Accept | None:  # noqa: ANN001
            self.log.append(("start", session.id))
            session.data["notes"] = []
            self.sessions.append(session)
            return None

        @agent.tool
        async def note(text: str, ctx: ToolContext) -> str:  # a read_only tool
            """Keeps a note on the call."""
            ctx.session.data["notes"].append(text)
            return f"{len(ctx.session.data['notes'])} notes"

        @agent.tool
        async def hang(ctx: ToolContext) -> str:
            """Waits until the link is lost."""
            try:
                await ctx.cancelled.wait()
            finally:
                self.log.append(("hang", ctx.cancel_reason))
            return "unreachable"

        @agent.on_user_text
        async def heard(session, event: UserText) -> None:  # noqa: ANN001
            self.log.append(("user", event.text))
            session.data["notes"].append(event.text)

        @agent.on_resumed
        async def resumed(session, event: Resumed) -> None:  # noqa: ANN001
            self.resumed_events.append(event)
            self.log.append(("resumed", event.attempt, event.previous_seq, event.down_ms, event.seq))

        @agent.on_events_lost
        async def lost(session, event) -> None:  # noqa: ANN001
            self.log.append(("lost", event.count))

        @agent.on_session_end
        async def end(session, event: SessionEnd) -> None:  # noqa: ANN001
            self.log.append(("end", event.reason))
            self.ended.set()

        self.server = agent.serve("127.0.0.1", 0, token=None)
        await self.server.start()
        self.addAsyncCleanup(self.server.close)

    async def connect(self) -> FakeServer:
        fake = await FakeServer.connect(self.server.url)
        self.addAsyncCleanup(fake.close)
        return fake

    async def first_link(self) -> tuple[FakeServer, dict[str, Any]]:
        fake = await self.connect()
        return fake, await fake.start()

    async def resume_link(self, **resume: Any) -> tuple[FakeServer, dict[str, Any]]:
        info = {**RESUME, **resume}
        fake = await self.connect()
        fake.seq = info["previous_seq"]  # the count continues on the new link
        return fake, await fake.start(resume=info)

    def entries(self, kind: str) -> list[tuple[Any, ...]]:
        return [e for e in self.log if e[0] == kind]


class Declaration(ResumeCase):
    async def test_the_accept_declares_resume_only_when_opted_in(self) -> None:
        _, answer = await self.first_link()
        self.assertIs(answer["resume"], True)
        plain = Agent("plain")
        async with plain.serve("127.0.0.1", 0, token=None) as server:
            fake = await FakeServer.connect(server.url)
            self.addAsyncCleanup(fake.close)
            self.assertNotIn("resume", await fake.start())

    async def test_a_resumption_is_not_a_new_call(self) -> None:
        fake, first = await self.first_link()
        await fake.send({"type": "user.text", "epoch": 1, "text": "table for two"})
        await until("the first event", lambda: self.entries("user"))
        await fake.close()
        session = self.sessions[0]
        await until("the session is parked", lambda: session.parked)
        self.assertTrue(session.closed)

        again, answer = await self.resume_link()
        self.assertEqual(answer["type"], "session.accept")
        self.assertIs(answer["resume"], True)
        self.assertEqual(answer["version"], 1)
        self.assertNotIn("greeting", answer, "a resumed call is not greeted again")
        self.assertEqual(answer["events"], first["events"], "the subscription is kept")
        await until("the resumed hook", lambda: self.entries("resumed"))
        self.assertEqual(self.entries("start"), [("start", "42")], "on_session_start ran once")
        self.assertEqual(len(self.sessions), 1)
        self.assertFalse(session.parked)
        self.assertFalse(session.closed)
        self.assertEqual(session.resumes, 1)
        self.assertEqual(session.data["notes"], ["table for two"], "the call's state is kept")
        # on_resumed gets attempt / previous_seq / down_ms, and runs after the earlier events.
        self.assertEqual(self.entries("resumed"), [("resumed", 1, 7, 120, 8)])
        self.assertLess(self.log.index(("user", "table for two")), self.log.index(("resumed", 1, 7, 120, 8)))

    async def test_events_commands_and_tools_work_on_the_resumed_link(self) -> None:
        fake, _ = await self.first_link()
        await fake.close()
        session = self.sessions[0]
        await until("parked", lambda: session.parked)
        again, _ = await self.resume_link()
        await until("resumed", lambda: not session.parked)

        await again.send({"type": "user.text", "epoch": 2, "text": "hello again"})
        await until("an event on the new link", lambda: ("user", "hello again") in self.log)

        say = asyncio.ensure_future(session.say("Back with you."))
        command = await again.recv_type("say")
        self.assertEqual(command["text"], "Back with you.")
        await again.ok(command["id"])
        await asyncio.wait_for(say, 5)

        call = {"type": "tool.call", "call_id": "c", "epoch": 2, "kind": "side_effect", "idempotency_key": "feedfeedfeedfeed", "timeout_ms": 1000}
        await again.send({**call, "id": 9, "name": "note", "arguments": {"text": "kept"}})
        reply = await again.recv_type("tool.result")
        self.assertEqual(reply, {"type": "tool.result", "id": 9, "result": "2 notes"})
        self.assertEqual(session.data["notes"], ["hello again", "kept"])

    async def test_the_end_after_a_resumption_is_delivered_once(self) -> None:
        fake, _ = await self.first_link()
        await fake.close()
        again, _ = await self.resume_link()
        await until("resumed", lambda: self.entries("resumed"))
        await again.send({"type": "events.lost", "count": 3})
        await again.send({"type": "session.end", "reason": "hangup"})
        await asyncio.wait_for(self.ended.wait(), 5)
        await again.close()
        await asyncio.sleep(0.05)
        self.assertEqual(self.entries("end"), [("end", "hangup")])
        self.assertEqual(self.entries("lost"), [("lost", 3)], "events.lost after a resumption is the server's")
        self.assertLess(self.log.index(("resumed", 1, 7, 120, 8)), self.log.index(("lost", 3)))


class Parking(ResumeCase):
    async def test_resumed_reports_the_last_seq_this_side_read(self) -> None:
        fake, _ = await self.first_link()  # session.start was seq 1
        await fake.send({"type": "user.text", "epoch": 1, "text": "one"})
        await fake.send({"type": "user.text", "epoch": 1, "text": "two"})
        await until("both events", lambda: len(self.entries("user")) == 2)
        await fake.close()
        await until("parked", lambda: self.sessions[0].parked)
        await self.resume_link(previous_seq=3)  # the server's count: start, one, two
        await until("resumed", lambda: self.resumed_events)
        event = self.resumed_events[0]
        self.assertEqual((event.previous_seq, event.seen_seq, event.seq), (3, 3, 4))
        self.assertEqual(self.sessions[0]._last_seq, 4)

    async def test_a_parked_call_counts_against_max_sessions(self) -> None:
        self.agent.max_sessions = 1
        fake, _ = await self.first_link()
        session = self.sessions[0]
        await fake.close()
        await until("parked", lambda: session.parked and self.agent._active == 0)
        other = await self.connect()
        answer = await other.start(session_id="43")
        self.assertEqual(answer["type"], "session.reject")
        self.assertEqual(answer["code"], "unavailable")
        # Once the parked call is resumed it is one call again (not counted twice).
        again, _ = await self.resume_link()
        await until("resumed", lambda: not session.parked)
        self.assertEqual(self.agent._parked_count(), 0)

    async def test_arming_the_park_timer_again_leaves_one_timer(self) -> None:
        fake, _ = await self.first_link()
        session = self.sessions[0]
        await fake.close()
        await until("parked", lambda: session.parked)
        first = session._park_timer
        self.assertIsNotNone(first)
        self.agent._arm_park(session)  # a failed resume answer re-arms the same deadline
        self.assertTrue(first.cancelled(), "the earlier timer must not survive to end a later window")
        self.assertIsNot(session._park_timer, first)
        self.assertFalse(session._park_timer.cancelled())

    async def test_commands_fail_while_the_session_is_parked(self) -> None:
        fake, _ = await self.first_link()
        session = self.sessions[0]
        waiting = asyncio.ensure_future(session.say("Hello?"))
        await fake.recv_type("say")
        await fake.close()
        with self.assertRaises(SessionClosed):
            await asyncio.wait_for(waiting, 5)  # the pending command fails at the loss
        await until("parked", lambda: session.parked)
        with self.assertRaises(SessionClosed):
            await session.say("Anyone there?")

    async def test_a_running_read_only_tool_hears_the_loss_and_the_tool_works_after(self) -> None:
        fake, _ = await self.first_link()
        call = {"type": "tool.call", "call_id": "c", "epoch": 1, "kind": "read_only", "idempotency_key": "feedfeedfeedfeed", "timeout_ms": 1000}
        await fake.send({**call, "id": 1, "name": "hang", "arguments": {}})
        await asyncio.sleep(0.05)
        await fake.close()
        session = self.sessions[0]
        await until("parked", lambda: session.parked)
        await until("the tool heard the loss", lambda: ("hang", "link_closed") in self.log)
        again, _ = await self.resume_link()
        await again.send({**call, "id": 2, "name": "note", "arguments": {"text": "x"}})
        reply = await again.recv_type("tool.result")
        self.assertEqual(reply["id"], 2)

    async def test_the_session_end_of_the_server_is_final(self) -> None:
        fake, _ = await self.first_link()
        await fake.send({"type": "session.end", "reason": "hangup"})
        await asyncio.wait_for(self.ended.wait(), 5)
        await fake.close()
        await asyncio.sleep(0.05)
        self.assertFalse(self.sessions[0].parked)
        again, answer = await self.resume_link()
        self.assertEqual(answer, {"type": "session.reject", "code": "unknown_session", "message": "no such call"})
        self.assertEqual(self.entries("end"), [("end", "hangup")])

    async def test_an_unknown_call_is_rejected_with_unknown_session(self) -> None:
        fake = await self.connect()
        fake.seq = 3
        answer = await fake.start(session_id="never-seen", resume=RESUME)
        self.assertEqual(answer["type"], "session.reject")
        self.assertEqual(answer["code"], "unknown_session")
        self.assertEqual(self.log, [], "no handler ran")

    async def test_an_agent_without_resume_rejects_a_resumption(self) -> None:
        plain = Agent("plain")
        async with plain.serve("127.0.0.1", 0, token=None) as server:
            fake = await FakeServer.connect(server.url)
            self.addAsyncCleanup(fake.close)
            answer = await fake.start(resume=RESUME)
            self.assertEqual(answer["code"], "unknown_session")

    async def test_a_link_that_never_declared_resume_ends_the_call_at_once(self) -> None:
        plain = Agent("plain")
        ends: list[str] = []

        @plain.on_session_end
        async def end(session, event: SessionEnd) -> None:  # noqa: ANN001
            ends.append(event.reason)

        async with plain.serve("127.0.0.1", 0, token=None) as server:
            fake = await FakeServer.connect(server.url)
            await fake.start()
            await fake.close()
            await until("the end", lambda: ends)
        self.assertEqual(ends, ["link_closed"])

    async def test_a_resumption_takes_over_a_link_the_agent_still_holds(self) -> None:
        old, _ = await self.first_link()
        session = self.sessions[0]
        again, answer = await self.resume_link()  # the old link is half-open: no close seen yet
        self.assertIs(answer["resume"], True)
        await until("resumed", lambda: self.entries("resumed"))
        self.assertEqual(session.resumes, 1)
        self.assertFalse(session.closed)
        # The old link is closed by the agent; that must not park or end the resumed call.
        async with asyncio.timeout(5):
            await old.ws.wait_closed()
        await asyncio.sleep(0.05)
        self.assertFalse(session.parked)
        self.assertEqual(self.entries("end"), [])
        await again.send({"type": "user.text", "epoch": 1, "text": "still here"})
        await until("the event", lambda: ("user", "still here") in self.log)

    async def test_two_losses_two_resumptions(self) -> None:
        fake, _ = await self.first_link()
        session = self.sessions[0]
        await fake.close()
        second, _ = await self.resume_link(attempt=1, previous_seq=7)
        await until("first resumption", lambda: session.resumes == 1)
        await second.close()
        await until("parked again", lambda: session.parked)
        third, _ = await self.resume_link(attempt=2, previous_seq=20)
        await until("second resumption", lambda: session.resumes == 2)
        self.assertEqual([e[1] for e in self.entries("resumed")], [1, 2])
        self.assertEqual(self.entries("start"), [("start", "42")])

    async def test_stopping_the_server_ends_the_parked_calls(self) -> None:
        fake, _ = await self.first_link()
        await fake.close()
        await until("parked", lambda: self.sessions[0].parked)
        await self.server.close()
        self.assertEqual(self.entries("end"), [("end", "link_closed")])
        self.assertFalse(self.sessions[0].parked)


class Expiry(ResumeCase):
    window = 0.15

    async def test_a_parked_call_that_is_not_resumed_ends_once_after_the_window(self) -> None:
        fake, _ = await self.first_link()
        await fake.close()
        session = self.sessions[0]
        await until("parked", lambda: session.parked)
        self.assertEqual(self.entries("end"), [])
        await asyncio.wait_for(self.ended.wait(), 5)
        self.assertEqual(self.entries("end"), [("end", "link_closed")])
        self.assertFalse(session.parked)
        self.assertEqual(self.agent._sessions, {}, "the session is forgotten")
        # Too late: the server's resumption is refused and nothing runs again.
        again, answer = await self.resume_link()
        self.assertEqual(answer["code"], "unknown_session")
        await asyncio.sleep(0.05)
        self.assertEqual(self.entries("end"), [("end", "link_closed")])
        self.assertEqual(self.entries("resumed"), [])

    async def test_a_timely_resumption_cancels_the_expiry(self) -> None:
        fake, _ = await self.first_link()
        await fake.close()
        session = self.sessions[0]
        await until("parked", lambda: session.parked)
        again, _ = await self.resume_link()
        await until("resumed", lambda: not session.parked)
        await asyncio.sleep(self.window * 3)
        self.assertEqual(self.entries("end"), [], "the old timer did not end the resumed call")
        self.assertFalse(session.closed)


class WireTypes(unittest.TestCase):
    def test_session_start_parses_resume(self) -> None:
        start = protocol.parse_server_message(
            {"seq": 57, "t_ms": 41200, "type": "session.start", **START_BODY, "resume": RESUME}
        )
        assert isinstance(start, SessionStart)
        self.assertEqual(start.resume, ResumeInfo(attempt=1, previous_seq=7, down_ms=120))
        first = protocol.parse_server_message(
            {"seq": 1, "t_ms": 0, "type": "session.start", **START_BODY}
        )
        assert isinstance(first, SessionStart)
        self.assertIsNone(first.resume)

    def test_a_malformed_resume_is_a_bad_message(self) -> None:
        for bad in ("yes", {"attempt": 1}, {"attempt": "1", "previous_seq": 1, "down_ms": 1}):
            with self.subTest(resume=bad), self.assertRaises(ProtocolError):
                protocol.parse_server_message(
                    {"seq": 1, "t_ms": 0, "type": "session.start", **START_BODY, "resume": bad}
                )

    def test_session_accept_resume_flag(self) -> None:
        self.assertNotIn("resume", protocol.session_accept(1))
        self.assertIs(protocol.session_accept(1, resume=True)["resume"], True)
        self.assertEqual(ErrorCode.UNKNOWN_SESSION.value, "unknown_session")

    def test_the_agent_validates_the_window(self) -> None:
        with self.assertRaises(ValueError):
            Agent(resume=True, resume_window=0)


if __name__ == "__main__":
    unittest.main()
