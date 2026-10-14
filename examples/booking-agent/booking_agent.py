"""A tool-heavy booking agent for brisvox: an out-of-process agent written with the Python SDK.

The brisvox server runs the call (audio, speech recognition, turn taking, the LLM, speech
synthesis); this process holds the salon's application logic and answers the LLM's tool calls.

Tools (typed, the JSON schema comes from the type hints and docstrings):

========================  ==========  =========================================================
tool                      kind        why
========================  ==========  =========================================================
``list_services``         read_only   the menu; may run speculatively, is cancelled on barge-in
``check_availability``    read_only   free start times of a service on a date
``find_booking``          read_only   look a booking up by confirmation code
``book_appointment``      side_effect never speculative, never run twice; idempotent by key
``cancel_booking``        side_effect never speculative, never run twice; idempotent by state
========================  ==========  =========================================================

Run (keys are the brisvox server's business; this process needs none)::

    pip install ./sdk/python                      # from the repository root
    python examples/booking-agent/booking_agent.py --port 8765
    BRISVOX_AGENT_URL=ws://127.0.0.1:8765 BRISVOX_AGENT_FILE=examples/booking-agent/agent.toml \\
        brisvox-server

Set ``BRISVOX_AGENT_TOKEN`` on both sides to require the bearer token. Set ``SALON_TZ`` (an IANA
name such as ``Australia/Sydney``) to the salon's time zone: opening hours, "today" in the prompt
and the one hour lead time are the salon's local time, and a container runs in UTC by default.
"""

from __future__ import annotations

import argparse
import datetime
import enum
import logging
import os
import sys
from typing import Annotated

from booking_store import BookingError, BookingStore, parse_time

from brisvox_agent import Accept, Agent, Session, SessionEnd, ToolContext, ToolError

log = logging.getLogger("booking")

SYSTEM_PROMPT = """\
You are the phone assistant of Maple Hair Studio. The caller is on a phone line: answer in one or
two short sentences, in plain words, and never read out lists longer than three items.

Use the tools for everything about services, free times and bookings; never guess a price, a
time or a confirmation code. Today is {today} ({weekday}); turn "tomorrow" or "next Friday"
into a date yourself and say the date back to the caller before you book. To book you need the
service, the date, a start time and the caller's name (a phone number is optional). After a
booking read the confirmation code back one character at a time. To cancel, ask for the
confirmation code. If a tool says a time is taken, offer the nearest free times instead.
"""


class Service(enum.Enum):
    """The services on the menu (``list_services`` has the details)."""

    HAIRCUT = "haircut"
    BEARD_TRIM = "beard_trim"
    COLOR = "color"


def build_agent(store: BookingStore) -> Agent:
    """The agent over ``store``. A function so tests (and your own deployment) can pass their
    own store and clock; the module's ``agent`` is this over a real-time store."""
    agent = Agent(
        "maple-hair-studio",
        system_prompt=SYSTEM_PROMPT.format(today="(unknown)", weekday="(unknown)"),
        greeting="Hello, Maple Hair Studio. How can I help?",
    )

    @agent.on_session_start
    async def personalise(session: Session) -> Accept:
        """The prompt carries today's date: the LLM cannot know it, and "next Friday" needs it."""
        today = store.today()
        return Accept(
            system_prompt=SYSTEM_PROMPT.format(today=today.isoformat(), weekday=today.strftime("%A"))
        )

    @agent.tool
    async def list_services() -> list[dict[str, object]]:
        """The services of the salon with their length in minutes and price in dollars."""
        return store.services()

    @agent.tool
    async def check_availability(
        service: Annotated[Service, "The service the caller wants."],
        day: Annotated[datetime.date, "The date, as YYYY-MM-DD."],
    ) -> dict[str, object]:
        """Free start times for a service on a date (the salon is closed on Sundays).

        Returns the earliest twelve; `more` says how many further times exist.
        """
        try:
            free = store.free_starts(service.value, day)
        except BookingError as exc:
            raise ToolError(str(exc)) from exc
        return {"day": day.isoformat(), "service": service.value, "free_starts": free[:12], "more": max(0, len(free) - 12)}

    @agent.tool
    async def find_booking(
        confirmation_code: Annotated[str, "The code read out at booking, such as MH1A2B3C4D."],
    ) -> dict[str, object]:
        """Look up a booking by its confirmation code."""
        try:
            return store.find(confirmation_code).public()
        except BookingError as exc:
            raise ToolError(str(exc)) from exc

    @agent.tool(kind="side_effect", timeout_ms=8000)
    async def book_appointment(
        service: Annotated[Service, "The service to book."],
        day: Annotated[datetime.date, "The date, as YYYY-MM-DD."],
        start: Annotated[str, "The start time as HH:MM on a 24 hour clock, on the hour or half hour."],
        customer_name: Annotated[str, "The caller's name."],
        phone: Annotated[str | None, "A number to reach the caller, if they gave one."] = None,
        *,
        ctx: ToolContext,
    ) -> dict[str, object]:
        """Book an appointment. Check availability first; the answer carries the confirmation code."""
        try:
            # ctx.idempotency_key is the same for every delivery of this tool call: a retry
            # gets the booking that was made, never a second one.
            booking, repeated = store.book(
                ctx.idempotency_key, service.value, day, parse_time(start), customer_name, phone
            )
        except BookingError as exc:
            raise ToolError(str(exc)) from exc
        log.info("call %s: booking %s (%s)", ctx.session.id, booking.code, "repeat" if repeated else "new")
        return {**booking.public(), "already_booked": repeated}

    @agent.tool(kind="side_effect", timeout_ms=8000)
    async def cancel_booking(
        confirmation_code: Annotated[str, "The confirmation code of the booking to cancel."],
        *,
        ctx: ToolContext,
    ) -> dict[str, object]:
        """Cancel a booking. Ask the caller to confirm first; cancelling twice is harmless."""
        try:
            booking, was_cancelled = store.cancel(ctx.idempotency_key, confirmation_code)
        except BookingError as exc:
            raise ToolError(str(exc)) from exc
        log.info("call %s: cancel %s (%s)", ctx.session.id, booking.code, "repeat" if was_cancelled else "new")
        return {**booking.public(), "already_cancelled": was_cancelled}

    @agent.on_session_end
    async def finished(session: Session, event: SessionEnd) -> None:
        log.info("call %s over (%s)", session.id, event.reason)

    return agent


agent = build_agent(BookingStore(timezone=os.environ.get("SALON_TZ")))
"""The agent over the in-memory store (bookings live as long as this process)."""


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8765)
    args = parser.parse_args(argv)
    logging.basicConfig(level=logging.INFO, stream=sys.stderr)

    def ready(server) -> None:  # noqa: ANN001
        print(f"LISTENING {server.url}", flush=True)  # one line on stdout for scripts to read

    agent.run(args.host, args.port, on_ready=ready)


if __name__ == "__main__":
    main()
