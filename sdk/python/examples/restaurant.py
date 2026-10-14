"""A restaurant host: typed tools, per-call setup, event handlers and every command.

* ``opening_hours`` is a ``read_only`` tool: the engine may run it speculatively and cancels it
  when the caller barges in.
* ``book_table`` is a ``side_effect`` tool: it never runs twice for one call, and
  ``ctx.idempotency_key`` goes to the booking system so even a retried request cannot book twice.
* ``on_session_start`` personalises the call (and shows how to decline one).
* The handlers show ``say``, ``end``, ``transfer``, ``update_context`` and ``set_tools``.

Run::

    python examples/restaurant.py --port 8765
    BRISVOX_AGENT_URL=ws://127.0.0.1:8765 BRISVOX_AGENT_TOKEN=... brisvox-server ...

Set ``BRISVOX_AGENT_TOKEN`` for the agent as well to require the bearer token.
"""

from __future__ import annotations

import argparse
import enum
import logging
import sys
from dataclasses import dataclass

from brisvox_agent import (
    Accept,
    Agent,
    CommandError,
    ErrorCode,
    Reject,
    Session,
    SessionEnd,
    ToolContext,
    ToolError,
    TurnMetrics,
    UserText,
)

log = logging.getLogger("restaurant")

agent = Agent(
    "luigi",
    system_prompt=(
        "You are Luigi's host on the phone. Be brief and warm. "
        "Use the tools for opening hours and bookings; never invent a booking."
    ),
    greeting="Hi, Luigi's here. How can I help?",
)


class Weekday(enum.Enum):
    MON = "mon"
    TUE = "tue"
    WED = "wed"
    THU = "thu"
    FRI = "fri"
    SAT = "sat"
    SUN = "sun"


@dataclass
class Contact:
    """Who to call back.

    Attributes:
        name: The caller's name.
        phone: A number in any format.
    """

    name: str
    phone: str | None = None


BOOKED: dict[str, dict[str, object]] = {}
"""Bookings by idempotency key (a stand-in for the booking system's own deduplication)."""


@agent.tool
async def opening_hours(day: Weekday) -> str:
    """Opening hours of the restaurant on a weekday.

    Args:
        day: The weekday, as three letters.
    """
    return "Closed on Monday." if day is Weekday.MON else "12:00 to 22:00."


@agent.tool(kind="side_effect", timeout_ms=4000)
async def book_table(
    time: str, party_size: int = 2, contact: Contact | None = None, *, ctx: ToolContext
) -> dict[str, object]:
    """Book a table.

    Args:
        time: The time as HH:MM, 24 hours.
        party_size: How many people (1 to 12).
        contact: Who to call back, if the caller gave a name.
    """
    if not 1 <= party_size <= 12:
        raise ToolError("We seat parties of 1 to 12.")  # the LLM reads this and apologises
    # The key is stable for this tool call: a repeated request books once.
    booking = BOOKED.setdefault(
        ctx.idempotency_key,
        {"time": time, "party_size": party_size, "name": contact.name if contact else None},
    )
    return {"status": "booked", **booking}


@agent.on_session_start
async def personalise(session: Session) -> Accept | None:
    """Runs inside the handshake budget (about 1.5 s): keep it fast."""
    if session.agent_name == "closed-for-private-event":
        raise Reject(ErrorCode.UNAVAILABLE, "no calls tonight")
    session.data["turns"] = 0
    if not session.capabilities.transfer:
        log.info("call %s: no transfer on this server", session.id)
    return None  # keep the agent's defaults; return Accept(...) to change them for this call


@agent.on_user_text
async def watch_user(session: Session, event: UserText) -> None:
    text = event.text.lower()
    session.data["turns"] += 1
    if "goodbye" in text or "bye" in text.split():
        await session.end("Goodbye, and thank you for calling!", reason="caller said goodbye")
    elif "manager" in text or "human" in text:
        if session.capabilities.transfer:
            try:
                await session.transfer(
                    "+4930123456789",
                    mode="warm",
                    summary="Caller asked for a person.",
                    id=f"handoff-{session.id}",  # same id on a retry = no second transfer
                )
            except CommandError as exc:
                log.warning("transfer failed: %s", exc)
                await session.say("I could not reach the manager, sorry. Can I help instead?")
        else:
            await session.update_context(
                append=[("system", "The caller wants a person but transfer is unavailable; "
                                   "offer to take a message.")]
            )
    elif "just chat" in text:
        await session.set_tools([])  # no tools from the next response on
    elif "tools back" in text:
        await session.set_tools(None)


@agent.on_turn
async def metrics(session: Session, event: object) -> None:
    if isinstance(event, TurnMetrics) and event.metrics.response_ms is not None:
        log.info("call %s epoch %d: first audio after %.0f ms", session.id, event.epoch,
                 event.metrics.response_ms)


@agent.on_session_end
async def finished(session: Session, event: SessionEnd) -> None:
    log.info("call %s over (%s) after %d user turns", session.id, event.reason, session.data["turns"])


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8765)
    args = parser.parse_args(argv)
    logging.basicConfig(level=logging.INFO, stream=sys.stderr)

    def ready(server) -> None:  # noqa: ANN001
        # One line on stdout for scripts (and the Rust integration test) to read the port from.
        print(f"LISTENING {server.url}", flush=True)

    agent.run(args.host, args.port, on_ready=ready)


if __name__ == "__main__":
    main()
