"""The shipped examples load, declare valid tools and behave against the fake server."""

from __future__ import annotations

import asyncio
import importlib.util
import logging
import sys
import unittest
from pathlib import Path
from types import ModuleType

from fake_server import FakeServer

EXAMPLES = Path(__file__).resolve().parents[1] / "examples"
logging.getLogger("brisvox_agent").setLevel(logging.CRITICAL)
logging.getLogger("restaurant").setLevel(logging.CRITICAL)


def load(name: str) -> ModuleType:
    spec = importlib.util.spec_from_file_location(f"example_{name}", EXAMPLES / f"{name}.py")
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module  # dataclasses with string annotations look themselves up
    spec.loader.exec_module(module)
    return module


class Examples(unittest.IsolatedAsyncioTestCase):
    def test_every_example_imports_and_has_an_agent(self) -> None:
        for path in sorted(EXAMPLES.glob("*.py")):
            with self.subTest(example=path.name):
                module = load(path.stem)
                self.assertTrue(hasattr(module, "agent"))

    def test_book_table_schema(self) -> None:
        tools = load("restaurant").agent.tools
        book = tools["book_table"].declaration()
        self.assertEqual(book["kind"], "side_effect")
        self.assertEqual(book["timeout_ms"], 4000)
        props = book["parameters"]["properties"]
        self.assertEqual(book["parameters"]["required"], ["time"])
        self.assertNotIn("ctx", props)
        self.assertEqual(props["time"]["description"], "The time as HH:MM, 24 hours.")
        self.assertEqual(props["party_size"]["default"], 2)
        contact = props["contact"]["anyOf"][0]
        self.assertEqual(contact["required"], ["name"])
        hours = tools["opening_hours"].declaration()
        self.assertEqual(
            hours["parameters"]["properties"]["day"]["enum"],
            ["mon", "tue", "wed", "thu", "fri", "sat", "sun"],
        )

    async def test_restaurant_end_to_end(self) -> None:
        agent = load("restaurant").agent
        async with agent.serve("127.0.0.1", 0, token=None) as server:
            fake = await FakeServer.connect(server.url)
            answer = await fake.start(capabilities={"transfer": False, "tools": True})
            self.assertEqual(answer["greeting"], "Hi, Luigi's here. How can I help?")
            self.assertEqual({t["name"] for t in answer["tools"]}, {"opening_hours", "book_table"})

            call = {"type": "tool.call", "call_id": "c", "epoch": 1, "kind": "side_effect",
                    "idempotency_key": "feedfeedfeedfeed", "timeout_ms": 4000}
            await fake.send({**call, "id": 1, "name": "book_table",
                             "arguments": {"time": "19:30", "party_size": 4, "contact": {"name": "Ann"}}})
            await fake.send({**call, "id": 2, "name": "book_table", "arguments": {"time": "19:30", "party_size": 99}})
            await fake.send({**call, "id": 3, "name": "opening_hours", "arguments": {"day": "mon"}, "kind": "read_only"})
            replies = {}
            for _ in range(3):
                reply = await fake.recv_type("tool.result")
                replies[reply["id"]] = reply
            self.assertEqual(
                replies[1]["result"],
                {"status": "booked", "time": "19:30", "party_size": 4, "name": "Ann"},
            )
            self.assertEqual(replies[2]["error"], "We seat parties of 1 to 12.")
            self.assertEqual(replies[3]["result"], "Closed on Monday.")

            # No transfer on this server: the handler falls back to a context update.
            seq = await fake.send({"type": "user.text", "epoch": 2, "text": "let me talk to a human"})
            update = await fake.recv_type("context.update")
            self.assertEqual(update["re"], seq)
            await fake.ok(update["id"])

            await fake.send({"type": "user.text", "epoch": 3, "text": "ok goodbye"})
            end = await fake.recv_type("end")
            self.assertEqual(end["say"], "Goodbye, and thank you for calling!")
            await fake.ok(end["id"])
            await fake.send({"type": "session.end", "reason": "ended_by_agent"})
            await asyncio.sleep(0.05)
            await fake.close()

    async def test_resumable_keeps_its_state_across_a_dropped_link(self) -> None:
        agent = load("resumable").agent
        call = {"type": "tool.call", "call_id": "c", "epoch": 1, "kind": "read_only",
                "idempotency_key": "feedfeedfeedfeed", "timeout_ms": 4000}
        async with agent.serve("127.0.0.1", 0, token=None) as server:
            fake = await FakeServer.connect(server.url)
            answer = await fake.start()
            self.assertIs(answer["resume"], True)
            self.assertEqual(answer["greeting"], "Hi, Luigi's here.")
            await fake.send({**call, "id": 1, "name": "remember", "kind": "side_effect",
                             "arguments": {"note": "window seat"}})
            self.assertEqual((await fake.recv_type("tool.result"))["result"], "noted (1)")
            await fake.close()

            again = await FakeServer.connect(server.url)
            # The agent read two messages on the first link (session.start, tool.call).
            again.seq = 2
            answer = await again.start(resume={"attempt": 1, "previous_seq": 2, "down_ms": 50})
            self.assertEqual(answer["type"], "session.accept")
            self.assertNotIn("greeting", answer)
            await asyncio.sleep(0.05)  # on_resumed runs behind the reader
            await again.send({**call, "id": 2, "name": "recall", "arguments": {}})
            recalled = (await again.recv_type("tool.result"))["result"]
            self.assertEqual(recalled["notes"], ["window seat"])
            self.assertEqual(recalled["starts"], 1)
            self.assertEqual(recalled["resumes"], 1)
            self.assertIs(recalled["seq_continues"], True)
            self.assertEqual(
                recalled["gaps"],
                [{"attempt": 1, "previous_seq": 2, "seen_seq": 2, "down_ms": 50, "seq": 3}],
            )
            await again.close()

    async def test_resumable_notices_a_server_count_that_does_not_match_what_it_read(self) -> None:
        agent = load("resumable").agent
        call = {"type": "tool.call", "call_id": "c", "epoch": 1, "kind": "read_only",
                "idempotency_key": "feedfeedfeedfeed", "timeout_ms": 4000}
        async with agent.serve("127.0.0.1", 0, token=None) as server:
            fake = await FakeServer.connect(server.url)
            await fake.start()
            await fake.close()
            again = await FakeServer.connect(server.url)
            again.seq = 10  # the agent only ever read session.start (seq 1)
            await again.start(resume={"attempt": 1, "previous_seq": 10, "down_ms": 50})
            await asyncio.sleep(0.05)
            await again.send({**call, "id": 2, "name": "recall", "arguments": {}})
            recalled = (await again.recv_type("tool.result"))["result"]
            self.assertIs(recalled["seq_continues"], False)
            await again.close()


if __name__ == "__main__":
    unittest.main()
