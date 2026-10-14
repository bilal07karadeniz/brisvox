"""Session tokens: the format the server verifies (docs/SECURITY.md)."""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import unittest

from brisvox_agent.session_token import MAX_TOKEN_LIFETIME, MIN_SECRET_BYTES, mint_session_token

SECRET = "0123456789abcdef0123456789abcdef"
NOW = 1_767_225_600  # 2026-01-01T00:00:00Z, the simulated clock's origin in the Rust tests

# The same vectors are verified by the Rust server's tests (crates/brisvox-transport/src/auth.rs):
# a change here that is not a change there breaks the contract.
GOLDEN_WEBRTC = (
    "v1.eyJhdWQiOiJicmlzdm94LXdlYnJ0YyIsImV4cCI6MTc2NzIyNjIwMCwic2lkIjoiZ29sZGVuLTEiLCJhZ2VudCI6InN1"
    "cHBvcnQiLCJtYXhfcyI6MzAwLCJtZXRhIjp7InBsYW4iOiJwcm8ifX0.zHMxezQrKQHSJbz6ffciHtW_RCV8WWYojeuQ9oa3p-A"
)
GOLDEN_RTVI = (
    "v1.eyJhdWQiOiJicmlzdm94LXJ0dmkiLCJleHAiOjE3NjcyMjYyMDAsInNpZCI6ImdvbGRlbi0yIn0."
    "kg-l-Emp5-CxMv7XqkIXi6RRHjtx0s1UzPiDxuVRzRU"
)


def b64d(text: str) -> bytes:
    return base64.urlsafe_b64decode(text + "=" * (-len(text) % 4))


class SessionTokenTest(unittest.TestCase):
    def test_golden_vectors(self) -> None:
        self.assertEqual(
            mint_session_token(
                SECRET,
                "brisvox-webrtc",
                ttl=600,
                sid="golden-1",
                agent="support",
                max_s=300,
                meta={"plan": "pro"},
                now=NOW,
            ),
            GOLDEN_WEBRTC,
        )
        self.assertEqual(
            mint_session_token(SECRET, "brisvox-rtvi", ttl=600, sid="golden-2", now=NOW),
            GOLDEN_RTVI,
        )

    def test_the_signature_covers_the_payload_bytes(self) -> None:
        token = mint_session_token(SECRET, "brisvox-ws", now=NOW)
        version, payload, signature = token.split(".")
        self.assertEqual(version, "v1")
        expected = hmac.new(SECRET.encode(), b64d(payload), hashlib.sha256).digest()
        self.assertEqual(b64d(signature), expected)
        claims = json.loads(b64d(payload))
        self.assertEqual(claims["aud"], "brisvox-ws")
        self.assertEqual(claims["exp"], NOW + 60)
        self.assertGreaterEqual(len(claims["sid"]), 16)
        self.assertNotIn("=", token, "base64url without padding")

    def test_every_token_has_its_own_sid(self) -> None:
        sids = {json.loads(b64d(mint_session_token(SECRET, "brisvox-ws").split(".")[1]))["sid"] for _ in range(50)}
        self.assertEqual(len(sids), 50)

    def test_bad_arguments(self) -> None:
        with self.assertRaises(ValueError):
            mint_session_token("short", "brisvox-ws")
        with self.assertRaises(ValueError):
            mint_session_token(SECRET, "brisvox-other")
        with self.assertRaises(ValueError):
            mint_session_token(SECRET, "brisvox-ws", ttl=0)
        self.assertEqual(MIN_SECRET_BYTES, 32)

    def test_the_lifetime_and_the_units_are_checked(self) -> None:
        self.assertEqual(MAX_TOKEN_LIFETIME, 86_400)
        mint_session_token(SECRET, "brisvox-ws", ttl=MAX_TOKEN_LIFETIME, now=NOW)
        for ttl in (MAX_TOKEN_LIFETIME + 1, 90_000, 10 * 365 * 86_400, -5):
            with self.assertRaises(ValueError, msg=f"ttl={ttl}"):
                mint_session_token(SECRET, "brisvox-ws", ttl=ttl, now=NOW)
        for bad in (60.5, True, "60", None):
            with self.assertRaises(ValueError, msg=f"ttl={bad!r}"):
                mint_session_token(SECRET, "brisvox-ws", ttl=bad, now=NOW)  # type: ignore[arg-type]
        # A "now" in milliseconds, a float, or a negative one is refused.
        for now in (NOW * 1000, float(NOW), -1, True):
            with self.assertRaises(ValueError, msg=f"now={now!r}"):
                mint_session_token(SECRET, "brisvox-ws", now=now)  # type: ignore[arg-type]
        # max_s is whole seconds within the server's 1..86400.
        for max_s in (0, 86_401, 1.5, 300_000):
            with self.assertRaises(ValueError, msg=f"max_s={max_s!r}"):
                mint_session_token(SECRET, "brisvox-ws", max_s=max_s, now=NOW)  # type: ignore[arg-type]
        mint_session_token(SECRET, "brisvox-ws", max_s=86_400, now=NOW)


if __name__ == "__main__":
    unittest.main()
