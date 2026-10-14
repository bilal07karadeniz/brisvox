"""The smallest useful agent: the Python twin of the Rust "minimal agent" in
docs/AGENT_PROTOCOL.md. It answers one question itself and offers one tool.

Run it, then point the brisvox server at it::

    python examples/minimal.py                      # listens on ws://127.0.0.1:8765
    BRISVOX_AGENT_URL=ws://127.0.0.1:8765 brisvox-server ...
"""

from __future__ import annotations

import logging

from brisvox_agent import Agent, Session, UserText

agent = Agent("minimal", system_prompt="You are a helpful phone assistant.")


@agent.tool
async def lookup(topic: str) -> str:
    """Look up a short fact about the restaurant (hours, address)."""
    facts = {"hours": "We open at noon.", "address": "12 Harbour Street."}
    return facts.get(topic.lower(), "I have nothing on that.")


@agent.on_user_text
async def answer_hours(session: Session, event: UserText) -> None:
    """React to the user's turn. `re` (the event's seq) is attached automatically."""
    if "hours" in event.text.lower():
        await session.say("We open at noon.")


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO)
    agent.run(port=8765)
