/**
 * Mint short-lived session tokens for the brisvox server's browser-facing listeners.
 *
 * Your application backend signs a token for one visitor; the visitor's page hands it to the
 * server (`Authorization: Bearer <token>` on the WebRTC `POST /whip`; `?token=`, a bearer header
 * or the `brisvox-token.<token>` WebSocket subprotocol on the RTVI and generic WebSocket
 * listeners). The server verifies it with the same secret (`BRISVOX_SESSION_SECRET`), checks the
 * audience and the time window, and lets it open exactly one call. See `docs/SECURITY.md`.
 *
 * ```ts
 * import { mintSessionToken } from "@brisvox/agent";
 *
 * const token = mintSessionToken(process.env.BRISVOX_SESSION_SECRET!, "brisvox-webrtc", {
 *   ttlSeconds: 60,
 *   agent: "support",
 *   maxSeconds: 600,
 *   meta: { user: "u-42" },
 * });
 * ```
 *
 * The secret never goes to the browser. Mint a token per page load or per call, with a short TTL.
 *
 * @module
 */

import { createHmac, randomBytes } from "node:crypto";

/** The listeners a token can be for (the `aud` claim). */
export const SESSION_TOKEN_AUDIENCES = ["brisvox-webrtc", "brisvox-rtvi", "brisvox-ws"] as const;

/** One of {@link SESSION_TOKEN_AUDIENCES}. */
export type SessionTokenAudience = (typeof SESSION_TOKEN_AUDIENCES)[number];

/** The server refuses a shorter secret. */
export const MIN_SESSION_SECRET_BYTES = 32;

/** The server refuses a token whose `exp` is more than this many seconds ahead of its clock. */
export const MAX_SESSION_TOKEN_LIFETIME_SECONDS = 86_400;

/** The longest call (`max_s`) the server accepts, in seconds. */
const MAX_CALL_SECONDS = 86_400;

/** A Unix time in seconds stays below this until the year 5138; a larger "now" is milliseconds. */
const MAX_PLAUSIBLE_UNIX_SECONDS = 100_000_000_000;

/** Optional claims of {@link mintSessionToken}. */
export interface SessionTokenOptions {
  /** Whole seconds the token stays valid (default 60, at most 86400). */
  ttlSeconds?: number | undefined;
  /**
   * The single-use session id (a random one by default). The server refuses a second token with
   * the same id until it would have expired.
   */
  sid?: string | undefined;
  /** The agent the application intends; reaches the call's metadata as `session.agent`. */
  agent?: string | undefined;
  /** Longest call in whole seconds (1 to 86400). */
  maxSeconds?: number | undefined;
  /** Key/value pairs for the call's metadata (`session.meta.<key>`). */
  meta?: Record<string, string | number | boolean> | undefined;
  /** Unix seconds "now" (not milliseconds: a value that looks like milliseconds throws); for tests. */
  now?: number | undefined;
}

/** Returns `v1.<payload>.<signature>` for `audience`. Throws a `RangeError` for bad arguments. */
export function mintSessionToken(
  secret: string,
  audience: SessionTokenAudience,
  options: SessionTokenOptions = {},
): string {
  if (!SESSION_TOKEN_AUDIENCES.includes(audience)) {
    throw new RangeError(`audience must be one of ${SESSION_TOKEN_AUDIENCES.join(", ")}`);
  }
  const key = Buffer.from(secret, "utf8");
  if (key.length < MIN_SESSION_SECRET_BYTES) {
    throw new RangeError(`the session secret must be at least ${MIN_SESSION_SECRET_BYTES} bytes`);
  }
  const ttl = options.ttlSeconds ?? 60;
  if (!Number.isInteger(ttl) || ttl < 1 || ttl > MAX_SESSION_TOKEN_LIFETIME_SECONDS) {
    throw new RangeError(
      `ttlSeconds must be a whole number of seconds from 1 to ${MAX_SESSION_TOKEN_LIFETIME_SECONDS}`,
    );
  }
  const maxSeconds = options.maxSeconds;
  if (
    maxSeconds !== undefined &&
    (!Number.isInteger(maxSeconds) || maxSeconds < 1 || maxSeconds > MAX_CALL_SECONDS)
  ) {
    throw new RangeError(`maxSeconds must be a whole number of seconds from 1 to ${MAX_CALL_SECONDS}`);
  }
  const issued = options.now ?? Math.floor(Date.now() / 1000);
  if (!Number.isInteger(issued) || issued < 0 || issued >= MAX_PLAUSIBLE_UNIX_SECONDS) {
    throw new RangeError("now must be a whole number of Unix seconds, not milliseconds");
  }
  const claims: Record<string, unknown> = {
    aud: audience,
    exp: issued + ttl,
    sid: options.sid ?? randomBytes(16).toString("base64url"),
  };
  if (options.agent !== undefined) claims["agent"] = options.agent;
  if (maxSeconds !== undefined) claims["max_s"] = maxSeconds;
  if (options.meta !== undefined && Object.keys(options.meta).length > 0) claims["meta"] = options.meta;
  const payload = Buffer.from(JSON.stringify(claims), "utf8");
  const signature = createHmac("sha256", key).update(payload).digest();
  return `v1.${payload.toString("base64url")}.${signature.toString("base64url")}`;
}
