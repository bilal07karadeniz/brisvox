"""A fake brisvox server for the tests: connects out to an agent like the real one does and
speaks the server side of docs/AGENT_PROTOCOL.md (envelope with ``seq`` and ``t_ms``,
``session.start`` first, ``command.result`` for commands that carry an ``id``)."""

from __future__ import annotations

import asyncio
import json
from typing import Any

from websockets.asyncio.client import ClientConnection, connect

START_BODY = {
    "protocol": "brisvox.agent",
    "versions": [1],
    "session_id": "42",
    "agent": "luigi",
    "capabilities": {"transfer": False, "tools": True},
}


class FakeServer:
    """One call's link, from the server's side."""

    def __init__(self, ws: ClientConnection) -> None:
        self.ws = ws
        self.seq = 0
        self.received: list[dict[str, Any]] = []

    @classmethod
    async def connect(cls, url: str, *, token: str | None = None) -> "FakeServer":
        headers = {"Authorization": f"Bearer {token}"} if token else None
        ws = await connect(url, additional_headers=headers, open_timeout=5)
        return cls(ws)

    async def send(self, body: dict[str, Any]) -> int:
        """Send a server message with the envelope; returns its ``seq``."""
        self.seq += 1
        await self.ws.send(json.dumps({"seq": self.seq, "t_ms": self.seq * 10, **body}))
        return self.seq

    async def start(self, **overrides: Any) -> dict[str, Any]:
        """Send ``session.start`` and return the agent's answer."""
        await self.send({"type": "session.start", **{**START_BODY, **overrides}})
        return await self.recv()

    async def recv(self, timeout: float = 5.0) -> dict[str, Any]:
        """The next message from the agent."""
        async with asyncio.timeout(timeout):
            raw = await self.ws.recv()
        message = json.loads(raw)
        self.received.append(message)
        return message

    async def recv_type(self, kind: str, timeout: float = 5.0) -> dict[str, Any]:
        """The next message of ``type`` ``kind`` (others are skipped, but kept in ``received``)."""
        while True:
            message = await self.recv(timeout)
            if message.get("type") == kind:
                return message

    async def ok(self, command_id: str) -> None:
        await self.send({"type": "command.result", "id": command_id, "ok": True})

    async def fail(self, command_id: str, code: str, message: str = "nope") -> None:
        await self.send(
            {
                "type": "command.result",
                "id": command_id,
                "ok": False,
                "error": {"code": code, "message": message},
            }
        )

    async def ack_next(self, kind: str) -> dict[str, Any]:
        """Receive the next ``kind`` command and answer it ``ok``."""
        command = await self.recv_type(kind)
        if "id" in command:
            await self.ok(command["id"])
        return command

    async def close(self) -> None:
        await self.ws.close()
