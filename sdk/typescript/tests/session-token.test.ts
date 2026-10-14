/** Session tokens: the format the server verifies (docs/SECURITY.md). */

import assert from "node:assert/strict";
import { createHmac } from "node:crypto";
import { describe, it } from "node:test";
import { MAX_SESSION_TOKEN_LIFETIME_SECONDS, mintSessionToken } from "../src/session-token.js";

const SECRET = "0123456789abcdef0123456789abcdef";
const NOW = 1_767_225_600; // 2026-01-01T00:00:00Z, the simulated clock's origin in the Rust tests

// The same vectors are verified by the Rust server's tests (crates/brisvox-transport/src/auth.rs)
// and produced by the Python helper: a change here that is not a change there breaks the contract.
const GOLDEN_WEBRTC =
  "v1.eyJhdWQiOiJicmlzdm94LXdlYnJ0YyIsImV4cCI6MTc2NzIyNjIwMCwic2lkIjoiZ29sZGVuLTEiLCJhZ2VudCI6InN1" +
  "cHBvcnQiLCJtYXhfcyI6MzAwLCJtZXRhIjp7InBsYW4iOiJwcm8ifX0.zHMxezQrKQHSJbz6ffciHtW_RCV8WWYojeuQ9oa3p-A";
const GOLDEN_RTVI =
  "v1.eyJhdWQiOiJicmlzdm94LXJ0dmkiLCJleHAiOjE3NjcyMjYyMDAsInNpZCI6ImdvbGRlbi0yIn0." +
  "kg-l-Emp5-CxMv7XqkIXi6RRHjtx0s1UzPiDxuVRzRU";

describe("session tokens", () => {
  it("match the golden vectors", () => {
    assert.equal(
      mintSessionToken(SECRET, "brisvox-webrtc", {
        ttlSeconds: 600,
        sid: "golden-1",
        agent: "support",
        maxSeconds: 300,
        meta: { plan: "pro" },
        now: NOW,
      }),
      GOLDEN_WEBRTC,
    );
    assert.equal(
      mintSessionToken(SECRET, "brisvox-rtvi", { ttlSeconds: 600, sid: "golden-2", now: NOW }),
      GOLDEN_RTVI,
    );
  });

  it("sign the payload bytes with the secret's UTF-8 bytes", () => {
    const token = mintSessionToken(SECRET, "brisvox-ws", { now: NOW });
    const [version, payload, signature] = token.split(".");
    assert.equal(version, "v1");
    const expected = createHmac("sha256", Buffer.from(SECRET, "utf8"))
      .update(Buffer.from(payload ?? "", "base64url"))
      .digest("base64url");
    assert.equal(signature, expected);
    const claims = JSON.parse(Buffer.from(payload ?? "", "base64url").toString("utf8")) as {
      aud: string;
      exp: number;
      sid: string;
    };
    assert.equal(claims.aud, "brisvox-ws");
    assert.equal(claims.exp, NOW + 60);
    assert.ok(claims.sid.length >= 16);
    assert.ok(!token.includes("="), "base64url without padding");
  });

  it("give every token its own sid", () => {
    const sids = new Set(
      Array.from({ length: 50 }, () => {
        const payload = mintSessionToken(SECRET, "brisvox-ws").split(".")[1] ?? "";
        return (JSON.parse(Buffer.from(payload, "base64url").toString("utf8")) as { sid: string }).sid;
      }),
    );
    assert.equal(sids.size, 50);
  });

  it("refuse bad arguments", () => {
    assert.throws(() => mintSessionToken("short", "brisvox-ws"), RangeError);
    assert.throws(() => mintSessionToken(SECRET, "brisvox-other" as "brisvox-ws"), RangeError);
    assert.throws(() => mintSessionToken(SECRET, "brisvox-ws", { ttlSeconds: 0 }), RangeError);
  });

  it("check the lifetime and the units", () => {
    mintSessionToken(SECRET, "brisvox-ws", { ttlSeconds: MAX_SESSION_TOKEN_LIFETIME_SECONDS, now: NOW });
    for (const ttlSeconds of [MAX_SESSION_TOKEN_LIFETIME_SECONDS + 1, 90_000, 10 * 365 * 86_400, -5, 60.5, Number.NaN]) {
      assert.throws(() => mintSessionToken(SECRET, "brisvox-ws", { ttlSeconds, now: NOW }), RangeError, `ttl ${ttlSeconds}`);
    }
    // A "now" in milliseconds, a fraction or a negative number.
    for (const now of [NOW * 1000, NOW + 0.5, -1, Number.NaN]) {
      assert.throws(() => mintSessionToken(SECRET, "brisvox-ws", { now }), RangeError, `now ${now}`);
    }
    for (const maxSeconds of [0, 86_401, 1.5, 300_000]) {
      assert.throws(() => mintSessionToken(SECRET, "brisvox-ws", { maxSeconds, now: NOW }), RangeError, `max ${maxSeconds}`);
    }
    mintSessionToken(SECRET, "brisvox-ws", { maxSeconds: 86_400, now: NOW });
  });
});
