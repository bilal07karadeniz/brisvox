# Security: threat model and secure deployment

What an attacker can do to a brisvox server, what the server does about it, and what
you must do yourself. Source of the findings: an independent security review of 2026-10-13. Code: `brisvox_transport::auth`
(tokens), `brisvox_transport::webrtc` (limits), `brisvox_server::hosting` (fail-closed rules),
`brisvox_server::health` (`/metrics`).

## What is worth attacking

- **Your provider bill.** Every call spends STT, LLM and TTS minutes. Anyone who can open a call
  can burn them.
- **Audio and transcripts** of callers and of the bot, the LLM context, recordings.
- **Tool calls.** `SideEffect` tools do real things (transfer a call, book a table).
- **Capacity.** The session limit is shared: a flood of idle connections turns real callers away.
- **Secrets in the environment**: provider keys, carrier tokens.

## Where the server listens, and what protects each door

| Listener | Plain or TLS | Protection |
|---|---|---|
| WebRTC signaling (`BRISVOX_WEBRTC_ADDR`), including its WebSocket fallback at `BRISVOX_WEBRTC_WS_PATH` | plain HTTP / `ws://` | session token or static token (the same for `POST /whip` and the `/ws` upgrade); `Origin` check on the upgrade; per-address limits; **needs an HTTPS proxy** |
| Generic WebSocket (`BRISVOX_WS_ADDR`) | plain `ws://` | session token or static token |
| RTVI (`BRISVOX_RTVI_ADDR`) | plain `ws://` | session token or static token; optional `Origin` allow-list |
| Twilio (`BRISVOX_TWILIO_ADDR`) | plain `ws://` | `X-Twilio-Signature` over the configured public URL |
| Telnyx (`BRISVOX_TELNYX_ADDR`) | plain `ws://` | the stream token (and an unguessable path) |
| SIP (`BRISVOX_SIP_ADDR`) | UDP/TCP: cleartext. SIPS (`BRISVOX_SIP_TLS_ADDR`): TLS 1.2+ with SDES-SRTP media (`BRISVOX_SIP_SRTP`) | source ACL, connection limits, handshake deadline and idle timeout, digest; SDES keys only over TLS, in-dialog requests of a TLS dialog only over TLS; SRTP authentication-failure limit; cleartext UDP/TCP only on a private trunk or behind an SBC |
| Health (`BRISVOX_HEALTH_ADDR`) | plain HTTP | probes carry counts only; `/metrics` behind a token off loopback |
| Callback (`BRISVOX_CALLBACK_ADDR`) | plain HTTP | bearer or basic secret, constant time |

**No listener terminates TLS.** Put a reverse proxy in front of every one that is reachable from
the internet. For WebRTC this is not optional: the media is DTLS-SRTP, but the DTLS certificate
fingerprint travels in the SDP offer and answer over the signaling request. If that request is
plain HTTP, anyone on the path can swap the fingerprints and sit in the middle of the media; the
encryption is then decoration. Browsers also require HTTPS for the microphone on every host but
`localhost`.

## Fail closed

The generic WebSocket, RTVI, WebRTC signaling and Telnyx listeners **refuse to start** when bound
to a non-loopback address (`0.0.0.0`, a LAN or public address) without authentication:

| Listener | What satisfies the check |
|---|---|
| Generic WebSocket | `BRISVOX_SESSION_SECRET`, or `BRISVOX_WS_TOKEN` |
| RTVI | `BRISVOX_SESSION_SECRET`, or `BRISVOX_RTVI_TOKEN` |
| WebRTC signaling | `BRISVOX_SESSION_SECRET`, or `BRISVOX_WEBRTC_TOKEN` |
| Telnyx | `BRISVOX_TELNYX_STREAM_TOKEN` (Telnyx sends it; there are no session tokens for a carrier) |

`BRISVOX_INSECURE_LISTENERS=1` starts them anyway and the server logs one warning per exposed
listener ("anyone who can reach it spends your provider keys"). Use it for a private network or a
container whose port you publish on loopback only (`deploy/compose.yaml` does). Loopback binds
(`127.0.0.1`, `[::1]`) need nothing: that is development. Twilio always verifies its signature,
and SIP refuses an open trunk by its own rule.

The same rule is in the transport: `WebRtcConfig::validate` refuses `cors_origin = "*"` without
`bearer_token` or `sessions`, because any web page could otherwise start calls.

Static tokens (`BRISVOX_*_TOKEN`) keep working for development. They are shared by every client
and never expire, so the server logs a warning at start for each one in use.

## Session tokens

A token is minted by **your backend** (which has your login, your user, your quota) for one
visitor, and lives for seconds to minutes. The server only verifies. One token opens at most one
call. If it leaks, it is useless a minute later and cannot be used twice.

```
v1.<base64url(payload JSON)>.<base64url(HMAC-SHA256(secret, payload JSON bytes))>
```

base64url without padding. The HMAC key is the secret's UTF-8 bytes, the signed bytes are the
decoded payload exactly as sent. Payload fields:

| Field | Type | Meaning |
|---|---|---|
| `aud` | string | `brisvox-webrtc`, `brisvox-rtvi` or `brisvox-ws`: which listener the token is for. A token for one is refused by the others. |
| `exp` | integer | Unix **seconds** (not milliseconds), at most **24 hours** after the server's now (plus the skew below); a later `exp` is refused (`401`, "token lifetime too long"). The token is refused from `exp` on (plus the skew). |
| `nbf` | integer, optional | Unix seconds. Refused before it (default 0). Must be before `exp`. |
| `sid` | string | Your opaque session id, 1 to 128 bytes, no control characters. **Single use**: the server remembers it until `exp` plus the skew and refuses a second token with the same `sid`. Use a random value per token. |
| `agent` | string, optional | The agent you intend; reaches the call's metadata (`session.agent`). |
| `max_s` | integer, optional | Longest call in seconds, 1 to 86400. Enforced on the WebRTC signaling port: for WebRTC calls (the call is hung up) and for the `/ws` WebSocket fallback served on that same port (the call ends with `stop{ended}`). The standalone WebSocket listener (`BRISVOX_WS_ADDR`) and the other listeners report it in the metadata (`session.max_s`) and do not enforce it yet. |
| `meta` | object, optional | At most 16 entries, keys up to 64 bytes, scalar values (strings up to 256 bytes, numbers, booleans). Reaches the call's metadata as `session.meta.<key>`. |

The whole token is at most 4096 bytes (413 above that).

**What the server checks, in this order:** length; three dot-separated parts and the `v1` prefix;
**the HMAC signature (constant time, current secret then the previous one) before the payload is
parsed**, so a stranger who cannot sign never reaches the JSON parser and every wrong signature,
tampered payload or wrong secret gives the same answer; the payload's limits; `aud`; `nbf`/`exp`
against the server's clock with **30 seconds of skew** on both sides (your backend's clock and
the server's differ a little; do not rely on the skew, run NTP); and last, the single-use set.

**Why `exp` is capped.** The server remembers every `sid` until `exp` plus the skew. A backend that
mints `exp` in milliseconds or with a ten-year TTL would park each `sid` in the set almost for good,
fill it, and make every later token a `503`. So a token whose `exp` is more than 24 hours
(`MAX_TOKEN_LIFETIME`) ahead is refused, and the minting helpers (Rust `mint_token`, Python
`mint_session_token`, TypeScript `mintSessionToken`) refuse such a TTL, a non-integer or
millisecond-looking `now`, and a `max_s` outside 1 to 86400. Mint TTLs of 30 to 120 seconds.

**The single-use set is bounded** (100000 live session ids). Expired entries are dropped first.
When it is still full the server **refuses new tokens (503)** instead of evicting an unexpired
entry, because evicting would let a spent token be used again. Only holders of the secret can add
entries, so an outsider cannot fill it; if you legitimately mint more than 100000 live tokens,
shorten the TTL. When the set is full the server logs a warning (at most once a minute) and answers
`503`. A token refused for capacity (the server is full, a per-address limit) is **not**
used up: the client can retry with the same token. A token is used up once the server has a call
slot for it, so a token whose offer is then refused as malformed (400), or fails on a server error
(500) while the call is built, is **spent**: mint a new one for the retry.

**The set lives in the server's memory.** A restart empties it: a token that was already used but
has not expired opens one more call after the restart, and several server instances (each with its
own set, behind a load balancer) each accept a token once. The TTL bounds this; keep it short (30
to 120 seconds) and do not mint long-lived tokens.

**What reaches the call.** `session_id`, `session.agent`, `session.max_s` and
`session.meta.<key>` are added to the call's metadata (`CallInfo::metadata`, in the transport's
`Started` event), where the host application can read them to know who is calling. The generic
WebSocket adapter strips any `session_id` / `session.*` keys the *client* put in its `start`
message: only the signed values count.

### Where the token goes

| Listener | How the client presents it |
|---|---|
| WebRTC | `Authorization: Bearer <token>` on `POST /whip`. `DELETE` on the session URL needs no token once session tokens are in use: the URL (128 random bits from the system RNG, known only to the client the `201` went to) is the capability, and the token that opened the call is single use and may have expired by hang-up time. With only a static token, `DELETE` needs it as before (RFC 9725). |
| RTVI | `?token=<token>` (what Pipecat client SDKs send), `Authorization: Bearer`, or the WebSocket subprotocol `brisvox-token.<token>`. |
| Generic WebSocket | the same three. |

A browser's `WebSocket` cannot set headers, so a web page uses the subprotocol:

```js
const ws = new WebSocket("wss://agent.example.com/rtvi", ["brisvox", "brisvox-token." + token]);
```

The server selects `brisvox` (so the token is not echoed back); a client that offers only the
`brisvox-token.` entry gets no subprotocol in the answer (the token entry is never echoed; a
browser then refuses the connection, so pages offer both entries). **Prefer the header or the subprotocol to the
query string**: a query string lands in proxy and CDN logs, browser history and `Referer`s. If you
must use `?token=`, strip the query from your proxy's access log (nginx: log `$uri`, not
`$request`; Caddy: `log { format filter { request>uri query { delete token } } }`). Every answer
the server produces during signaling or the upgrade carries `Cache-Control: no-store`.

### Minting

The secret is `BRISVOX_SESSION_SECRET` on the server (at least 32 bytes; shorter is refused at
start). Generate it with `openssl rand -base64 48`. Your backend holds the same value and never
sends it to a browser.

Rust (`brisvox_transport::auth`, `features = ["auth"]`):

```rust
use std::collections::BTreeMap;
use brisvox_core::Secret;
use brisvox_transport::auth::{Audience, TokenSpec, mint_token};

let now = clock.wall().duration_since(std::time::UNIX_EPOCH)?.as_secs();
let token = mint_token(
    &Secret::new(std::env::var("BRISVOX_SESSION_SECRET")?),
    &TokenSpec {
        audience: Audience::Webrtc,
        sid: random_id(),               // unique per token
        not_before: 0,
        expires: now + 60,
        agent: Some("support".into()),
        max_s: Some(600),
        meta: BTreeMap::from([("user".into(), "u-42".into())]),
    },
)?;
```

Python (`sdk/python/brisvox_agent/session_token.py` has this, with tests):

```python
import base64, hashlib, hmac, json, os, secrets, time

def mint(aud, ttl=60, **claims):
    payload = json.dumps(
        {"aud": aud, "exp": int(time.time()) + ttl, "sid": secrets.token_urlsafe(16), **claims},
        separators=(",", ":"),
    ).encode()
    sig = hmac.new(os.environ["BRISVOX_SESSION_SECRET"].encode(), payload, hashlib.sha256).digest()
    b64 = lambda b: base64.urlsafe_b64encode(b).rstrip(b"=").decode()
    return f"v1.{b64(payload)}.{b64(sig)}"

token = mint("brisvox-webrtc", agent="support", max_s=600, meta={"user": "u-42"})
```

TypeScript / Node (`sdk/typescript/src/session-token.ts`, exported as `mintSessionToken`):

```ts
import { createHmac, randomBytes } from "node:crypto";

export function mint(aud: string, ttl = 60, claims: object = {}): string {
  const payload = Buffer.from(JSON.stringify({
    aud, exp: Math.floor(Date.now() / 1000) + ttl, sid: randomBytes(16).toString("base64url"), ...claims,
  }));
  const sig = createHmac("sha256", process.env.BRISVOX_SESSION_SECRET!).update(payload).digest();
  return `v1.${payload.toString("base64url")}.${sig.toString("base64url")}`;
}
```

The three implementations produce byte-identical tokens for the same input (the Python and
TypeScript tests and the Rust test `tokens_minted_by_the_sdk_helpers_verify` share golden
vectors). Integers only for `exp`, `nbf` and `max_s` (a float timestamp does not parse).

Advice: mint on demand, per page load or per call; TTL 30 to 120 seconds (the token is only
needed until the call starts); never log tokens; do not mint for anonymous visitors without your
own rate limit, because every token you hand out is a call you pay for.

### Rotation

1. Generate a new secret. Set it as `BRISVOX_SESSION_SECRET` and the old one as
   `BRISVOX_SESSION_SECRET_PREV`; restart. Tokens signed with either verify.
2. Point your backend at the new secret.
3. After the longest token TTL, remove `BRISVOX_SESSION_SECRET_PREV`.

## Flood limits (WebRTC signaling)

Opening a WebRTC call costs the server a call slot for up to `connect_timeout` (15 s) before the
browser even connects, and an attacker needs no media to do that. The signaling endpoint therefore
limits, in this order, after the credential check:

1. **Offers per address per minute**: a token bucket, default 5 (burst 5, one more every 12 s;
   `BRISVOX_WEBRTC_OFFERS_PER_MINUTE`, `off` disables). Over it: `429` with `Retry-After`. An IPv6
   client counts as its `/64`. The table of tracked addresses is capped at 8192; when it is full
   and no bucket can be dropped, the offer is let through untracked (counted in
   `rate_table_full`), because failing closed there would let an attacker lock everyone out; the
   other limits still apply.
2. **Calls in total** (`BRISVOX_MAX_SESSIONS`, twice that at the transport): `503`.
3. **Calls still handshaking**: a smaller pool, default a quarter of `BRISVOX_MAX_SESSIONS` (a library user of the transport: a quarter of `max_calls`)
   (`BRISVOX_WEBRTC_MAX_HANDSHAKING`). A call leaves it when ICE and DTLS complete, or when
   `connect_timeout` abandons it. Offers that never connect cannot take more than that share.
   Over it: `503` with `Retry-After`.
4. **Calls per address at once**, default 8 (`BRISVOX_WEBRTC_MAX_CALLS_PER_IP`): `429`.

**Behind a reverse proxy the TCP peer is the proxy**, so without more every visitor would share
one address and one bucket. Set `BRISVOX_TRUSTED_PROXIES` (CIDR list, for example
`10.0.0.0/8,172.16.0.0/12`). Only when the TCP peer is in that list does the server read
**one** forwarding header, chosen with `BRISVOX_CLIENT_IP_HEADER`: `x-forwarded-for` (the default),
`forwarded` (RFC 7239) or `x-real-ip` (one address; a list or a repeated header is ignored, and the
TCP peer stands). The other headers are never looked at, so a client cannot plant a value in the
header your proxy does not write. Set it to the one your proxy writes. The server takes **the last
hop that is not itself a trusted proxy**: the entry your nearest proxy wrote. Entries further left
are whatever the client claimed and are ignored, so a client cannot choose its identity by sending
its own header. From any other peer the headers are ignored entirely. Make your proxy *append* to
(or replace) the header rather than pass the client's through. If the list is empty, the
per-address limits apply to the proxy's address: set it.

**List the proxies' real addresses, never `0.0.0.0/0` (or `::/0`).** A catch-all makes every
client a "trusted proxy": a client that connects directly then writes the header itself. Every
entry of the same address family is skipped as "a proxy" (the limit falls back to the peer), but
an entry of the other family (an IPv6 hop when only `0.0.0.0/0` is listed) is believed, so a client
picks its own address and the per-address limits stop limiting. Treat a catch-all as "limits off".

The per-address **call cap** (item 4) is keyed like the offer bucket: an IPv4-mapped IPv6 address
counts as its IPv4 address and any other IPv6 address as its `/64`, so a client cannot multiply its
cap with the addresses of its own prefix.

**Signaling connections per address at once**: default 8 (`BRISVOX_WEBRTC_HTTP_MAX_PER_IP`,
`0` = no limit), so a stranger cannot hold every signaling slot with idle connections (the excess
is closed unanswered). Connections from a trusted proxy are exempt (they are many visitors'; the
proxy's own limits apply). An IPv6 client counts as its `/64`.

Counters (`WebRtcServer::stats`): `offers_unauthorized`, `offers_rate_limited`,
`offers_ip_limited`, `offers_handshake_limited`, `rate_table_full`.

The generic WebSocket, RTVI and Telnyx listeners are bounded by `max_calls`, the handshake and
start timeouts and the manager's admission (`docs/OPERATIONS.md`); they have no per-address limit:
put one in the proxy.

## Health and metrics

`/healthz` and `/readyz` carry counts only and stay open: Kubernetes probes the pod's address and
a container runtime probes from outside the network namespace, so binding the health listener to
loopback would break them. `/metrics` carries call counts, error rates and latencies; it is
**off (404) on a non-loopback health address unless `BRISVOX_METRICS_TOKEN` is set**, and then
requires `Authorization: Bearer <token>` (constant time, `401` without it). The one endpoint
that changes anything, `POST /admin/reload` (agent reload, `docs/OPERATIONS.md`), has its own
token, `BRISVOX_ADMIN_TOKEN`: a scrape token never opens it, and without a token it is served
only on a loopback address, to local processes and not to a browser (`Origin` is refused), and
is off on any other address. Prometheus:

```yaml
scrape_configs:
  - job_name: brisvox
    authorization: { credentials_file: /etc/prometheus/brisvox-metrics-token }
    static_configs: [{ targets: ["brisvox:8081"] }]
```

The server logs a warning when it turns `/metrics` off for this reason.
`BRISVOX_INSECURE_LISTENERS=1` serves it openly (a private network).

## The WebSocket fallback on the signaling port

`GET /ws` with a WebSocket upgrade starts a call exactly as `POST /whip` does, so it gets the same
protection and nothing weaker:

- **Credential.** The signaling listener's own: the static `BRISVOX_WEBRTC_TOKEN`, or a session
  token with `aud` `brisvox-webrtc` (the same token a page would send to `/whip`; no new
  audience). It travels in `Authorization: Bearer`, the subprotocol `brisvox-token.<token>` (what
  a browser can set) or `?token=` (logged by proxies: prefer the others). Missing: 401. Invalid,
  expired, wrong audience or already used: 403. Over 4 KiB: 413. Fail closed: with neither
  secret configured the listener is open exactly as `POST /whip` is, and the server refuses to
  start such a listener on a non-loopback address.
- **Single use.** A session token is checked, then used up only after the call has its slot (a
  refusal for capacity does not burn it). Two exceptions, both for a request that
  never started a call. (1) The page hangs up (`DELETE`) a WebRTC session that **had not
  connected** (ICE and DTLS incomplete): the token is given back **before the `200` is sent**, so
  the page's WebSocket fallback right after it finds the token usable. (2) The server could not
  serve a request it had already accepted the token for (the media relay is unavailable: 503).
  Nothing else gives a token back: not a connect timeout, not a closed or lost connection, not a
  `DELETE` of a session that had connected (the 4 s page timer can fire while the browser is
  still finishing ICE, so a late WebSocket attempt may find the token spent: the page says so,
  mint a new token). **A session id is refunded at most once** (until `exp` + skew, it is
  remembered as refunded in a set bounded like the single-use set; when that set is full a
  refund is refused, never an entry evicted): a token cannot be cycled through
  POST / `DELETE` / POST to hold handshake slots until it expires. A token that opened a call is
  never given back, so a token still opens at most one call; the retries the one refund allows
  are bounded by the offer rate and the call caps below, and a refunded session cost the provider
  nothing (no engine call existed).
- **Origin.** A browser does not apply CORS to WebSockets, so the server checks `Origin` itself,
  to the same effect as the CORS header of `POST`: with `BRISVOX_WEBRTC_CORS_ORIGIN=*` any
  origin (credentials are then required), with an origin that origin or the page's own host,
  with none only a page served from the request's `Host` (compared without the scheme). A
  request without `Origin` (not a browser) is judged by its token alone. A proxy that rewrites
  `Host` makes the default rule fail: set `BRISVOX_WEBRTC_CORS_ORIGIN` to the page's origin.
- **Limits.** The same offer rate per address (`BRISVOX_WEBRTC_OFFERS_PER_MINUTE`: an upgrade
  counts as an offer, so a page that falls back uses two), per-address call cap
  (`BRISVOX_WEBRTC_MAX_CALLS_PER_IP`), handshake pool (`BRISVOX_WEBRTC_MAX_HANDSHAKING`, held
  until the `start` message), the call limit, and `BRISVOX_MAX_SESSIONS` when the call reaches
  the engine. The upgrade leaves the HTTP connection pools once it is a call (it is bounded by the
  call limits then). A session token's `max_s` ends the call (`stop{ended}`); this is the one
  WebSocket path that enforces it (the standalone `BRISVOX_WS_ADDR` listener only reports it).
- **Bounds.** HTTP head 8 KiB and 64 fields, `Sec-WebSocket-Key` a 16-byte base64 value, version 13
  only, every refusal plain HTTP before the upgrade; after it the generic protocol's frame limits
  (64 KiB audio, 16 KiB control), `start` in 5 s, pings, bounded queues
  (`docs/WEBSOCKET_PROTOCOL.md`). `BRISVOX_WEBRTC_WS_PATH=off` closes the door (404).

## Reverse proxy checklist

- HTTPS on every public listener; HTTP/1.1 upgrade for the WebSocket paths; a read timeout above
  the longest silence you expect (the server pings every 10 s).
- WebRTC: proxy only the TCP signaling port (it also carries the WebSocket fallback at `/ws`, which needs the `Upgrade` pass-through above). Media is UDP on `BRISVOX_WEBRTC_UDP_ADDR`; forward it
  and set `BRISVOX_WEBRTC_CANDIDATE_IP`. Serve the page from the proxied origin, or set
  `BRISVOX_WEBRTC_CORS_ORIGIN` to the page's exact origin (never `*` without credentials: the
  server refuses that).
- Pass the client address in `X-Forwarded-For` and list the proxy in `BRISVOX_TRUSTED_PROXIES`.
- Do not log query strings for RTVI/WebSocket paths if clients use `?token=`.
- Keep `8081` (health), the callback listener and SIP off the public internet.
- Limit connections per address and request rates in the proxy too; the server's own limits are
  the last line.

nginx fragment for WebRTC signaling:

```nginx
location /whip {
    proxy_pass http://127.0.0.1:8088;
    proxy_set_header X-Forwarded-For $remote_addr;   # replaces what the client sent
    proxy_set_header Host $host;
    client_max_body_size 128k;
    limit_req zone=whip burst=10 nodelay;
}
```

## Covered since, and what is still open

Delivered after this document's first version (2026-10-13): an agent link refuses `ws://` to a
public host unless `allow_insecure`; Twilio stream URLs are signed per call and single
use; `SideEffect` tools can require a confirmation in a later user turn (`confirm`,
docs/TOOLS.md); recordings can be encrypted at rest with age (docs/PRIVACY.md);
browsers get short-lived TURN credentials, ICE-TCP covers UDP-blocking networks, and a server
without UDP can relay its media through TURN (docs/DEPLOY.md).

Still open: SIP over TLS with SDES-SRTP is built and tested against scripted peers (
docs/TELEPHONY.md "SIP over TLS and SRTP") but not yet against a real carrier, has no
DTLS-SRTP, no AEAD/256-bit suites and no certificate hot reload (an adversarial review of the SIP/SRTP work is fixed: the dialog's transport is recorded
from the real connection and in-dialog requests must stay on TLS, a re-key brings a fresh local key, a
suite change needs a new key, TLS connections have an idle timeout, key lifetimes below 2^31 are
refused, SDES keys are redacted from logs, outbound TLS through a proxy verifies the callee's domain,
and a burst of failed SRTP authentications ends the call), and the cleartext UDP/TCP listeners stay private-trunk-only; `max_s` is enforced
on the WebRTC signaling port only (WebRTC calls and its `/ws` fallback), not on the standalone WebSocket listener or the carriers' listeners; TLS for every listener terminates at a reverse proxy, never in the server
itself (mandatory for WebRTC signaling: without HTTPS the DTLS fingerprints can be swapped); the
replay set and the Twilio used-set live in one process's memory (see above).

## Variable reference

| Variable | Default | Meaning |
|---|---|---|
| `BRISVOX_SESSION_SECRET` | unset | Signs session tokens (at least 32 bytes). Enables them on the generic WebSocket, RTVI and WebRTC listeners. |
| `BRISVOX_SESSION_SECRET_PREV` | unset | The previous secret, still verified during a rotation. Needs `BRISVOX_SESSION_SECRET`. |
| `BRISVOX_WS_TOKEN` | unset | Static token of the generic WebSocket listener (development). |
| `BRISVOX_INSECURE_LISTENERS` | `false` | Allow non-loopback listeners without authentication, and an open `/metrics` there. |
| `BRISVOX_TRUSTED_PROXIES` | empty | CIDR list of reverse proxies whose forwarding header is believed (WebRTC). Never `0.0.0.0/0`. |
| `BRISVOX_CLIENT_IP_HEADER` | `x-forwarded-for` | The only forwarding header believed behind a trusted proxy: `x-forwarded-for`, `forwarded` or `x-real-ip` (one address). |
| `BRISVOX_WEBRTC_HTTP_MAX_PER_IP` | `8` | Signaling connections per client address at once (`0` = no limit); trusted proxies are exempt. |
| `BRISVOX_WEBRTC_MAX_CALLS_PER_IP` | `8` | Calls per client address at once, or `off`. |
| `BRISVOX_WEBRTC_MAX_HANDSHAKING` | a quarter of `BRISVOX_MAX_SESSIONS` | Calls in their ICE/DTLS handshake at once. |
| `BRISVOX_WEBRTC_OFFERS_PER_MINUTE` | `5` | Offers per client address per minute, or `off`. |
| `BRISVOX_METRICS_TOKEN` | unset | Bearer token `/metrics` requires on a non-loopback health address. |
| `BRISVOX_ADMIN_TOKEN` | unset | Bearer token `POST /admin/reload` requires. Not the metrics token. Unset: open on a loopback health address (a request with an `Origin` header is refused), off (404) elsewhere, `BRISVOX_INSECURE_LISTENERS` does not open it. |
