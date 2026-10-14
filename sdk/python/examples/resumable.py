"""A reconnecting agent: ``resume=True`` keeps a call's state when its link drops.

The brisvox server reopens a lost link of an agent that declared ``resume`` (backoff, at most five
attempts, a 15 s window) and sends ``session.start`` again with ``resume``. The SDK re-attaches the
call to its parked :class:`Session`: ``session.data`` is still there, ``on_session_start`` does not
run again, nothing is greeted twice, and ``on_resumed`` tells the app about the gap.

* ``remember`` is a ``side_effect`` tool that keeps a note in ``session.data``.
* ``recall`` is a ``read_only`` tool that reports the notes, how often the call started in this
  process (once, however often the link dropped) and the gaps ``on_resumed`` saw.

Run::

    python examples/resumable.py --port 8765
"""

from __future__ import annotations

import argparse
import logging
import sys

from brisvox_agent import Agent, Resumed, Session, SessionEnd, ToolContext, UserText

log = logging.getLogger("resumable")

agent = Agent(
    "luigi",
    system_prompt="You are Luigi's host. Use remember to keep what the caller asks for, recall to repeat it.",
    greeting="Hi, Luigi's here.",
    resume=True,  # declare `resume` in session.accept: a lost link is taken up again
    resume_window=15.0,  # how long a lost call is kept for the server (the server's own window)
)


@agent.on_session_start
async def started(session: Session) -> None:
    """Runs once per call, never again for a resumption."""
    session.data.update(notes=[], starts=1, gaps=[])


@agent.tool(kind="side_effect")
async def remember(note: str, *, ctx: ToolContext) -> str:
    """Keep a note about the caller's wishes for the rest of the call.

    Args:
        note: What to remember.
    """
    ctx.session.data["notes"].append(note)
    return f"noted ({len(ctx.session.data['notes'])})"


@agent.tool
async def recall(*, ctx: ToolContext) -> dict[str, object]:
    """Repeat the notes of this call and what the link did meanwhile."""
    data = ctx.session.data
    gaps = list(data["gaps"])
    return {
        "notes": list(data["notes"]),
        "starts": data["starts"],
        "resumes": ctx.session.resumes,
        "gaps": gaps,
        # The server's count continues across a resumption: the last message this agent read on
        # the lost link is the one the server says it wrote last (`previous_seq`), and the
        # resuming session.start is the message right after it.
        "seq_continues": all(
            g["seen_seq"] == g["previous_seq"] and g["seq"] == g["previous_seq"] + 1 for g in gaps
        ),
    }


@agent.on_resumed
async def resumed(session: Session, event: Resumed) -> None:
    """The link was lost and is back. `event.seq` continues the server's count after
    `event.previous_seq`; whatever the server could not deliver meanwhile arrives as `events.lost`."""
    session.data["gaps"].append(
        {
            "attempt": event.attempt,
            "previous_seq": event.previous_seq,
            "seen_seq": event.seen_seq,
            "down_ms": event.down_ms,
            "seq": event.seq,
        }
    )
    log.info("call %s: back after %d ms (attempt %d)", session.id, event.down_ms, event.attempt)


@agent.on_user_text
async def watch_user(session: Session, event: UserText) -> None:
    if "goodbye" in event.text.lower():
        await session.end("Goodbye, and thank you for calling!", reason="caller said goodbye")


@agent.on_session_end
async def finished(session: Session, event: SessionEnd) -> None:
    log.info("call %s over (%s) after %d resumption(s)", session.id, event.reason, session.resumes)


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
