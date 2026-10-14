"""Tests of the booking agent's rules and tools. No keys, no brisvox server: the store is tested
directly, the tools through the SDK, and the whole agent through the SDK's fake server.

    PYTHONPATH=sdk/python python3 -m unittest discover -s examples/booking-agent -p 'test_*.py'

(Python 3.11+ with ``websockets``, like the SDK's own tests; ``scripts/verify.sh`` runs this in
its "examples" stage.)
"""

from __future__ import annotations

import asyncio
import datetime as dt
import logging
import re
import sys
import unittest
import unittest.mock
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
for extra in (ROOT / "sdk" / "python", ROOT / "sdk" / "python" / "tests"):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

import booking_agent  # noqa: E402
import booking_store  # noqa: E402
from booking_store import SERVICES, BookingError, BookingStore, parse_time  # noqa: E402
from fake_server import FakeServer  # noqa: E402

from brisvox_agent import ToolContext  # noqa: E402

logging.getLogger("brisvox_agent").setLevel(logging.CRITICAL)
logging.getLogger("booking").setLevel(logging.CRITICAL)

NOW = dt.datetime(2026, 10, 14, 10, 0)  # a Wednesday, 10:00
THURSDAY = dt.date(2026, 10, 15)
SUNDAY = dt.date(2026, 10, 18)
T = dt.time


def store() -> BookingStore:
    return BookingStore(now=lambda: NOW)


class Rules(unittest.TestCase):
    def test_the_fixture_dates_are_what_the_tests_say(self) -> None:
        self.assertEqual((NOW.weekday(), THURSDAY.weekday(), SUNDAY.weekday()), (2, 3, 6))

    def test_services(self) -> None:
        listed = store().services()
        self.assertEqual({s["service"] for s in listed}, set(SERVICES))
        self.assertTrue(all(isinstance(s["price_usd"], int) and s["minutes"] % 30 == 0 for s in listed))

    def test_free_starts_fit_the_service_before_closing(self) -> None:
        s = store()
        haircut = s.free_starts("haircut", THURSDAY)
        self.assertEqual((haircut[0], haircut[-1], len(haircut)), ("09:00", "16:30", 16))
        color = s.free_starts("color", THURSDAY)  # 90 minutes
        self.assertEqual((color[0], color[-1]), ("09:00", "15:30"))

    def test_today_needs_an_hour_of_notice(self) -> None:
        self.assertEqual(store().free_starts("haircut", NOW.date())[0], "11:00")

    def test_closed_past_and_far_dates_are_refused(self) -> None:
        s = store()
        for day, text in (
            (SUNDAY, "closed on Sundays"),
            (NOW.date() - dt.timedelta(days=1), "passed"),
            (NOW.date() + dt.timedelta(days=61), "60 days"),
        ):
            with self.subTest(day=day), self.assertRaisesRegex(BookingError, text):
                s.free_starts("haircut", day)
        with self.assertRaisesRegex(BookingError, "not offer"):
            s.free_starts("massage", THURSDAY)

    def test_times_are_parsed_strictly(self) -> None:
        self.assertEqual(parse_time(" 14:30 "), T(14, 30))
        for bad in ("2:30pm", "14:15", "25:00", "14:30:00", ""):
            with self.subTest(bad=bad), self.assertRaises(BookingError):
                parse_time(bad)


class Booking(unittest.TestCase):
    def book(self, s: BookingStore, key: str, start: dt.time = T(10, 0), name: str = "Ann", service: str = "haircut"):
        return s.book(key, service, THURSDAY, start, name, "+1 555 0100")

    def test_a_booking_takes_its_slot_and_overlapping_ones(self) -> None:
        s = store()
        booking, repeated = self.book(s, "aaaa0000aaaa0000")
        self.assertFalse(repeated)
        self.assertEqual((booking.status, booking.code), ("booked", "MHAAAA0000"))
        self.assertEqual(booking.public()["end"], "10:30")
        free = s.free_starts("haircut", THURSDAY)
        self.assertNotIn("10:00", free)
        self.assertIn("10:30", free)
        # A 90 minute colour from 09:30 would run into the 10:00 haircut; from 08:30 is not open.
        color = s.free_starts("color", THURSDAY)
        self.assertNotIn("09:00", color)
        self.assertNotIn("09:30", color)
        self.assertIn("10:30", color)
        with self.assertRaisesRegex(BookingError, "not available"):
            self.book(s, "bbbb0000bbbb0000", name="Bob")

    def test_a_retry_of_one_tool_call_books_once(self) -> None:
        s = store()
        first, _ = self.book(s, "aaaa0000aaaa0000")
        again, repeated = self.book(s, "aaaa0000aaaa0000")
        self.assertTrue(repeated)
        self.assertIs(first, again)
        self.assertEqual(sum(b.status == "booked" for b in s._by_code.values()), 1)

    def test_a_late_redelivery_gets_the_original_answer_not_a_new_booking(self) -> None:
        # The key, not the slot, identifies a tool call: after the booking was cancelled and the
        # slot taken by someone else, a redelivery of the first request is still the first
        # booking's answer. It must neither fail with "not available" nor book anything.
        s = store()
        first, _ = self.book(s, "aaaa0000aaaa0000")
        s.cancel("eeee0000eeee0000", first.code)
        self.book(s, "bbbb0000bbbb0000", name="Bob")
        again, repeated = self.book(s, "aaaa0000aaaa0000")
        self.assertTrue(repeated)
        self.assertIs(again, first)
        self.assertEqual([b.customer for b in s._by_code.values() if b.status == "booked"], ["Bob"])

    def test_the_same_customer_asking_twice_is_not_booked_twice(self) -> None:
        s = store()
        first, _ = self.book(s, "aaaa0000aaaa0000", name="Ann Lee")
        again, repeated = self.book(s, "cccc0000cccc0000", name="  ann   LEE ")
        self.assertTrue(repeated)
        self.assertIs(first, again)
        self.assertEqual(len(s._by_code), 1)

    def test_codes_never_clash(self) -> None:
        s = store()
        a, _ = self.book(s, "aaaa0000aaaa0000", T(9, 0))
        b, _ = self.book(s, "aaaa0000aaaa0001", T(9, 30), "Bob")  # the first 8 digits clash
        self.assertEqual(a.code, "MHAAAA0000")
        self.assertNotEqual(a.code, b.code)
        self.assertTrue(b.code.startswith("MHAAAA0000A"))

    def test_bad_input_is_refused_without_booking(self) -> None:
        s = store()
        for bad in (
            dict(customer=" "),
            dict(customer="x" * 61),
            dict(phone="call me"),
            dict(start=T(8, 30)),
            dict(start=T(16, 30), service="color"),  # would end after closing
            dict(day=SUNDAY),
        ):
            args = dict(service="haircut", day=THURSDAY, start=T(10, 0), customer="Ann", phone=None) | bad
            with self.subTest(bad=bad), self.assertRaises(BookingError):
                s.book("dddd0000dddd0000", **args)
        self.assertEqual(s._by_code, {})

    def test_cancel_frees_the_slot_and_is_idempotent(self) -> None:
        s = store()
        booking, _ = self.book(s, "aaaa0000aaaa0000")
        cancelled, was = s.cancel("eeee0000eeee0000", booking.code.lower())
        self.assertEqual((cancelled.status, was), ("cancelled", False))
        self.assertIn("10:00", s.free_starts("haircut", THURSDAY))
        again, was = s.cancel("ffff0000ffff0000", booking.code)
        self.assertEqual((again.status, was), ("cancelled", True))
        # The slot can be booked by someone else, and the old code stays cancelled.
        self.book(s, "gggg0000gggg0000", name="Bob")
        self.assertEqual(s.find(booking.code).status, "cancelled")
        with self.assertRaisesRegex(BookingError, "No booking"):
            s.cancel("hhhh0000hhhh0000", "MHNOPE")

    def test_an_appointment_that_started_cannot_be_cancelled(self) -> None:
        now = [NOW]
        s = BookingStore(now=lambda: now[0])
        booking, _ = s.book("aaaa0000aaaa0000", "haircut", THURSDAY, T(10, 0), "Ann")
        now[0] = dt.datetime.combine(THURSDAY, T(10, 5))
        with self.assertRaisesRegex(BookingError, "already started"):
            s.cancel("bbbb0000bbbb0000", booking.code)
        self.assertEqual(s.find(booking.code).status, "booked")


def tool_ctx(key: str = "0123456789abcdef") -> ToolContext:
    return ToolContext(
        session=SimpleNamespace(id="s1"),  # type: ignore[arg-type]
        call_id="call-1",
        request_id=1,
        name="t",
        kind="side_effect",
        epoch=1,
        idempotency_key=key,
        timeout_ms=8000,
    )


class Tools(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        self.tools = booking_agent.build_agent(store()).tools

    def test_declarations(self) -> None:
        self.assertEqual(
            set(self.tools),
            {"list_services", "check_availability", "find_booking", "book_appointment", "cancel_booking"},
        )
        kinds = {name: tool.kind for name, tool in self.tools.items()}
        self.assertEqual(
            kinds,
            {
                "list_services": "read_only",
                "check_availability": "read_only",
                "find_booking": "read_only",
                "book_appointment": "side_effect",
                "cancel_booking": "side_effect",
            },
        )
        book = self.tools["book_appointment"].declaration()
        props = book["parameters"]["properties"]
        self.assertNotIn("ctx", props)
        self.assertEqual(book["parameters"]["required"], ["service", "day", "start", "customer_name"])
        self.assertEqual(props["service"]["enum"], ["haircut", "beard_trim", "color"])
        self.assertEqual(props["day"], {"type": "string", "format": "date", "description": "The date, as YYYY-MM-DD."})
        self.assertEqual(book["timeout_ms"], 8000)
        for name, tool in self.tools.items():
            with self.subTest(tool=name):
                self.assertTrue(tool.description.strip())
                self.assertEqual(tool.declaration()["parameters"]["type"], "object")

    async def run_tool(self, name: str, arguments: dict, key: str = "0123456789abcdef"):
        return await self.tools[name].run(arguments, tool_ctx(key))

    async def test_availability_book_find_cancel(self) -> None:
        free = await self.run_tool("check_availability", {"service": "haircut", "day": "2026-10-15"})
        self.assertEqual(free["free_starts"][:2], ["09:00", "09:30"])
        self.assertEqual((len(free["free_starts"]), free["more"]), (12, 4))
        booked = await self.run_tool(
            "book_appointment",
            {"service": "haircut", "day": "2026-10-15", "start": "09:00", "customer_name": "Ann"},
        )
        self.assertEqual(booked["code"], "MH01234567")
        self.assertEqual((booked["weekday"], booked["start"], booked["end"]), ("Thursday", "09:00", "09:30"))
        self.assertFalse(booked["already_booked"])
        found = await self.run_tool("find_booking", {"confirmation_code": "mh01234567"})
        self.assertEqual(found["customer"], "Ann")
        cancelled = await self.run_tool("cancel_booking", {"confirmation_code": "MH01234567"}, key="f" * 16)
        self.assertEqual((cancelled["status"], cancelled["already_cancelled"]), ("cancelled", False))

    async def test_a_redelivered_booking_is_not_booked_again(self) -> None:
        args = {"service": "color", "day": "2026-10-15", "start": "13:00", "customer_name": "Ann"}
        first = await self.run_tool("book_appointment", args)
        second = await self.run_tool("book_appointment", args)
        self.assertEqual(first["code"], second["code"])
        self.assertTrue(second["already_booked"])

    async def test_refusals_reach_the_llm_as_tool_errors(self) -> None:
        from brisvox_agent import ToolError

        with self.assertRaisesRegex(ToolError, "closed on Sundays"):
            await self.run_tool("check_availability", {"service": "haircut", "day": "2026-10-18"})
        with self.assertRaisesRegex(ToolError, "HH:MM"):
            await self.run_tool(
                "book_appointment",
                {"service": "haircut", "day": "2026-10-15", "start": "ten", "customer_name": "Ann"},
            )
        with self.assertRaisesRegex(ToolError, "No booking"):
            await self.run_tool("find_booking", {"confirmation_code": "MHZZZZZZZZ"})


class EndToEnd(unittest.IsolatedAsyncioTestCase):
    """The whole agent over the protocol, with the SDK's fake brisvox server."""

    async def test_a_call(self) -> None:
        agent = booking_agent.build_agent(store())
        async with agent.serve("127.0.0.1", 0, token=None) as server:
            fake = await FakeServer.connect(server.url)
            answer = await fake.start(capabilities={"transfer": False, "tools": True})
            self.assertEqual(answer["type"], "session.accept")
            self.assertIn("Today is 2026-10-14 (Wednesday)", answer["system_prompt"])
            self.assertEqual(answer["greeting"], "Hello, Maple Hair Studio. How can I help?")
            self.assertEqual(len(answer["tools"]), 5)

            def call(i: int, name: str, key: str, arguments: dict) -> dict:
                kind = next(t["kind"] for t in answer["tools"] if t["name"] == name)
                return {"type": "tool.call", "id": i, "call_id": f"c{i}", "epoch": 1, "name": name,
                        "kind": kind, "idempotency_key": key, "timeout_ms": 8000, "arguments": arguments}

            book = {"service": "haircut", "day": "2026-10-15", "start": "10:00", "customer_name": "Ann"}
            await fake.send(call(1, "book_appointment", "1111111111111111", book))
            await fake.send(call(2, "book_appointment", "1111111111111111", book))  # the same call again
            await fake.send(call(3, "book_appointment", "2222222222222222", {**book, "customer_name": "Bob"}))
            await fake.send(call(4, "check_availability", "3333333333333333", {"service": "haircut", "day": "2026-10-15"}))
            replies = {}
            for _ in range(4):
                reply = await fake.recv_type("tool.result")
                replies[reply["id"]] = reply
            self.assertEqual(replies[1]["result"]["code"], "MH11111111")
            self.assertEqual(replies[2]["result"]["code"], "MH11111111")
            self.assertTrue(replies[2]["result"]["already_booked"])
            self.assertIn("not available", replies[3]["error"])
            self.assertNotIn("10:00", replies[4]["result"]["free_starts"])

            await fake.send(call(5, "cancel_booking", "4444444444444444", {"confirmation_code": "MH11111111"}))
            await fake.send(call(6, "cancel_booking", "5555555555555555", {"confirmation_code": "MH11111111"}))
            results = {}
            for _ in range(2):
                reply = await fake.recv_type("tool.result")
                results[reply["id"]] = reply["result"]
            self.assertFalse(results[5]["already_cancelled"])
            self.assertTrue(results[6]["already_cancelled"])
            await fake.send({"type": "session.end", "reason": "call_ended"})
            await asyncio.sleep(0.05)
            await fake.close()

    def test_the_salon_clock_follows_its_time_zone_not_the_host(self) -> None:
        # 22:00 UTC on Tuesday 13 Oct is already 09:00 Wednesday 14 Oct in Sydney (UTC+11).
        utc_now = dt.datetime(2026, 10, 13, 22, 0, tzinfo=dt.timezone.utc)

        class Frozen(dt.datetime):
            @classmethod
            def now(cls, tz=None):  # type: ignore[no-untyped-def]
                return utc_now.astimezone(tz) if tz else utc_now.replace(tzinfo=None)

        with unittest.mock.patch.object(booking_store.dt, "datetime", Frozen):
            sydney = BookingStore(timezone="Australia/Sydney")
            host = BookingStore()
            self.assertEqual(sydney.today(), dt.date(2026, 10, 14))
            self.assertEqual(host.today(), dt.date(2026, 10, 13))  # the UTC host: a day behind
            # The lead time and "that date has passed" use the salon's day, not the host's.
            self.assertEqual(sydney.free_starts("haircut", dt.date(2026, 10, 14))[0], "10:00")  # 09:00 + lead
            with self.assertRaises(BookingError):
                sydney.free_starts("haircut", dt.date(2026, 10, 13))
        with self.assertRaises(ValueError):
            BookingStore(timezone="Mars/Olympus")

    def test_the_module_level_agent_is_the_real_clock_one(self) -> None:
        self.assertEqual(set(booking_agent.agent.tools), set(booking_agent.build_agent(store()).tools))


class Dockerfile(unittest.TestCase):
    """The image builds from the repository root: every path it copies has to exist there."""

    PATH = Path(__file__).resolve().parent / "Dockerfile"

    @staticmethod
    def instructions(text: str) -> list[str]:
        joined = re.sub(r"\\\r?\n", " ", text)
        return [line.strip() for line in joined.splitlines() if line.strip() and not line.lstrip().startswith("#")]

    def test_every_copied_path_exists_in_the_repository(self) -> None:
        copies = [line.split()[1:] for line in self.instructions(self.PATH.read_text("utf-8")) if line.startswith("COPY ")]
        sources = [arg for args in copies for arg in args[:-1] if not arg.startswith("--")]
        self.assertIn("sdk/python", sources)
        self.assertIn("examples/booking-agent/booking_agent.py", sources)
        for source in sources:
            self.assertTrue((ROOT / source).exists(), f"COPY {source}: not in the repository")

    def test_the_command_runs_a_copied_file_and_binds_all_interfaces(self) -> None:
        cmd = next(line for line in self.instructions(self.PATH.read_text("utf-8")) if line.startswith("CMD "))
        self.assertIn('"booking_agent.py"', cmd)
        self.assertIn('"0.0.0.0"', cmd)  # the SDK default (127.0.0.1) is unreachable from the server's container
        self.assertTrue((Path(__file__).resolve().parent / "booking_agent.py").exists())


if __name__ == "__main__":
    unittest.main()
