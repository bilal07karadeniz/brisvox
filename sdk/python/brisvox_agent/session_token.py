"""Mint short-lived session tokens for the brisvox server's browser-facing listeners.

Your application backend signs a token for one visitor; the visitor's page hands it to the
server (``Authorization: Bearer <token>`` on the WebRTC ``POST /whip``; ``?token=``, a bearer
header or the ``brisvox-token.<token>`` WebSocket subprotocol on the RTVI and generic WebSocket
listeners). The server verifies it with the same secret (``BRISVOX_SESSION_SECRET``), checks the
audience and the time window, and lets it open exactly one call. See ``docs/SECURITY.md``.

    from brisvox_agent.session_token import mint_session_token

    token = mint_session_token(os.environ["BRISVOX_SESSION_SECRET"], "brisvox-webrtc", ttl=60,
                               agent="support", max_s=600, meta={"user": "u-42"})

The secret never goes to the browser. Mint a token per page load or per call, with a short ``ttl``.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import secrets
import time
from collections.abc import Mapping

__all__ = ["AUDIENCES", "MAX_TOKEN_LIFETIME", "MIN_SECRET_BYTES", "mint_session_token"]

#: The listeners a token can be for (the ``aud`` claim).
AUDIENCES = ("brisvox-webrtc", "brisvox-rtvi", "brisvox-ws")

#: The server refuses a shorter secret.
MIN_SECRET_BYTES = 32

#: The server refuses a token whose ``exp`` is more than this many seconds ahead of its clock.
MAX_TOKEN_LIFETIME = 86_400

#: The longest call (``max_s``) the server accepts, in seconds.
MAX_CALL_SECONDS = 86_400

#: A Unix time in seconds stays below this until the year 5138; a larger "now" is milliseconds.
_MAX_PLAUSIBLE_UNIX_SECONDS = 100_000_000_000


def _whole_seconds(name: str, value: object) -> int:
    """``value`` as an int, or ``ValueError`` (a ``bool`` or a float is not a count of seconds)."""
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError(f"{name} must be a whole number of seconds")
    return value


def _b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def mint_session_token(
    secret: str,
    audience: str,
    *,
    ttl: int = 60,
    sid: str | None = None,
    agent: str | None = None,
    max_s: int | None = None,
    meta: Mapping[str, str | int | float | bool] | None = None,
    now: int | None = None,
) -> str:
    """Return ``v1.<payload>.<signature>`` for ``audience``, valid for ``ttl`` seconds.

    ``sid`` is the single-use session id (a random one by default; the server refuses a second
    token with the same ``sid`` until it would have expired). ``agent`` and ``meta`` reach the
    call's metadata; ``max_s`` caps the call duration. ``ttl``, ``max_s`` and ``now`` are whole
    seconds (``now`` is for tests; a value that looks like milliseconds is refused), and ``ttl`` is
    at most ``MAX_TOKEN_LIFETIME`` (24 hours): the server refuses a token that lives longer.
    """
    if audience not in AUDIENCES:
        raise ValueError(f"audience must be one of {AUDIENCES}")
    key = secret.encode("utf-8")
    if len(key) < MIN_SECRET_BYTES:
        raise ValueError(f"the session secret must be at least {MIN_SECRET_BYTES} bytes")
    ttl = _whole_seconds("ttl", ttl)
    if not 1 <= ttl <= MAX_TOKEN_LIFETIME:
        raise ValueError(f"ttl must be 1 to {MAX_TOKEN_LIFETIME} seconds")
    if max_s is not None:
        max_s = _whole_seconds("max_s", max_s)
        if not 1 <= max_s <= MAX_CALL_SECONDS:
            raise ValueError(f"max_s must be 1 to {MAX_CALL_SECONDS} seconds")
    issued = int(time.time()) if now is None else _whole_seconds("now", now)
    if not 0 <= issued < _MAX_PLAUSIBLE_UNIX_SECONDS:
        raise ValueError("now must be Unix seconds, not milliseconds")
    claims: dict[str, object] = {
        "aud": audience,
        "exp": issued + ttl,
        "sid": sid if sid is not None else secrets.token_urlsafe(16),
    }
    if agent is not None:
        claims["agent"] = agent
    if max_s is not None:
        claims["max_s"] = max_s
    if meta:
        claims["meta"] = dict(meta)
    payload = json.dumps(claims, separators=(",", ":")).encode("utf-8")
    signature = hmac.new(key, payload, hashlib.sha256).digest()
    return f"v1.{_b64url(payload)}.{_b64url(signature)}"
